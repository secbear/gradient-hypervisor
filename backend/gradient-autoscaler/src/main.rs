use anyhow::Context;
use clap::Parser;
use gradient_autoscaler::catch_up::run_connection;
use gradient_autoscaler::config::{Config, Connection, LauncherConfig};
use gradient_autoscaler::github::GitHubLauncher;
use gradient_autoscaler::github::client::GitHubClient;
use gradient_autoscaler::github::oidc::OidcVerifier;
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::http::{App, router};
use gradient_autoscaler::kubernetes::KubernetesLauncher;
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::launcher::Launcher;
use gradient_autoscaler::service::ConnectionService;
use gradient_autoscaler::state::State;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, env = "GRADIENT_AUTOSCALER_CONFIG")]
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
    tracing::info!(listen = %config.listen, connections = config.connections.len(), "gradient-autoscaler started");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

fn service(conn: Connection, public_url: &Url, state: State) -> anyhow::Result<ConnectionService> {
    let gradient = GradientClient::new(conn.gradient_url.clone(), read_secret(&conn.api_key_file)?);
    let launcher = match &conn.launcher {
        LauncherConfig::GitHubActions(github) => {
            let token = read_secret(&github.token_file)?;
            let client = GitHubClient::new(token, github.repository.clone());
            Launcher::GitHub(GitHubLauncher::new(github.clone(), client))
        }
        LauncherConfig::Kubernetes(kubernetes) => {
            let client = KubernetesClient::new(kubernetes)
                .with_context(|| format!("connection {}: Kubernetes API", conn.name))?;
            Launcher::Kubernetes(KubernetesLauncher::new(kubernetes.clone(), client))
        }
    };
    Ok(ConnectionService::new(
        conn,
        public_url.clone(),
        gradient,
        launcher,
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
