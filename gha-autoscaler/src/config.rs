use anyhow::{Context, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_url: Url,
    pub state_dir: PathBuf,
    #[serde(default = "default_catch_up_interval")]
    pub catch_up_interval: u64,
    #[serde(default)]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Connection {
    pub name: String,
    pub gradient_url: Url,
    pub team: String,
    pub api_key_file: PathBuf,
    pub github_token_file: PathBuf,
    pub repository: String,
    #[serde(default = "default_allowed_kinds")]
    pub allowed_kinds: Vec<String>,
    #[serde(default)]
    pub projects: Option<Vec<String>>,
    #[serde(default = "default_max_launches")]
    pub max_launches: u32,
    #[serde(default = "default_max_launches_per_kind")]
    pub max_launches_per_kind: u32,
    #[serde(default)]
    pub worker_flake: Option<String>,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    #[serde(default = "default_max_lifetime")]
    pub max_lifetime: u64,
}

fn default_catch_up_interval() -> u64 {
    300
}

fn default_allowed_kinds() -> Vec<String> {
    vec!["ubuntu-latest".into(), "ubuntu-24.04-arm".into()]
}

fn default_max_launches() -> u32 {
    4
}

fn default_max_launches_per_kind() -> u32 {
    2
}

fn default_idle_timeout() -> u64 {
    120
}

fn default_max_lifetime() -> u64 {
    21600
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_str(raw).context("parsing config")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let mut names = HashSet::new();
        for connection in &self.connections {
            connection.validate()?;
            if !names.insert(connection.name.as_str()) {
                bail!("connection name {:?} is used twice", connection.name);
            }
        }
        Ok(())
    }
}

impl Connection {
    fn validate(&self) -> anyhow::Result<()> {
        let valid_name = !self.name.is_empty()
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid_name {
            bail!(
                "connection name {:?} may only hold a-z, 0-9 and -",
                self.name
            );
        }
        if self
            .repository
            .split('/')
            .filter(|part| !part.is_empty())
            .count()
            != 2
        {
            bail!(
                "connection {}: repository {:?} is not owner/name",
                self.name,
                self.repository
            );
        }
        Ok(())
    }

    pub fn owner(&self) -> &str {
        self.repository.split('/').next().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_connections(connections: &str) -> String {
        format!(
            r#"{{"listen":"127.0.0.1:8080","publicUrl":"https://scaler.example.com","stateDir":"/tmp/x","connections":[{connections}]}}"#
        )
    }

    fn connection(name: &str) -> String {
        format!(
            r#"{{"name":"{name}","gradientUrl":"https://gradient.example.com","team":"ops","apiKeyFile":"/k","githubTokenFile":"/g","repository":"acme/runners"}}"#
        )
    }

    #[test]
    fn config_rejects_duplicate_connection_names() {
        let raw = with_connections(&format!("{},{}", connection("a"), connection("a")));
        let error = Config::parse(&raw).unwrap_err().to_string();
        assert!(error.contains("used twice"), "{error}");
    }

    #[test]
    fn config_rejects_names_outside_the_url_safe_set() {
        assert!(Config::parse(&with_connections(&connection("Team_A"))).is_err());
    }

    #[test]
    fn config_rejects_repository_without_owner() {
        let raw = with_connections(&connection("a")).replace("acme/runners", "runners");
        assert!(Config::parse(&raw).is_err());
    }
}
