use anyhow::Context;
use clap::Parser;
use gradient_gha_autoscaler::catch_up::run_connection;
use gradient_gha_autoscaler::config::{Config, Connection};
use gradient_gha_autoscaler::github::GitHubClient;
use gradient_gha_autoscaler::gradient::GradientClient;
use gradient_gha_autoscaler::http::{App, router};
use gradient_gha_autoscaler::oidc::OidcVerifier;
use gradient_gha_autoscaler::service::ConnectionService;
use gradient_gha_autoscaler::state::State;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "GRADIENT_GHA_AUTOSCALER_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::load(&Args::parse().config)?;
    std::fs::create_dir_all(&config.state_dir)
        .with_context(|| format!("creating state directory {}", config.state_dir.display()))?;
    let state = State::open(&config.state_dir.join("state.sqlite")).await?;

    let mut services = Vec::new();
    for conn in &config.connections {
        services.push(Arc::new(service(
            conn.clone(),
            &config.public_url,
            state.clone(),
        )?));
    }
    let interval = Duration::from_secs(config.catch_up_interval);
    for svc in &services {
        tokio::spawn(run_connection(svc.clone(), interval));
    }

    let app = Arc::new(App::new(services, OidcVerifier::github()));
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listening on {}", config.listen))?;
    tracing::info!(listen = %config.listen, connections = config.connections.len(), "gradient-gha-autoscaler started");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn service(conn: Connection, public_url: &Url, state: State) -> anyhow::Result<ConnectionService> {
    let api_key = read_secret(&conn.api_key_file)?;
    let github_token = read_secret(&conn.github_token_file)?;
    let gradient = GradientClient::new(conn.gradient_url.clone(), api_key);
    let github = GitHubClient::new(github_token, conn.repository.clone());
    Ok(ConnectionService::new(
        conn,
        public_url.clone(),
        gradient,
        github,
        state,
    ))
}

fn read_secret(path: &Path) -> anyhow::Result<String> {
    let secret =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(secret.trim().to_owned())
}

async fn shutdown() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}
