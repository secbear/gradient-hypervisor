{ lib, craneLib }:
let
  root = ../.;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources root)
      ../rustfmt.toml
      ../gha-autoscaler/migrations
      ../gha-autoscaler/src/gradient-worker.yml
      ../gha-autoscaler/tests/data
    ];
  };

  commonArgs = {
    inherit src;
    pname = "gradient-gha-autoscaler";
    version = "0.1.0";
    strictDeps = true;
    cargoExtraArgs = "--locked --package gradient-gha-autoscaler";
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
    });
  };

  meta = {
    description = "Starts temporary Gradient team workers as GitHub Actions runs";
    homepage = "https://github.com/wavelens/gradient-hypervisor";
    license = lib.licenses.mit;
    mainProgram = "gradient-gha-autoscaler";
    platforms = lib.platforms.unix;
  };
})
