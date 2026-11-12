use crate::build_planner::{BuildPlan, BuildPlanningContext, plan_build_capacity};
use crate::capacity::{
    BuildCapacityProfile, CapacityAssessmentRequest, CapacityCapabilities, CapacityProfileRequest,
};
use crate::config::LauncherConfig;
use crate::planner::{Limits, backed_off_labels};
use crate::service::ConnectionService;
use crate::state::LaunchState;
use anyhow::{Context, ensure};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug)]
pub struct BuildPreview {
    pub captured_at: String,
    pub total_ready_builds: usize,
    pub assessed_builds: usize,
    pub plan: BuildPlan,
}

pub async fn preview_build_capacity(svc: &ConnectionService) -> anyhow::Result<BuildPreview> {
    let config = svc
        .conn
        .build_scaling
        .as_ref()
        .context("build scaling preview is not configured")?;
    ensure!(svc.conn.is_preview(), "build scaling mode is not preview");
    let LauncherConfig::Kubernetes(kubernetes) = &svc.conn.launcher else {
        anyhow::bail!("build scaling preview requires Kubernetes");
    };
    let open = svc.state.open_launches(&svc.conn.name).await?;
    let now = OffsetDateTime::now_utc();
    let mut request = CapacityAssessmentRequest {
        projects: svc.conn.capacity_projects()?,
        include_waiting_builds: true,
        ..Default::default()
    };
    let caps = svc.conn.worker_capabilities;
    let mut labels = Vec::new();
    for label in &config.profiles {
        let pool = kubernetes
            .pools
            .get(label)
            .context("preview pool is not configured")?;
        let resources = pool
            .build_resources
            .as_ref()
            .context("preview pool has no build resources")?;
        for _ in 0..config.max_new_workers.max(1) {
            let worker_id = Uuid::new_v4();
            request.profiles.push(CapacityProfileRequest {
                profile: BuildCapacityProfile {
                    name: format!("{label}-{worker_id}"),
                    worker_id,
                    capabilities: CapacityCapabilities {
                        build: caps.build,
                        network: caps.network,
                        secrets: caps.secrets,
                        exec: caps.exec,
                        eval: caps.eval,
                        ..Default::default()
                    },
                    resources: resources.for_pool(pool),
                },
                enable_build: caps.build,
                enable_network: caps.network,
            });
            labels.push(label.clone());
        }
    }
    for launch in &open {
        if let Some(intent) = &launch.build_intent
            && launch.registration_started
            && launch.state < LaunchState::Connected
            && launch.dispatched_at <= now
            && intent.startup_deadline > now
            && svc
                .conn
                .worker_capabilities
                .permits(intent.profile.registration_capabilities())
            && intent.kubernetes.as_ref().is_some_and(|spec| {
                spec.gradient_url == *svc.gradient.base_url() && spec.team == svc.conn.team
            })
        {
            request.tracked_profiles.push(CapacityProfileRequest {
                profile: intent.profile.clone(),
                enable_build: intent.profile.capabilities.build,
                enable_network: intent.profile.capabilities.network,
            });
        }
    }
    request.validate()?;
    let kinds = svc.kinds().await?;
    let backoff = backed_off_labels(&svc.state, &svc.conn.name, &kinds).await?;
    let limits = Limits {
        allowed: config.profiles.clone(),
        max_launches: svc.conn.max_launches,
        max_launches_per_kind: svc.conn.max_launches_per_kind,
    };
    let capacity = svc
        .gradient
        .capacity_assessment(&svc.conn.team, &request)
        .await?;
    let policy = config.policy();
    let plan = plan_build_capacity(
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
    )?;
    Ok(BuildPreview {
        captured_at: capacity.captured_at,
        total_ready_builds: capacity.total_ready_builds,
        assessed_builds: capacity.builds.len(),
        plan,
    })
}
