use crate::service::ConnectionService;
use crate::state::SecretKind;

const HOOK_NAME: &str = "gradient-autoscaler";
const GRADIENT_EVENTS: [&str; 1] = ["worker.demand"];

pub async fn setup(svc: &ConnectionService) {
    report(
        svc,
        "Gradient team webhook",
        "the Admin role in the team",
        gradient_hook(svc).await,
    );
    if let Some(kinds) = svc.launcher.setup(svc).await {
        svc.set_kinds(kinds);
    }
}

pub fn report(svc: &ConnectionService, step: &str, needs: &str, result: anyhow::Result<()>) {
    if let Err(error) = result {
        tracing::error!(
            connection = %svc.conn.name,
            "setting up the {step} failed; it needs {needs}: {error:#}"
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
