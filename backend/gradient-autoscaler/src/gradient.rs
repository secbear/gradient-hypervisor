use crate::config::WorkerCapabilities;
use crate::rest::send;
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct GradientClient {
    http: Client,
    base: Url,
    api_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TeamDemand {
    pub builds: Vec<UnmetRequirement>,
    pub network: u32,
    pub secrets: u32,
    pub eval: u32,
}

impl TeamDemand {
    pub fn restricted_to(mut self, caps: WorkerCapabilities) -> Self {
        if !caps.build {
            self.builds.clear();
        }
        for (enabled, count) in [
            (caps.network, &mut self.network),
            (caps.network && caps.secrets, &mut self.secrets),
            (caps.eval, &mut self.eval),
        ] {
            if !enabled {
                *count = 0;
            }
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UnmetRequirement {
    pub architecture: String,
    pub required_features: Vec<String>,
    pub build_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamWorker {
    pub worker_id: String,
    pub display_name: String,
    #[serde(default)]
    pub connected: bool,
    #[serde(default)]
    pub live: Option<WorkerLive>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerLive {
    pub architectures: Vec<String>,
    pub system_features: Vec<String>,
    pub max_concurrent_builds: u32,
    #[serde(default)]
    pub ram_total_mb: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RegisteredWorker {
    pub team: Uuid,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamWebhook {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
}

#[derive(Deserialize)]
struct Envelope<T> {
    message: T,
}

#[derive(Deserialize)]
struct ServerConfig {
    version: String,
}

#[derive(Deserialize)]
struct RegisteredTeamWorker {
    team: Uuid,
    token: Option<String>,
}

#[derive(Serialize)]
struct RegisterTeamWorkerRequest<'a> {
    worker_id: Uuid,
    display_name: &'a str,
    #[serde(flatten)]
    capabilities: WorkerCapabilities,
}

#[derive(Deserialize)]
struct CreatedWebhook {
    secret: String,
}

#[derive(Deserialize)]
struct RotatedSecret {
    secret: String,
}

impl GradientClient {
    pub fn new(base: Url, api_key: String) -> GradientClient {
        GradientClient {
            http: crate::rest::http_client(),
            base,
            api_key,
        }
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut url = self.base.clone();
        url.set_path(path);
        self.http
            .request(method, url)
            .bearer_auth(&self.api_key)
            .header("Accept", "application/json")
    }

    async fn message<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> anyhow::Result<T> {
        let envelope: Envelope<T> = send(&self.http, request).await?.json().await?;
        Ok(envelope.message)
    }

    pub async fn version(&self) -> anyhow::Result<String> {
        let config: ServerConfig = self
            .message(self.request(Method::GET, "/api/v1/config"))
            .await?;
        Ok(config.version)
    }

    pub async fn team_demand(
        &self,
        team: &str,
        projects: Option<&[String]>,
    ) -> anyhow::Result<TeamDemand> {
        let mut request =
            self.request(Method::GET, &format!("/api/v1/teams/{team}/workers/demand"));
        if let Some(projects) = projects {
            request = request.query(&[("projects", projects.join(","))]);
        }
        self.message(request).await
    }

    pub async fn team_workers(&self, team: &str) -> anyhow::Result<Vec<TeamWorker>> {
        self.message(self.request(Method::GET, &format!("/api/v1/teams/{team}/workers")))
            .await
    }

    pub async fn register_team_worker(
        &self,
        team: &str,
        worker_id: Uuid,
        display_name: &str,
    ) -> anyhow::Result<RegisteredWorker> {
        self.register_team_worker_with_capabilities(
            team,
            worker_id,
            display_name,
            WorkerCapabilities::default(),
        )
        .await
    }

    pub async fn register_team_worker_with_capabilities(
        &self,
        team: &str,
        worker_id: Uuid,
        display_name: &str,
        capabilities: WorkerCapabilities,
    ) -> anyhow::Result<RegisteredWorker> {
        let request = self
            .request(Method::POST, &format!("/api/v1/teams/{team}/workers"))
            .json(&RegisterTeamWorkerRequest {
                worker_id,
                display_name,
                capabilities,
            });
        let registered: RegisteredTeamWorker = self.message(request).await?;
        let token = registered.token.ok_or_else(|| {
            anyhow::anyhow!("Gradient registered worker {worker_id} without a token")
        })?;
        Ok(RegisteredWorker {
            team: registered.team,
            token,
        })
    }

    pub async fn delete_team_worker(&self, team: &str, worker_id: Uuid) -> anyhow::Result<()> {
        let request = self.request(
            Method::DELETE,
            &format!("/api/v1/teams/{team}/workers/{worker_id}"),
        );
        send(&self.http, request).await?;
        Ok(())
    }

    pub async fn team_webhooks(&self, team: &str) -> anyhow::Result<Vec<TeamWebhook>> {
        self.message(self.request(Method::GET, &format!("/api/v1/teams/{team}/webhooks")))
            .await
    }

    pub async fn create_team_webhook(
        &self,
        team: &str,
        name: &str,
        url: &str,
        events: &[&str],
    ) -> anyhow::Result<String> {
        let request = self
            .request(Method::POST, &format!("/api/v1/teams/{team}/webhooks"))
            .json(&json!({ "name": name, "url": url, "events": events, "active": true }));
        let created: CreatedWebhook = self.message(request).await?;
        Ok(created.secret)
    }

    pub async fn rotate_team_webhook_secret(&self, team: &str, id: Uuid) -> anyhow::Result<String> {
        let request = self.request(
            Method::POST,
            &format!("/api/v1/teams/{team}/webhooks/{id}/rotate-secret"),
        );
        let rotated: RotatedSecret = self.message(request).await?;
        Ok(rotated.secret)
    }
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

    #[test]
    fn demand_excludes_roles_this_connection_cannot_supply() {
        let demand = TeamDemand {
            network: 1,
            secrets: 2,
            eval: 3,
            builds: vec![UnmetRequirement {
                architecture: "x86_64-linux".into(),
                required_features: vec![],
                build_count: 4,
            }],
        };
        assert_eq!(
            demand.clone().restricted_to(WorkerCapabilities::default()),
            demand
        );
        let build_only = demand.clone().restricted_to(WorkerCapabilities {
            secrets: false,
            eval: false,
            ..Default::default()
        });
        assert_eq!(build_only.builds, demand.builds);
        assert_eq!(
            (build_only.network, build_only.secrets, build_only.eval),
            (1, 0, 0)
        );
        let without_network = demand.clone().restricted_to(WorkerCapabilities {
            network: false,
            ..Default::default()
        });
        assert_eq!(without_network.builds, demand.builds);
        assert_eq!(
            (
                without_network.network,
                without_network.secrets,
                without_network.eval
            ),
            (0, 0, 3)
        );
        assert_eq!(
            demand.restricted_to(WorkerCapabilities {
                build: false,
                network: false,
                secrets: false,
                exec: false,
                eval: false,
            }),
            TeamDemand::default()
        );
    }
}
