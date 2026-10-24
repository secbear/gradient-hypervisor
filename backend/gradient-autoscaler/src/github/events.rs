use crate::events::{advance, finish_launch};
use crate::service::ConnectionService;
use crate::state::LaunchState;
use serde::Deserialize;

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
    let run_id = event.workflow_run.id.to_string();
    let Some(launch) = svc
        .state
        .launch_by_external_id(&svc.conn.name, &run_id)
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
    let run_id = event.workflow_job.run_id.to_string();
    let Some(launch) = svc
        .state
        .launch_by_external_id(&svc.conn.name, &run_id)
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
