//! `berth` — the Berth server binary.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Path to `berth.toml` (default: `./berth.toml` if it exists).
        #[arg(long, env = "BERTH_CONFIG")]
        config: Option<PathBuf>,
    },
    /// Print the `OpenAPI` specification as JSON.
    Openapi,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { config } => serve(config).await,
        Command::Openapi => {
            println!("{}", berth_server::openapi().to_pretty_json()?);
            Ok(())
        }
    }
}

async fn serve(config: Option<PathBuf>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let state = berth_server::bootstrap(config.as_deref()).await?;
    let addr = SocketAddr::new(state.config.host, state.config.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, server_id = %state.server_id, "listening");
    berth_server::cleanup::spawn(state.db.clone());

    axum::serve(listener, berth_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
