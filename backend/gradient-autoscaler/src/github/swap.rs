use crate::config::Connection;
use crate::github::oidc::Claims;
use crate::gradient::{GradientClient, worker_server_url};
use crate::state::State;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct SwapRequest {
    pub token: String,
    pub launch_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwapResponse {
    pub server_url: String,
    pub team: Uuid,
    pub worker_id: Uuid,
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("token of repository {0} does not belong to this connection")]
    WrongRepository(String),
    #[error("no open launch matches this run")]
    UnknownLaunch,
    #[error("this launch already received its worker token")]
    AlreadySwapped,
    #[error("{0:#}")]
    Failed(#[from] anyhow::Error),
}

pub async fn swap(
    state: &State,
    conn: &Connection,
    repository: &str,
    gradient: &GradientClient,
    claims: &Claims,
    launch_id: Uuid,
) -> Result<SwapResponse, SwapError> {
    if !claims.repository.eq_ignore_ascii_case(repository) {
        return Err(SwapError::WrongRepository(claims.repository.clone()));
    }

    let run_id = claims.run_id.to_string();
    let launch = state
        .launch(&conn.name, launch_id)
        .await?
        .filter(|launch| launch.external_id == run_id && launch.state.is_open())
        .ok_or(SwapError::UnknownLaunch)?;

    let worker_id = Uuid::new_v4();
    if !state.mark_swapped(launch.id, worker_id).await? {
        return Err(SwapError::AlreadySwapped);
    }

    let display_name = format!("gha-{}-{}", claims.run_id, claims.run_attempt);
    let registered = gradient
        .register_team_worker_with_capabilities(
            &conn.team,
            worker_id,
            &display_name,
            conn.worker_capabilities,
        )
        .await?;
    Ok(SwapResponse {
        server_url: worker_server_url(&conn.gradient_url)?,
        team: registered.team,
        worker_id,
        token: registered.token,
    })
}
