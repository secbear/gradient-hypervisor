use crate::kinds::Profile;
use crate::rest::status_of;
use crate::service::ConnectionService;
use crate::state::{Launch, LaunchState};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

const LEARN_INTERVAL: Duration = Duration::from_secs(10);

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
    svc.state.advance_launch(launch.id, to).await?;
    Ok(())
}

pub async fn finish_launch(svc: &ConnectionService, launch: &Launch) -> anyhow::Result<()> {
    let Some(launch) = svc.state.launch(&svc.conn.name, launch.id).await? else {
        return Ok(());
    };
    if !launch.state.is_open() {
        return Ok(());
    }
    if let Some(worker_id) = launch.worker_id {
        match svc
            .gradient
            .delete_team_worker(&svc.conn.team, worker_id)
            .await
        {
            Err(error) if status_of(&error) != Some(StatusCode::NOT_FOUND) => return Err(error),
            _ => {}
        }
    }
    let outcome = if launch.state == LaunchState::Connected {
        LaunchState::Completed
    } else {
        tracing::warn!(connection = %svc.conn.name, run_id = launch.run_id, label = %launch.label, "run ended before its worker connected");
        LaunchState::Failed
    };
    svc.state.close_launch(launch.id, outcome).await?;
    Ok(())
}

pub async fn learn_profile(svc: &ConnectionService, launch_id: Uuid) {
    let attempts = (svc.conn.max_lifetime + 600) / LEARN_INTERVAL.as_secs();
    for _ in 0..attempts {
        tokio::time::sleep(LEARN_INTERVAL).await;
        match svc.state.launch(&svc.conn.name, launch_id).await {
            Ok(Some(launch)) if launch.state < LaunchState::Connected => {}
            Ok(_) => return,
            Err(error) => tracing::warn!(connection = %svc.conn.name, "reading launch: {error:#}"),
        }
        if let Err(error) = record_connected_workers(svc).await {
            tracing::warn!(connection = %svc.conn.name, "reading team workers: {error:#}");
        }
    }
}

pub async fn record_connected_workers(svc: &ConnectionService) -> anyhow::Result<()> {
    let waiting: Vec<Launch> = svc
        .state
        .open_launches(&svc.conn.name)
        .await?
        .into_iter()
        .filter(|launch| launch.worker_id.is_some() && launch.state < LaunchState::Connected)
        .collect();
    if waiting.is_empty() {
        return Ok(());
    }
    let workers = svc.gradient.team_workers(&svc.conn.team).await?;
    for launch in waiting {
        let worker_id = launch.worker_id.map(|id| id.to_string());
        let live = workers
            .iter()
            .find(|worker| Some(&worker.worker_id) == worker_id.as_ref())
            .and_then(|worker| worker.live.clone());
        if let Some(live) = live {
            let profile = Profile {
                systems: live.architectures,
                features: live.system_features,
                max_jobs: live.max_concurrent_builds,
                ram_mb: live.ram_total_mb,
            };
            svc.state
                .put_profile(&svc.conn.name, &launch.label, &profile)
                .await?;
            svc.state
                .advance_launch(launch.id, LaunchState::Connected)
                .await?;
            tracing::info!(connection = %svc.conn.name, label = %launch.label, ?profile, "worker connected; runner profile learned");
        }
    }
    Ok(())
}
