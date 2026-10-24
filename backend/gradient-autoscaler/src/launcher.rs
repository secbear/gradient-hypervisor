use crate::github::GitHubLauncher;
use crate::kinds::Kind;
use crate::kubernetes::KubernetesLauncher;
use crate::service::ConnectionService;
use crate::state::Launch;
use std::sync::Arc;

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
