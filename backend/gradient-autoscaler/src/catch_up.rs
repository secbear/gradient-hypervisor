use crate::events::{advance, finish_launch, record_connected_workers};
use crate::launcher::Progress;
use crate::scaler::scale;
use crate::service::ConnectionService;
use crate::setup::setup;
use crate::state::LaunchState;
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
                reconcile_and_scale(&svc).await;
            }
        }
    }
}

pub async fn catch_up(svc: &Arc<ConnectionService>) {
    setup(svc).await;
    reconcile_and_scale(svc).await;
}

async fn reconcile_and_scale(svc: &Arc<ConnectionService>) {
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
        let progress = match svc.launcher.progress(&launch).await {
            Ok(progress) => progress,
            Err(error) => {
                tracing::warn!(connection = %svc.conn.name, external_id = %launch.external_id, "reading launch progress: {error:#}");
                continue;
            }
        };
        let result = match progress {
            Progress::Ended => finish_launch(svc, &launch).await,
            Progress::Running => advance(svc, &launch, LaunchState::Running).await,
            Progress::Waiting => advance(svc, &launch, LaunchState::Queued).await,
        };
        if let Err(error) = result {
            tracing::warn!(connection = %svc.conn.name, external_id = %launch.external_id, "closing launch: {error:#}");
        }
    }

    Ok(())
}
