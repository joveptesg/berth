//! `/static/*`: embedded admin assets. Pages link them with `?v=<version>`, so release builds
//! cache them forever.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/static/"]
struct StaticAssets;

/// Short hash of all assets: pages link `?v=<fingerprint>`, so a release with changed CSS or
/// scripts is fetched again even when the version number stays the same.
pub(crate) fn fingerprint() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut names: Vec<_> = StaticAssets::iter().collect();
    names.sort();
    for name in names {
        if let Some(file) = StaticAssets::get(&name) {
            hasher.update(name.as_bytes());
            hasher.update(file.metadata.sha256_hash());
        }
    }
    hex::encode(&hasher.finalize()[..6])
}

pub(crate) async fn serve(Path(path): Path<String>) -> Response {
    let Some(file) = StaticAssets::get(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    let cache = if cfg!(debug_assertions) {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
        ],
        file.data,
    )
        .into_response()
}
