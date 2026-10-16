# gradient-hypervisor

Worker deployment tools for [Gradient](https://github.com/wavelens/gradient).

## gradient-gha-autoscaler

Temporary Gradient team workers start as GitHub Actions and start when connected workers cannot handle waiting jobs. The service can terminate these workers after they become idle.

- A `worker.demand` webhook from Gradient can activate the service. GitHub `workflow_run` and `workflow_job` webhooks track each run.
- A run can start only on detection of a missing capability. That capability can be a system, a system feature, or fetch and evaluation that no connected worker eligible for the project can offer. No new run can start from a busy but capable worker.
- Each run can exchange the GitHub OIDC token for a one-time Gradient worker token. Worker tokens never appear in workflow inputs or logs.
- Workers exit after `idleTimeout` seconds without a job or after `maxLifetime` seconds. The service can cancel a run on a loss of demand before the start of that run.
- The service can delete the team worker for a run after the end of that run. The service can delete only workers that the service registered.

### Setup

1. Create a Gradient API key for a team user with the Admin role.
2. Create a fine-grained GitHub token for one existing repository. Grant that token Actions, Contents, Workflows and Webhooks write access. Organization hosted runners read access can additionally detect larger runners for an organization repository.
3. Add the module to a NixOS configuration.

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

The service can create or repair the Gradient team webhook, the GitHub repository webhook and `.github/workflows/gradient-worker.yml` on the default branch. That action can occur on every start and every `catchUpInterval` seconds. Removal of the first line `# managed by gradient-gha-autoscaler v...` from that file can hand control to the operator. The service can then stop rewriting that file.

### Requirements

- GitHub and GitHub-hosted runners must reach both `publicUrl` and the Gradient server's `/proto` endpoint. Servers behind a firewall need self-hosted runner kinds inside the network.
- Projects receive the workers through their grant of the team's workers. `projects` can limit which granting projects can start runs.
- Gradient can abort an evaluation after 300 seconds with a wait on a missing system. Tasks using the autoscaler need `wait_for_workers`, so evaluations wait for a run to start.
- A public repository can work, but the logs of that repository show the names of the projects being built. The service can warn about that.

### Runner Kinds

- `ubuntu-latest` and `ubuntu-24.04-arm` come enabled by default.
- The service can detect `macos-latest`, larger runners and self-hosted runners already registered on the repository. The service can use those runners only after their addition to `allowedKinds`. macOS runs need the `gradient` package of the Gradient flake to build on Darwin.
- The first worker on a label can report systems, features, job slots and RAM. That profile can replace the built-in default for the label. An unknown label can get one trial run.
- Two runs that end without their worker connecting within 30 minutes pause that label for 10 minutes.

### Worker Version

The run can initiate `gradient-worker` from `github:wavelens/gradient/v<server version>#gradient`, using the version reported by the Gradient server. `workerFlake` can override this. Every run can build Gradient from source without a binary cache for that closure. Point `workerSubstituters` and `workerTrustedPublicKeys` at a binary cache for that reason.
