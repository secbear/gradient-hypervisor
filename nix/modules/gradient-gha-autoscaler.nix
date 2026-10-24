self:
{ config, lib, pkgs, ... }:
let
  inherit (lib) literalExpression mapAttrsToList mkEnableOption mkIf mkOption types;

  cfg = config.services.gradient-gha-autoscaler;
  credentialsDir = "/run/credentials/gradient-gha-autoscaler.service";

  connectionOptions = { name, ... }: {
    options = {
      gradientUrl = mkOption {
        type = types.str;
        example = "https://gradient.example.com";
        description = ''
          Base URL of the Gradient server.
          GitHub-hosted runners connect to its `/proto` endpoint, so it must be reachable from the internet.
        '';
      };

      team = mkOption {
        type = types.str;
        example = "platform";
        description = ''
          Name of the Gradient team whose workers the runs provide.
          Projects receive them through their grant of the team's workers.
        '';
      };

      apiKeyFile = mkOption {
        type = types.str;
        example = "/run/secrets/gradient-api-key";
        description = ''
          Path to a file holding a Gradient API key of a user with the Admin role in {option}`services.gradient-gha-autoscaler.connections.<name>.team`.
          The service reads it through a systemd credential, so it stays out of the Nix store.
        '';
      };

      githubTokenFile = mkOption {
        type = types.str;
        example = "/run/secrets/github-token";
        description = ''
          Path to a file holding a fine-grained GitHub token for {option}`services.gradient-gha-autoscaler.connections.<name>.repository`
          with Actions, Contents, Workflows and Webhooks write access.
        '';
      };

      repository = mkOption {
        type = types.str;
        example = "acme/gradient-runners";
        description = ''
          GitHub repository, as `owner/name`, whose Actions minutes the worker runs spend.
          The service commits {file}`.github/workflows/gradient-worker.yml` to its default branch.
        '';
      };

      allowedKinds = mkOption {
        type = types.listOf types.str;
        default = [ "ubuntu-latest" "ubuntu-24.04-arm" ];
        example = [ "ubuntu-latest" "ubuntu-24.04-arm" "macos-latest" ];
        description = ''
          Runner labels the service may start runs on.
          macOS, larger and self-hosted runners are detected but only used once listed here.
        '';
      };

      projects = mkOption {
        type = types.nullOr (types.listOf types.str);
        default = null;
        example = [ "website" "backend" ];
        description = ''
          Names or IDs of the granting projects whose waiting jobs may start runs.
          `null` counts every project granting the team its workers.
        '';
      };

      maxLaunches = mkOption {
        type = types.ints.positive;
        default = 4;
        description = "Maximum number of runs that are pending or running at the same time.";
      };

      maxLaunchesPerKind = mkOption {
        type = types.ints.positive;
        default = 2;
        description = "Maximum number of runs per runner label that are pending or running at the same time.";
      };

      workerFlake = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "github:wavelens/gradient/v2.0.0#gradient";
        description = ''
          Flake reference of the package providing {command}`gradient-worker` on the runner.
          `null` picks the release tag matching the version the Gradient server reports.
        '';
      };

      workerSubstituters = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "https://gradient.example.com/cache/main" ];
        description = ''
          Binary caches the runner adds to its Nix configuration before fetching {option}`services.gradient-gha-autoscaler.connections.<name>.workerFlake`.
          A cache holding the worker closure keeps the run from building Gradient from source.
        '';
      };

      workerTrustedPublicKeys = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "gradient.example.com-1:AAAA..." ];
        description = "Public keys that sign the paths in {option}`services.gradient-gha-autoscaler.connections.<name>.workerSubstituters`.";
      };

      idleTimeout = mkOption {
        type = types.ints.positive;
        default = 120;
        description = "Seconds without an assigned job after which a worker drains and its run ends.";
      };

      maxLifetime = mkOption {
        type = types.ints.positive;
        default = 18000;
        description = ''
          Seconds after which a worker drains, finishes its running jobs and ends its run.
          GitHub stops a job after 6 hours, so keep room for installing Nix and fetching the worker.
        '';
      };
    };
  };

  configFile = (pkgs.formats.json { }).generate "gradient-gha-autoscaler.json" {
    inherit (cfg) listen publicUrl catchUpInterval;
    stateDir = "/var/lib/gradient-gha-autoscaler";
    connections = mapAttrsToList (name: connection: {
      inherit name;
      inherit (connection)
        gradientUrl team repository allowedKinds projects maxLaunches maxLaunchesPerKind
        workerFlake workerSubstituters workerTrustedPublicKeys idleTimeout maxLifetime;
      apiKeyFile = "${credentialsDir}/${name}-api-key";
      githubTokenFile = "${credentialsDir}/${name}-github-token";
    }) cfg.connections;
  };
in
{
  options.services.gradient-gha-autoscaler = {
    enable = mkEnableOption "the service that starts temporary Gradient team workers as GitHub Actions runs";

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.gradient-gha-autoscaler;
      defaultText = literalExpression "gradient-hypervisor.packages.\${pkgs.stdenv.hostPlatform.system}.gradient-gha-autoscaler";
      description = "Package providing {command}`gradient-gha-autoscaler`.";
    };

    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
      example = "[::]:8080";
      description = "Address and port of the HTTP listener for webhooks and the worker token swap.";
    };

    publicUrl = mkOption {
      type = types.str;
      example = "https://autoscaler.example.com";
      description = ''
        URL under which GitHub, Gradient and GitHub-hosted runners reach {option}`services.gradient-gha-autoscaler.listen`.
        Each connection uses the paths `/c/<name>/github`, `/c/<name>/gradient` and `/c/<name>/swap` below it.
      '';
    };

    catchUpInterval = mkOption {
      type = types.ints.positive;
      default = 300;
      description = ''
        Seconds between two catch-up checks.
        A check repairs the setup, closes runs whose webhooks got lost and recomputes the demand.
      '';
    };

    connections = mkOption {
      type = types.attrsOf (types.submodule connectionOptions);
      default = { };
      example = literalExpression ''
        {
          platform = {
            gradientUrl = "https://gradient.example.com";
            team = "platform";
            apiKeyFile = "/run/secrets/gradient-api-key";
            githubTokenFile = "/run/secrets/github-token";
            repository = "acme/gradient-runners";
          };
        }
      '';
      description = ''
        Connections between a Gradient team and a GitHub repository.
        The attribute name names the connection and may only hold `a-z`, `0-9` and `-`.
      '';
    };
  };

  config = mkIf cfg.enable {
    systemd.services.gradient-gha-autoscaler = {
      description = "Gradient GitHub Actions autoscaler";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      environment.GRADIENT_GHA_AUTOSCALER_CONFIG = configFile;

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        DynamicUser = true;
        StateDirectory = "gradient-gha-autoscaler";
        LoadCredential = lib.flatten (mapAttrsToList (name: connection: [
          "${name}-api-key:${connection.apiKeyFile}"
          "${name}-github-token:${connection.githubTokenFile}"
        ]) cfg.connections);
        Restart = "on-failure";
        RestartSec = 5;

        CapabilityBoundingSet = "";
        LockPersonality = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
      };
    };
  };
}
