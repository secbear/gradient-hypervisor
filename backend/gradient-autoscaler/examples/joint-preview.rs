use anyhow::{Context, ensure};
use clap::Parser;
use gradient_autoscaler::config::{BuildScalingMode, Config, LauncherConfig};
use gradient_autoscaler::gradient::GradientClient;
use gradient_autoscaler::joint_preview::preview_joint_capacity;
use gradient_autoscaler::kubernetes::KubernetesLauncher;
use gradient_autoscaler::kubernetes::client::KubernetesClient;
use gradient_autoscaler::launcher::Launcher;
use gradient_autoscaler::service::ConnectionService;
use gradient_autoscaler::state::State;
use reqwest::{Client, redirect::Policy};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::{Host, Url};

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    expect_build_workers: usize,
    #[arg(long)]
    expect_role_workers: usize,
    #[arg(long)]
    expect_deferred_capabilities: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Receipt {
    build_workers: usize,
    role_workers: usize,
    deferred_capabilities: usize,
}

fn is_fixture_url(url: &Url) -> bool {
    url.scheme() == "http"
        && match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        }
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
}

fn guard_fixture(config: &Config, directory: &Path) -> anyhow::Result<()> {
    ensure!(
        config.connections.len() == 1,
        "fixture requires exactly one connection"
    );
    let conn = &config.connections[0];
    ensure!(
        conn.name == "joint-preview"
            && conn.team == "platform"
            && is_fixture_url(&conn.gradient_url)
            && is_fixture_url(&config.public_url)
            && config.listen.ip().is_loopback(),
        "refusing a connection outside the literal loopback joint-preview fixture"
    );
    ensure!(
        conn.api_key_file == directory.join("gteam.key"),
        "fixture requires its local team key file"
    );
    let scaling = conn
        .build_scaling
        .as_ref()
        .context("fixture requires joint preview")?;
    ensure!(
        scaling.mode == BuildScalingMode::JointPreview,
        "fixture requires joint preview mode"
    );
    let LauncherConfig::Kubernetes(cluster) = &conn.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    ensure!(
        cluster.api_server.as_ref().map(Url::as_str) == Some("http://127.0.0.1:9/")
            && cluster.namespace == "joint-preview"
            && cluster.ca_file.is_none()
            && cluster.token_file == directory.join("absent-kubernetes-token"),
        "fixture requires its unreachable Kubernetes target and absent local token path"
    );
    ensure!(
        !cluster.token_file.try_exists()?,
        "fixture Kubernetes token must not exist"
    );
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = Arguments::parse();
    let path = args
        .config
        .canonicalize()
        .context("resolve fixture config")?;
    let config = Config::load(&path)?;
    guard_fixture(
        &config,
        path.parent().context("fixture config has no parent")?,
    )?;
    let conn = config
        .connections
        .into_iter()
        .next()
        .context("missing fixture connection")?;
    let key = std::fs::read_to_string(&conn.api_key_file)?;
    ensure!(
        key.trim() == format!("GTEAM{}", "c".repeat(64)),
        "fixture requires its fixed test team key"
    );
    let gradient = GradientClient::with_http_client(
        conn.gradient_url.clone(),
        key.trim().to_owned(),
        Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(10))
            .build()?,
    );
    let LauncherConfig::Kubernetes(cluster) = &conn.launcher else {
        anyhow::bail!("fixture requires Kubernetes");
    };
    let token_file = cluster.token_file.clone();
    let launcher = Launcher::Kubernetes(KubernetesLauncher::new(
        cluster.clone(),
        KubernetesClient::new(cluster)?,
    ));
    let svc = ConnectionService::new(
        conn,
        config.public_url,
        gradient,
        launcher,
        State::memory().await?,
    );
    ensure!(
        svc.ensure_mutations_allowed().is_err(),
        "fixture must prohibit worker mutations"
    );
    ensure!(
        svc.state.open_launches(&svc.conn.name).await?.is_empty(),
        "fixture launch ledger must start empty"
    );
    let preview = preview_joint_capacity(&svc).await?;
    ensure!(
        preview.plan.bootstrap_complete
            && !preview.plan.build.server_draining
            && !preview.plan.build.builds_truncated,
        "fixture must capture complete, non-draining capacity"
    );
    let receipt = Receipt {
        build_workers: preview.plan.build.proposed_workers.len(),
        role_workers: preview.plan.capability_workers.len(),
        deferred_capabilities: preview.plan.deferred_capabilities.len(),
    };
    ensure!(
        receipt
            == Receipt {
                build_workers: args.expect_build_workers,
                role_workers: args.expect_role_workers,
                deferred_capabilities: args.expect_deferred_capabilities,
            },
        "unexpected proposal counts: {receipt:?}"
    );
    ensure!(
        svc.ensure_mutations_allowed().is_err(),
        "preview enabled worker mutations"
    );
    ensure!(
        svc.state.open_launches(&svc.conn.name).await?.is_empty(),
        "preview changed the launch ledger"
    );
    ensure!(
        !token_file.try_exists()?,
        "preview created a Kubernetes token file"
    );
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_urls_require_literal_loopback_http_without_extra_components() {
        for (url, allowed) in [
            ("http://127.0.0.1:3000/", true),
            ("http://[::1]:3000/", true),
            ("https://127.0.0.1:3000/", false),
            ("http://localhost:3000/", false),
            ("http://10.0.0.1:3000/", false),
            ("http://192.168.1.1:3000/", false),
            ("http://169.254.169.254/", false),
            ("http://example.invalid/", false),
            ("http://user@127.0.0.1/", false),
            ("http://127.0.0.1/api/", false),
            ("http://127.0.0.1/?query", false),
            ("http://127.0.0.1/#fragment", false),
        ] {
            assert_eq!(is_fixture_url(&url.parse().unwrap()), allowed, "{url}");
        }
    }
}
