{ runCommand, python3 }:
runCommand "gradient-kubernetes-fixture" { nativeBuildInputs = [ python3 ]; } ''
  python -B -m unittest discover -s ${./.} -p test_prepared.py -v
  touch "$out"
''
