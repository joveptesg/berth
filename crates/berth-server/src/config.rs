//! Configuration layers: defaults → settings in the database → `berth.toml` → `BERTH_*` env.
//!
//! File and env win over the database so ops can fix a setting broken from the UI.

use std::{
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};
use url::Url;

const DEFAULT_CONFIG_FILE: &str = "berth.toml";

/// Server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directory with the database and `secret.key`.
    pub data_dir: PathBuf,
    /// Listen address.
    pub host: IpAddr,
    /// Listen port.
    pub port: u16,
    /// Server name shown to clients.
    pub name: String,
    /// Address of the server in the home network, put into pairing QR codes.
    pub lan_url: Option<Url>,
    /// Address of the server from the internet, put into pairing QR codes.
    pub wan_url: Option<Url>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 8080,
            name: "Berth".to_owned(),
            lan_url: None,
            wan_url: None,
        }
    }
}

/// Builds the layered configuration; `db` holds server-scope settings from the database.
pub(crate) fn layers(
    file: Option<&Path>,
    db: &serde_json::Map<String, serde_json::Value>,
) -> Figment {
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Serialized::defaults(db))
        .merge(external(file))
}

/// Layers that override the database: `berth.toml` and env. Keys set here are read-only in the UI.
pub(crate) fn external(file: Option<&Path>) -> Figment {
    let path = file.unwrap_or(Path::new(DEFAULT_CONFIG_FILE));
    let mut figment = Figment::new();
    if path.is_file() {
        figment = figment.merge(Toml::file_exact(path));
    }
    figment.merge(Env::prefixed("BERTH_").ignore(&["config", "secret_key"]))
}

#[cfg(test)]
mod tests {
    use figment::Jail;
    use serde_json::json;

    use super::*;

    #[test]
    #[expect(
        clippy::result_large_err,
        reason = "Jail requires figment::Result in the closure"
    )]
    fn later_layers_win() {
        Jail::expect_with(|jail| {
            let db = json!({ "name": "from-db", "port": 7000, "host": "127.0.0.1" })
                .as_object()
                .unwrap()
                .clone();

            let config: Config = layers(None, &db).extract()?;
            assert_eq!(config.name, "from-db");
            assert_eq!(config.port, 7000);

            jail.create_file("berth.toml", "name = 'from-file'\nport = 9000")?;
            let config: Config = layers(None, &db).extract()?;
            assert_eq!(config.name, "from-file");
            assert_eq!(config.port, 9000);
            assert_eq!(config.host.to_string(), "127.0.0.1");

            jail.set_env("BERTH_PORT", "9100");
            let config: Config = layers(None, &db).extract()?;
            assert_eq!(config.port, 9100);
            assert_eq!(config.name, "from-file");
            assert_eq!(config.data_dir, PathBuf::from("data"));

            assert!(external(None).contains("name"));
            assert!(!external(None).contains("lan_url"));
            Ok(())
        });
    }
}
