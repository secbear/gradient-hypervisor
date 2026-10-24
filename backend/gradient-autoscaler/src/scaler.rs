use crate::events::delete_worker;
use crate::kinds::Kind;
use crate::planner::{Limits, backed_off_labels, plan};
use crate::service::ConnectionService;
use crate::state::Launch;
use std::sync::Arc;

pub async fn scale(svc: &Arc<ConnectionService>) -> anyhow::Result<()> {
    let conn = &svc.conn;
    let demand = svc
        .gradient
        .team_demand(&conn.team, conn.projects.as_deref())
        .await?;
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
