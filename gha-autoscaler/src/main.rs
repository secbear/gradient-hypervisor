use clap::Parser;
use gradient_gha_autoscaler::config::Config;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

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
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    tracing::info!(
        connections = config.connections.len(),
        "configuration loaded"
    );
    Ok(())
}
