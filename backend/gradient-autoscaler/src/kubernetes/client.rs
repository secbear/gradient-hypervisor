use crate::config::Kubernetes;
use crate::rest::{http_client, http_client_trusting, send, status_of};
use anyhow::Context;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use url::Url;

const IN_CLUSTER_CA: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";

#[derive(Clone)]
pub struct KubernetesClient {
    http: Client,
    api: Url,
    token_file: PathBuf,
    namespace: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pod {
    pub metadata: ObjectMeta,
    #[serde(default)]
    pub status: Option<PodStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObjectMeta {
    pub name: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerStatus {
    #[serde(default)]
    pub state: Option<ContainerState>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerState {
    #[serde(default)]
    pub waiting: Option<Waiting>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Waiting {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize)]
struct PodList {
    items: Vec<Pod>,
}

impl KubernetesClient {
    pub fn new(config: &Kubernetes) -> anyhow::Result<KubernetesClient> {
        let (api, ca_file) = match &config.api_server {
            Some(api) => (api.clone(), config.ca_file.clone()),
            None => (
                in_cluster_api()?,
                Some(
                    config
                        .ca_file
                        .clone()
                        .unwrap_or_else(|| PathBuf::from(IN_CLUSTER_CA)),
                ),
            ),
        };
        let http = match ca_file {
            Some(path) => http_client_trusting(
                &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            )?,
            None => http_client(),
        };
        Ok(KubernetesClient {
            http,
            api,
            token_file: config.token_file.clone(),
            namespace: config.namespace.clone(),
        })
    }

    pub fn with_api_url(api: Url, token_file: PathBuf, namespace: &str) -> KubernetesClient {
        KubernetesClient {
            http: http_client(),
            api,
            token_file,
            namespace: namespace.to_owned(),
        }
    }

    fn request(&self, method: Method, resource: &str) -> anyhow::Result<RequestBuilder> {
        let token = std::fs::read_to_string(&self.token_file)
            .with_context(|| format!("reading {}", self.token_file.display()))?;
        let mut url = self.api.clone();
        let base = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!(
            "{base}/api/v1/namespaces/{}/{resource}",
            self.namespace
        ));
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token.trim())
            .header("Accept", "application/json"))
    }

    pub async fn create_pod(&self, pod: &Value) -> anyhow::Result<Pod> {
        let request = self.request(Method::POST, "pods")?.json(pod);
        Ok(send(&self.http, request).await?.json().await?)
    }

    pub async fn create_secret(&self, secret: &Value) -> anyhow::Result<()> {
        let request = self.request(Method::POST, "secrets")?.json(secret);
        send(&self.http, request).await?;
        Ok(())
    }

    pub async fn pod(&self, name: &str) -> anyhow::Result<Option<Pod>> {
        let request = self.request(Method::GET, &format!("pods/{name}"))?;
        match send(&self.http, request).await {
            Ok(response) => Ok(Some(response.json().await?)),
            Err(error) if status_of(&error) == Some(StatusCode::NOT_FOUND) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn pods(&self, label_selector: &str) -> anyhow::Result<Vec<Pod>> {
        let request = self
            .request(Method::GET, "pods")?
            .query(&[("labelSelector", label_selector)]);
        let list: PodList = send(&self.http, request).await?.json().await?;
        Ok(list.items)
    }

    pub async fn delete_pod(&self, name: &str) -> anyhow::Result<()> {
        let request = self.request(Method::DELETE, &format!("pods/{name}"))?;
        match send(&self.http, request).await {
            Err(error) if status_of(&error) != Some(StatusCode::NOT_FOUND) => Err(error),
            _ => Ok(()),
        }
    }
}

fn in_cluster_api() -> anyhow::Result<Url> {
    let host = std::env::var("KUBERNETES_SERVICE_HOST")
        .context("kubernetes.apiServer is unset and KUBERNETES_SERVICE_HOST is missing")?;
    let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_owned());
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Ok(Url::parse(&format!("https://{host}:{port}"))?)
}
