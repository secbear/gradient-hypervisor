{ lib, craneLib, cacert }:
let
  root = ../../backend;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources root)
      ../../backend/rustfmt.toml
      ../../backend/gradient-autoscaler/migrations
      ../../backend/gradient-autoscaler/src/github/gradient-worker.yml
      ../../backend/gradient-autoscaler/tests/data
    ];
  };

  commonArgs = {
    inherit src;
    pname = "gradient-autoscaler";
    version = "0.1.0";
    strictDeps = true;
    cargoExtraArgs = "--locked --package gradient-autoscaler";
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (commonArgs // {
  inherit cargoArtifacts;
  doCheck = false;

  passthru = {
    clippy = craneLib.cargoClippy (commonArgs // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
    });

    fmt = craneLib.cargoFmt { inherit src; inherit (commonArgs) pname version; };

    tests = craneLib.cargoNextest (commonArgs // {
      inherit cargoArtifacts;
      cargoNextestExtraArgs = "--no-fail-fast";
      SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    });
  };

  meta = {
    description = "Temporary Gradient team workers launch as GitHub Actions runs or Kubernetes pods.";
    homepage = "https://github.com/wavelens/gradient-hypervisor";
    license = lib.licenses.mit;
    mainProgram = "gradient-autoscaler";
    platforms = lib.platforms.unix;
  };
})
