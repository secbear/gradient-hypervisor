use crate::capacity::BuildCapacityProfile;
use crate::config::pod_options::{CAPACITY_ENV, PEERS_DIR, PEERS_VOLUME, STATE_DIR, STATE_VOLUME};
use crate::config::{Kubernetes, Pool, WorkerCapabilities};
use crate::kubernetes::client::Pod;
use crate::launcher::Progress;
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub const CONNECTION_LABEL: &str = "gradient.wavelens.io/connection";
pub const LAUNCH_LABEL: &str = "gradient.wavelens.io/launch";
pub const POOL_LABEL: &str = "gradient.wavelens.io/pool";
const GRACE_SECONDS: u64 = 600;
const START_FAILURES: [&str; 5] = [
    "ErrImagePull",
    "ImagePullBackOff",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
];

pub struct WorkerPod<'a> {
    pub connection: &'a str,
    pub launch_id: Uuid,
    pub pool_name: &'a str,
    pub pool: &'a Pool,
    pub cluster: &'a Kubernetes,
    pub server_url: &'a str,
    pub worker_id: Uuid,
    pub idle_timeout: u64,
    pub max_lifetime: u64,
    pub nix_config: Option<String>,
    pub capabilities: WorkerCapabilities,
}

pub fn pod_name(launch_id: Uuid) -> String {
    format!("gradient-worker-{launch_id}")
}

impl WorkerPod<'_> {
    pub(crate) fn capacity_manifest(&self, profile: &BuildCapacityProfile) -> Value {
        let mut manifest = self.manifest();
        manifest["spec"]["containers"][0]["env"]
            .as_array_mut()
            .unwrap()
            .extend(
                capacity_environment(profile)
                    .into_iter()
                    .filter_map(|(name, value)| {
                        value.map(|value| json!({ "name": name, "value": value }))
                    }),
            );
        manifest
    }

    pub fn manifest(&self) -> Value {
        let name = pod_name(self.launch_id);
        let mut manifest = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": name,
                "labels": {
                    "app.kubernetes.io/name": "gradient-worker",
                    "app.kubernetes.io/managed-by": "gradient-autoscaler",
                    CONNECTION_LABEL: self.connection,
                    LAUNCH_LABEL: self.launch_id.to_string(),
                    POOL_LABEL: self.pool_name,
                },
            },
            "spec": {
                "restartPolicy": "Never",
                "automountServiceAccountToken": false,
                "enableServiceLinks": false,
                "activeDeadlineSeconds": self.max_lifetime + GRACE_SECONDS,
                "nodeSelector": self.pool.node_selector,
                "tolerations": self.pool.tolerations,
                "imagePullSecrets": self.cluster.image_pull_secrets.iter()
                    .map(|secret| json!({ "name": secret }))
                    .collect::<Vec<_>>(),
                "containers": [{
                    "name": "worker",
                    "image": self.cluster.image,
                    "env": self.env(),
                    "resources": self.resources(),
                    "securityContext": { "privileged": self.pool.privileged },
                    "volumeMounts": [
                        { "name": PEERS_VOLUME, "mountPath": PEERS_DIR, "readOnly": true },
                        { "name": STATE_VOLUME, "mountPath": STATE_DIR },
                    ],
                }],
                "volumes": [
                    { "name": PEERS_VOLUME, "secret": { "secretName": name, "defaultMode": 0o400 } },
                    { "name": STATE_VOLUME, "emptyDir": {} },
                ],
            },
        });
        let options = &self.pool.pod_options;
        manifest["spec"]["volumes"]
            .as_array_mut()
            .unwrap()
            .extend(options.volumes.iter().map(|volume| json!(volume)));
        manifest["spec"]["containers"][0]["volumeMounts"]
            .as_array_mut()
            .unwrap()
            .extend(options.volume_mounts.iter().map(|mount| json!(mount)));
        manifest
    }

    fn env(&self) -> Vec<Value> {
        let mut env = vec![
            ("GRADIENT_WORKER_SERVER_URL", self.server_url.to_owned()),
            ("GRADIENT_WORKER_ID", self.worker_id.to_string()),
            ("GRADIENT_WORKER_PEERS_FILE", format!("{PEERS_DIR}/peers")),
            ("GRADIENT_WORKER_BASE_DIR", STATE_DIR.to_owned()),
            (
                "GRADIENT_WORKER_CAPABILITIES_BUILD",
                self.capabilities.build.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_NETWORK",
                self.capabilities.network.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_SECRETS",
                self.capabilities.secrets.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_EXEC",
                self.capabilities.exec.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_EVAL",
                self.capabilities.eval.to_string(),
            ),
            (
                "GRADIENT_WORKER_BUILD_MAX_CONCURRENT",
                self.pool.max_jobs.to_string(),
            ),
            (
                "GRADIENT_WORKER_IDLE_TIMEOUT",
                self.idle_timeout.to_string(),
            ),
            (
                "GRADIENT_WORKER_MAX_LIFETIME",
                self.max_lifetime.to_string(),
            ),
        ];
        if let Some(nix_config) = &self.nix_config {
            env.push(("NIX_CONFIG", nix_config.clone()));
        }

        let mut env: Vec<Value> = env
            .into_iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect();
        env.extend(self.pool.pod_options.env.iter().map(|var| json!(var)));
        env
    }

    fn resources(&self) -> Value {
        let (mut requests, mut limits) = (Map::new(), Map::new());
        if let Some(cpu) = &self.pool.cpu {
            requests.insert("cpu".into(), json!(cpu));
        }

        if let Some(memory) = &self.pool.memory {
            requests.insert("memory".into(), json!(memory));
            limits.insert("memory".into(), json!(memory));
        }

        json!({ "requests": requests, "limits": limits })
    }
}

pub fn peers_secret(pod: &Pod, team: Uuid, token: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": pod.metadata.name,
            "labels": pod.metadata.labels,
            "ownerReferences": [{
                "apiVersion": "v1",
                "kind": "Pod",
                "name": pod.metadata.name,
                "uid": pod.metadata.uid,
            }],
        },
        "type": "Opaque",
        "stringData": { "peers": format!("{team}:{token}\n") },
    })
}

pub(crate) fn capacity_environment(
    profile: &BuildCapacityProfile,
) -> [(&'static str, Option<String>); 4] {
    let resources = &profile.resources;
    [
        (CAPACITY_ENV[0], Some(resources.architectures.join(","))),
        (
            CAPACITY_ENV[1],
            (!resources.system_features.is_empty()).then(|| resources.system_features.join(",")),
        ),
        (
            CAPACITY_ENV[2],
            (resources.cpu_core_score > 0).then(|| resources.cpu_core_score.to_string()),
        ),
        (
            CAPACITY_ENV[3],
            resources
                .build_cores
                .filter(|cores| *cores > 0)
                .map(|cores| cores.to_string()),
        ),
    ]
    .map(|(name, value)| (name, value.map(literal_environment_value)))
}

fn literal_environment_value(value: String) -> String {
    // Kubernetes expands dollars in EnvVar values; profile settings are literals.
    value.replace('$', "$$")
}

pub(crate) fn capacity_features_setting(features: &[String]) -> Option<String> {
    (!features.is_empty()).then(|| {
        literal_environment_value(format!("extra-system-features = {}", features.join(" ")))
    })
}

pub(crate) fn capacity_nix_config(
    substituters: &[String],
    trusted_public_keys: &[String],
    features: &[String],
) -> Option<String> {
    let caches = nix_config(substituters, trusted_public_keys).map(literal_environment_value);
    let settings = caches
        .into_iter()
        .chain(capacity_features_setting(features))
        .collect::<Vec<_>>();
    (!settings.is_empty()).then(|| settings.join("\n"))
}

pub fn nix_config(substituters: &[String], trusted_public_keys: &[String]) -> Option<String> {
    if substituters.is_empty() && trusted_public_keys.is_empty() {
        return None;
    }

    Some(format!(
        "extra-substituters = {}\nextra-trusted-public-keys = {}",
        substituters.join(" "),
        trusted_public_keys.join(" ")
    ))
}

pub fn progress_of(pod: Option<&Pod>) -> Progress {
    let Some(status) = pod.and_then(|pod| pod.status.as_ref()) else {
        return match pod {
            Some(_) => Progress::Waiting,
            None => Progress::Ended,
        };
    };

    let failed_to_start = status.container_statuses.iter().any(|container| {
        container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .and_then(|waiting| waiting.reason.as_deref())
            .is_some_and(|reason| START_FAILURES.contains(&reason))
    });
    match status.phase.as_deref() {
        _ if failed_to_start => Progress::Ended,
        Some("Succeeded" | "Failed") => Progress::Ended,
        Some("Running" | "Unknown") => Progress::Running,
        _ => Progress::Waiting,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cluster() -> Kubernetes {
        serde_json::from_value(json!({
            "namespace": "ci",
            "image": "registry.example.com/gradient-worker:2.0.0",
            "imagePullSecrets": ["registry"],
            "pools": { "x86": { "system": "x86_64-linux" } },
        }))
        .unwrap()
    }

    fn pool() -> Pool {
        serde_json::from_value(json!({
            "system": "x86_64-linux",
            "features": ["kvm"],
            "cpu": "4",
            "memory": "16Gi",
            "nodeSelector": { "kubernetes.io/arch": "amd64" },
            "tolerations": [{ "key": "builders", "operator": "Exists" }],
            "privileged": true,
        }))
        .unwrap()
    }

    fn capacity_profile() -> BuildCapacityProfile {
        BuildCapacityProfile {
            name: "x86".into(),
            worker_id: Uuid::new_v4(),
            capabilities: crate::capacity::CapacityCapabilities {
                build: true,
                network: true,
                secrets: true,
                eval: true,
                ..Default::default()
            },
            resources: crate::capacity::BuilderResources {
                architectures: vec!["x86_64-linux".into()],
                system_features: vec![],
                max_concurrent_builds: 2,
                cpu_count: 4,
                ram_total_mb: 8192,
                cpu_core_score: 0,
                build_cores: Some(0),
            },
        }
    }

    fn worker_pod<'a>(cluster: &'a Kubernetes, pool: &'a Pool) -> WorkerPod<'a> {
        WorkerPod {
            connection: "ops",
            launch_id: Uuid::nil(),
            pool_name: "x86",
            pool,
            cluster,
            server_url: "wss://gradient.example.com/proto",
            worker_id: Uuid::from_u128(7),
            idle_timeout: 120,
            max_lifetime: 3600,
            nix_config: nix_config(&["https://cache.example.com".into()], &[]),
            capabilities: WorkerCapabilities::default(),
        }
    }

    fn env_of(manifest: &Value) -> BTreeMap<String, String> {
        manifest["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|var| {
                let name = var["name"].as_str().unwrap().to_owned();
                (name, var["value"].as_str().unwrap().to_owned())
            })
            .collect()
    }

    fn pod_with(phase: &str, waiting: Option<&str>) -> Pod {
        serde_json::from_value(json!({
            "metadata": { "name": "p" },
            "status": {
                "phase": phase,
                "containerStatuses": [{ "state": match waiting {
                    Some(reason) => json!({ "waiting": { "reason": reason } }),
                    None => json!({ "running": {} }),
                } }],
            },
        }))
        .unwrap()
    }

    #[test]
    fn pod_runs_once_on_the_pool_and_reads_its_token_from_the_secret() {
        let (cluster, pool) = (cluster(), pool());
        let manifest = worker_pod(&cluster, &pool).manifest();
        let spec = &manifest["spec"];
        let container = &spec["containers"][0];
        let env = env_of(&manifest);

        assert_eq!(spec["restartPolicy"], "Never");
        assert_eq!(spec["automountServiceAccountToken"], false);
        assert_eq!(spec["nodeSelector"]["kubernetes.io/arch"], "amd64");
        assert_eq!(spec["tolerations"][0]["key"], "builders");
        assert_eq!(spec["imagePullSecrets"], json!([{ "name": "registry" }]));
        assert_eq!(container["securityContext"]["privileged"], true);
        assert_eq!(
            container["resources"]["requests"],
            json!({ "cpu": "4", "memory": "16Gi" })
        );
        assert_eq!(
            container["resources"]["limits"],
            json!({ "memory": "16Gi" })
        );
        assert_eq!(
            spec["volumes"][0]["secret"]["secretName"],
            manifest["metadata"]["name"]
        );
        assert_eq!(env["GRADIENT_WORKER_ID"], Uuid::from_u128(7).to_string());
        assert_eq!(env["GRADIENT_WORKER_CAPABILITIES_EXEC"], "false");
        assert_eq!(
            env["GRADIENT_WORKER_PEERS_FILE"],
            "/run/secrets/gradient/peers"
        );
        assert!(env["NIX_CONFIG"].contains("extra-substituters = https://cache.example.com"));
        assert_eq!(manifest["metadata"]["labels"][CONNECTION_LABEL], "ops");
    }

    #[test]
    fn secret_holds_the_peer_line_and_is_owned_by_the_pod() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": { "name": "gradient-worker-1", "uid": "abc", "labels": { CONNECTION_LABEL: "ops" } },
        }))
        .unwrap();
        let team = Uuid::from_u128(3);
        let secret = peers_secret(&pod, team, "tok");

        assert_eq!(secret["stringData"]["peers"], format!("{team}:tok\n"));
        assert_eq!(secret["metadata"]["ownerReferences"][0]["uid"], "abc");
        assert_eq!(secret["metadata"]["labels"][CONNECTION_LABEL], "ops");
    }

    #[test]
    fn pod_without_substituters_gets_no_nix_config() {
        let (cluster, pool) = (cluster(), pool());
        let manifest = WorkerPod {
            nix_config: nix_config(&[], &[]),
            ..worker_pod(&cluster, &pool)
        }
        .manifest();
        assert!(!env_of(&manifest).contains_key("NIX_CONFIG"));
    }

    #[test]
    fn prepared_environment_retains_native_automatic_settings() {
        let profile = capacity_profile();
        let (cluster, mut pool) = (cluster(), pool());
        pool.features.clear();
        pool.max_jobs = 2;
        let manifest = WorkerPod {
            worker_id: profile.worker_id,
            nix_config: capacity_nix_config(&[], &[], &[]),
            ..worker_pod(&cluster, &pool)
        }
        .capacity_manifest(&profile);
        let env = env_of(&manifest);
        assert_eq!(env["GRADIENT_WORKER_SYSTEM_ARCHITECTURES"], "x86_64-linux");
        for name in [
            "GRADIENT_WORKER_SYSTEM_FEATURES",
            "GRADIENT_WORKER_SYSTEM_CPU_CORE_SCORE",
            "GRADIENT_WORKER_BUILD_MAX_CORES",
            "NIX_CONFIG",
        ] {
            assert!(!env.contains_key(name), "{name}");
        }
    }

    #[test]
    fn capacity_environment_preserves_literal_dollars_in_native_settings() {
        let mut profile = capacity_profile();
        profile.resources.architectures = vec!["arch$(NODE)".into()];
        profile.resources.system_features = vec!["key$$name".into(), "key$(NODE)".into()];
        let (cluster, mut pool) = (cluster(), pool());
        pool.system = profile.resources.architectures[0].clone();
        pool.features = profile.resources.system_features.clone();
        pool.max_jobs = 2;
        pool.validate_capacity_environment().unwrap();
        let manifest = WorkerPod {
            worker_id: profile.worker_id,
            nix_config: capacity_nix_config(
                &["https://cache.example.com/$(TENANT)".into()],
                &["cache-key".into()],
                &profile.resources.system_features,
            ),
            ..worker_pod(&cluster, &pool)
        }
        .capacity_manifest(&profile);
        let env = env_of(&manifest);
        assert_eq!(env["GRADIENT_WORKER_SYSTEM_ARCHITECTURES"], "arch$$(NODE)");
        assert_eq!(
            env["GRADIENT_WORKER_SYSTEM_FEATURES"],
            "key$$$$name,key$$(NODE)"
        );
        assert_eq!(
            env["NIX_CONFIG"],
            "extra-substituters = https://cache.example.com/$$(TENANT)\nextra-trusted-public-keys = cache-key\nextra-system-features = key$$$$name key$$(NODE)"
        );
    }

    #[test]
    fn capacity_nix_features_do_not_require_cache_settings_or_replace_native_features() {
        assert_eq!(
            capacity_nix_config(&[], &[], &["signing-key".into()]),
            Some("extra-system-features = signing-key".into())
        );
        assert_eq!(
            capacity_nix_config(&["https://cache.example.com".into()], &["cache-key".into()], &[]),
            Some("extra-substituters = https://cache.example.com\nextra-trusted-public-keys = cache-key".into())
        );
    }

    #[test]
    fn progress_follows_the_pod_phase() {
        let cases = [
            (
                Some(pod_with("Pending", Some("ContainerCreating"))),
                Progress::Waiting,
            ),
            (
                Some(pod_with("Pending", Some("ImagePullBackOff"))),
                Progress::Ended,
            ),
            (Some(pod_with("Running", None)), Progress::Running),
            (Some(pod_with("Succeeded", None)), Progress::Ended),
            (Some(pod_with("Failed", None)), Progress::Ended),
            (None, Progress::Ended),
        ];
        for (pod, expected) in cases {
            assert_eq!(progress_of(pod.as_ref()), expected, "{pod:?}");
        }
    }

    #[test]
    fn pod_options_append_to_generated_volumes_mounts_and_environment() {
        let (cluster, mut pool) = (cluster(), pool());
        let baseline = worker_pod(&cluster, &pool).manifest();
        pool.pod_options = serde_json::from_value(json!({
            "volumes": [
                {"name": "fetch-auth", "secret": {"secretName": "tenant-fetch-auth"}},
                {"name": "scratch"}
            ],
            "volumeMounts": [{"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}],
            "env": [
                {"name": "FETCH_MODE", "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}},
                {"name": "NODE_NAME", "value": "", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}
            ]
        })).unwrap();
        pool.pod_options.validate().unwrap();
        let manifest = worker_pod(&cluster, &pool).manifest();
        for (path, expected) in [
            (
                "/spec/volumes",
                json!([
                    {"name": "fetch-auth", "secret": {"secretName": "tenant-fetch-auth"}},
                    {"name": "scratch"}
                ]),
            ),
            (
                "/spec/containers/0/volumeMounts",
                json!([{"name": "fetch-auth", "mountPath": "/creds", "readOnly": true}]),
            ),
            (
                "/spec/containers/0/env",
                json!([
                    {"name": "FETCH_MODE", "valueFrom": {"configMapKeyRef": {"name": "worker-config", "key": "mode"}}},
                    {"name": "NODE_NAME", "value": "", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}
                ]),
            ),
        ] {
            let original = baseline.pointer(path).unwrap().as_array().unwrap();
            let extended = manifest.pointer(path).unwrap().as_array().unwrap();
            assert!(extended.starts_with(original), "{path}");
            assert_eq!(
                &extended[original.len()..],
                expected.as_array().unwrap(),
                "{path}"
            );
        }
    }

    #[test]
    fn all_generated_environment_fields_are_protected_by_config_validation() {
        let (cluster, pool) = (cluster(), pool());
        let manifest = worker_pod(&cluster, &pool).manifest();
        for env in manifest["spec"]["containers"][0]["env"].as_array().unwrap() {
            let options: crate::config::pod_options::PodOptions =
                serde_json::from_value(json!({"env": [env]})).unwrap();
            assert!(
                options.validate().is_err(),
                "unprotected generated environment: {env}"
            );
        }
    }

    #[test]
    fn pod_enforces_the_pool_build_slots() {
        let (cluster, mut pool) = (cluster(), pool());
        pool.max_jobs = 2;
        let manifest = worker_pod(&cluster, &pool).manifest();
        assert_eq!(
            env_of(&manifest)["GRADIENT_WORKER_BUILD_MAX_CONCURRENT"],
            "2"
        );
    }
}
