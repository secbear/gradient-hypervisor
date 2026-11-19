use super::KubernetesLauncher;
use super::client::{KubernetesClient, Pod};
use super::pod::{
    CONNECTION_LABEL, LAUNCH_LABEL, POOL_LABEL, PROFILE_ENVIRONMENT_ANNOTATION,
    PROFILE_ENVIRONMENT_VERSION, WorkerPod, capacity_environment, capacity_features_setting,
    capacity_nix_config, pod_name, progress_of,
};
use crate::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use crate::capacity::BuildCapacityProfile;
use crate::gradient::{TeamWorker, worker_server_url};
use crate::launcher::Progress;
use crate::rest::status_of;
use crate::service::ConnectionService;
use crate::state::{BuildLaunchIntent, Launch, LaunchState};
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

fn profile_environment_enabled(pod: &NativePod) -> anyhow::Result<bool> {
    // Version the captured Pod without changing the strict launch-state schema.
    let version = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(PROFILE_ENVIRONMENT_ANNOTATION));
    ensure!(
        version.is_none_or(|version| version == PROFILE_ENVIRONMENT_VERSION),
        "unsupported prepared profile environment version"
    );
    Ok(version.is_some())
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

fn validate_profile_environment(
    worker: &Container,
    profile: &BuildCapacityProfile,
) -> anyhow::Result<()> {
    for (name, expected) in capacity_environment(profile) {
        let entries = worker
            .env
            .iter()
            .flatten()
            .filter(|entry| entry.name == name)
            .collect::<Vec<_>>();
        match expected {
            Some(expected) => ensure!(
                entries.len() == 1 && entries[0].value.as_deref() == Some(expected.as_str()),
                "prepared pod {name} differs from assessed profile"
            ),
            None => ensure!(
                entries.is_empty(),
                "prepared pod overrides automatic {name}"
            ),
        }
    }
    if let Some(expected) = capacity_features_setting(&profile.resources.system_features) {
        let entries = worker
            .env
            .iter()
            .flatten()
            .filter(|entry| entry.name == "NIX_CONFIG")
            .collect::<Vec<_>>();
        ensure!(
            entries.len() == 1
                && entries[0]
                    .value
                    .as_deref()
                    .and_then(|value| value.lines().last())
                    == Some(expected.as_str()),
            "prepared Nix system features differ from assessed profile"
        );
    }
    Ok(())
}

impl KubernetesLaunchSpec {
    async fn validate_scope(
        &self,
        svc: &ConnectionService,
        client: &KubernetesClient,
    ) -> anyhow::Result<()> {
        ensure!(
            self.gradient_url == *svc.gradient.base_url()
                && self.team == svc.conn.team
                && self.api_url == *client.api_url()
                && self.namespace == client.namespace(),
            "prepared launch belongs to a different server or Kubernetes target"
        );
        ensure!(
            svc.gradient.team_id(&self.team).await? == self.team_id,
            "prepared launch team identity changed"
        );
        Ok(())
    }

    fn validate_registration(&self, launch: &Launch, worker: &TeamWorker) -> anyhow::Result<()> {
        let intent = launch
            .build_intent
            .as_ref()
            .context("launch has no build intent")?;
        ensure!(
            launch.registration_started
                && worker.registration_matches(
                    &format!("k8s-{}-{}", launch.label, launch.id),
                    intent.profile.registration_capabilities(),
                ),
            "worker registration differs from prepared launch"
        );
        Ok(())
    }

    fn validate_secret_owner(
        &self,
        secret: &Secret,
        launch: &Launch,
        pod: Option<&Pod>,
    ) -> anyhow::Result<()> {
        if let Some(owners) = &secret.metadata.owner_references
            && !owners.is_empty()
        {
            let uid = pod
                .map(|pod| pod.metadata.uid.as_str())
                .or(launch.pod_uid.as_deref())
                .context("owned secret has no observed pod identity")?;
            ensure!(
                owners.len() == 1
                    && owners[0].api_version == "v1"
                    && owners[0].kind == "Pod"
                    && owners[0].name == launch.external_id
                    && owners[0].uid == uid,
                "prepared secret has a different pod owner"
            );
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        launch: &Launch,
        profile: &BuildCapacityProfile,
    ) -> anyhow::Result<()> {
        let pod: NativePod = serde_json::from_value(self.pod.clone())?;
        let profile_environment = profile_environment_enabled(&pod)?;
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
        if profile_environment {
            validate_profile_environment(worker, profile)?;
        }
        Ok(())
    }

    fn owned_pod(
        &self,
        pod: &Pod,
        launch: &Launch,
        profile: &BuildCapacityProfile,
    ) -> anyhow::Result<()> {
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
        let profile_environment = profile_environment_enabled(&expected)?;
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
        if profile_environment {
            validate_profile_environment(observed, profile)?;
        }
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
    pub async fn reconcile_prepared(
        &self,
        svc: &ConnectionService,
        launch_id: Uuid,
    ) -> anyhow::Result<()> {
        svc.ensure_mutations_allowed()?;
        let mut launch = svc
            .state
            .launch(&svc.conn.name, launch_id)
            .await?
            .context("prepared launch does not belong to this connection")?;
        if !launch.state.is_open() {
            return Ok(());
        }
        let intent = launch
            .build_intent
            .as_ref()
            .context("launch has no build intent")?;
        let spec = intent
            .kubernetes
            .as_ref()
            .context("launch has no Kubernetes specification")?;
        spec.validate_scope(svc, &self.client).await?;
        if let Some(outcome) = launch.cleanup_outcome {
            return self.finish_prepared(svc, &launch, outcome).await;
        }
        let pod = self.client.pod(&launch.external_id).await?;
        if let Some(pod) = &pod {
            spec.owned_pod(pod, &launch, &intent.profile)?;
            ensure!(
                launch.registration_started,
                "pod exists before prepared registration started"
            );
        }
        let workers = svc.gradient.team_workers(&spec.team).await?;
        if let Some(worker) = workers
            .iter()
            .find(|worker| worker.worker_id == intent.profile.worker_id.to_string())
        {
            spec.validate_registration(&launch, worker)?;
            if worker.connected {
                ensure!(
                    pod.is_some() || launch.pod_uid.is_some(),
                    "connected worker has no owned prepared pod"
                );
                if pod.is_some() {
                    svc.state
                        .advance_launch(launch.id, LaunchState::Connected)
                        .await?;
                    launch.state = LaunchState::Connected;
                }
            }
        }
        let ended = pod.as_ref().is_some_and(|pod| {
            pod.metadata.deletion_timestamp.is_some() || progress_of(Some(pod)) == Progress::Ended
        });
        let lost =
            pod.is_none() && (launch.pod_uid.is_some() || launch.state == LaunchState::Connected);
        let expired = launch.state != LaunchState::Connected
            && OffsetDateTime::now_utc() >= intent.startup_deadline;
        if ended || lost || expired {
            let succeeded = pod.as_ref().is_some_and(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|status| status.phase.as_deref())
                    == Some("Succeeded")
            });
            let outcome = if succeeded && launch.state == LaunchState::Connected {
                LaunchState::Completed
            } else {
                LaunchState::Failed
            };
            return self.finish_prepared(svc, &launch, outcome).await;
        }
        self.launch_prepared(svc, launch_id).await?;
        let progress = progress_of(self.client.pod(&launch.external_id).await?.as_ref());
        let state = match progress {
            Progress::Running => LaunchState::Running,
            Progress::Waiting => LaunchState::Queued,
            Progress::Ended => return Ok(()),
        };
        svc.state.advance_launch(launch.id, state).await?;
        Ok(())
    }

    async fn finish_prepared(
        &self,
        svc: &ConnectionService,
        launch: &Launch,
        outcome: LaunchState,
    ) -> anyhow::Result<()> {
        let intent = launch
            .build_intent
            .as_ref()
            .context("launch has no build intent")?;
        let spec = intent
            .kubernetes
            .as_ref()
            .context("launch has no Kubernetes specification")?;
        spec.validate_scope(svc, &self.client).await?;
        svc.state.begin_build_cleanup(launch, outcome).await?;
        let pod = self.client.pod(&launch.external_id).await?;
        if let Some(pod) = &pod {
            spec.owned_pod(pod, launch, &intent.profile)?;
            ensure!(
                launch.registration_started,
                "pod exists before prepared registration started"
            );
        }
        let secret = self.client.secret(&launch.external_id).await?;
        if let Some(secret) = &secret {
            spec.credential(secret, launch)?;
            spec.validate_secret_owner(secret, launch, pod.as_ref())?;
        }
        let workers = svc.gradient.team_workers(&spec.team).await?;
        let worker = workers
            .iter()
            .find(|worker| worker.worker_id == intent.profile.worker_id.to_string());
        if let Some(worker) = worker {
            spec.validate_registration(launch, worker)?;
        }
        if let Some(secret_uid) = secret
            .as_ref()
            .and_then(|secret| secret.metadata.uid.as_deref())
            .or(launch.secret_uid.as_deref())
        {
            svc.state
                .bind_build_objects(
                    launch,
                    secret_uid,
                    pod.as_ref().map(|pod| pod.metadata.uid.as_str()),
                )
                .await?;
        }
        if let Some(pod) = pod {
            self.client
                .delete_pod_with_uid(&launch.external_id, &pod.metadata.uid)
                .await?;
            if self.client.pod(&launch.external_id).await?.is_some() {
                return Ok(());
            }
        }
        if let Some(secret) = secret {
            self.client
                .delete_secret_with_uid(
                    &launch.external_id,
                    secret
                        .metadata
                        .uid
                        .as_deref()
                        .context("credential secret has no UID")?,
                )
                .await?;
            if self.client.secret(&launch.external_id).await?.is_some() {
                return Ok(());
            }
        }
        if worker.is_some() {
            crate::events::delete_worker(svc, launch).await?;
            ensure!(
                !svc.gradient
                    .team_workers(&spec.team)
                    .await?
                    .iter()
                    .any(|worker| { worker.worker_id == intent.profile.worker_id.to_string() }),
                "prepared worker registration is still present"
            );
        }
        svc.state.close_launch(launch.id, outcome).await?;
        tracing::info!(
            connection = %svc.conn.name, launch_id = %launch.id, ?outcome,
            "finished prepared worker launch"
        );
        Ok(())
    }

    pub async fn prepare_build_launch(
        &self,
        svc: &ConnectionService,
        planned: &PlannedBuildWorker,
        profile: &BuildCapacityProfile,
        policy: &BuildScalingPolicy,
        now: OffsetDateTime,
    ) -> anyhow::Result<Launch> {
        svc.ensure_mutations_allowed()?;
        profile.validate()?;
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
        pool.validate_capacity_environment()?;
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
            nix_config: capacity_nix_config(
                &svc.conn.worker_substituters,
                &svc.conn.worker_trusted_public_keys,
                &profile.resources.system_features,
            ),
        }
        .capacity_manifest(profile);
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
            launch.cleanup_outcome.is_none(),
            "prepared launch cleanup has started"
        );
        ensure!(
            svc.conn
                .worker_capabilities
                .permits(intent.profile.registration_capabilities()),
            "prepared profile exceeds current connection permissions"
        );
        spec.validate_scope(svc, &self.client).await?;
        let mut pod = self.client.pod(&launch.external_id).await?;
        if let Some(pod) = &pod {
            spec.owned_pod(pod, &launch, &intent.profile)?;
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
        ensure!(
            secret
                .metadata
                .owner_references
                .as_ref()
                .is_none_or(Vec::is_empty)
                || pod.is_some(),
            "owned secret's pod is missing; refusing worker replacement"
        );
        spec.validate_secret_owner(&secret, &launch, pod.as_ref())?;
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
                spec.validate_registration(&launch, &worker)?;
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
        spec.owned_pod(&pod, &launch, &intent.profile)?;
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
