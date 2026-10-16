# gradient-hypervisor

Tools that deploy workers for [Gradient](https://github.com/wavelens/gradient).

## gradient-gha-autoscaler

Starts temporary Gradient team workers as GitHub Actions runs whenever the team's connected workers cannot take a waiting job, and lets them exit once they sit idle.

- Gradient's `worker.demand` team webhook wakes the service. GitHub `workflow_run` and `workflow_job` webhooks track each run.
- A run starts only for a missing capability: a system, a system feature, or fetch and evaluation that no connected worker eligible for the project offers. A busy but capable worker starts nothing.
- Each run swaps its GitHub OIDC token for a one-time Gradient worker token. Worker tokens never appear in workflow inputs or logs.
- Workers exit after `idleTimeout` seconds without a job or after `maxLifetime` seconds. A run whose demand disappears before it starts gets cancelled.
- When a run ends, the service deletes its team worker. It only deletes workers it registered itself.

### Setup

1. Create a Gradient API key for a user with the Admin role in the team.
2. Create a fine-grained GitHub token for one existing repository with Actions, Contents, Workflows and Webhooks write access. For an organization repository, organization hosted runners read access additionally detects larger runners.
3. Add the module to your NixOS configuration:

```nix
{
  inputs.gradient-hypervisor.url = "github:wavelens/gradient-hypervisor";

  outputs = { nixpkgs, gradient-hypervisor, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        gradient-hypervisor.nixosModules.gradient-gha-autoscaler
        {
          services.gradient-gha-autoscaler = {
            enable = true;
            publicUrl = "https://autoscaler.example.com";
            connections.platform = {
              gradientUrl = "https://gradient.example.com";
              team = "platform";
              apiKeyFile = "/run/secrets/gradient-api-key";
              githubTokenFile = "/run/secrets/github-token";
              repository = "acme/gradient-runners";
            };
          };
        }
      ];
    };
  };
}
```

On every start and every `catchUpInterval` seconds, the service creates or repairs the Gradient team webhook, the GitHub repository webhook and `.github/workflows/gradient-worker.yml` on the default branch. Removing the first line `# managed by gradient-gha-autoscaler v...` from that file hands it over to you, and the service stops rewriting it.

### Requirements

- GitHub and GitHub-hosted runners must reach both `publicUrl` and the Gradient server's `/proto` endpoint. Servers behind a firewall need self-hosted runner kinds inside the network.
- Projects receive the workers through their grant of the team's workers. `projects` limits which granting projects may start runs.
- Gradient aborts an evaluation that waits on a missing system after 300 seconds. Tasks using the autoscaler need `wait_for_workers`, so evaluations wait for a run to start.
- A public repository works, but its logs show the names of the projects being built. The service warns about it.

### Runner Kinds

- `ubuntu-latest` and `ubuntu-24.04-arm` are on by default.
- `macos-latest`, larger runners and self-hosted runners already registered on the repository are detected, but only used once listed in `allowedKinds`. macOS runs need the `gradient` package of the Gradient flake to build on Darwin.
- The first worker on a label reports its systems, features, job slots and RAM. That profile replaces the built-in default for the label. An unknown label gets one trial run.
- Two runs ending without their worker connecting within 30 minutes pause that label for 10 minutes.

### Worker Version

The run starts `gradient-worker` from `github:wavelens/gradient/v<server version>#gradient`, using the version the Gradient server reports. `workerFlake` overrides it.
