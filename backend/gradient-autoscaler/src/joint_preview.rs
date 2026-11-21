use crate::build_planner::BuildPlanningContext;
use crate::build_preview::build_preview_request;
use crate::capacity::{
    BootstrapCapacityRequest, BootstrapProfileRequest, BuildPageRequest, BuilderResources,
    CapabilityProfile, CapacityCapabilities, RegistrationCapabilities, TrackedCapacityWorker,
};
use crate::config::LauncherConfig;
use crate::joint_planner::{JointPlan, plan_joint_capacity};
use crate::planner::{Limits, backed_off_labels};
use crate::service::ConnectionService;
use anyhow::{Context, ensure};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug)]
pub struct JointPreview {
    pub captured_at: String,
    pub total_ready_builds: usize,
    pub assessed_builds: usize,
    pub tracked_workers: Vec<TrackedCapacityWorker>,
    pub plan: JointPlan,
}

pub async fn preview_joint_capacity(svc: &ConnectionService) -> anyhow::Result<JointPreview> {
    let mut cursor = svc.joint_preview_cursor.lock().await;
    let result = preview_joint_page(svc, &mut cursor).await;
    if result.is_err() {
        *cursor = None;
    }
    result
}

async fn preview_joint_page(
    svc: &ConnectionService,
    cursor: &mut Option<String>,
) -> anyhow::Result<JointPreview> {
    ensure!(
        svc.conn.is_joint_preview(),
        "build scaling mode is not joint preview"
    );
    let config = svc
        .conn
        .build_scaling
        .as_ref()
        .context("joint preview is not configured")?;
    let LauncherConfig::Kubernetes(kubernetes) = &svc.conn.launcher else {
        anyhow::bail!("joint capacity preview requires Kubernetes");
    };
    let open = svc.state.open_launches(&svc.conn.name).await?;
    let (mut request, labels) = build_preview_request(svc, &open, OffsetDateTime::now_utc(), true)?;
    let mut bootstrap = BootstrapCapacityRequest::default();
    let mut extra_labels = Vec::new();
    for configured in &config.bootstrap_profiles {
        let label = &configured.pool;
        let pool = kubernetes
            .pools
            .get(label)
            .context("bootstrap pool is not configured")?;
        let caps = configured.capabilities;
        ensure!(
            !caps.build
                && (caps.eval || caps.network)
                && svc.conn.worker_capabilities.permits(caps),
            "bootstrap profile is not a permitted role-only worker"
        );
        for _ in 0..config.max_new_workers.max(1) {
            let worker_id = Uuid::new_v4();
            bootstrap.profiles.push(BootstrapProfileRequest {
                profile: CapabilityProfile {
                    name: format!("{label}-{worker_id}"),
                    worker_id,
                    capabilities: CapacityCapabilities {
                        build: false,
                        network: caps.network,
                        eval: caps.eval,
                        secrets: caps.secrets,
                        exec: caps.exec,
                        ..Default::default()
                    },
                    resources: BuilderResources {
                        architectures: vec![pool.system.clone()],
                        system_features: pool.features.clone(),
                        max_concurrent_builds: pool.max_jobs,
                        cpu_count: 0,
                        ram_total_mb: 0,
                        cpu_core_score: 0,
                        build_cores: None,
                    },
                },
                registration: RegistrationCapabilities::from(caps),
            });
            extra_labels.push(label.clone());
        }
    }
    request.bootstrap = Some(bootstrap);
    // A cursor selects one fresh page, never a concatenation of snapshots.
    request.build_page = Some(BuildPageRequest {
        after: cursor.clone(),
    });
    request.validate()?;
    let kinds = svc.kinds().await?;
    let backoff = backed_off_labels(&svc.state, &svc.conn.name, &kinds).await?;
    let mut allowed = config.profiles.clone();
    for profile in &config.bootstrap_profiles {
        if !allowed.contains(&profile.pool) {
            allowed.push(profile.pool.clone());
        }
    }
    let limits = Limits {
        allowed,
        max_launches: svc.conn.max_launches,
        max_launches_per_kind: svc.conn.max_launches_per_kind,
    };
    let capacity = svc
        .gradient
        .capacity_assessment(&svc.conn.team, &request)
        .await?;
    *cursor = capacity
        .build_page
        .as_ref()
        .context("requested capacity build page is missing")?
        .next
        .clone();
    let policy = config.policy();
    let plan = plan_joint_capacity(
        &request,
        &capacity,
        BuildPlanningContext {
            connection: &svc.conn.name,
            open: &open,
            candidate_labels: &labels,
            limits: &limits,
            backoff: &backoff,
            policy: &policy,
            now: OffsetDateTime::now_utc(),
        },
        &extra_labels,
    )?;
    Ok(JointPreview {
        captured_at: capacity.captured_at,
        total_ready_builds: capacity.total_ready_builds,
        assessed_builds: capacity.builds.len(),
        tracked_workers: capacity.tracked_workers,
        plan,
    })
}
