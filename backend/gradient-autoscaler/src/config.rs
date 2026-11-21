use crate::build_planner::BuildScalingPolicy;
use crate::capacity::{BuildCapacityProfile, BuilderResources, CapacityCapabilities, MAX_PROFILES};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use time::Duration;
use url::Url;
use uuid::Uuid;

pub mod pod_options;
use pod_options::PodOptions;

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
#[serde(try_from = "ConnectionFields")]
pub struct Connection {
    pub name: String,
    pub gradient_url: Url,
    pub team: String,
    pub api_key_file: PathBuf,
    pub projects: Option<Vec<String>>,
    pub max_launches: u32,
    pub max_launches_per_kind: u32,
    pub worker_substituters: Vec<String>,
    pub worker_trusted_public_keys: Vec<String>,
    pub worker_capabilities: WorkerCapabilities,
    pub idle_timeout: u64,
    pub max_lifetime: u64,
    pub launcher: LauncherConfig,
    pub build_scaling: Option<BuildScalingConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildScalingMode {
    Preview,
    JointPreview,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildScalingConfig {
    pub mode: BuildScalingMode,
    pub profiles: Vec<String>,
    #[serde(default)]
    pub bootstrap_profiles: Vec<BootstrapProfileConfig>,
    #[serde(default = "default_queue_wait")]
    pub queue_wait: u32,
    #[serde(default = "default_startup_grace")]
    pub startup_grace: u32,
    #[serde(default = "default_max_new_workers")]
    pub max_new_workers: u32,
    pub fallback_ram_mb: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapProfileConfig {
    pub pool: String,
    pub capabilities: WorkerCapabilities,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildResources {
    pub cpu_count: u32,
    pub ram_total_mb: u64,
    pub cpu_core_score: u32,
    #[serde(default)]
    pub build_cores: Option<u32>,
}

impl BuildResources {
    pub(crate) fn for_pool(&self, pool: &Pool) -> BuilderResources {
        BuilderResources {
            architectures: vec![pool.system.clone()],
            system_features: pool.features.clone(),
            max_concurrent_builds: pool.max_jobs,
            cpu_count: self.cpu_count,
            ram_total_mb: self.ram_total_mb,
            cpu_core_score: self.cpu_core_score,
            build_cores: self.build_cores,
        }
    }
}

impl BuildScalingConfig {
    pub fn policy(&self) -> BuildScalingPolicy {
        BuildScalingPolicy {
            queue_wait: Duration::seconds(i64::from(self.queue_wait)),
            startup_grace: Duration::seconds(i64::from(self.startup_grace)),
            max_new_workers: self.max_new_workers,
            fallback_ram_mb: self.fallback_ram_mb,
        }
    }
}

impl Connection {
    pub fn is_preview(&self) -> bool {
        self.build_scaling.as_ref().is_some_and(|scaling| {
            matches!(
                scaling.mode,
                BuildScalingMode::Preview | BuildScalingMode::JointPreview
            )
        })
    }

    pub fn is_joint_preview(&self) -> bool {
        self.build_scaling
            .as_ref()
            .is_some_and(|scaling| scaling.mode == BuildScalingMode::JointPreview)
    }

    pub fn capacity_projects(&self) -> anyhow::Result<Option<Vec<Uuid>>> {
        self.projects
            .as_ref()
            .map(|projects| {
                projects
                    .iter()
                    .map(|project| {
                        Uuid::parse_str(project).with_context(|| {
                            format!(
                                "connection {}: build scaling preview needs project IDs, not names",
                                self.name
                            )
                        })
                    })
                    .collect()
            })
            .transpose()
    }
}

#[derive(Debug, Clone)]
pub enum LauncherConfig {
    GitHubActions(GitHubActions),
    Kubernetes(Kubernetes),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerCapabilities {
    #[serde(rename(serialize = "enable_build"))]
    pub build: bool,
    #[serde(rename(serialize = "enable_network"))]
    pub network: bool,
    #[serde(rename(serialize = "enable_secrets"))]
    pub secrets: bool,
    #[serde(rename(serialize = "enable_exec"))]
    pub exec: bool,
    #[serde(rename(serialize = "enable_eval"))]
    pub eval: bool,
}

impl Default for WorkerCapabilities {
    fn default() -> Self {
        Self {
            build: true,
            network: true,
            secrets: true,
            exec: false,
            eval: true,
        }
    }
}

impl WorkerCapabilities {
    pub fn permits(self, requested: Self) -> bool {
        [
            (self.build, requested.build),
            (self.network, requested.network),
            (self.secrets, requested.secrets),
            (self.exec, requested.exec),
            (self.eval, requested.eval),
        ]
        .into_iter()
        .all(|(enabled, required)| enabled || !required)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubActions {
    pub token_file: PathBuf,
    pub repository: String,
    #[serde(default = "default_allowed_kinds")]
    pub allowed_kinds: Vec<String>,
    #[serde(default)]
    pub worker_flake: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Kubernetes {
    #[serde(default)]
    pub api_server: Option<Url>,
    #[serde(default = "default_service_account_token")]
    pub token_file: PathBuf,
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    pub namespace: String,
    pub image: String,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    pub pools: BTreeMap<String, Pool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pool {
    pub system: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default = "default_pool_max_jobs")]
    pub max_jobs: u32,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default)]
    pub tolerations: Vec<serde_json::Value>,
    #[serde(default)]
    pub privileged: bool,
    #[serde(default)]
    pub pod_options: PodOptions,
    #[serde(default)]
    pub build_resources: Option<BuildResources>,
}

impl Pool {
    pub(crate) fn validate_capacity_environment(&self) -> anyhow::Result<()> {
        self.pod_options.validate_capacity_environment()?;
        if self.system.is_empty() || self.system.contains(',') {
            bail!("system cannot be encoded as one worker architecture");
        }
        for feature in &self.features {
            if feature.is_empty()
                || feature
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, ',' | '#'))
            {
                bail!("feature {feature:?} cannot be encoded as one worker and Nix system feature");
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionFields {
    name: String,
    gradient_url: Url,
    team: String,
    api_key_file: PathBuf,
    #[serde(default)]
    projects: Option<Vec<String>>,
    #[serde(default = "default_max_launches")]
    max_launches: u32,
    #[serde(default = "default_max_launches_per_kind")]
    max_launches_per_kind: u32,
    #[serde(default)]
    worker_substituters: Vec<String>,
    #[serde(default)]
    worker_trusted_public_keys: Vec<String>,
    #[serde(default)]
    worker_capabilities: WorkerCapabilities,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u64,
    #[serde(default = "default_max_lifetime")]
    max_lifetime: u64,
    #[serde(default)]
    github_actions: Option<GitHubActions>,
    #[serde(default)]
    kubernetes: Option<Kubernetes>,
    #[serde(default)]
    build_scaling: Option<BuildScalingConfig>,
}

fn default_queue_wait() -> u32 {
    60
}
fn default_startup_grace() -> u32 {
    120
}
fn default_max_new_workers() -> u32 {
    2
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
    18000
}

fn default_pool_max_jobs() -> u32 {
    4
}

fn default_service_account_token() -> PathBuf {
    PathBuf::from("/var/run/secrets/kubernetes.io/serviceaccount/token")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_str(raw).context("parsing config")?;
        let mut names = HashSet::new();
        for connection in &config.connections {
            if !names.insert(connection.name.as_str()) {
                bail!("connection name {:?} is used twice", connection.name);
            }
        }
        for connection in config
            .connections
            .iter()
            .filter(|conn| conn.is_joint_preview())
        {
            if config.connections.iter().any(|other| {
                other.name != connection.name
                    && other.gradient_url.origin() == connection.gradient_url.origin()
            }) {
                bail!(
                    "connection {}: joint preview cannot share Gradient URL ownership",
                    connection.name
                );
            }
        }

        Ok(config)
    }
}

impl TryFrom<ConnectionFields> for Connection {
    type Error = anyhow::Error;

    fn try_from(fields: ConnectionFields) -> anyhow::Result<Connection> {
        let name = fields.name;
        if !is_lowercase_name(&name) {
            bail!("connection name {name:?} may only hold a-z, 0-9 and -");
        }

        let launcher = match (fields.github_actions, fields.kubernetes) {
            (Some(github), None) => LauncherConfig::GitHubActions(github.validated(&name)?),
            (None, Some(kubernetes)) => LauncherConfig::Kubernetes(kubernetes.validated(&name)?),
            _ => bail!("connection {name}: set exactly one of githubActions and kubernetes"),
        };
        let connection = Connection {
            name,
            gradient_url: fields.gradient_url,
            team: fields.team,
            api_key_file: fields.api_key_file,
            projects: fields.projects,
            max_launches: fields.max_launches,
            max_launches_per_kind: fields.max_launches_per_kind,
            worker_substituters: fields.worker_substituters,
            worker_trusted_public_keys: fields.worker_trusted_public_keys,
            worker_capabilities: fields.worker_capabilities,
            idle_timeout: fields.idle_timeout,
            max_lifetime: fields.max_lifetime,
            launcher,
            build_scaling: fields.build_scaling,
        };
        if let Some(scaling) = &connection.build_scaling {
            let LauncherConfig::Kubernetes(kubernetes) = &connection.launcher else {
                bail!(
                    "connection {}: build scaling preview requires Kubernetes",
                    connection.name
                );
            };
            if (!scaling.profiles.is_empty() && !connection.worker_capabilities.build)
                || scaling.startup_grace == 0
                || scaling.fallback_ram_mb == 0
            {
                bail!(
                    "connection {}: preview needs build permission, positive startupGrace and fallbackRamMb",
                    connection.name
                );
            }
            if scaling.mode != BuildScalingMode::JointPreview
                && !scaling.bootstrap_profiles.is_empty()
            {
                bail!(
                    "connection {}: bootstrapProfiles requires joint_preview",
                    connection.name
                );
            }
            let alternatives = scaling
                .profiles
                .len()
                .checked_add(scaling.bootstrap_profiles.len())
                .and_then(|count| count.checked_mul(scaling.max_new_workers.max(1) as usize));
            if alternatives.is_none_or(|count| count == 0 || count > MAX_PROFILES) {
                bail!(
                    "connection {}: preview accepts at most {MAX_PROFILES} profile alternatives",
                    connection.name
                );
            }
            let mut selected = HashSet::new();
            for label in &scaling.profiles {
                if !selected.insert(label) {
                    bail!(
                        "connection {}: duplicate preview profile {label}",
                        connection.name
                    );
                }
                let pool = kubernetes
                    .pools
                    .get(label)
                    .with_context(|| format!("preview pool {label} is not configured"))?;
                if pool.build_resources.is_none() {
                    bail!(
                        "connection {}: pool {label} needs explicit buildResources for preview",
                        connection.name
                    );
                }
                pool.validate_capacity_environment().map_err(|error| {
                    anyhow::anyhow!("connection {}: pool {label}: {error:#}", connection.name)
                })?;
            }
            for profile in &scaling.bootstrap_profiles {
                let label = &profile.pool;
                let capabilities = profile.capabilities;
                if capabilities.build || !(capabilities.eval || capabilities.network) {
                    bail!(
                        "connection {}: bootstrap profile {label} must be role-only with eval or network",
                        connection.name
                    );
                }
                if !connection.worker_capabilities.permits(capabilities) {
                    bail!(
                        "connection {}: bootstrap profile {label} exceeds connection permissions",
                        connection.name
                    );
                }
                let pool = kubernetes
                    .pools
                    .get(label)
                    .with_context(|| format!("bootstrap pool {label} is not configured"))?;
                pool.validate_capacity_environment().map_err(|error| {
                    anyhow::anyhow!("connection {}: pool {label}: {error:#}", connection.name)
                })?;
            }
            connection.capacity_projects()?;
        }
        Ok(connection)
    }
}

impl GitHubActions {
    fn validated(self, connection: &str) -> anyhow::Result<GitHubActions> {
        let parts = self.repository.split('/').filter(|part| !part.is_empty());
        if parts.count() != 2 {
            bail!(
                "connection {connection}: repository {:?} is not owner/name",
                self.repository
            );
        }

        Ok(self)
    }

    pub fn owner(&self) -> &str {
        self.repository.split('/').next().unwrap_or_default()
    }
}

impl Kubernetes {
    fn validated(self, connection: &str) -> anyhow::Result<Kubernetes> {
        if self.pools.is_empty() {
            bail!("connection {connection}: kubernetes needs at least one pool");
        }

        if let Some(label) = self.pools.keys().find(|label| !is_lowercase_name(label)) {
            bail!("connection {connection}: pool name {label:?} may only hold a-z, 0-9 and -");
        }

        if let Some((label, _)) = self.pools.iter().find(|(_, pool)| pool.max_jobs == 0) {
            bail!("connection {connection}: pool {label}: maxJobs must be greater than zero");
        }

        for (label, pool) in &self.pools {
            pool.pod_options.validate().map_err(|error| {
                anyhow::anyhow!("connection {connection}: pool {label}: podOptions: {error:#}")
            })?;
            if let Some(resources) = &pool.build_resources {
                BuildCapacityProfile {
                    name: label.clone(),
                    worker_id: Uuid::nil(),
                    capabilities: CapacityCapabilities {
                        build: true,
                        ..Default::default()
                    },
                    resources: resources.for_pool(pool),
                }
                .validate()
                .map_err(|error| {
                    anyhow::anyhow!(
                        "connection {connection}: pool {label}: buildResources: {error:#}"
                    )
                })?;
            }
        }

        Ok(self)
    }
}

fn is_lowercase_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = r#""githubActions":{"tokenFile":"/g","repository":"acme/runners"}"#;
    const KUBERNETES: &str = r#""kubernetes":{"namespace":"ci","image":"worker:1","pools":{"x86":{"system":"x86_64-linux"}}}"#;

    fn with_connections(connections: &str) -> String {
        format!(
            r#"{{"listen":"127.0.0.1:8080","publicUrl":"https://scaler.example.com","stateDir":"/tmp/x","connections":[{connections}]}}"#
        )
    }

    fn connection(name: &str, launcher: &str) -> String {
        format!(
            r#"{{"name":"{name}","gradientUrl":"https://gradient.example.com","team":"ops","apiKeyFile":"/k",{launcher}}}"#
        )
    }

    fn error_of(connections: &str) -> String {
        format!(
            "{:#}",
            Config::parse(&with_connections(connections)).unwrap_err()
        )
    }

    #[test]
    fn config_rejects_duplicate_connection_names() {
        let both = format!(
            "{},{}",
            connection("a", GITHUB),
            connection("a", KUBERNETES)
        );
        assert!(error_of(&both).contains("used twice"));
    }

    #[test]
    fn config_rejects_names_outside_the_url_safe_set() {
        assert!(error_of(&connection("Team_A", GITHUB)).contains("may only hold"));
    }

    #[test]
    fn config_rejects_repository_without_owner() {
        let launcher = GITHUB.replace("acme/runners", "runners");
        assert!(error_of(&connection("a", &launcher)).contains("owner/name"));
    }

    #[test]
    fn config_needs_exactly_one_launcher() {
        let both = format!("{GITHUB},{KUBERNETES}");
        for launcher in [both.as_str(), r#""projects":null"#] {
            assert!(error_of(&connection("a", launcher)).contains("exactly one"));
        }
    }

    #[test]
    fn kubernetes_rejects_invalid_pool_configuration() {
        let pools = r#"{"x86":{"system":"x86_64-linux"}}"#;
        for (launcher, message) in [
            (KUBERNETES.replace(pools, "{}"), "at least one pool"),
            (KUBERNETES.replace("\"x86\"", "\"X86\""), "pool name"),
            (
                KUBERNETES.replace(
                    "\"system\":\"x86_64-linux\"",
                    "\"system\":\"x86_64-linux\",\"podOptions\":{\"env\":[{\"name\":\"GRADIENT_WORKER_ID\",\"value\":\"override\"}]}",
                ),
                "controlled by the autoscaler",
            ),
        ] {
            assert!(error_of(&connection("a", &launcher)).contains(message));
        }
    }

    #[test]
    fn partial_worker_capabilities_preserve_unspecified_defaults() {
        for (capabilities, exec) in [
            (r#"{"eval":false,"secrets":false}"#, false),
            (r#"{"eval":false,"secrets":false,"exec":true}"#, true),
        ] {
            let entry = connection("a", KUBERNETES).replace(
                "\"name\":\"a\"",
                &format!("\"name\":\"a\",\"workerCapabilities\":{capabilities}"),
            );
            let config = Config::parse(&with_connections(&entry)).unwrap();
            assert_eq!(
                config.connections[0].worker_capabilities,
                WorkerCapabilities {
                    eval: false,
                    secrets: false,
                    exec,
                    ..Default::default()
                }
            );
        }
    }

    #[test]
    fn omitted_and_empty_worker_capabilities_preserve_defaults() {
        for launcher in [GITHUB, KUBERNETES] {
            let entry = connection("a", launcher);
            for entry in [
                entry.clone(),
                entry.replace("\"name\":\"a\"", "\"name\":\"a\",\"workerCapabilities\":{}"),
            ] {
                let config = Config::parse(&with_connections(&entry)).unwrap();
                assert_eq!(
                    config.connections[0].worker_capabilities,
                    WorkerCapabilities {
                        build: true,
                        network: true,
                        secrets: true,
                        exec: false,
                        eval: true,
                    }
                );
            }
        }
    }

    #[test]
    fn worker_capabilities_reject_unknown_roles_and_non_booleans() {
        for capabilities in [
            r#"{"secret":false}"#,
            r#"{"eval":"false"}"#,
            r#"{"exec":"false"}"#,
            r#"{"enable_build":false}"#,
        ] {
            let entry = connection("a", KUBERNETES).replace(
                "\"name\":\"a\"",
                &format!("\"name\":\"a\",\"workerCapabilities\":{capabilities}"),
            );
            assert!(Config::parse(&with_connections(&entry)).is_err());
        }
    }

    #[test]
    fn kubernetes_rejects_zero_build_slots() {
        let zero = KUBERNETES.replace(
            r#""system":"x86_64-linux""#,
            r#""system":"x86_64-linux","maxJobs":0"#,
        );
        assert!(error_of(&connection("a", &zero)).contains("maxJobs must be greater than zero"));
    }

    fn preview_connection() -> serde_json::Value {
        let mut entry: serde_json::Value =
            serde_json::from_str(&connection("a", KUBERNETES)).unwrap();
        entry["buildScaling"] = serde_json::json!({
            "mode": "preview", "profiles": ["x86"], "fallbackRamMb": 2048
        });
        entry["kubernetes"]["pools"]["x86"]["buildResources"] = serde_json::json!({
            "cpuCount": 4, "ramTotalMb": 8192, "cpuCoreScore": 4000
        });
        entry
    }

    #[test]
    fn build_preview_is_opt_in_and_preserves_policy_defaults() {
        for launcher in [GITHUB, KUBERNETES] {
            let config = Config::parse(&with_connections(&connection("a", launcher))).unwrap();
            assert!(config.connections[0].build_scaling.is_none());
        }
        let mut entry = preview_connection();
        entry["kubernetes"]["pools"]["x86"]["cpu"] = "3500m".into();
        entry["kubernetes"]["pools"]["x86"]["memory"] = "8Gi".into();
        let config = Config::parse(&with_connections(&entry.to_string())).unwrap();
        let conn = &config.connections[0];
        assert!(conn.is_preview());
        let policy = conn.build_scaling.as_ref().unwrap().policy();
        assert_eq!(policy.queue_wait, Duration::seconds(60));
        assert_eq!(policy.startup_grace, Duration::seconds(120));
        assert_eq!(policy.max_new_workers, 2);
        assert_eq!(policy.fallback_ram_mb, 2048);
        let LauncherConfig::Kubernetes(kubernetes) = &conn.launcher else {
            panic!()
        };
        assert_eq!(kubernetes.pools["x86"].cpu.as_deref(), Some("3500m"));
        assert_eq!(kubernetes.pools["x86"].memory.as_deref(), Some("8Gi"));
    }

    #[test]
    fn build_preview_preserves_project_scope_and_legacy_names() {
        let id = "70000000-0000-0000-0000-000000000001";
        for (projects, expected) in [
            (serde_json::Value::Null, None),
            (serde_json::json!([]), Some(vec![])),
            (
                serde_json::json!([id]),
                Some(vec![Uuid::parse_str(id).unwrap()]),
            ),
        ] {
            let mut entry = preview_connection();
            entry["projects"] = projects;
            let config = Config::parse(&with_connections(&entry.to_string())).unwrap();
            assert_eq!(config.connections[0].capacity_projects().unwrap(), expected);
        }
        let mut entry = preview_connection();
        entry["projects"] = serde_json::json!(["website"]);
        assert!(error_of(&entry.to_string()).contains("project IDs"));
        entry["buildScaling"] = serde_json::Value::Null;
        assert!(Config::parse(&with_connections(&entry.to_string())).is_ok());
    }

    #[test]
    fn build_preview_rejects_invalid_settings() {
        for (pointer, value, message) in [
            (
                "/buildScaling/mode",
                serde_json::json!("active"),
                "unknown variant",
            ),
            (
                "/buildScaling/profiles",
                serde_json::json!([]),
                "profile alternatives",
            ),
            (
                "/buildScaling/profiles",
                serde_json::json!(["x86", "x86"]),
                "duplicate",
            ),
            (
                "/buildScaling/profiles",
                serde_json::json!(["missing"]),
                "not configured",
            ),
            (
                "/buildScaling/maxNewWorkers",
                serde_json::json!(17),
                "profile alternatives",
            ),
            (
                "/buildScaling/startupGrace",
                serde_json::json!(0),
                "positive startupGrace",
            ),
            (
                "/buildScaling/fallbackRamMb",
                serde_json::json!(0),
                "fallbackRamMb",
            ),
            (
                "/kubernetes/pools/x86/buildResources",
                serde_json::Value::Null,
                "explicit buildResources",
            ),
            (
                "/kubernetes/pools/x86/buildResources/cpuCount",
                serde_json::json!(0),
                "CPU capacity",
            ),
            (
                "/kubernetes/pools/x86/buildResources/ramTotalMb",
                serde_json::json!(0),
                "RAM budget",
            ),
        ] {
            let mut entry = preview_connection();
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            entry.pointer_mut(parent).unwrap()[field] = value;
            assert!(error_of(&entry.to_string()).contains(message), "{pointer}");
        }
        let mut entry = preview_connection();
        entry["workerCapabilities"] = serde_json::json!({"build": false});
        assert!(error_of(&entry.to_string()).contains("build permission"));
        entry = preview_connection();
        entry.as_object_mut().unwrap().remove("kubernetes");
        entry["githubActions"] =
            serde_json::json!({"tokenFile": "/g", "repository": "acme/runners"});
        assert!(error_of(&entry.to_string()).contains("requires Kubernetes"));
    }

    #[test]
    fn build_preview_allows_zero_growth_and_rejects_unknown_fields() {
        let mut entry = preview_connection();
        entry["buildScaling"]["maxNewWorkers"] = serde_json::json!(0);
        entry["buildScaling"]["queueWait"] = serde_json::json!(0);
        assert!(Config::parse(&with_connections(&entry.to_string())).is_ok());
        entry["buildScaling"]["startupTimeout"] = serde_json::json!(30);
        assert!(error_of(&entry.to_string()).contains("unknown field"));
        entry["buildScaling"]
            .as_object_mut()
            .unwrap()
            .remove("startupTimeout");
        entry["kubernetes"]["pools"]["x86"]["buildResources"]["memory"] = serde_json::json!("8Gi");
        assert!(error_of(&entry.to_string()).contains("unknown field"));
    }

    #[test]
    fn capacity_environment_rejects_lossy_feature_encoding_only_when_enabled() {
        for feature in [
            "",
            "key one",
            "key,two",
            "key#comment",
            "key\nsetting = value",
        ] {
            let mut entry = preview_connection();
            entry["kubernetes"]["pools"]["x86"]["features"] = serde_json::json!([feature]);
            assert!(error_of(&entry.to_string()).contains("cannot be encoded"));
            entry["buildScaling"] = serde_json::Value::Null;
            assert!(Config::parse(&with_connections(&entry.to_string())).is_ok());
        }
        for system in ["", "x86_64-linux,aarch64-linux"] {
            let mut entry = preview_connection();
            entry["kubernetes"]["pools"]["x86"]["system"] = serde_json::json!(system);
            assert!(error_of(&entry.to_string()).contains("one worker architecture"));
        }
    }

    #[test]
    fn capacity_environment_controls_native_settings_without_restricting_legacy_extensions() {
        for name in pod_options::CAPACITY_ENV {
            let mut entry = preview_connection();
            entry["kubernetes"]["pools"]["x86"]["podOptions"] = serde_json::json!({
                "env": [{"name": name, "value": "override"}]
            });
            assert!(error_of(&entry.to_string()).contains("controlled by the assessed profile"));
            entry["buildScaling"] = serde_json::Value::Null;
            assert!(Config::parse(&with_connections(&entry.to_string())).is_ok());
        }
    }

    fn joint_connection() -> serde_json::Value {
        let mut entry = preview_connection();
        entry["buildScaling"]["mode"] = serde_json::json!("joint_preview");
        entry["buildScaling"]["bootstrapProfiles"] = serde_json::json!([{
            "pool": "role",
            "capabilities": { "build": false, "network": true, "eval": true, "secrets": false, "exec": false }
        }]);
        entry["kubernetes"]["pools"]["role"] = serde_json::json!({
            "system": "aarch64-linux", "features": ["kvm"], "maxJobs": 1
        });
        entry
    }

    #[test]
    fn joint_preview_is_read_only_and_role_profiles_need_no_build_resources() {
        let entry = joint_connection();
        let config = Config::parse(&with_connections(&entry.to_string())).unwrap();
        let conn = &config.connections[0];
        assert!(conn.is_preview());
        assert!(conn.is_joint_preview());
        let scaling = conn.build_scaling.as_ref().unwrap();
        assert_eq!(scaling.bootstrap_profiles[0].pool, "role");
        assert!(!scaling.bootstrap_profiles[0].capabilities.build);
        let legacy = Config::parse(&with_connections(&preview_connection().to_string())).unwrap();
        assert!(!legacy.connections[0].is_joint_preview());
        assert!(
            legacy.connections[0]
                .build_scaling
                .as_ref()
                .unwrap()
                .bootstrap_profiles
                .is_empty()
        );

        let mut role_only = entry;
        role_only["buildScaling"]["profiles"] = serde_json::json!([]);
        role_only["workerCapabilities"] = serde_json::json!({"build": false});
        assert!(Config::parse(&with_connections(&role_only.to_string())).is_ok());
    }

    #[test]
    fn joint_preview_rejects_invalid_roles_permissions_and_combined_bounds() {
        for (pointer, value, message) in [
            (
                "/buildScaling/mode",
                serde_json::json!("preview"),
                "requires joint_preview",
            ),
            (
                "/buildScaling/bootstrapProfiles/0/pool",
                serde_json::json!("missing"),
                "not configured",
            ),
            (
                "/buildScaling/bootstrapProfiles/0/capabilities/build",
                serde_json::json!(true),
                "role-only",
            ),
            (
                "/buildScaling/bootstrapProfiles/0/capabilities",
                serde_json::json!({
                    "build": false, "eval": false, "network": false
                }),
                "role-only",
            ),
            (
                "/buildScaling/bootstrapProfiles/0/capabilities/exec",
                serde_json::json!(true),
                "exceeds connection permissions",
            ),
            (
                "/buildScaling/maxNewWorkers",
                serde_json::json!(9),
                "profile alternatives",
            ),
            (
                "/kubernetes/pools/role/system",
                serde_json::json!(""),
                "one worker architecture",
            ),
            (
                "/kubernetes/pools/role/features",
                serde_json::json!(["bad,feature"]),
                "cannot be encoded",
            ),
            (
                "/kubernetes/pools/x86/buildResources",
                serde_json::Value::Null,
                "explicit buildResources",
            ),
        ] {
            let mut entry = joint_connection();
            let (parent, field) = pointer.rsplit_once('/').unwrap();
            entry.pointer_mut(parent).unwrap()[field] = value;
            assert!(error_of(&entry.to_string()).contains(message), "{pointer}");
        }
        let mut entry = joint_connection();
        entry.as_object_mut().unwrap().remove("kubernetes");
        entry["githubActions"] =
            serde_json::json!({"tokenFile": "/g", "repository": "acme/runners"});
        assert!(error_of(&entry.to_string()).contains("requires Kubernetes"));
    }

    #[test]
    fn joint_preview_rejects_shared_url_ownership_within_one_config_only() {
        let first = joint_connection();
        for mut second in [preview_connection(), joint_connection()] {
            second["name"] = serde_json::json!("b");
            let base = second["gradientUrl"].as_str().unwrap().to_owned();
            for suffix in ["", "/ignored-path", "/#ignored-fragment", "/?ignored-query"] {
                second["gradientUrl"] = serde_json::json!(format!("{base}{suffix}"));
                assert!(
                    error_of(&format!("{first},{second}")).contains("share Gradient URL ownership")
                );
            }
            second["gradientUrl"] = serde_json::json!("https://other.example.com");
            assert!(Config::parse(&with_connections(&format!("{first},{second}"))).is_ok());
        }
    }
}
