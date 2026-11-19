use anyhow::{Context, ensure};
use clap::{Parser, ValueEnum};
use gradient_autoscaler::build_planner::PlannedBuildWorker;
use gradient_autoscaler::build_preview::preview_build_capacity;
use gradient_autoscaler::capacity::{
    BuildCapacityProfile, CapacityAssessmentRequest, CapacityProfileRequest,
};
use gradient_autoscaler::config::{BuildScalingMode, Config, LauncherConfig};
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::kubernetes::KubernetesLauncher;
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::launcher::Launcher;
use gradient_autoscaler::service::ConnectionService;
use gradient_autoscaler::state::{LaunchState, State};
use reqwest::{Client, ClientBuilder, redirect::Policy};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

const WORKER_ID: Uuid = Uuid::from_u128(0xa0000000_0000_0000_0000_000000000001);
const UNCHECKED_WORKER_ID: Uuid = Uuid::from_u128(0xa0000000_0000_0000_0000_000000000002);
const TOKEN_FILE: &str = "/run/gradient-preview-no-kubernetes-token";

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    profile: PathBuf,
    #[arg(long, value_enum)]
    expect_live: Readiness,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Readiness {
    Ready,
    Unready,
}

#[derive(Serialize)]
struct Receipt {
    connected: bool,
    live_index: Option<usize>,
    total_ready_builds: usize,
    proposed_workers: usize,
    credited_starting_workers: usize,
}

fn fixture_http_client(builder: ClientBuilder) -> reqwest::Result<Client> {
    builder
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
}

fn guard_fixture(config: &Config) -> anyhow::Result<()> {
    ensure!(
        config.connections.len() == 1,
        "fixture requires exactly one connection"
    );
    let conn = &config.connections[0];
    ensure!(
        conn.name == "readiness-preview"
            && conn.team == "readiness"
            && conn.gradient_url.as_str() == "http://127.0.0.1:3000/",
        "refusing a connection outside the loopback readiness fixture"
    );
    let LauncherConfig::Kubernetes(cluster) = &conn.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    ensure!(
        cluster.api_server.as_ref().map(|url| url.as_str()) == Some("http://127.0.0.1:9/")
            && cluster.namespace == "readiness-preview"
            && cluster.token_file == Path::new(TOKEN_FILE),
        "fixture requires its unreachable Kubernetes target and absent token path"
    );
    let scaling = conn
        .build_scaling
        .as_ref()
        .context("fixture requires build scaling preview")?;
    ensure!(
        scaling.mode == BuildScalingMode::Preview
            && scaling.profiles.len() == 1
            && scaling.max_new_workers == 1
            && scaling.queue_wait == 0
            && scaling.startup_grace == 600
            && scaling.fallback_ram_mb == 64,
        "fixture requires one preview profile and the readiness policy"
    );
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Arguments::parse();
    let config = Config::load(&args.config)?;
    guard_fixture(&config)?;
    ensure!(
        !Path::new(TOKEN_FILE).try_exists()?,
        "fixture Kubernetes token must be absent"
    );
    let profile: BuildCapacityProfile = serde_json::from_slice(&std::fs::read(&args.profile)?)?;
    ensure!(
        profile.worker_id == WORKER_ID,
        "profile must identify the registered readiness fixture worker"
    );
    let mut conn = config
        .connections
        .into_iter()
        .next()
        .context("missing fixture connection")?;

    // Fixture setup seeds pre-connected controller state locally. Preview starts
    // only after restoring buildScaling; no worker registration or Pod is created.
    let scaling = conn
        .build_scaling
        .take()
        .context("missing preview policy")?;
    let LauncherConfig::Kubernetes(cluster) = &conn.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    let launcher = Launcher::Kubernetes(KubernetesLauncher::new(
        cluster.clone(),
        KubernetesClient::new(cluster)?,
    ));
    let gradient = GradientClient::with_http_client(
        conn.gradient_url.clone(),
        std::fs::read_to_string(&conn.api_key_file)?
            .trim()
            .to_owned(),
        fixture_http_client(Client::builder())?,
    );
    let mut svc = ConnectionService::new(
        conn,
        config.public_url,
        gradient,
        launcher,
        State::memory().await?,
    );
    let workers_before = svc.gradient.team_workers(&svc.conn.team).await?;
    ensure!(
        workers_before.len() == 2,
        "fixture must have exactly two registrations"
    );
    let workers_before = workers_before
        .into_iter()
        .map(|worker| Uuid::parse_str(&worker.worker_id))
        .collect::<Result<BTreeSet<Uuid>, _>>()?;
    ensure!(
        workers_before == BTreeSet::from([WORKER_ID, UNCHECKED_WORKER_ID]),
        "fixture must have its checked and unchecked worker registrations"
    );
    let Launcher::Kubernetes(kubernetes) = &svc.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    let launch = kubernetes
        .prepare_build_launch(
            &svc,
            &PlannedBuildWorker {
                profile_index: 0,
                label: scaling.profiles[0].clone(),
                worker_id: profile.worker_id,
            },
            &profile,
            &scaling.policy(),
            OffsetDateTime::now_utc(),
        )
        .await?;
    ensure!(
        svc.state.claim_build_registration(&launch).await?,
        "synthetic registration intent was not claimed"
    );
    svc.conn.build_scaling = Some(scaling);
    let before = svc.state.open_launches(&svc.conn.name).await?;
    ensure!(
        before.len() == 1
            && before[0].registration_started
            && before[0].state < LaunchState::Connected,
        "fixture requires a started, pre-connected synthetic intent"
    );

    let capacity = svc
        .gradient
        .capacity_assessment(
            &svc.conn.team,
            &CapacityAssessmentRequest {
                projects: svc.conn.capacity_projects()?,
                include_waiting_builds: true,
                tracked_profiles: vec![CapacityProfileRequest {
                    profile: profile.clone(),
                    enable_build: profile.capabilities.build,
                    enable_network: profile.capabilities.network,
                }],
                ..Default::default()
            },
        )
        .await?;
    let tracked = capacity
        .tracked_workers
        .first()
        .context("fixture worker was not tracked")?;
    ensure!(tracked.connected, "fixture worker must be connected");
    ensure!(
        tracked.worker_index.is_some() == (args.expect_live == Readiness::Ready),
        "live worker index differs from expected readiness"
    );
    ensure!(
        capacity.total_ready_builds > 0 && capacity.builds.len() == 1 && !capacity.builds_truncated,
        "fixture requires one assessed queued build"
    );
    ensure!(
        capacity
            .builds
            .iter()
            .all(|build| build.tracked_workers == [None]),
        "connected worker must not receive hypothetical tracked assessments"
    );

    let preview = preview_build_capacity(&svc).await?;
    ensure!(
        preview.tracked_workers.len() == 1,
        "production preview must track exactly the checked fixture worker"
    );
    let preview_tracked = &preview.tracked_workers[0];
    ensure!(
        preview_tracked.profile.worker_id == WORKER_ID && preview_tracked.connected,
        "production preview must observe the connected checked fixture worker"
    );
    ensure!(
        preview_tracked.worker_index.is_some() == (args.expect_live == Readiness::Ready),
        "production preview live worker index differs from expected readiness"
    );
    ensure!(
        preview.plan.credited_starting_workers.is_empty(),
        "connected API truth must override synthetic startup credit"
    );
    ensure!(
        preview.plan.proposed_workers.len() == usize::from(args.expect_live == Readiness::Unready),
        "proposed worker count differs from expected readiness"
    );
    ensure!(
        preview.total_ready_builds == capacity.total_ready_builds
            && preview.assessed_builds == 1
            && !preview.plan.builds_truncated
            && !preview.plan.server_draining
            && preview.plan.deferred_builds.is_empty(),
        "preview must fully cover the single fitting queued build"
    );
    ensure!(
        svc.state.open_launches(&svc.conn.name).await? == before,
        "preview changed the SQLite launch ledger"
    );
    let workers_after = svc.gradient.team_workers(&svc.conn.team).await?;
    ensure!(
        workers_after.len() == 2,
        "preview changed team worker count"
    );
    let workers_after = workers_after
        .into_iter()
        .map(|worker| Uuid::parse_str(&worker.worker_id))
        .collect::<Result<BTreeSet<Uuid>, _>>()?;
    ensure!(
        workers_after == workers_before,
        "preview changed team worker IDs or count"
    );
    println!(
        "{}",
        serde_json::to_string(&Receipt {
            connected: preview_tracked.connected,
            live_index: preview_tracked.worker_index,
            total_ready_builds: preview.total_ready_builds,
            proposed_workers: preview.plan.proposed_workers.len(),
            credited_starting_workers: preview.plan.credited_starting_workers.len(),
        })?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config() -> Config {
        Config::parse(
            &json!({
                "listen": "127.0.0.1:8080",
                "publicUrl": "http://127.0.0.1:8080/",
                "stateDir": "/unused",
                "connections": [{
                    "name": "readiness-preview",
                    "team": "readiness",
                    "gradientUrl": "http://127.0.0.1:3000/",
                    "apiKeyFile": "/unused",
                    "kubernetes": {
                        "apiServer": "http://127.0.0.1:9/",
                        "namespace": "readiness-preview",
                        "tokenFile": TOKEN_FILE,
                        "image": "fixture",
                        "pools": {
                            "fixture": {
                                "system": "x86_64-linux",
                                "buildResources": {
                                    "cpuCount": 1, "ramTotalMb": 256, "cpuCoreScore": 1000
                                }
                            }
                        }
                    },
                    "buildScaling": {
                        "mode": "preview", "profiles": ["fixture"],
                        "maxNewWorkers": 1, "queueWait": 0,
                        "startupGrace": 600, "fallbackRamMb": 64
                    }
                }]
            })
            .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn guards_connection_identity_and_exact_gradient_target() {
        let fixture = config();
        guard_fixture(&fixture).unwrap();
        for field in ["count", "name", "team", "url", "query"] {
            let mut changed = fixture.clone();
            match field {
                "count" => changed.connections.clear(),
                "name" => changed.connections[0].name = "other".into(),
                "team" => changed.connections[0].team = "other".into(),
                "url" => {
                    changed.connections[0].gradient_url = "http://127.0.0.1:3001/".parse().unwrap()
                }
                "query" => changed.connections[0].gradient_url.set_query(Some("other")),
                _ => unreachable!(),
            }
            assert!(guard_fixture(&changed).is_err(), "{field}");
        }
    }

    #[test]
    fn guards_kubernetes_target_namespace_and_token_path() {
        for field in ["in-cluster", "url", "namespace", "token"] {
            let mut changed = config();
            let LauncherConfig::Kubernetes(cluster) = &mut changed.connections[0].launcher else {
                unreachable!()
            };
            match field {
                "in-cluster" => cluster.api_server = None,
                "url" => cluster.api_server = Some("http://127.0.0.1:3000/".parse().unwrap()),
                "namespace" => cluster.namespace = "other".into(),
                "token" => cluster.token_file = "/run/other-token".into(),
                _ => unreachable!(),
            }
            assert!(guard_fixture(&changed).is_err(), "{field}");
        }
    }

    #[test]
    fn guards_preview_policy() {
        for field in ["missing", "profiles", "limit", "queue", "startup", "ram"] {
            let mut changed = config();
            if field == "missing" {
                changed.connections[0].build_scaling = None;
            } else {
                let scaling = changed.connections[0].build_scaling.as_mut().unwrap();
                match field {
                    "profiles" => scaling.profiles.push("other".into()),
                    "limit" => scaling.max_new_workers = 2,
                    "queue" => scaling.queue_wait = 1,
                    "startup" => scaling.startup_grace = 120,
                    "ram" => scaling.fallback_ram_mb = 2048,
                    _ => unreachable!(),
                }
            }
            assert!(guard_fixture(&changed).is_err(), "{field}");
        }
    }

    #[tokio::test]
    async fn fixture_client_rejects_redirects_without_forwarding_requests_or_bearer() {
        let origin = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/teams/readiness"))
            .and(header("Authorization", "Bearer fixture-key"))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", target.uri()))
            .expect(1)
            .mount(&origin)
            .await;
        let client = GradientClient::with_http_client(
            origin.uri().parse().unwrap(),
            "fixture-key".into(),
            fixture_http_client(Client::builder()).unwrap(),
        );

        assert!(client.team_id("readiness").await.is_err());
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fixture_client_clears_explicit_proxy_without_forwarding_requests_or_bearer() {
        let origin = MockServer::start().await;
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/teams/readiness"))
            .and(header("Authorization", "Bearer fixture-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"id": Uuid::nil()}
            })))
            .expect(1)
            .mount(&origin)
            .await;
        let client = GradientClient::with_http_client(
            origin.uri().parse().unwrap(),
            "fixture-key".into(),
            fixture_http_client(Client::builder().proxy(reqwest::Proxy::all(proxy.uri()).unwrap()))
                .unwrap(),
        );

        assert_eq!(client.team_id("readiness").await.unwrap(), Uuid::nil());
        assert!(proxy.received_requests().await.unwrap().is_empty());
    }
}
