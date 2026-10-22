//! Server settings editable from the admin UI.

use std::sync::Arc;

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::{AppState, Config, config, db};

const MAX_NAME_LEN: usize = 64;

/// Raw form input; empty addresses clear the setting.
pub struct ServerSettingsInput<'a> {
    /// Server name.
    pub name: &'a str,
    /// Home network address.
    pub lan_url: &'a str,
    /// Internet address.
    pub wan_url: &'a str,
}

/// Settings fixed by `berth.toml` or `BERTH_*` env: shown read-only, never written.
#[derive(Debug, Default, Serialize)]
pub struct Locked {
    /// `name` is set outside the database.
    pub name: bool,
    /// `lan_url` is set outside the database.
    pub lan_url: bool,
    /// `wan_url` is set outside the database.
    pub wan_url: bool,
}

/// Which settings are fixed by file or env.
#[must_use]
pub fn locked(state: &AppState) -> Locked {
    let external = config::external(state.config_file.as_deref());
    Locked {
        name: external.contains("name"),
        lan_url: external.contains("lan_url"),
        wan_url: external.contains("wan_url"),
    }
}

/// Validates and saves the settings, then applies them to the running server.
///
/// # Errors
/// `Validation` with `name_required`, `name_too_long`, `lan_url_invalid` or `wan_url_invalid`.
pub async fn update(state: &AppState, input: &ServerSettingsInput<'_>) -> Result<()> {
    let name = validate_name(input.name)?;
    let lan_url = parse_url(input.lan_url, "lan_url_invalid")?;
    let wan_url = parse_url(input.wan_url, "wan_url_invalid")?;
    let locked = locked(state);

    let mut tx = state.db.begin().await.context("begin")?;
    let values = [
        ("name", locked.name, Some(Value::String(name))),
        (
            "lan_url",
            locked.lan_url,
            lan_url.map(|u| Value::String(u.into())),
        ),
        (
            "wan_url",
            locked.wan_url,
            wan_url.map(|u| Value::String(u.into())),
        ),
    ];
    for (key, is_locked, value) in values {
        if !is_locked {
            db::put_server_setting(&mut tx, key, value.as_ref())
                .await
                .with_context(|| format!("save setting {key}"))?;
        }
    }
    tx.commit().await.context("commit settings")?;
    reload(state).await?;
    Ok(())
}

/// Rebuilds the configuration from all layers and swaps it in.
pub(crate) async fn reload(state: &AppState) -> anyhow::Result<()> {
    let mut next: Config = config::layers(
        state.config_file.as_deref(),
        &db::server_settings(&state.db).await?,
    )
    .extract()
    .context("invalid configuration")?;
    // data_dir and the listen address cannot change while running.
    let current = state.config.load();
    next.data_dir.clone_from(&current.data_dir);
    next.host = current.host;
    next.port = current.port;
    state.config.store(Arc::new(next));
    Ok(())
}

/// An address as people type it: without the trailing slash of an empty path.
#[must_use]
pub fn display_url(url: &Url) -> String {
    url.as_str()
        .strip_suffix('/')
        .filter(|_| url.path() == "/")
        .unwrap_or(url.as_str())
        .to_owned()
}

/// Server addresses for pairing QR codes: home network first, then internet.
#[must_use]
pub fn server_urls(config: &Config) -> Vec<String> {
    [&config.lan_url, &config.wan_url]
        .into_iter()
        .flatten()
        .map(display_url)
        .collect()
}

pub(crate) fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("name_too_long".into()));
    }
    Ok(name.to_owned())
}

fn parse_url(raw: &str, code: &str) -> Result<Option<Url>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    match Url::parse(raw) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => Ok(Some(url)),
        _ => Err(BerthError::Validation(code.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_addresses() {
        assert_eq!(parse_url("  ", "x").unwrap(), None);
        assert_eq!(
            parse_url("http://192.168.0.4:8080", "x")
                .unwrap()
                .unwrap()
                .as_str(),
            "http://192.168.0.4:8080/"
        );
        for bad in ["192.168.0.4", "ftp://host", "http://"] {
            assert!(parse_url(bad, "x").is_err(), "{bad}");
        }
        assert!(validate_name(" ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert_eq!(validate_name(" Home ").unwrap(), "Home");
    }
}
