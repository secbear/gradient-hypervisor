{ lib, runCommand, kubernetes-helm }:
let
  chart = lib.fileset.toSource {
    root = ../../helm/gradient-autoscaler;
    fileset = ../../helm/gradient-autoscaler;
  };
in
runCommand "gradient-autoscaler-helm" { nativeBuildInputs = [ kubernetes-helm ]; } ''
  export HOME=$TMPDIR
  helm lint --strict ${chart}
  helm lint --strict ${chart} -f ${chart}/ci/kubernetes-values.yaml
  helm template check ${chart} -n ci -f ${chart}/ci/kubernetes-values.yaml > $out
''
