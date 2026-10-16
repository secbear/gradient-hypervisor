use crate::events::{advance, finish_launch, record_connected_workers};
use crate::rest::status_of;
use crate::scaler::scale;
use crate::service::ConnectionService;
use crate::setup::setup;
use crate::state::LaunchState;
use reqwest::StatusCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, sleep, timeout};

const DEBOUNCE: Duration = Duration::from_secs(2);

pub async fn run_connection(svc: Arc<ConnectionService>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => catch_up(&svc).await,
            () = svc.wake.notified() => {
                sleep(DEBOUNCE).await;
                let _ = timeout(Duration::ZERO, svc.wake.notified()).await;
                if let Err(error) = scale(&svc).await {
                    tracing::warn!(connection = %svc.conn.name, "scaling: {error:#}");
                }
            }
        }
    }
}

pub async fn catch_up(svc: &ConnectionService) {
    setup(svc).await;
    if let Err(error) = reconcile(svc).await {
        tracing::warn!(connection = %svc.conn.name, "reconciling launches: {error:#}");
    }
    if let Err(error) = scale(svc).await {
        tracing::warn!(connection = %svc.conn.name, "scaling: {error:#}");
    }
}

async fn reconcile(svc: &ConnectionService) -> anyhow::Result<()> {
    if let Err(error) = record_connected_workers(svc).await {
        tracing::warn!(connection = %svc.conn.name, "reading team workers: {error:#}");
    }
    for launch in svc.state.open_launches(&svc.conn.name).await? {
        let status = match svc.github.run(launch.run_id).await {
            Ok(run) => run.status,
            Err(error) if status_of(&error) == Some(StatusCode::NOT_FOUND) => {
                "completed".to_owned()
            }
            Err(error) => {
                tracing::warn!(connection = %svc.conn.name, run_id = launch.run_id, "reading run: {error:#}");
                continue;
            }
        };
        let result = match status.as_str() {
            "completed" => finish_launch(svc, &launch).await,
            "in_progress" => advance(svc, &launch, LaunchState::Running).await,
            "queued" | "requested" | "waiting" | "pending" => {
                advance(svc, &launch, LaunchState::Queued).await
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            tracing::warn!(connection = %svc.conn.name, run_id = launch.run_id, "closing launch: {error:#}");
        }
    }
    Ok(())
}
