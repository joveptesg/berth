use axum::{Json, extract::State};
use serde::Serialize;
use utoipa::ToSchema;

use crate::AppState;

const MIN_MOBILE_VERSION: &str = "0.1.0";

/// Public server description used for discovery before login.
#[derive(Serialize, ToSchema)]
pub(crate) struct ServerInfo {
    /// Server name set by the admin.
    name: String,
    /// Stable server identity: a client switching between LAN and WAN addresses
    /// accepts only the address that reports the same id.
    server_id: String,
    /// Server version.
    version: String,
    /// Login methods available to clients.
    auth_modes: Vec<AuthMode>,
    /// Oldest client versions the server supports.
    min_client_version: MinClientVersion,
}

/// Client login method.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    /// QR code or pairing code issued by the admin.
    Pairing,
}

/// Oldest supported versions per client type.
#[derive(Serialize, ToSchema)]
pub(crate) struct MinClientVersion {
    /// Mobile app.
    mobile: String,
}

/// Server info (no authentication).
#[utoipa::path(
    get,
    path = "/v1/server-info",
    tag = "server",
    security(()),
    responses((status = OK, description = "Server info", body = ServerInfo))
)]
pub(crate) async fn server_info(State(state): State<AppState>) -> Json<ServerInfo> {
    Json(ServerInfo {
        name: state.config.load().name.clone(),
        server_id: state.server_id.to_string(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        auth_modes: vec![AuthMode::Pairing],
        min_client_version: MinClientVersion {
            mobile: MIN_MOBILE_VERSION.to_owned(),
        },
    })
}
