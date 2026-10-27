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
  yq -r 'select(.kind == "ConfigMap") | .data."config.json"' $out | jq -e '
    .connections[] | select(.name == "platform") |
    .workerCapabilities == {"eval": false, "secrets": false}
  ' >/dev/null
''
