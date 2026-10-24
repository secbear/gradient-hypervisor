use crate::github::DispatchInputs;
use crate::kinds::Kind;
use crate::planner::{backed_off_labels, plan};
use crate::service::ConnectionService;
use crate::state::Launch;
use crate::workflow::WORKFLOW_FILE;
use uuid::Uuid;

pub async fn scale(svc: &ConnectionService) -> anyhow::Result<()> {
    let conn = &svc.conn;
    let demand = svc
        .gradient
        .team_demand(&conn.team, conn.projects.as_deref())
        .await?;
    let open = svc.state.open_launches(&conn.name).await?;
    let kinds = svc.kinds().await?;
    let backoff = backed_off_labels(&svc.state, conn, &kinds).await?;
    let plan = plan(&demand, &open, &kinds, conn, &backoff);

    for (system, features) in &plan.uncovered {
        tracing::warn!(connection = %conn.name, system, ?features, "no allowed runner kind covers this demand");
    }
    for launch in open
        .iter()
        .filter(|launch| plan.cancel.contains(&launch.run_id))
    {
        cancel(svc, launch).await;
    }
    if plan.launch.is_empty() {
        return Ok(());
    }
    let branch = svc.github.repository_info().await?.default_branch;
    let worker_flake = worker_flake(svc).await?;
    for label in &plan.launch {
        let Some(kind) = kinds.iter().find(|kind| &kind.label == label) else {
            continue;
        };
        if let Err(error) = dispatch(svc, kind, &branch, &worker_flake).await {
            tracing::error!(connection = %conn.name, label, "dispatching worker run: {error:#}");
        }
    }
    Ok(())
}

async fn cancel(svc: &ConnectionService, launch: &Launch) {
    match svc.state.claim_cancel(launch.id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::error!(connection = %svc.conn.name, "recording cancelled run: {error:#}");
            return;
        }
    }
    match svc.github.cancel_run(launch.run_id).await {
        Ok(()) => {
            tracing::info!(connection = %svc.conn.name, run_id = launch.run_id, "cancelled unneeded run")
        }
        Err(error) => {
            tracing::warn!(connection = %svc.conn.name, run_id = launch.run_id, "cancelling run: {error:#}")
        }
    }
}

async fn worker_flake(svc: &ConnectionService) -> anyhow::Result<String> {
    if let Some(flake) = &svc.conn.worker_flake {
        return Ok(flake.clone());
    }
    let version = svc.gradient.version().await?;
    Ok(format!("github:wavelens/gradient/v{version}#gradient"))
}

async fn dispatch(
    svc: &ConnectionService,
    kind: &Kind,
    branch: &str,
    worker_flake: &str,
) -> anyhow::Result<()> {
    let launch_id = Uuid::new_v4();
    let inputs = DispatchInputs {
        runs_on: kind.runs_on(),
        autoscaler_url: svc.public_url.as_str().trim_end_matches('/').to_owned(),
        connection: svc.conn.name.clone(),
        launch_id,
        worker_flake: worker_flake.to_owned(),
        idle_timeout: svc.conn.idle_timeout,
        max_lifetime: svc.conn.max_lifetime,
        substituters: svc.conn.worker_substituters.join(" "),
        trusted_public_keys: svc.conn.worker_trusted_public_keys.join(" "),
    };
    let run_id = svc.github.dispatch(WORKFLOW_FILE, branch, &inputs).await?;
    let launch = Launch {
        id: launch_id,
        ..Launch::new(&svc.conn.name, run_id, &kind.label)
    };
    svc.state.insert_launch(&launch).await?;
    tracing::info!(connection = %svc.conn.name, run_id, label = %kind.label, "dispatched worker run");
    Ok(())
}
