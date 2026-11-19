{ lib, runCommand, kubernetes-helm, yq-go, jq }:
let
  chart = lib.fileset.toSource {
    root = ../../helm/gradient-autoscaler;
    fileset = ../../helm/gradient-autoscaler;
  };
in
runCommand "gradient-autoscaler-helm" { nativeBuildInputs = [ kubernetes-helm yq-go jq ]; } ''
  export HOME=$TMPDIR
  helm lint --strict ${chart}
  helm lint --strict ${chart} -f ${chart}/ci/kubernetes-values.yaml
  helm template check ${chart} -n ci -f ${chart}/ci/kubernetes-values.yaml > $out
  yq -o=json 'select(.kind == "Role" and .metadata.namespace == "gradient-workers") | .rules' $out | jq -e '
    map(.verbs |= sort) | sort_by(.resources) == [
      {"apiGroups": [""], "resources": ["pods"], "verbs": ["create", "delete", "get", "list"]},
      {"apiGroups": [""], "resources": ["secrets"], "verbs": ["create", "delete", "get", "update"]}
    ]
  ' >/dev/null
  yq -r 'select(.kind == "ConfigMap") | .data."config.json"' $out | jq -e '
    .connections[] | select(.name == "platform") |
    .workerCapabilities == {"eval": false, "secrets": false} and
    (.kubernetes.pools.x86.podOptions |
      .volumes[0].secret.secretName == "worker-credentials" and
      .volumes[1].emptyDir == {} and
      .volumeMounts[0].mountPath == "/run/secrets/worker" and
      .volumeMounts[1].mountPath == "/scratch" and
      .env[0].valueFrom.fieldRef.fieldPath == "spec.nodeName")
  ' >/dev/null
  helm template preview ${chart} -n ci -f ${chart}/ci/kubernetes-values.yaml \
    --set connections.platform.buildScaling.mode=preview \
    --set 'connections.platform.buildScaling.profiles={x86}' \
    --set connections.platform.buildScaling.fallbackRamMb=2048 \
    --set connections.platform.kubernetes.pools.x86.buildResources.cpuCount=8 \
    --set connections.platform.kubernetes.pools.x86.buildResources.ramTotalMb=28000 \
    --set connections.platform.kubernetes.pools.x86.buildResources.cpuCoreScore=4000 \
    > preview.yaml
  yq -r 'select(.kind == "ConfigMap") | .data."config.json"' preview.yaml | jq -e '
    .connections[] | select(.name == "platform") |
    .buildScaling == {"mode": "preview", "profiles": ["x86"], "fallbackRamMb": 2048} and
    (.kubernetes.pools.x86 |
      .cpu == "8" and .memory == "32Gi" and
      .buildResources == {"cpuCount": 8, "ramTotalMb": 28000, "cpuCoreScore": 4000})
  ' >/dev/null
''
