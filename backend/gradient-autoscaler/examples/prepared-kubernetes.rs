use anyhow::{Context, ensure};
use clap::{Parser, ValueEnum};
use gradient_autoscaler::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use gradient_autoscaler::capacity::{
    BuildCapacityProfile, CapacityAssessmentRequest, CapacityProfileRequest,
};
use gradient_autoscaler::config::{Config, LauncherConfig};
use gradient_autoscaler::events::delete_worker;
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::kubernetes::KubernetesLauncher;
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::kubernetes::pod::CONNECTION_LABEL;
use gradient_autoscaler::launcher::Launcher;
use gradient_autoscaler::service::ConnectionService;
use gradient_autoscaler::state::{BuildLaunchIntent, Launch, LaunchState, State};
use k8s_openapi::api::core::v1::{Container, Pod as NativePod, PodSpec, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    profile: PathBuf,
    #[arg(long, value_enum)]
    phase: Option<Phase>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Phase {
    Launch,
    Recover,
    Cleanup,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Receipt {
    launch_id: Uuid,
    intent: BuildLaunchIntent,
    pod_uid: String,
    secret_uid: String,
    credential_digest: String,
}

fn kubernetes(svc: &ConnectionService) -> anyhow::Result<&KubernetesLauncher> {
    match &svc.launcher {
        Launcher::Kubernetes(launcher) => Ok(launcher),
        Launcher::GitHub(_) => anyhow::bail!("fixture requires Kubernetes"),
    }
}

async fn connected_worker(
    svc: &ConnectionService,
    profile: &BuildCapacityProfile,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + StdDuration::from_secs(90);
    loop {
        let workers = svc.gradient.team_workers(&svc.conn.team).await?;
        ensure!(
            workers.len() == 1,
            "fixture must have exactly one registration"
        );
        let worker = &workers[0];
        ensure!(
            worker.worker_id == profile.worker_id.to_string(),
            "registration changed worker identity"
        );
        let ready = worker.connected
            && worker.live.as_ref().is_some_and(|live| {
                live.architectures == profile.resources.architectures
                    && live.system_features.iter().collect::<BTreeSet<_>>()
                        == profile
                            .resources
                            .system_features
                            .iter()
                            .collect::<BTreeSet<_>>()
                    && live.max_concurrent_builds == profile.resources.max_concurrent_builds
            });
        if ready {
            let live = worker
                .live
                .as_ref()
                .context("connected worker has no inventory")?;
            let assessment = svc
                .gradient
                .capacity_assessment(
                    &svc.conn.team,
                    &CapacityAssessmentRequest {
                        projects: svc.conn.capacity_projects()?,
                        tracked_profiles: vec![CapacityProfileRequest {
                            profile: profile.clone(),
                            enable_build: profile.capabilities.build,
                            enable_network: profile.capabilities.network,
                        }],
                        ..Default::default()
                    },
                )
                .await?;
            let tracked = &assessment.tracked_workers[0];
            if tracked.connected
                && tracked.worker_index.is_some_and(|index| {
                    assessment.workers[index].free_slots == live.max_concurrent_builds
                })
            {
                println!(
                    "PASS: connected worker advertises configured architectures, features and slots; RAM report {:?} MiB",
                    live.ram_total_mb
                );
                return Ok(());
            }
        }
        ensure!(
            Instant::now() < deadline,
            "worker inventory and scoped capacity did not become ready within 90 seconds"
        );
        tokio::time::sleep(StdDuration::from_millis(500)).await;
    }
}

async fn receipt(svc: &ConnectionService, launch: &Launch) -> anyhow::Result<Receipt> {
    let client = &kubernetes(svc)?.client;
    let pod = client
        .pod(&launch.external_id)
        .await?
        .context("worker Pod is missing")?;
    let secret = client
        .secret(&launch.external_id)
        .await?
        .context("worker credential is missing")?;
    ensure!(
        secret.immutable == Some(true),
        "worker credential is mutable"
    );
    let secret_uid = secret.metadata.uid.context("credential has no UID")?;
    ensure!(
        launch.pod_uid.as_deref() == Some(pod.metadata.uid.as_str())
            && launch.secret_uid.as_deref() == Some(secret_uid.as_str()),
        "saved object identities differ from Kubernetes"
    );
    let owners = secret
        .metadata
        .owner_references
        .context("credential has no owner")?;
    ensure!(
        owners.len() == 1
            && owners[0].kind == "Pod"
            && owners[0].name == launch.external_id
            && owners[0].uid == pod.metadata.uid,
        "credential owner differs from the worker Pod"
    );
    let data = secret.data.context("credential has no data")?;
    let peers = data.get("peers").context("credential has no peers")?;
    Ok(Receipt {
        launch_id: launch.id,
        intent: launch
            .build_intent
            .clone()
            .context("launch has no captured intent")?,
        pod_uid: pod.metadata.uid,
        secret_uid,
        credential_digest: hex::encode(Sha256::digest(&peers.0)),
    })
}

fn run_phase(args: &Arguments, phase: &str) -> anyhow::Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg("--config")
        .arg(&args.config)
        .arg("--profile")
        .arg(&args.profile)
        .args(["--phase", phase])
        .status()?;
    ensure!(status.success(), "fixture {phase} process failed");
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Arguments::parse();
    let Some(phase) = args.phase else {
        for phase in ["launch", "recover"] {
            run_phase(&args, phase)?;
        }
        let state_dir = Config::load(&args.config)?.state_dir;
        std::fs::write(state_dir.join("ready"), [])?;
        let deadline = Instant::now() + StdDuration::from_secs(120);
        while !state_dir.join("inspected").exists() {
            ensure!(Instant::now() < deadline, "worker inspection timed out");
            tokio::time::sleep(StdDuration::from_millis(500)).await;
        }
        run_phase(&args, "cleanup")?;
        return Ok(());
    };
    let config = Config::load(&args.config)?;
    ensure!(
        config.connections.len() == 1,
        "fixture requires one connection"
    );
    let conn = config
        .connections
        .into_iter()
        .next()
        .context("missing connection")?;
    ensure!(
        conn.name == "fixture"
            && conn.team == "fixture"
            && conn.gradient_url.as_str() == "http://gradient.gradient-prepared-test.svc:3000/"
            && conn.build_scaling.is_none(),
        "refusing a connection outside the disposable fixture"
    );
    let LauncherConfig::Kubernetes(cluster) = &conn.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    ensure!(
        cluster.namespace == "gradient-prepared-test" && cluster.api_server.is_none(),
        "fixture requires its in-cluster Kubernetes target"
    );
    let client = KubernetesClient::new(cluster)?;
    let launcher = Launcher::Kubernetes(KubernetesLauncher::new(cluster.clone(), client));
    let gradient = GradientClient::new(
        conn.gradient_url.clone(),
        std::fs::read_to_string(&conn.api_key_file)?
            .trim()
            .to_owned(),
    );
    std::fs::create_dir_all(&config.state_dir)?;
    let receipt_path = config.state_dir.join("receipt.json");
    let svc = ConnectionService::new(
        conn,
        config.public_url,
        gradient,
        launcher,
        State::open(&config.state_dir.join("launches.sqlite")).await?,
    );
    let profile: BuildCapacityProfile = serde_json::from_slice(&std::fs::read(&args.profile)?)?;
    match phase {
        Phase::Launch => {
            ensure!(
                svc.state.open_launches(&svc.conn.name).await?.is_empty()
                    && svc.gradient.team_workers(&svc.conn.team).await?.is_empty(),
                "fixture must start without workers or launches"
            );
            let assessment = svc
                .gradient
                .capacity_assessment(
                    &svc.conn.team,
                    &CapacityAssessmentRequest {
                        projects: svc.conn.capacity_projects()?,
                        profiles: vec![CapacityProfileRequest {
                            profile: profile.clone(),
                            enable_build: profile.capabilities.build,
                            enable_network: profile.capabilities.network,
                        }],
                        ..Default::default()
                    },
                )
                .await?;
            ensure!(
                assessment.total_ready_builds == 0 && assessment.workers.is_empty(),
                "fixture must assess an empty scheduler"
            );
            ensure!(
                svc.gradient.team_workers(&svc.conn.team).await?.is_empty(),
                "prospective assessment registered a worker"
            );
            let launch = svc
                .launcher
                .prepare_build_launch(
                    &svc,
                    &PlannedBuildWorker {
                        profile_index: 0,
                        label: "x86".into(),
                        worker_id: profile.worker_id,
                    },
                    &assessment.profiles[0],
                    &BuildScalingPolicy {
                        queue_wait: Duration::seconds(30),
                        startup_grace: Duration::minutes(2),
                        max_new_workers: 1,
                        fallback_ram_mb: 1024,
                    },
                    OffsetDateTime::now_utc(),
                )
                .await?;
            svc.launcher.launch_prepared(&svc, launch.id).await?;
            connected_worker(&svc, &profile).await?;
            svc.state
                .advance_launch(launch.id, LaunchState::Connected)
                .await?;
            let saved = svc
                .state
                .launch(&svc.conn.name, launch.id)
                .await?
                .context("launch lost")?;
            std::fs::write(
                receipt_path,
                serde_json::to_vec(&receipt(&svc, &saved).await?)?,
            )?;
            println!("PASS: real assessment, prepared registration, Pod and immutable credential");
        }
        Phase::Recover => {
            let original: Receipt = serde_json::from_slice(&std::fs::read(receipt_path)?)?;
            let saved = svc
                .state
                .launch(&svc.conn.name, original.launch_id)
                .await?
                .context("launch lost across process restart")?;
            let mut svc = svc;
            if let Launcher::Kubernetes(launcher) = &mut svc.launcher {
                launcher.config.image = "registry.invalid/changed-after-restart:fixture".into();
                launcher.config.pools.clear();
            }
            for _ in 0..2 {
                svc.launcher.launch_prepared(&svc, saved.id).await?;
                let current = svc
                    .state
                    .launch(&svc.conn.name, saved.id)
                    .await?
                    .context("recovery lost the saved launch")?;
                ensure!(
                    receipt(&svc, &current).await? == original,
                    "restart changed captured intent, object identity or credential"
                );
            }
            connected_worker(&svc, &profile).await?;
            ensure!(
                svc.state.open_launches(&svc.conn.name).await?.len() == 1
                    && kubernetes(&svc)?
                        .client
                        .pods(&format!("{CONNECTION_LABEL}={}", svc.conn.name))
                        .await?
                        .len()
                        == 1,
                "restart duplicated a launch or worker Pod"
            );
            println!("PASS: process restart and repeated recovery preserve the frozen launch");
        }
        Phase::Cleanup => {
            let original: Receipt = serde_json::from_slice(&std::fs::read(receipt_path)?)?;
            let saved = svc
                .state
                .launch(&svc.conn.name, original.launch_id)
                .await?
                .context("cleanup lost the saved launch")?;
            ensure!(
                receipt(&svc, &saved).await? == original,
                "cleanup target differs from the saved worker"
            );
            connected_worker(&svc, &profile).await?;
            let client = &kubernetes(&svc)?.client;
            // Same-team and same-connection controls expose overly broad cleanup.
            let control_id = Uuid::new_v4();
            let control_name = format!("gradient-cleanup-control-{control_id}");
            let control_caps = svc.conn.worker_capabilities;
            svc.gradient
                .register_team_worker_with_capabilities(
                    &svc.conn.team,
                    control_id,
                    &control_name,
                    control_caps,
                )
                .await?;
            let control_pod = NativePod {
                metadata: ObjectMeta {
                    name: Some(control_name.clone()),
                    labels: Some(BTreeMap::from([
                        (CONNECTION_LABEL.into(), svc.conn.name.clone()),
                        (
                            "app.kubernetes.io/managed-by".into(),
                            "gradient-autoscaler".into(),
                        ),
                    ])),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    // An unmatched selector keeps this control from starting a workload.
                    node_selector: Some(BTreeMap::from([(
                        "gradient.wavelens.io/cleanup-control".into(),
                        control_name.clone(),
                    )])),
                    restart_policy: Some("Never".into()),
                    containers: vec![Container {
                        name: "control".into(),
                        image: Some("gradient-kubernetes-test:fixture".into()),
                        image_pull_policy: Some("Never".into()),
                        command: Some(vec!["/bin/true".into()]),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            };
            let control_pod = client
                .create_pod(&serde_json::to_value(control_pod)?)
                .await?;
            let control_secret = Secret {
                metadata: ObjectMeta {
                    name: Some(control_name.clone()),
                    labels: Some(control_pod.metadata.labels.clone()),
                    owner_references: Some(vec![OwnerReference {
                        api_version: "v1".into(),
                        kind: "Pod".into(),
                        name: control_name.clone(),
                        uid: control_pod.metadata.uid.clone(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                immutable: Some(true),
                type_: Some("Opaque".into()),
                string_data: Some(BTreeMap::from([("control".into(), "unrelated".into())])),
                ..Default::default()
            };
            client
                .create_secret(&serde_json::to_value(control_secret)?)
                .await?;
            let control_secret = client
                .secret(&control_name)
                .await?
                .context("cleanup control Secret is missing")?;
            svc.launcher.cancel(&saved).await?;
            let deadline = Instant::now() + StdDuration::from_secs(60);
            loop {
                if client.pod(&saved.external_id).await?.is_none()
                    && client.secret(&saved.external_id).await?.is_none()
                {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "Pod or credential cleanup timed out"
                );
                tokio::time::sleep(StdDuration::from_millis(500)).await;
            }
            delete_worker(&svc, &saved).await?;
            let workers = svc.gradient.team_workers(&svc.conn.team).await?;
            ensure!(
                workers.len() == 1
                    && workers[0].worker_id == control_id.to_string()
                    && workers[0].registration_matches(&control_name, control_caps),
                "cleanup left the prepared worker or changed the unrelated registration"
            );
            let surviving_pod = client
                .pod(&control_name)
                .await?
                .context("cleanup deleted the unrelated Pod")?;
            let surviving_secret = client
                .secret(&control_name)
                .await?
                .context("cleanup deleted the unrelated Secret")?;
            ensure!(
                surviving_pod.metadata.uid == control_pod.metadata.uid
                    && surviving_pod.metadata.deletion_timestamp.is_none()
                    && surviving_secret.metadata.uid == control_secret.metadata.uid
                    && surviving_secret.metadata.deletion_timestamp.is_none()
                    && surviving_secret.data == control_secret.data
                    && surviving_secret.metadata.owner_references
                        == control_secret.metadata.owner_references,
                "cleanup replaced or changed the unrelated Pod or Secret"
            );
            ensure!(
                svc.state
                    .close_launch(saved.id, LaunchState::Cancelled)
                    .await?,
                "cleanup did not close the saved launch"
            );
            println!(
                "PASS: scoped Pod deletion, credential garbage collection and unregistration preserve unrelated resources"
            );
        }
    }
    Ok(())
}
