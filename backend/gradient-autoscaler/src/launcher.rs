use crate::build_planner::{BuildScalingPolicy, PlannedBuildWorker};
use crate::capacity::BuildCapacityProfile;
use crate::github::GitHubLauncher;
use crate::kinds::Kind;
use crate::kubernetes::KubernetesLauncher;
use crate::service::ConnectionService;
use crate::state::Launch;
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

pub enum Launcher {
    GitHub(GitHubLauncher),
    Kubernetes(KubernetesLauncher),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Waiting,
    Running,
    Ended,
}

impl Launcher {
    pub async fn prepare_build_launch(
        &self,
        svc: &ConnectionService,
        planned: &PlannedBuildWorker,
        profile: &BuildCapacityProfile,
        policy: &BuildScalingPolicy,
        now: OffsetDateTime,
    ) -> anyhow::Result<Launch> {
        svc.ensure_mutations_allowed()?;
        match self {
            Self::Kubernetes(kubernetes) => {
                kubernetes
                    .prepare_build_launch(svc, planned, profile, policy, now)
                    .await
            }
            Self::GitHub(_) => {
                anyhow::bail!("prepared build launches are not supported for GitHub Actions")
            }
        }
    }

    pub async fn launch_prepared(
        &self,
        svc: &ConnectionService,
        launch_id: Uuid,
    ) -> anyhow::Result<()> {
        svc.ensure_mutations_allowed()?;
        match self {
            Self::Kubernetes(kubernetes) => kubernetes.launch_prepared(svc, launch_id).await,
            Self::GitHub(_) => {
                anyhow::bail!("prepared build launches are not supported for GitHub Actions")
            }
        }
    }

    pub async fn reconcile_prepared(
        &self,
        svc: &ConnectionService,
        launch_id: Uuid,
    ) -> anyhow::Result<()> {
        svc.ensure_mutations_allowed()?;
        match self {
            Self::Kubernetes(kubernetes) => kubernetes.reconcile_prepared(svc, launch_id).await,
            Self::GitHub(_) => {
                anyhow::bail!("prepared build launches are not supported for GitHub Actions")
            }
        }
    }

    pub fn github(&self) -> Option<&GitHubLauncher> {
        match self {
            Launcher::GitHub(github) => Some(github),
            Launcher::Kubernetes(_) => None,
        }
    }

    pub fn initial_kinds(&self) -> Vec<Kind> {
        match self {
            Launcher::GitHub(github) => github.initial_kinds(),
            Launcher::Kubernetes(kubernetes) => kubernetes.kinds(),
        }
    }

    pub fn allowed_kinds(&self) -> Vec<String> {
        match self {
            Launcher::GitHub(github) => github.allowed_kinds(),
            Launcher::Kubernetes(kubernetes) => kubernetes.allowed_kinds(),
        }
    }

    pub async fn setup(&self, svc: &ConnectionService) -> Option<Vec<Kind>> {
        if svc.conn.is_preview() {
            return None;
        }
        match self {
            Launcher::GitHub(github) => github.setup(svc).await,
            Launcher::Kubernetes(kubernetes) => kubernetes.setup(svc).await,
        }
    }

    pub async fn launch(
        &self,
        svc: &Arc<ConnectionService>,
        kinds: &[&Kind],
    ) -> anyhow::Result<()> {
        svc.ensure_mutations_allowed()?;
        match self {
            Launcher::GitHub(github) => github.launch(svc, kinds).await,
            Launcher::Kubernetes(kubernetes) => kubernetes.launch(svc, kinds).await,
        }
    }

    pub async fn progress(&self, launch: &Launch) -> anyhow::Result<Progress> {
        match self {
            Launcher::GitHub(github) => github.progress(launch).await,
            Launcher::Kubernetes(kubernetes) => kubernetes.progress(launch).await,
        }
    }

    pub async fn cancel(&self, launch: &Launch) -> anyhow::Result<()> {
        match self {
            Launcher::GitHub(github) => github.cancel(launch).await,
            Launcher::Kubernetes(kubernetes) => kubernetes.remove(launch).await,
        }
    }

    pub async fn clean_up(&self, launch: &Launch) -> anyhow::Result<()> {
        match self {
            Launcher::GitHub(_) => Ok(()),
            Launcher::Kubernetes(kubernetes) => kubernetes.remove(launch).await,
        }
    }
}
