pub mod client;
pub mod pod;

use crate::config::{Kubernetes, Pool};
use crate::events::{finish_launch, learn_profile};
use crate::gradient::worker_server_url;
use crate::kinds::{Kind, Profile};
use crate::launcher::Progress;
use crate::service::ConnectionService;
use crate::setup::report;
use crate::state::Launch;
use anyhow::Context;
use client::KubernetesClient;
use pod::{
    CONNECTION_LABEL, LAUNCH_LABEL, WorkerPod, nix_config, peers_secret, pod_name, progress_of,
};
use std::collections::HashSet;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const PENDING_LIMIT: Duration = Duration::minutes(30);

pub struct KubernetesLauncher {
    pub config: Kubernetes,
    pub client: KubernetesClient,
}

impl KubernetesLauncher {
    pub fn new(config: Kubernetes, client: KubernetesClient) -> KubernetesLauncher {
        KubernetesLauncher { config, client }
    }

    pub fn kinds(&self) -> Vec<Kind> {
        self.config
            .pools
            .iter()
            .map(|(name, pool)| Kind {
                label: name.clone(),
                cost: 0,
                profile: Some(Profile {
                    systems: vec![pool.system.clone()],
                    features: pool.features.clone(),
                    max_jobs: pool.max_jobs,
                    ram_mb: None,
                }),
            })
            .collect()
    }

    pub fn allowed_kinds(&self) -> Vec<String> {
        self.config.pools.keys().cloned().collect()
    }

    pub async fn setup(&self, svc: &ConnectionService) -> Option<Vec<Kind>> {
        report(
            svc,
            "cleanup of leftover worker pods",
            "list and delete on pods in the namespace",
            self.remove_leftover_pods(svc).await,
        );
        None
    }

    pub async fn launch(
        &self,
        svc: &Arc<ConnectionService>,
        kinds: &[&Kind],
    ) -> anyhow::Result<()> {
        for kind in kinds {
            match self.start_pod(svc, kind).await {
                Ok(launch_id) => {
                    let learner = svc.clone();
                    tokio::spawn(async move { learn_profile(&learner, launch_id).await });
                }
                Err(error) => {
                    tracing::error!(connection = %svc.conn.name, pool = %kind.label, "starting worker pod: {error:#}")
                }
            }
        }

        Ok(())
    }

    pub async fn progress(&self, launch: &Launch) -> anyhow::Result<Progress> {
        let pod = self.client.pod(&launch.external_id).await?;
        let progress = progress_of(pod.as_ref());
        if progress == Progress::Waiting
            && OffsetDateTime::now_utc() - launch.dispatched_at > PENDING_LIMIT
        {
            tracing::warn!(pod = %launch.external_id, "worker pod is still pending after {PENDING_LIMIT}");
            return Ok(Progress::Ended);
        }

        Ok(progress)
    }

    pub async fn remove(&self, launch: &Launch) -> anyhow::Result<()> {
        self.client.delete_pod(&launch.external_id).await
    }

    async fn start_pod(&self, svc: &ConnectionService, kind: &Kind) -> anyhow::Result<Uuid> {
        let pool = self
            .config
            .pools
            .get(&kind.label)
            .with_context(|| format!("pool {} is not configured", kind.label))?;
        let launch_id = Uuid::new_v4();
        let launch = Launch {
            id: launch_id,
            worker_id: Some(Uuid::new_v4()),
            ..Launch::new(&svc.conn.name, &pod_name(launch_id), &kind.label)
        };
        svc.state.insert_launch(&launch).await?;
        if let Err(error) = self.create_pod(svc, &launch, pool).await {
            if let Err(cleanup) = finish_launch(svc, &launch).await {
                tracing::warn!(connection = %svc.conn.name, pod = %launch.external_id, "cleaning up the failed launch: {cleanup:#}");
            }

            return Err(error);
        }

        tracing::info!(connection = %svc.conn.name, pod = %launch.external_id, pool = %kind.label, "started worker pod");
        Ok(launch_id)
    }

    async fn create_pod(
        &self,
        svc: &ConnectionService,
        launch: &Launch,
        pool: &Pool,
    ) -> anyhow::Result<()> {
        let worker_id = launch.worker_id.unwrap_or_default();
        let display_name = format!(
            "k8s-{}-{}",
            launch.label,
            &launch.id.simple().to_string()[..8]
        );
        let registered = svc
            .gradient
            .register_team_worker_with_capabilities(
                &svc.conn.team,
                worker_id,
                &display_name,
                svc.conn.worker_capabilities,
            )
            .await?;
        let server_url = worker_server_url(&svc.conn.gradient_url)?;
        let manifest = WorkerPod {
            capabilities: svc.conn.worker_capabilities,
            connection: &svc.conn.name,
            launch_id: launch.id,
            pool_name: &launch.label,
            pool,
            cluster: &self.config,
            server_url: &server_url,
            worker_id,
            idle_timeout: svc.conn.idle_timeout,
            max_lifetime: svc.conn.max_lifetime,
            nix_config: nix_config(
                &svc.conn.worker_substituters,
                &svc.conn.worker_trusted_public_keys,
            ),
        }
        .manifest();
        let pod = self.client.create_pod(&manifest).await?;
        let secret = peers_secret(&pod, registered.team, &registered.token);
        self.client.create_secret(&secret).await
    }

    async fn remove_leftover_pods(&self, svc: &ConnectionService) -> anyhow::Result<()> {
        let open: HashSet<String> = svc
            .state
            .open_launches(&svc.conn.name)
            .await?
            .into_iter()
            .map(|launch| launch.id.to_string())
            .collect();
        let selector = format!("{CONNECTION_LABEL}={}", svc.conn.name);
        for pod in self.client.pods(&selector).await? {
            let launch = pod.metadata.labels.get(LAUNCH_LABEL);
            if launch.is_none_or(|launch| !open.contains(launch)) {
                self.client.delete_pod(&pod.metadata.name).await?;
                tracing::info!(connection = %svc.conn.name, pod = %pod.metadata.name, "deleted leftover worker pod");
            }
        }

        Ok(())
    }
}
