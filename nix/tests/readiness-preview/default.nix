{
  gradient,
  autoscaler,
  system,
}:
let
  driver =
    (autoscaler.override {
      cargoBuildExtraArgs = "--example readiness-preview";
    }).overrideAttrs
      (old: {
        pname = "readiness-preview";
        meta = old.meta // {
          mainProgram = "readiness-preview";
        };
      });
  config = {
    listen = "127.0.0.1:9";
    publicUrl = "http://127.0.0.1:9/";
    stateDir = "/run/readiness-preview-unused";
    connections = [
      {
        name = "readiness-preview";
        team = "readiness";
        gradientUrl = "http://127.0.0.1:3000/";
        apiKeyFile = "/run/readiness-preview-api-key";
        workerCapabilities = {
          build = true;
          network = true;
          eval = true;
          secrets = false;
          exec = false;
        };
        maxLaunches = 4;
        maxLaunchesPerKind = 4;
        buildScaling = {
          mode = "preview";
          profiles = [ "checked" ];
          maxNewWorkers = 1;
          queueWait = 0;
          startupGrace = 600;
          fallbackRamMb = 64;
        };
        kubernetes = {
          apiServer = "http://127.0.0.1:9/";
          namespace = "readiness-preview";
          tokenFile = "/run/gradient-preview-no-kubernetes-token";
          image = "registry.invalid/readiness-preview:fixture";
          pools.checked = {
            inherit system;
            features = [ "readiness-fixture" ];
            maxJobs = 2;
            buildResources = {
              cpuCount = 2;
              ramTotalMb = 1024;
              cpuCoreScore = 111;
              buildCores = 1;
            };
          };
        };
      }
    ];
  };
in
gradient.checks.${system}.gradient-worker-readiness.extend {
  modules = [
    ({ lib, ... }: {
      name = lib.mkForce "gradient-worker-readiness-preview";
      nodes.server = {
        environment.systemPackages = [ driver ];
        environment.etc."readiness-preview.json".text = builtins.toJSON config;
      };
      readiness.checkpointScript = builtins.readFile ./checkpoint.py;
      testScript = lib.mkAfter ''
        assert PREVIEW_CHECKPOINTS == set(PREVIEW_EXPECTED), PREVIEW_CHECKPOINTS
      '';
    })
  ];
}
