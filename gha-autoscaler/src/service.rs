use crate::config::Connection;
use crate::github::GitHubClient;
use crate::gradient::GradientClient;
use crate::kinds::{Kind, apply_learned, standard_kinds};
use crate::state::State;
use std::sync::RwLock;
use tokio::sync::Notify;
use url::Url;

pub struct ConnectionService {
    pub conn: Connection,
    pub public_url: Url,
    pub gradient: GradientClient,
    pub github: GitHubClient,
    pub state: State,
    pub wake: Notify,
    discovered_kinds: RwLock<Vec<Kind>>,
}

impl ConnectionService {
    pub fn new(
        conn: Connection,
        public_url: Url,
        gradient: GradientClient,
        github: GitHubClient,
        state: State,
    ) -> ConnectionService {
        ConnectionService {
            conn,
            public_url,
            gradient,
            github,
            state,
            wake: Notify::new(),
            discovered_kinds: RwLock::new(standard_kinds()),
        }
    }

    pub fn endpoint(&self, kind: &str) -> String {
        let base = self.public_url.as_str().trim_end_matches('/');
        format!("{base}/c/{}/{kind}", self.conn.name)
    }

    pub fn set_kinds(&self, kinds: Vec<Kind>) {
        *self.discovered_kinds.write().expect("kinds lock") = kinds;
    }

    pub async fn kinds(&self) -> anyhow::Result<Vec<Kind>> {
        let mut kinds = self.discovered_kinds.read().expect("kinds lock").clone();
        let labels: Vec<String> = kinds.iter().map(|kind| kind.label.clone()).collect();
        for label in labels {
            if let Some(profile) = self.state.profile(&self.conn.name, &label).await? {
                apply_learned(&mut kinds, &label, profile);
            }
        }
        Ok(kinds)
    }
}
