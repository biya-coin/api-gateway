use std::path::PathBuf;

use anyhow::{Context, Result};
use api_gateway::{config::Config, server};
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "config/default.toml")]
    config: PathBuf,

    /// Validate configuration and exit without opening a listening socket.
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    let filter = EnvFilter::try_new(&config.logging.filter).context("invalid logging.filter")?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if args.check_config {
        tracing::info!(config = %args.config.display(), "configuration is valid");
        return Ok(());
    }

    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to register SIGTERM handler")?;

    let shutdown = async move {
        let interrupt = async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                tracing::error!(%error, "failed to receive interrupt signal");
            }
        };
        #[cfg(unix)]
        tokio::select! {
            () = interrupt => {},
            _ = terminate.recv() => {},
        }
        #[cfg(not(unix))]
        interrupt.await;
        tracing::info!("shutdown requested; draining active connections");
    };

    let listener = TcpListener::bind(config.server.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", config.server.listen_addr))?;
    tracing::info!(address = %listener.local_addr()?, "api-gateway started");
    server::serve(listener, config, shutdown)
        .await
        .context("HTTP server failed")?;
    tracing::info!("api-gateway stopped");
    Ok(())
}
