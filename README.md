# gradient-hypervisor

Worker deployment tools for [Gradient](https://github.com/wavelens/gradient).

## gradient-autoscaler

Starts temporary Gradient team workers when connected workers cannot handle waiting jobs, and stops them once they are idle. Each connection starts its workers either as GitHub Actions runs or as Kubernetes pods.

- A `worker.demand` webhook from Gradient wakes the service, and a catch-up check runs every `catchUpInterval` seconds.
- A worker starts only for a missing capability: a system, a system feature, or network, secrets and evaluation access that no connected worker eligible for the project offers. A busy but capable worker starts nothing.
- Workers exit after `idleTimeout` seconds without a job or after `maxLifetime` seconds. A worker that has not started yet is stopped again once its demand is gone.
- The service deletes the team worker of a launch after the launch ends, and only deletes workers it registered.
- Two launches of one runner label or pool that end without their worker connecting within 30 minutes pause that label for 10 minutes.

### Kubernetes

- Each pool in `kubernetes.pools` is one kind of worker pod with its system, features, job slots, resources, node selector and tolerations.
- `maxJobs` sets the worker's concurrent build slots. A value of zero is rejected during configuration loading.
- For each launch, the service registers a team worker and creates the pod `gradient-worker-<launch id>` with `restartPolicy: Never`. The worker token goes into a Secret of the same name that the pod owns, so deleting the pod deletes the token.
- The service polls pod phases. A pod that exits, fails or cannot pull its image ends its launch, and the service deletes it.
- The service needs `create`, `get`, `list` and `delete` on pods and `create`, `get`, `update` and `delete` on secrets in the worker namespace.
- Builds get the Nix sandbox and `/dev/kvm` when pods run with `privileged`.
- Nix can fall back to unsandboxed builds when a pod can't create namespaces. Most clusters forbid that setup unless pods run with `privileged`.

### Build Capacity Preview

Set a Kubernetes connection's `buildScaling` to `{"mode":"preview","profiles":["x86"],"fallbackRamMb":2048}` to report ordinary-build capacity plans from Gradient's capacity assessment API. Preview replaces legacy scaling for that connection: it does not register workers, create or delete Pods or Secrets, repair webhooks, or reconcile launches. Other connections retain their configured behavior.

Each selected pool needs `buildResources`, for example `{"cpuCount":4,"ramTotalMb":7000,"cpuCoreScore":4000,"buildCores":null}`. These are planning assumptions, not measured resources or conversions of Kubernetes `cpu` and `memory` quantities. `ramTotalMb` excludes worker overhead.

Prepared launches pass the assessed architectures, nonempty feature list, positive core score and positive build-core limit through the worker's native options. The same features are added to Nix's system features. Empty features retain detection; a zero core score or null/zero core limit retains the native automatic behavior. CPU count and usable RAM still require validation against actual worker budgets.

`queueWait` defaults to 60 seconds, `startupGrace` to 120 seconds, and `maxNewWorkers` to 2. Plans retain the existing total and per-pool launch limits and failure backoff. The number of profiles times `max(1, maxNewWorkers)` must not exceed 16. Scoped `projects` must contain IDs; `null` means all granting projects and `[]` means none. A server without the assessment API produces an error, not a legacy scaling fallback. Preview uses the existing catch-up interval; it does not install a demand webhook.

### GitHub Actions

- The service creates or repairs the Gradient team webhook, the GitHub repository webhook and `.github/workflows/gradient-worker.yml` on the default branch.
- Removing the first line `# managed by gradient-autoscaler v...` from that file hands it to the operator, and the service stops rewriting it.
- Each run exchanges its GitHub OIDC token for a one-time Gradient worker token, so worker tokens never appear in workflow inputs or logs.
- GitHub and GitHub-hosted runners must reach both `publicUrl` and the Gradient server's `/proto` endpoint.
- `ubuntu-latest` and `ubuntu-24.04-arm` are enabled by default. `macos-latest`, larger runners and self-hosted runners are detected but only used once listed in `allowedKinds`.
- Runs start `gradient-worker` from `github:wavelens/gradient/v<server version>#gradient` unless `workerFlake` is set. Point `workerSubstituters` and `workerTrustedPublicKeys` at a cache holding that closure, or every run builds Gradient from source.

### Requirements

- Tasks using the autoscaler need `wait_for_workers`, so evaluations wait for a worker to start instead of aborting after 300 seconds.
- Gradient must reach `publicUrl` for the `worker.demand` webhook.
- The API key belongs to a user with the Admin role in the team.

## Images

| Package | Contents |
| --- | --- |
| `gradient-worker-image` | `gradient-worker`, Gradient's Nix, git, ssh and CA certificates, with a registered store |
| `gradient-autoscaler-image` | `gradient-autoscaler` and CA certificates |

```sh
nix build .#gradient-worker-image
skopeo copy docker-archive:result docker://registry.example.com/gradient-worker:2.0.1
```

The worker image uses the `gradient` flake input. Match it to the server version, or build an image for another Gradient package with `lib.mkWorkerImage { pkgs, gradient, gradient-nix }`.

## Local Integration Test

The local fixture starts real Gradient and PostgreSQL in a fresh Kind cluster and provisions a private cache for its test project. It tests prepared-worker registration, live inventory, process-restart recovery, literal system features and scoped cleanup. It does not test backlog scaling or CPU/RAM enforcement.

Set `GRADIENT_SOURCE` to a Git flake URL selecting a revision with the capacity assessment API. Git-filtered sources exclude local build artifacts. Build the matched test and worker images:

```sh
nix build --builders '' --no-write-lock-file --no-link --print-out-paths \
  --override-input gradient "$GRADIENT_SOURCE" \
  .#gradient-kubernetes-test-image .#gradient-worker-image
```

Pass the printed archives as `TEST_IMAGE_ARCHIVE` and `WORKER_IMAGE_ARCHIVE`. For rootless Podman, run the fixture in a delegated scope:

```sh
systemd-run --user --scope --property=Delegate=yes \
  nix shell --builders '' --inputs-from . \
  nixpkgs#kind nixpkgs#kubectl nixpkgs#kubernetes-helm nixpkgs#python3 \
  --command python3 nix/tests/kubernetes/prepared.py \
  --test-image-archive "$TEST_IMAGE_ARCHIVE" \
  --worker-image-archive "$WORKER_IMAGE_ARCHIVE"
```

The fixture uses temporary credentials and its own kubeconfig, refuses unexpected cluster endpoints or nodes, and deletes its cluster after success or failure. Use `--provider docker` to select Docker instead of Podman.

API-key authentication is the default and fails on session-only team endpoints. Add `--auth session` to test the lifecycle with the disposable admin login. Session mode does not validate API-key compatibility, and API-key failures never fall back to a session.

## Helm

`helm/gradient-autoscaler` deploys the autoscaler into a cluster: a Deployment with a state volume, its config, a Service, an optional Ingress for `/c/`, and a ServiceAccount with a Role in every worker namespace.

```sh
kubectl create secret generic gradient-api-key --from-file=api-key=./api-key
helm install autoscaler ./helm/gradient-autoscaler -f values.yaml
```

`helm/gradient-autoscaler/ci/kubernetes-values.yaml` shows a Kubernetes and a GitHub Actions connection. A connection inside the cluster talks to the API server with its service account; `apiServer`, `tokenFile` and `caFile` are only needed outside it.

## NixOS

```nix
{
  inputs.gradient-hypervisor.url = "github:wavelens/gradient-hypervisor";

  outputs = { nixpkgs, gradient-hypervisor, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        gradient-hypervisor.nixosModules.gradient-autoscaler
        {
          services.gradient-autoscaler = {
            enable = true;
            publicUrl = "https://autoscaler.example.com";
            connections.cluster = {
              gradientUrl = "https://gradient.example.com";
              team = "platform";
              apiKeyFile = "/run/secrets/gradient-api-key";
              kubernetes = {
                apiServer = "https://k8s.example.com:6443";
                tokenFile = "/run/secrets/kubernetes-token";
                namespace = "gradient-workers";
                image = "registry.example.com/gradient-worker:2.0.1";
                pools.x86 = { system = "x86_64-linux"; cpu = "8"; memory = "32Gi"; };
              };
            };
            connections.actions = {
              gradientUrl = "https://gradient.example.com";
              team = "platform";
              apiKeyFile = "/run/secrets/gradient-api-key";
              githubActions = {
                tokenFile = "/run/secrets/github-token";
                repository = "acme/gradient-runners";
              };
            };
          };
        }
      ];
    };
  };
}
```
