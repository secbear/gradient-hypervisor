use super::KubernetesLauncher;
use super::client::Pod;
use super::pod::{CONNECTION_LABEL, LAUNCH_LABEL, POOL_LABEL, WorkerPod, nix_config, pod_name};
use crate::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use crate::capacity::BuildCapacityProfile;
use crate::gradient::worker_server_url;
use crate::rest::status_of;
use crate::service::ConnectionService;
use crate::state::{BuildLaunchIntent, Launch};
use anyhow::{Context, ensure};
use base64::Engine as _;
use k8s_openapi::api::core::v1::{Container, Pod as NativePod, PodSpec, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use rand::Rng as _;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesLaunchSpec {
    pub gradient_url: Url,
    pub team: String,
    pub team_id: Uuid,
    pub api_url: Url,
    pub namespace: String,
    pub pod: Value,
}

fn labels(launch: &Launch) -> BTreeMap<String, String> {
    BTreeMap::from([
        (CONNECTION_LABEL.into(), launch.connection.clone()),
        (LAUNCH_LABEL.into(), launch.id.to_string()),
        (POOL_LABEL.into(), launch.label.clone()),
        (
            "app.kubernetes.io/managed-by".into(),
            "gradient-autoscaler".into(),
        ),
    ])
}

fn owns_labels(actual: &BTreeMap<String, String>, launch: &Launch) -> bool {
    labels(launch)
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value))
}

fn worker_container(spec: &PodSpec) -> anyhow::Result<&Container> {
    spec.containers
        .iter()
        .find(|container| container.name == "worker")
        .context("pod has no worker container")
}

fn pod_worker_id(spec: &PodSpec) -> anyhow::Result<Uuid> {
    let worker = worker_container(spec)?;
    let mut ids = worker
        .env
        .iter()
        .flatten()
        .filter(|var| var.name == "GRADIENT_WORKER_ID");
    let id = ids
        .next()
        .and_then(|var| var.value.as_deref())
        .context("pod has no literal worker identity")?
        .parse()?;
    ensure!(ids.next().is_none(), "pod has duplicate worker identities");
    Ok(id)
}

impl KubernetesLaunchSpec {
    pub(crate) fn validate(
        &self,
        launch: &Launch,
        profile: &BuildCapacityProfile,
    ) -> anyhow::Result<()> {
        let pod: NativePod = serde_json::from_value(self.pod.clone())?;
        ensure!(
            pod.metadata.name.as_deref() == Some(launch.external_id.as_str())
                && launch.external_id == pod_name(launch.id),
            "prepared pod name does not match launch"
        );
        ensure!(
            pod.metadata
                .labels
                .as_ref()
                .is_some_and(|actual| owns_labels(actual, launch)),
            "prepared pod has different ownership labels"
        );
        ensure!(
            pod_worker_id(pod.spec.as_ref().context("prepared pod has no spec")?)?
                == profile.worker_id,
            "prepared pod differs from assessed worker identity"
        );
        let worker = worker_container(pod.spec.as_ref().context("prepared pod has no spec")?)?;
        for (name, value) in [
            (
                "GRADIENT_WORKER_BUILD_MAX_CONCURRENT",
                profile.resources.max_concurrent_builds.to_string(),
            ),
            (
                "GRADIENT_WORKER_SERVER_URL",
                worker_server_url(&self.gradient_url)?,
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_BUILD",
                profile.capabilities.build.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_NETWORK",
                profile.capabilities.network.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_SECRETS",
                profile.capabilities.secrets.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_EXEC",
                profile.capabilities.exec.to_string(),
            ),
            (
                "GRADIENT_WORKER_CAPABILITIES_EVAL",
                profile.capabilities.eval.to_string(),
            ),
        ] {
            let entries = worker
                .env
                .iter()
                .flatten()
                .filter(|var| var.name == name)
                .collect::<Vec<_>>();
            ensure!(
                entries.len() == 1 && entries[0].value.as_deref() == Some(value.as_str()),
                "prepared pod {name} differs from profile"
            );
        }
        Ok(())
    }

    fn owned_pod(&self, pod: &Pod, launch: &Launch) -> anyhow::Result<()> {
        ensure!(
            pod.metadata.name == launch.external_id
                && !pod.metadata.uid.is_empty()
                && launch
                    .pod_uid
                    .as_ref()
                    .is_none_or(|uid| uid == &pod.metadata.uid)
                && owns_labels(&pod.metadata.labels, launch)
                && Some(pod_worker_id(
                    pod.spec.as_ref().context("pod has no worker spec")?
                )?) == launch.worker_id,
            "existing pod does not belong to prepared launch"
        );
        let expected: NativePod = serde_json::from_value(self.pod.clone())?;
        let expected =
            worker_container(expected.spec.as_ref().context("prepared pod has no spec")?)?;
        let observed = worker_container(pod.spec.as_ref().context("pod has no spec")?)?;
        ensure!(
            observed.image == expected.image
                && expected.env.iter().flatten().all(|var| {
                    observed
                        .env
                        .iter()
                        .flatten()
                        .filter(|actual| actual.name == var.name)
                        .eq(std::iter::once(var))
                }),
            "existing pod worker settings differ from prepared launch"
        );
        Ok(())
    }

    fn credential(&self, secret: &Secret, launch: &Launch) -> anyhow::Result<String> {
        ensure!(
            secret.metadata.name.as_deref() == Some(launch.external_id.as_str())
                && secret.immutable == Some(true)
                && secret.type_.as_deref() == Some("Opaque")
                && secret
                    .metadata
                    .uid
                    .as_deref()
                    .is_some_and(|uid| !uid.is_empty())
                && launch
                    .secret_uid
                    .as_ref()
                    .is_none_or(|uid| secret.metadata.uid.as_ref() == Some(uid))
                && secret
                    .metadata
                    .labels
                    .as_ref()
                    .is_some_and(|actual| owns_labels(actual, launch)),
            "existing secret does not belong to prepared launch"
        );
        let peers = secret
            .data
            .as_ref()
            .and_then(|data| data.get("peers"))
            .context("prepared secret has no peers credential")?;
        let peers = std::str::from_utf8(&peers.0)?;
        let prefix = format!("{}:", self.team_id);
        let token = peers
            .strip_prefix(&prefix)
            .and_then(|peers| peers.strip_suffix('\n'))
            .context("prepared secret belongs to a different team")?;
        ensure!(
            base64::engine::general_purpose::STANDARD
                .decode(token)?
                .len()
                == 48,
            "prepared secret has an invalid worker token"
        );
        Ok(token.into())
    }
}

impl KubernetesLauncher {
    pub async fn prepare_build_launch(
        &self,
        svc: &ConnectionService,
        planned: &PlannedBuildWorker,
        profile: &BuildCapacityProfile,
        policy: &BuildScalingPolicy,
        now: OffsetDateTime,
    ) -> anyhow::Result<Launch> {
        svc.ensure_mutations_allowed()?;
        ensure!(
            planned.worker_id == profile.worker_id,
            "planned worker identity differs from profile"
        );
        ensure!(
            policy.startup_grace > time::Duration::ZERO,
            "startup grace must be positive"
        );
        let pool = self
            .config
            .pools
            .get(&planned.label)
            .context("planned pool is not configured")?;
        ensure!(
            profile.resources.architectures == [pool.system.clone()]
                && profile
                    .resources
                    .system_features
                    .iter()
                    .collect::<HashSet<_>>()
                    == pool.features.iter().collect()
                && profile.resources.max_concurrent_builds == pool.max_jobs,
            "assessed profile differs from pool build capabilities"
        );
        let caps = profile.registration_capabilities();
        ensure!(
            svc.conn.worker_capabilities.permits(caps),
            "prepared profile exceeds connection permissions"
        );
        ensure!(
            svc.conn.gradient_url == *svc.gradient.base_url(),
            "connection and client target differ"
        );
        let launch_id = Uuid::new_v4();
        let mut launch = Launch {
            id: launch_id,
            worker_id: Some(planned.worker_id),
            dispatched_at: now,
            ..Launch::new(&svc.conn.name, &pod_name(launch_id), &planned.label)
        };
        let pod = WorkerPod {
            capabilities: caps,
            connection: &launch.connection,
            launch_id,
            pool_name: &launch.label,
            pool,
            cluster: &self.config,
            server_url: &worker_server_url(&svc.conn.gradient_url)?,
            worker_id: planned.worker_id,
            idle_timeout: svc.conn.idle_timeout,
            max_lifetime: svc.conn.max_lifetime,
            nix_config: nix_config(
                &svc.conn.worker_substituters,
                &svc.conn.worker_trusted_public_keys,
            ),
        }
        .manifest();
        launch.build_intent = Some(BuildLaunchIntent {
            profile: profile.clone(),
            startup_deadline: now
                .checked_add(policy.startup_grace)
                .context("startup deadline overflows")?,
            kubernetes: Some(KubernetesLaunchSpec {
                gradient_url: svc.gradient.base_url().clone(),
                team: svc.conn.team.clone(),
                team_id: svc.gradient.team_id(&svc.conn.team).await?,
                api_url: self.client.api_url().clone(),
                namespace: self.client.namespace().into(),
                pod,
            }),
        });
        svc.state.insert_launch(&launch).await?;
        Ok(launch)
    }

    pub async fn launch_prepared(
        &self,
        svc: &ConnectionService,
        launch_id: Uuid,
    ) -> anyhow::Result<()> {
        svc.ensure_mutations_allowed()?;
        let launch = svc
            .state
            .launch(&svc.conn.name, launch_id)
            .await?
            .context("prepared launch does not belong to this connection")?;
        let intent = launch
            .build_intent
            .as_ref()
            .context("launch has no build intent")?;
        let spec = intent
            .kubernetes
            .as_ref()
            .context("launch has no Kubernetes specification")?;
        ensure!(launch.state.is_open(), "prepared launch is closed");
        ensure!(
            svc.conn
                .worker_capabilities
                .permits(intent.profile.registration_capabilities()),
            "prepared profile exceeds current connection permissions"
        );
        ensure!(
            spec.gradient_url == *svc.gradient.base_url()
                && spec.team == svc.conn.team
                && spec.api_url == *self.client.api_url()
                && spec.namespace == self.client.namespace(),
            "prepared launch belongs to a different server or Kubernetes target"
        );
        ensure!(
            svc.gradient.team_id(&spec.team).await? == spec.team_id,
            "prepared launch team identity changed"
        );
        let mut pod = self.client.pod(&launch.external_id).await?;
        if let Some(pod) = &pod {
            spec.owned_pod(pod, &launch)?;
            ensure!(
                launch.registration_started,
                "pod exists before prepared registration started"
            );
        }
        let secret = self.client.secret(&launch.external_id).await?;
        ensure!(
            pod.is_none() || secret.is_some(),
            "existing pod has no saved worker credential; refusing token replacement"
        );
        if pod.is_none() {
            ensure!(
                OffsetDateTime::now_utc() < intent.startup_deadline,
                "prepared launch startup deadline expired"
            );
            ensure!(
                launch.state != crate::state::LaunchState::Connected,
                "connected launch lost its pod"
            );
            ensure!(
                launch.pod_uid.is_none(),
                "prepared launch lost its previously observed pod"
            );
        }
        let mut secret = match secret {
            Some(secret) => secret,
            None => {
                ensure!(
                    !launch.registration_started && launch.secret_uid.is_none(),
                    "prepared launch lost its saved worker credential"
                );
                let token = base64::engine::general_purpose::STANDARD
                    .encode(rand::rng().random::<[u8; 48]>());
                let secret = Secret {
                    immutable: Some(true),
                    metadata: ObjectMeta {
                        name: Some(launch.external_id.clone()),
                        labels: Some(labels(&launch)),
                        ..Default::default()
                    },
                    type_: Some("Opaque".into()),
                    string_data: Some(BTreeMap::from([(
                        "peers".into(),
                        format!("{}:{token}\n", spec.team_id),
                    )])),
                    ..Default::default()
                };
                if let Err(error) = self
                    .client
                    .create_secret(&serde_json::to_value(secret)?)
                    .await
                    && status_of(&error) != Some(StatusCode::CONFLICT)
                {
                    return Err(error);
                }
                self.client
                    .secret(&launch.external_id)
                    .await?
                    .context("created secret is not readable")?
            }
        };
        let token = spec.credential(&secret, &launch)?;
        let secret_uid = secret
            .metadata
            .uid
            .as_deref()
            .context("credential secret has no UID")?;
        if let Some(owners) = &secret.metadata.owner_references
            && !owners.is_empty()
        {
            let pod = pod
                .as_ref()
                .context("owned secret's pod is missing; refusing worker replacement")?;
            ensure!(
                owners.len() == 1
                    && owners[0].api_version == "v1"
                    && owners[0].kind == "Pod"
                    && owners[0].name == pod.metadata.name
                    && owners[0].uid == pod.metadata.uid,
                "prepared secret has a different pod owner"
            );
        }
        svc.state
            .bind_build_objects(
                &launch,
                secret_uid,
                pod.as_ref().map(|pod| pod.metadata.uid.as_str()),
            )
            .await?;
        let display_name = format!("k8s-{}-{}", launch.label, launch.id);
        let caps = intent.profile.registration_capabilities();
        let existing = svc
            .gradient
            .team_workers(&spec.team)
            .await?
            .into_iter()
            .find(|worker| worker.worker_id == intent.profile.worker_id.to_string());
        match existing {
            Some(worker) => {
                ensure!(
                    launch.registration_started && worker.registration_matches(&display_name, caps),
                    "worker registration differs from prepared launch"
                );
                ensure!(
                    !worker.connected || pod.is_some(),
                    "connected worker has no owned prepared pod"
                );
            }
            None => {
                svc.state.claim_build_registration(&launch).await?;
                let team = svc
                    .gradient
                    .register_team_worker_with_token(
                        &spec.team,
                        intent.profile.worker_id,
                        &display_name,
                        caps,
                        &token,
                    )
                    .await?;
                ensure!(
                    team == spec.team_id,
                    "worker registration returned a different team"
                );
            }
        }
        if pod.is_none() {
            ensure!(
                OffsetDateTime::now_utc() < intent.startup_deadline,
                "prepared launch startup deadline expired"
            );
            pod = Some(match self.client.create_pod(&spec.pod).await {
                Ok(pod) => pod,
                Err(error) if status_of(&error) == Some(StatusCode::CONFLICT) => self
                    .client
                    .pod(&launch.external_id)
                    .await?
                    .context("conflicting pod is not readable")?,
                Err(error) => return Err(error),
            });
        }
        let pod = pod.context("prepared pod is missing")?;
        spec.owned_pod(&pod, &launch)?;
        svc.state
            .bind_build_objects(&launch, secret_uid, Some(&pod.metadata.uid))
            .await?;
        let owner = OwnerReference {
            api_version: "v1".into(),
            kind: "Pod".into(),
            name: pod.metadata.name,
            uid: pod.metadata.uid,
            ..Default::default()
        };
        if secret
            .metadata
            .owner_references
            .as_ref()
            .is_none_or(Vec::is_empty)
        {
            secret.metadata.owner_references = Some(vec![owner]);
            self.client.update_secret(&secret).await?;
        }
        Ok(())
    }
}
