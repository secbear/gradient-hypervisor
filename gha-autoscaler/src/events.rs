use crate::kinds::Profile;
use crate::rest::status_of;
use crate::service::ConnectionService;
use crate::state::{Launch, LaunchState};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

const LEARN_INTERVAL: Duration = Duration::from_secs(10);
const LEARN_ATTEMPTS: u32 = 30;

#[derive(Debug, Deserialize)]
pub struct WorkflowRunEvent {
    pub action: String,
    pub workflow_run: RunPayload,
}

#[derive(Debug, Deserialize)]
pub struct RunPayload {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowJobEvent {
    pub action: String,
    pub workflow_job: JobPayload,
}

#[derive(Debug, Deserialize)]
pub struct JobPayload {
    pub run_id: u64,
}

pub async fn on_workflow_run(
    svc: &ConnectionService,
    event: &WorkflowRunEvent,
) -> anyhow::Result<()> {
    let Some(launch) = svc
        .state
        .launch_by_run(&svc.conn.name, event.workflow_run.id)
        .await?
    else {
        return Ok(());
    };
    match event.action.as_str() {
        "requested" => advance(svc, &launch, LaunchState::Queued).await,
        "in_progress" => advance(svc, &launch, LaunchState::Running).await,
        "completed" => finish_launch(svc, &launch).await,
        _ => Ok(()),
    }
}

pub async fn on_workflow_job(
    svc: &ConnectionService,
    event: &WorkflowJobEvent,
) -> anyhow::Result<()> {
    let Some(launch) = svc
        .state
        .launch_by_run(&svc.conn.name, event.workflow_job.run_id)
        .await?
    else {
        return Ok(());
    };
    match event.action.as_str() {
        "queued" => advance(svc, &launch, LaunchState::Queued).await,
        "in_progress" => advance(svc, &launch, LaunchState::Running).await,
        _ => Ok(()),
    }
}

pub async fn advance(
    svc: &ConnectionService,
    launch: &Launch,
    to: LaunchState,
) -> anyhow::Result<()> {
    if launch.state.is_open() && to > launch.state {
        svc.state.set_launch_state(launch.id, to).await?;
    }
    Ok(())
}

pub async fn finish_launch(svc: &ConnectionService, launch: &Launch) -> anyhow::Result<()> {
    if !launch.state.is_open() {
        return Ok(());
    }
    let Some(worker_id) = launch.worker_id else {
        tracing::warn!(connection = %svc.conn.name, run_id = launch.run_id, label = %launch.label, "run ended before its worker connected");
        return svc
            .state
            .set_launch_state(launch.id, LaunchState::Failed)
            .await;
    };
    match svc
        .gradient
        .delete_team_worker(&svc.conn.team, worker_id)
        .await
    {
        Err(error) if status_of(&error) != Some(StatusCode::NOT_FOUND) => return Err(error),
        _ => {}
    }
    svc.state
        .set_launch_state(launch.id, LaunchState::Completed)
        .await
}

pub async fn learn_profile(svc: &ConnectionService, launch_id: Uuid, worker_id: Uuid) {
    for _ in 0..LEARN_ATTEMPTS {
        tokio::time::sleep(LEARN_INTERVAL).await;
        match try_learn(svc, launch_id, worker_id).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(connection = %svc.conn.name, "reading team workers: {error:#}")
            }
        }
    }
}

async fn try_learn(
    svc: &ConnectionService,
    launch_id: Uuid,
    worker_id: Uuid,
) -> anyhow::Result<bool> {
    let workers = svc.gradient.team_workers(&svc.conn.team).await?;
    let wanted = worker_id.to_string();
    let Some(live) = workers
        .into_iter()
        .find(|worker| worker.worker_id == wanted)
        .and_then(|worker| worker.live)
    else {
        return Ok(false);
    };
    let Some(launch) = svc.state.launch(&svc.conn.name, launch_id).await? else {
        return Ok(true);
    };
    let profile = Profile {
        systems: live.architectures,
        features: live.system_features,
        max_jobs: live.max_concurrent_builds,
        ram_mb: live.ram_total_mb,
    };
    svc.state
        .put_profile(&svc.conn.name, &launch.label, &profile)
        .await?;
    advance(svc, &launch, LaunchState::Connected).await?;
    tracing::info!(connection = %svc.conn.name, label = %launch.label, ?profile, "learned runner profile");
    Ok(true)
}
