use crate::github::GitHubLauncher;
use crate::github::client::RepositoryInfo;
use crate::github::kinds::{larger_kind, self_hosted_kind, standard_kinds};
use crate::github::workflow::{WORKFLOW_PATH, WorkflowAction, decide, render};
use crate::kinds::Kind;
use crate::service::ConnectionService;
use crate::setup::report;
use crate::state::SecretKind;
use rand::Rng;

const GITHUB_EVENTS: [&str; 2] = ["workflow_run", "workflow_job"];

pub async fn setup(svc: &ConnectionService, github: &GitHubLauncher) -> Option<Vec<Kind>> {
    report(
        svc,
        "GitHub repository webhook",
        "Webhooks write",
        github_hook(svc, github).await,
    );
    report(
        svc,
        "workflow file",
        "Contents and Workflows write",
        workflow_file(svc, github).await,
    );
    match github.client.repository_info().await {
        Ok(repo) => {
            if repo.public {
                tracing::warn!(
                    connection = %svc.conn.name,
                    repository = %github.config.repository,
                    "the repository is public: anyone can read the worker logs, including names of private projects"
                );
            }

            Some(discover_kinds(svc, github, &repo).await)
        }
        Err(error) => {
            report(svc, "repository details", "Metadata read", Err(error));
            None
        }
    }
}

async fn github_hook(svc: &ConnectionService, github: &GitHubLauncher) -> anyhow::Result<()> {
    let url = svc.endpoint("github");
    let hooks = github.client.hooks().await?;
    let existing = hooks
        .iter()
        .find(|hook| hook.config.url.as_deref() == Some(url.as_str()));
    let stored = svc
        .state
        .secret(&svc.conn.name, SecretKind::GitHubHook)
        .await?;
    let secret = match (existing, stored) {
        (Some(_), Some(_)) => return Ok(()),
        (Some(hook), None) => {
            let secret = new_secret();
            github
                .client
                .update_hook(hook.id, &url, &secret, &GITHUB_EVENTS)
                .await?;
            secret
        }
        (None, stored) => {
            let secret = stored.unwrap_or_else(new_secret);
            github
                .client
                .create_hook(&url, &secret, &GITHUB_EVENTS)
                .await?;
            secret
        }
    };
    svc.state
        .put_secret(&svc.conn.name, SecretKind::GitHubHook, &secret)
        .await?;
    tracing::info!(connection = %svc.conn.name, "GitHub repository webhook ready");
    Ok(())
}

async fn workflow_file(svc: &ConnectionService, github: &GitHubLauncher) -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let existing = github.client.file(WORKFLOW_PATH).await?;
    let content = existing.as_ref().map(|(content, _)| content.as_str());
    let sha = existing.as_ref().map(|(_, sha)| sha.as_str());
    match decide(content, version) {
        WorkflowAction::Create => {
            github
                .client
                .put_file(
                    WORKFLOW_PATH,
                    &render(version),
                    None,
                    "add gradient worker workflow",
                )
                .await?
        }
        WorkflowAction::Update => {
            github
                .client
                .put_file(
                    WORKFLOW_PATH,
                    &render(version),
                    sha,
                    "update gradient worker workflow",
                )
                .await?
        }
        WorkflowAction::Keep => {}
        WorkflowAction::UserOwned => tracing::warn!(
            connection = %svc.conn.name,
            "{WORKFLOW_PATH} lost its managed marker line, so it stays as the user edited it"
        ),
    }

    Ok(())
}

async fn discover_kinds(
    svc: &ConnectionService,
    github: &GitHubLauncher,
    repo: &RepositoryInfo,
) -> Vec<Kind> {
    let mut kinds = standard_kinds();
    if repo.owner_is_organization {
        match github.client.hosted_runners(github.config.owner()).await {
            Ok(runners) => kinds.extend(runners.iter().filter_map(larger_kind)),
            Err(error) => tracing::info!(
                connection = %svc.conn.name,
                "skipping larger runners; the token needs organization hosted runners read: {error:#}"
            ),
        }
    }

    match github.client.self_hosted_runners().await {
        Ok(runners) => kinds.extend(runners.iter().filter_map(self_hosted_kind)),
        Err(error) => {
            tracing::info!(connection = %svc.conn.name, "skipping self-hosted runners: {error:#}")
        }
    }

    let mut seen = std::collections::HashSet::new();
    kinds.retain(|kind| seen.insert(kind.label.clone()));
    let idle: Vec<&str> = kinds
        .iter()
        .filter(|kind| !github.config.allowed_kinds.contains(&kind.label))
        .map(|kind| kind.label.as_str())
        .collect();
    if !idle.is_empty() {
        tracing::info!(connection = %svc.conn.name, ?idle, "runner kinds detected but not in allowedKinds");
    }

    kinds
}

fn new_secret() -> String {
    hex::encode(rand::rng().random::<[u8; 32]>())
}
