{ pkgs, runCommand, jq, nixosModule }:
let
  evaluated = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit pkgs;
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      nixosModule
      {
        system.stateVersion = "26.05";
        services.gradient-autoscaler = {
          enable = true;
          publicUrl = "https://autoscaler.example.com";
          connections.platform = {
            gradientUrl = "https://gradient.example.com";
            team = "platform";
            apiKeyFile = "/run/secrets/gradient-api-key";
            workerCapabilities = { eval = false; secrets = false; };
            kubernetes = {
              tokenFile = "/run/secrets/kubernetes-token";
              namespace = "gradient-workers";
              image = "registry.example.com/gradient-worker:2.0.1";
              pools.x86.system = "x86_64-linux";
            };
          };
        };
      }
    ];
  };
  configFile = evaluated.config.systemd.services.gradient-autoscaler.environment.GRADIENT_AUTOSCALER_CONFIG;
in
runCommand "gradient-autoscaler-capabilities-module" { nativeBuildInputs = [ jq ]; } ''
  jq -e '.connections[0].workerCapabilities ==
    {"build": true, "network": true, "secrets": false, "eval": false}' ${configFile} >$out
''
