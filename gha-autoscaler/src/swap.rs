use crate::config::Connection;
use crate::gradient::GradientClient;
use crate::oidc::Claims;
use crate::state::State;
use serde::{Deserialize, Serialize};
use url::Url;
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
    gradient: &GradientClient,
    claims: &Claims,
    launch_id: Uuid,
) -> Result<SwapResponse, SwapError> {
    if !claims.repository.eq_ignore_ascii_case(&conn.repository) {
        return Err(SwapError::WrongRepository(claims.repository.clone()));
    }
    let launch = state
        .launch(&conn.name, launch_id)
        .await?
        .filter(|launch| launch.run_id == claims.run_id && launch.state.is_open())
        .ok_or(SwapError::UnknownLaunch)?;

    let worker_id = Uuid::new_v4();
    if !state.mark_swapped(launch.id, worker_id).await? {
        return Err(SwapError::AlreadySwapped);
    }
    let display_name = format!("gha-{}-{}", claims.run_id, claims.run_attempt);
    let registered = gradient
        .register_team_worker(&conn.team, worker_id, &display_name)
        .await?;
    Ok(SwapResponse {
        server_url: worker_server_url(&conn.gradient_url)?,
        team: registered.team,
        worker_id,
        token: registered.token,
    })
}

pub fn worker_server_url(gradient_url: &Url) -> anyhow::Result<String> {
    let mut url = gradient_url.clone();
    let scheme = if url.scheme() == "http" { "ws" } else { "wss" };
    url.set_scheme(scheme)
        .map_err(|()| anyhow::anyhow!("cannot turn {gradient_url} into a websocket URL"))?;
    url.set_path("/proto");
    url.set_query(None);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_server_url_uses_websocket_proto_endpoint() {
        let https = Url::parse("https://gradient.example.com/ui?x=1").unwrap();
        let http = Url::parse("http://10.0.0.2:3000").unwrap();
        assert_eq!(
            worker_server_url(&https).unwrap(),
            "wss://gradient.example.com/proto"
        );
        assert_eq!(
            worker_server_url(&http).unwrap(),
            "ws://10.0.0.2:3000/proto"
        );
    }
}
