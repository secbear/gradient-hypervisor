use crate::events::delete_worker;
use crate::kinds::Kind;
use crate::planner::{Limits, backed_off_labels, plan};
use crate::service::ConnectionService;
use crate::state::Launch;
use std::sync::Arc;

pub async fn scale(svc: &Arc<ConnectionService>) -> anyhow::Result<()> {
    if svc.conn.is_preview() {
        let preview = crate::build_preview::preview_build_capacity(svc).await?;
        let pools = preview
            .plan
            .proposed_workers
            .iter()
            .map(|worker| worker.label.as_str())
            .collect::<Vec<_>>();
        tracing::info!(
            connection = %svc.conn.name, captured_at = %preview.captured_at,
            ready = preview.total_ready_builds, assessed = preview.assessed_builds,
            ?pools, deferred = preview.plan.deferred_builds.len(),
            starting = preview.plan.credited_starting_workers.len(),
            draining = preview.plan.server_draining, truncated = preview.plan.builds_truncated,
            "build capacity preview; no worker changes"
        );
        tracing::debug!(connection = %svc.conn.name, plan = ?preview.plan, "build capacity preview details");
        return Ok(());
    }
    let conn = &svc.conn;
    let demand = svc
        .gradient
        .team_demand(&conn.team, conn.projects.as_deref())
        .await?
        .restricted_to(conn.worker_capabilities);
    let open = svc.state.open_launches(&conn.name).await?;
    let kinds = svc.kinds().await?;
    let backoff = backed_off_labels(&svc.state, &conn.name, &kinds).await?;
    let limits = Limits {
        allowed: svc.launcher.allowed_kinds(),
        max_launches: conn.max_launches,
        max_launches_per_kind: conn.max_launches_per_kind,
    };
    let plan = plan(&demand, &open, &kinds, &limits, &backoff);

    for (system, features) in &plan.uncovered {
        tracing::warn!(connection = %conn.name, system, ?features, "no allowed kind covers this demand");
    }

    for launch in open
        .iter()
        .filter(|launch| plan.cancel.contains(&launch.id))
    {
        cancel(svc, launch).await;
    }

    let launching: Vec<&Kind> = plan
        .launch
        .iter()
        .filter_map(|label| kinds.iter().find(|kind| &kind.label == label))
        .collect();
    if launching.is_empty() {
        return Ok(());
    }

    svc.launcher.launch(svc, &launching).await
}

async fn cancel(svc: &ConnectionService, launch: &Launch) {
    match svc.state.claim_cancel(launch.id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::error!(connection = %svc.conn.name, "recording cancelled launch: {error:#}");
            return;
        }
    }

    let result = match svc.launcher.cancel(launch).await {
        Ok(()) => delete_worker(svc, launch).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            tracing::info!(connection = %svc.conn.name, external_id = %launch.external_id, "cancelled unneeded launch")
        }
        Err(error) => {
            tracing::warn!(connection = %svc.conn.name, external_id = %launch.external_id, "cancelling launch: {error:#}")
        }
    }
}
