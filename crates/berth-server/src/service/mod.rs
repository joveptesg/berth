//! Service layer shared by the JSON API, the web admin and the CLI.
//!
//! Validation failures are `BerthError::Validation` with a stable `snake_case` code
//! (e.g. `password_too_short`); the admin UI translates it as `errors.<code>`.

pub mod auth;
pub mod blobs;
pub mod books;
pub mod categories;
pub mod delivery;
pub mod demo;
pub mod devices;
pub mod ingest;
pub mod languages;
pub mod pairing;
pub mod progress;
pub mod r2;
pub mod settings;
pub mod setup;
pub mod storage;
pub mod tokens;
pub mod users;
pub mod web_login;

use base64::Engine;
use sha2::{Digest, Sha256};

/// Random 256-bit token, base64url without padding.
pub(crate) fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Stored form of a bearer token: hex sha256.
pub(crate) fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}
