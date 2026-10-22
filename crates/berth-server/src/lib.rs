//! Berth HTTP server: JSON API v1 and the web admin on axum.

mod admin;
pub mod api;
pub mod cleanup;
pub mod config;
mod db;
pub mod error;
mod secret;
pub mod service;

use std::{path::Path, sync::Arc};

use anyhow::Context;
use arc_swap::ArcSwap;
use sqlx::SqlitePool;

pub use api::{openapi, router};
pub use config::Config;
pub use secret::SecretKey;
use service::setup::SetupToken;

rust_i18n::i18n!("locales", fallback = "en");

/// Shared state of a running server.
#[derive(Clone)]
pub struct AppState {
    /// Effective configuration; replaced when settings are saved in the admin.
    pub config: Arc<ArcSwap<Config>>,
    /// Explicit `berth.toml`, kept to rebuild the configuration layers.
    config_file: Option<Arc<Path>>,
    /// Database pool.
    pub db: SqlitePool,
    /// Stable server identity; clients use it to match LAN and WAN addresses.
    pub server_id: Arc<str>,
    /// Key that encrypts stored secrets and signs tokens.
    pub secret: Arc<SecretKey>,
    /// One-time `/setup` token while the server has no tenant.
    pub setup: Arc<SetupToken>,
    templates: Arc<minijinja::Environment<'static>>,
}

impl AppState {
    /// Setup link to print on start while the server is not set up.
    #[must_use]
    pub fn setup_url(&self) -> Option<String> {
        let token = self.setup.get()?;
        let config = self.config.load();
        let host = if config.host.is_unspecified() {
            "localhost".to_owned()
        } else {
            config.host.to_string()
        };
        Some(format!("http://{host}:{}/setup?token={token}", config.port))
    }
}

/// Loads the configuration and prepares the data directory: creates the database,
/// applies migrations, creates `secret.key` and the server id on first start.
///
/// `config_file` is an explicit `berth.toml`; without it `./berth.toml` is used if present.
///
/// # Errors
/// Fails on invalid configuration, I/O or database errors.
pub async fn bootstrap(config_file: Option<&Path>) -> anyhow::Result<AppState> {
    if let Some(path) = config_file {
        anyhow::ensure!(path.is_file(), "config file {} not found", path.display());
    }
    let base: Config = config::layers(config_file, &serde_json::Map::new())
        .extract()
        .context("invalid configuration")?;
    let data_dir = base.data_dir;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;

    let db = db::connect(&data_dir.join(db::FILE_NAME)).await?;
    let secret = SecretKey::load_or_create(&data_dir)?;
    let server_id = db::server_id(&db).await?;

    let mut config: Config = config::layers(config_file, &db::server_settings(&db).await?)
        .extract()
        .context("invalid configuration")?;
    // The database lives in data_dir, so the database layer cannot move it.
    config.data_dir = data_dir;
    let setup = SetupToken::new(service::setup::is_configured(&db).await?)?;

    Ok(AppState {
        config: Arc::new(ArcSwap::from_pointee(config)),
        config_file: config_file.map(Arc::from),
        db,
        server_id: server_id.into(),
        secret: Arc::new(secret),
        setup: Arc::new(setup),
        templates: Arc::new(admin::templates::environment()),
    })
}
