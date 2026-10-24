//! `berth` — the Berth server binary.

use std::{
    io::{BufRead, IsTerminal},
    net::SocketAddr,
    path::PathBuf,
};

use anyhow::Context;
use berth_server::service::{auth, setup, storage::ProfileInput};
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
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Print the `OpenAPI` specification as JSON.
    Openapi,
    /// Server administration from the console.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(clap::Args)]
struct ConfigArg {
    /// Path to `berth.toml` (default: `./berth.toml` if it exists).
    #[arg(long, env = "BERTH_CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Set up the server without the web wizard: create the tenant and the admin password.
    Bootstrap {
        #[command(flatten)]
        config: ConfigArg,
        /// Server name.
        #[arg(long, default_value = "Berth")]
        name: String,
    },
    /// Set a new admin password and sign out all admin browsers.
    ResetPassword {
        #[command(flatten)]
        config: ConfigArg,
    },
    /// Replace the database with a backup (a downloaded one or one of `<data_dir>/backups`).
    /// The current database is saved to the automatic backups first. Stop the server before.
    Restore {
        #[command(flatten)]
        config: ConfigArg,
        /// Backup file (`.sqlite`).
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().command {
        Command::Serve { config } => serve(config.config).await,
        Command::Openapi => {
            println!("{}", berth_server::openapi().to_pretty_json()?);
            Ok(())
        }
        Command::Admin { command } => admin(command).await,
    }
}

async fn serve(config: Option<PathBuf>) -> anyhow::Result<()> {
    let state = berth_server::bootstrap(config.as_deref()).await?;
    let addr = {
        let config = state.config.load();
        SocketAddr::new(config.host, config.port)
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!(%addr, server_id = %state.server_id, "listening");
    if let (Some(code), Some(url)) = (state.setup.get(), state.setup_url()) {
        let (first, second) = code.split_at(code.len() / 2);
        tracing::warn!(
            "server is not set up: open this server's address in a browser and enter the setup \
             code {first}-{second} (or open {url}, or run `berth admin bootstrap`)"
        );
    }
    berth_server::cleanup::spawn(state.db.clone());
    berth_server::jobs::spawn(state.clone());

    // Peer addresses feed the rate limits of the login and setup forms.
    let app = berth_server::router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn admin(command: AdminCommand) -> anyhow::Result<()> {
    match command {
        AdminCommand::Bootstrap { config, name } => {
            let state = berth_server::bootstrap(config.config.as_deref()).await?;
            anyhow::ensure!(
                !setup::is_configured(&state.db).await?,
                "the server is already set up; use `berth admin reset-password` to change the password"
            );
            let (password, confirm) = read_new_password()?;
            setup::create_owner(
                &state,
                &name,
                &password,
                &confirm,
                &ProfileInput::local_default(),
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Server \"{name}\" is set up. Sign in at /login with the new password.");
        }
        AdminCommand::ResetPassword { config } => {
            let state = berth_server::bootstrap(config.config.as_deref()).await?;
            let (password, confirm) = read_new_password()?;
            auth::reset_password(&state.db, &password, &confirm)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Password changed; all admin browsers were signed out.");
        }
        AdminCommand::Restore { config, file } => {
            let state = berth_server::bootstrap(config.config.as_deref()).await?;
            let data_dir = state.config.load().data_dir.clone();
            berth_server::backup::restore(&state.db, &data_dir, &file)
                .await
                .map_err(|code| anyhow::anyhow!("cannot restore {}: {code}", file.display()))?;
            println!(
                "Restored from {}. The previous database is in {}.",
                file.display(),
                data_dir.join(berth_server::backup::DIR).display()
            );
        }
    }
    Ok(())
}

/// Asks twice on a terminal; otherwise reads one line from stdin (for scripts).
fn read_new_password() -> anyhow::Result<(String, String)> {
    if std::io::stdin().is_terminal() {
        let password = rpassword::prompt_password(format!(
            "New admin password (at least {} characters): ",
            auth::MIN_PASSWORD_LEN
        ))?;
        let confirm = rpassword::prompt_password("Repeat the password: ")?;
        Ok((password, confirm))
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let password = line.trim_end_matches(['\r', '\n']).to_owned();
        Ok((password.clone(), password))
    }
}
