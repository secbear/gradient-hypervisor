use crate::github::RepositoryInfo;
use crate::kinds::{Kind, larger_kind, self_hosted_kind, standard_kinds};
use crate::service::ConnectionService;
use crate::state::SecretKind;
use crate::workflow::{WORKFLOW_PATH, WorkflowAction, decide, render};
use rand::Rng;

const HOOK_NAME: &str = "gradient-gha-autoscaler";
const GRADIENT_EVENTS: [&str; 1] = ["worker.demand"];
const GITHUB_EVENTS: [&str; 2] = ["workflow_run", "workflow_job"];

pub async fn setup(svc: &ConnectionService) {
    report(
        svc,
        "Gradient team webhook",
        "the Admin role in the team",
        gradient_hook(svc).await,
    );
    report(
        svc,
        "GitHub repository webhook",
        "Webhooks write",
        github_hook(svc).await,
    );
    report(
        svc,
        "workflow file",
        "Contents and Workflows write",
        workflow_file(svc).await,
    );
    match svc.github.repository_info().await {
        Ok(repo) => {
            if repo.public {
                tracing::warn!(
                    connection = %svc.conn.name,
                    repository = %svc.conn.repository,
                    "the repository is public: anyone can read the worker logs, including names of private projects"
                );
            }
            svc.set_kinds(discover_kinds(svc, &repo).await);
        }
        Err(error) => report(svc, "repository details", "Metadata read", Err(error)),
    }
}

fn report(svc: &ConnectionService, step: &str, needs: &str, result: anyhow::Result<()>) {
    if let Err(error) = result {
        tracing::error!(
            connection = %svc.conn.name,
            "setting up the {step} failed; the token needs {needs}: {error:#}"
        );
    }
}

async fn gradient_hook(svc: &ConnectionService) -> anyhow::Result<()> {
    let (team, url) = (&svc.conn.team, svc.endpoint("gradient"));
    let hooks = svc.gradient.team_webhooks(team).await?;
    let secret = match hooks.iter().find(|hook| hook.url == url) {
        None => Some(
            svc.gradient
                .create_team_webhook(team, HOOK_NAME, &url, &GRADIENT_EVENTS)
                .await?,
        ),
        Some(hook)
            if svc
                .state
                .secret(&svc.conn.name, SecretKind::GradientHook)
                .await?
                .is_none() =>
        {
            Some(
                svc.gradient
                    .rotate_team_webhook_secret(team, hook.id)
                    .await?,
            )
        }
        Some(_) => None,
    };
    if let Some(secret) = secret {
        svc.state
            .put_secret(&svc.conn.name, SecretKind::GradientHook, &secret)
            .await?;
        tracing::info!(connection = %svc.conn.name, "Gradient team webhook ready");
    }
    Ok(())
}

async fn github_hook(svc: &ConnectionService) -> anyhow::Result<()> {
    let url = svc.endpoint("github");
    let hooks = svc.github.hooks().await?;
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
            svc.github
                .update_hook(hook.id, &url, &secret, &GITHUB_EVENTS)
                .await?;
            secret
        }
        (None, stored) => {
            let secret = stored.unwrap_or_else(new_secret);
            svc.github
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

async fn workflow_file(svc: &ConnectionService) -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    let existing = svc.github.file(WORKFLOW_PATH).await?;
    let content = existing.as_ref().map(|(content, _)| content.as_str());
    let sha = existing.as_ref().map(|(_, sha)| sha.as_str());
    match decide(content, version) {
        WorkflowAction::Create => {
            svc.github
                .put_file(
                    WORKFLOW_PATH,
                    &render(version),
                    None,
                    "add gradient worker workflow",
                )
                .await?
        }
        WorkflowAction::Update => {
            svc.github
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

async fn discover_kinds(svc: &ConnectionService, repo: &RepositoryInfo) -> Vec<Kind> {
    let mut kinds = standard_kinds();
    if repo.owner_is_organization {
        match svc.github.hosted_runners(svc.conn.owner()).await {
            Ok(runners) => kinds.extend(runners.iter().filter_map(larger_kind)),
            Err(error) => tracing::info!(
                connection = %svc.conn.name,
                "skipping larger runners; the token needs organization hosted runners read: {error:#}"
            ),
        }
    }
    match svc.github.self_hosted_runners().await {
        Ok(runners) => kinds.extend(runners.iter().filter_map(self_hosted_kind)),
        Err(error) => {
            tracing::info!(connection = %svc.conn.name, "skipping self-hosted runners: {error:#}")
        }
    }
    let mut seen = std::collections::HashSet::new();
    kinds.retain(|kind| seen.insert(kind.label.clone()));
    let idle: Vec<&str> = kinds
        .iter()
        .filter(|kind| !svc.conn.allowed_kinds.contains(&kind.label))
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
