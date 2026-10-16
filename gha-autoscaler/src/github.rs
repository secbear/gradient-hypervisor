use crate::rest::{send, status_of};
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::json;
use url::Url;
use uuid::Uuid;

const API_VERSION: &str = "2026-03-10";

#[derive(Clone)]
pub struct GitHubClient {
    http: Client,
    api: Url,
    token: String,
    repository: String,
}

#[derive(Debug, Clone)]
pub struct DispatchInputs {
    pub runs_on: String,
    pub autoscaler_url: String,
    pub connection: String,
    pub launch_id: Uuid,
    pub worker_flake: String,
    pub idle_timeout: u64,
    pub max_lifetime: u64,
    pub substituters: String,
    pub trusted_public_keys: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub id: u64,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hook {
    pub id: u64,
    pub config: HookConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepositoryInfo {
    pub default_branch: String,
    pub public: bool,
    pub owner_is_organization: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostedRunner {
    pub name: String,
    pub platform: String,
    pub machine_size_details: MachineSize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineSize {
    pub cpu_cores: u32,
    #[serde(default)]
    pub memory_gb: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelfHostedRunner {
    pub name: String,
    pub labels: Vec<RunnerLabel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunnerLabel {
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Deserialize)]
struct DispatchResponse {
    workflow_run_id: u64,
}

#[derive(Deserialize)]
struct FileResponse {
    content: String,
    sha: String,
}

#[derive(Deserialize)]
struct RepositoryResponse {
    default_branch: String,
    private: bool,
    owner: Owner,
}

#[derive(Deserialize)]
struct Owner {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct HostedRunners {
    runners: Vec<HostedRunner>,
}

#[derive(Deserialize)]
struct SelfHostedRunners {
    runners: Vec<SelfHostedRunner>,
}

impl DispatchInputs {
    pub(crate) fn as_json(&self) -> serde_json::Value {
        json!({
            "runs_on": self.runs_on,
            "autoscaler_url": self.autoscaler_url,
            "connection": self.connection,
            "launch_id": self.launch_id.to_string(),
            "worker_flake": self.worker_flake,
            "idle_timeout": self.idle_timeout.to_string(),
            "max_lifetime": self.max_lifetime.to_string(),
            "substituters": self.substituters,
            "trusted_public_keys": self.trusted_public_keys,
        })
    }
}

impl GitHubClient {
    pub fn new(token: String, repository: String) -> GitHubClient {
        let api = Url::parse("https://api.github.com").expect("static URL");
        Self::with_api_url(api, token, repository)
    }

    pub fn with_api_url(api: Url, token: String, repository: String) -> GitHubClient {
        GitHubClient {
            http: crate::rest::http_client(),
            api,
            token,
            repository,
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let mut url = self.api.clone();
        url.set_path(path);
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "gradient-gha-autoscaler")
    }

    fn repo_path(&self, rest: &str) -> String {
        format!("/repos/{}{rest}", self.repository)
    }

    pub async fn dispatch(
        &self,
        workflow: &str,
        git_ref: &str,
        inputs: &DispatchInputs,
    ) -> anyhow::Result<u64> {
        let path = self.repo_path(&format!("/actions/workflows/{workflow}/dispatches"));
        let request = self.request(Method::POST, &path).json(&json!({
            "ref": git_ref,
            "inputs": inputs.as_json(),
            "return_run_details": true,
        }));
        let response = send(&self.http, request).await?;
        if response.status() != StatusCode::OK {
            bail!(
                "POST {path} answered {} without a run ID; API version {API_VERSION} is expected to answer 200",
                response.status()
            );
        }
        let dispatched: DispatchResponse = response.json().await?;
        Ok(dispatched.workflow_run_id)
    }

    pub async fn cancel_run(&self, run_id: u64) -> anyhow::Result<()> {
        let path = self.repo_path(&format!("/actions/runs/{run_id}/cancel"));
        send(&self.http, self.request(Method::POST, &path)).await?;
        Ok(())
    }

    pub async fn run(&self, run_id: u64) -> anyhow::Result<Run> {
        let path = self.repo_path(&format!("/actions/runs/{run_id}"));
        Ok(send(&self.http, self.request(Method::GET, &path))
            .await?
            .json()
            .await?)
    }

    pub async fn hooks(&self) -> anyhow::Result<Vec<Hook>> {
        let request = self
            .request(Method::GET, &self.repo_path("/hooks"))
            .query(&[("per_page", "100")]);
        Ok(send(&self.http, request).await?.json().await?)
    }

    pub async fn create_hook(
        &self,
        url: &str,
        secret: &str,
        events: &[&str],
    ) -> anyhow::Result<()> {
        let request = self
            .request(Method::POST, &self.repo_path("/hooks"))
            .json(&hook_body(url, secret, events));
        send(&self.http, request).await?;
        Ok(())
    }

    pub async fn update_hook(
        &self,
        id: u64,
        url: &str,
        secret: &str,
        events: &[&str],
    ) -> anyhow::Result<()> {
        let request = self
            .request(Method::PATCH, &self.repo_path(&format!("/hooks/{id}")))
            .json(&hook_body(url, secret, events));
        send(&self.http, request).await?;
        Ok(())
    }

    pub async fn file(&self, path: &str) -> anyhow::Result<Option<(String, String)>> {
        let request = self.request(Method::GET, &self.repo_path(&format!("/contents/{path}")));
        let response = match send(&self.http, request).await {
            Ok(response) => response,
            Err(error) if status_of(&error) == Some(StatusCode::NOT_FOUND) => return Ok(None),
            Err(error) => return Err(error),
        };
        let file: FileResponse = response.json().await?;
        let encoded: String = file.content.split_whitespace().collect();
        let content = String::from_utf8(STANDARD.decode(encoded)?)?;
        Ok(Some((content, file.sha)))
    }

    pub async fn put_file(
        &self,
        path: &str,
        content: &str,
        sha: Option<&str>,
        message: &str,
    ) -> anyhow::Result<()> {
        let mut body = json!({ "message": message, "content": STANDARD.encode(content) });
        if let Some(sha) = sha {
            body["sha"] = json!(sha);
        }
        let request = self
            .request(Method::PUT, &self.repo_path(&format!("/contents/{path}")))
            .json(&body);
        send(&self.http, request).await?;
        Ok(())
    }

    pub async fn repository_info(&self) -> anyhow::Result<RepositoryInfo> {
        let response: RepositoryResponse =
            send(&self.http, self.request(Method::GET, &self.repo_path("")))
                .await?
                .json()
                .await?;
        Ok(RepositoryInfo {
            default_branch: response.default_branch,
            public: !response.private,
            owner_is_organization: response.owner.kind == "Organization",
        })
    }

    pub async fn hosted_runners(&self, org: &str) -> anyhow::Result<Vec<HostedRunner>> {
        let request = self
            .request(Method::GET, &format!("/orgs/{org}/actions/hosted-runners"))
            .query(&[("per_page", "100")]);
        let runners: HostedRunners = send(&self.http, request).await?.json().await?;
        Ok(runners.runners)
    }

    pub async fn self_hosted_runners(&self) -> anyhow::Result<Vec<SelfHostedRunner>> {
        let request = self
            .request(Method::GET, &self.repo_path("/actions/runners"))
            .query(&[("per_page", "100")]);
        let runners: SelfHostedRunners = send(&self.http, request).await?.json().await?;
        Ok(runners.runners)
    }
}

fn hook_body(url: &str, secret: &str, events: &[&str]) -> serde_json::Value {
    json!({
        "name": "web",
        "active": true,
        "events": events,
        "config": { "url": url, "content_type": "json", "secret": secret, "insecure_ssl": "0" },
    })
}
