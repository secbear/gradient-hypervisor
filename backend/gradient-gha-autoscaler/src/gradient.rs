use crate::rest::send;
use reqwest::{Client, Method};
use serde::Deserialize;
use serde::de::DeserializeOwned;
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
        let request = self
            .request(Method::POST, &format!("/api/v1/teams/{team}/workers"))
            .json(&json!({
                "worker_id": worker_id.to_string(),
                "display_name": display_name,
                "enable_network": true,
                "enable_secrets": true,
                "enable_eval": true,
                "enable_build": true,
            }));
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
