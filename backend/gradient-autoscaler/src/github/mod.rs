pub mod client;
pub mod events;
pub mod kinds;
pub mod oidc;
pub mod setup;
pub mod swap;
pub mod workflow;

use crate::config::GitHubActions;
use crate::kinds::Kind;
use crate::launcher::Progress;
use crate::rest::status_of;
use crate::service::ConnectionService;
use crate::state::Launch;
use client::{DispatchInputs, GitHubClient};
use reqwest::StatusCode;
use std::sync::Arc;
use uuid::Uuid;
use workflow::WORKFLOW_FILE;

pub struct GitHubLauncher {
    pub config: GitHubActions,
    pub client: GitHubClient,
}

impl GitHubLauncher {
    pub fn new(config: GitHubActions, client: GitHubClient) -> GitHubLauncher {
        GitHubLauncher { config, client }
    }

    pub fn initial_kinds(&self) -> Vec<Kind> {
        kinds::standard_kinds()
    }

    pub fn allowed_kinds(&self) -> Vec<String> {
        self.config.allowed_kinds.clone()
    }

    pub async fn setup(&self, svc: &ConnectionService) -> Option<Vec<Kind>> {
        setup::setup(svc, self).await
    }

    pub async fn launch(
        &self,
        svc: &Arc<ConnectionService>,
        kinds: &[&Kind],
    ) -> anyhow::Result<()> {
        let branch = self.client.repository_info().await?.default_branch;
        let worker_flake = self.worker_flake(svc).await?;
        for kind in kinds {
            if let Err(error) = self.dispatch(svc, kind, &branch, &worker_flake).await {
                tracing::error!(connection = %svc.conn.name, label = %kind.label, "dispatching worker run: {error:#}");
            }
        }

        Ok(())
    }

    pub async fn progress(&self, launch: &Launch) -> anyhow::Result<Progress> {
        let status = match self.client.run(run_id(launch)?).await {
            Ok(run) => run.status,
            Err(error) if status_of(&error) == Some(StatusCode::NOT_FOUND) => {
                return Ok(Progress::Ended);
            }
            Err(error) => return Err(error),
        };
        Ok(match status.as_str() {
            "completed" => Progress::Ended,
            "in_progress" => Progress::Running,
            _ => Progress::Waiting,
        })
    }

    pub async fn cancel(&self, launch: &Launch) -> anyhow::Result<()> {
        self.client.cancel_run(run_id(launch)?).await
    }

    async fn worker_flake(&self, svc: &ConnectionService) -> anyhow::Result<String> {
        if let Some(flake) = &self.config.worker_flake {
            return Ok(flake.clone());
        }

        let version = svc.gradient.version().await?;
        Ok(format!("github:wavelens/gradient/v{version}#gradient"))
    }

    async fn dispatch(
        &self,
        svc: &ConnectionService,
        kind: &Kind,
        branch: &str,
        worker_flake: &str,
    ) -> anyhow::Result<()> {
        let launch_id = Uuid::new_v4();
        let inputs = DispatchInputs {
            runs_on: kinds::runs_on(kind),
            autoscaler_url: svc.public_url.as_str().trim_end_matches('/').to_owned(),
            connection: svc.conn.name.clone(),
            launch_id,
            worker_flake: worker_flake.to_owned(),
            idle_timeout: svc.conn.idle_timeout,
            max_lifetime: svc.conn.max_lifetime,
            substituters: svc.conn.worker_substituters.join(" "),
            trusted_public_keys: svc.conn.worker_trusted_public_keys.join(" "),
        };
        let run_id = self.client.dispatch(WORKFLOW_FILE, branch, &inputs).await?;
        let launch = Launch {
            id: launch_id,
            ..Launch::new(&svc.conn.name, &run_id.to_string(), &kind.label)
        };
        svc.state.insert_launch(&launch).await?;
        tracing::info!(connection = %svc.conn.name, run_id, label = %kind.label, "dispatched worker run");
        Ok(())
    }
}

fn run_id(launch: &Launch) -> anyhow::Result<u64> {
    Ok(launch.external_id.parse()?)
}
