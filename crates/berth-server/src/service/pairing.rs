//! Phone pairing: the admin issues a one-time code (QR or typed), the app exchanges it for
//! device tokens (mvp-spec §5).
//!
//! The code is 8 characters without look-alikes (no 0/O, 1/I/L): about 40 bits. That is enough
//! for a token that lives two minutes behind a rate limit; only its hash is stored.

use anyhow::Context;
use berth_core::{AuthError, BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    token_hash,
    tokens::{self, TokenPair},
};
use crate::SecretKey;

/// Length of a pairing code.
pub const CODE_LEN: usize = 8;
/// Lifetime of a pairing code.
pub const TTL_MS: i64 = 120_000;
const ALPHABET: &[u8; 31] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const MAX_DEVICE_NAME: usize = 64;

/// A freshly issued pairing code.
#[derive(Debug, Clone)]
pub struct PairingCode {
    /// The code, shown as text and put into the QR.
    pub code: String,
    /// Stable id of the code for status checks: its hash.
    pub id: String,
    /// Expiry, epoch milliseconds.
    pub expires_at: i64,
}

/// What became of a pairing code.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairingStatus {
    /// Not used yet.
    Waiting {
        /// Expiry, epoch milliseconds.
        expires_at: i64,
    },
    /// A device paired with it.
    Connected {
        /// Name the device reported.
        device_name: String,
        /// Its platform.
        platform: String,
    },
    /// Expired unused.
    Expired,
}

/// A device that just paired.
#[derive(Debug, Clone)]
pub struct Paired {
    /// The new device.
    pub device_id: String,
    /// Its first tokens.
    pub tokens: TokenPair,
}

/// Upper-cases a typed code and drops everything but letters and digits, like the app does.
#[must_use]
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn generate_code() -> anyhow::Result<String> {
    // Rejection sampling keeps every character equally likely.
    let limit = u8::try_from(256 - 256 % ALPHABET.len()).expect("fits");
    let mut code = String::with_capacity(CODE_LEN);
    while code.len() < CODE_LEN {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
        for byte in bytes.into_iter().filter(|b| *b < limit) {
            if code.len() < CODE_LEN {
                code.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            }
        }
    }
    Ok(code)
}

/// Issues a pairing code for `user_id`.
///
/// # Errors
/// `NotFound` when the user does not exist in the tenant.
pub async fn create(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
    now_ms: i64,
) -> Result<PairingCode> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL)",
    )
    .bind(user_id)
    .bind(tenant_id)
    .fetch_one(db)
    .await
    .context("load user")?;
    if !exists {
        return Err(BerthError::NotFound);
    }
    let code = generate_code()?;
    let id = token_hash(&code);
    let expires_at = now_ms + TTL_MS;
    sqlx::query(
        "INSERT INTO pairing_tokens (token_hash, tenant_id, user_id, flow_type, expires_at)
         VALUES ($1, $2, $3, 'qr', $4)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(expires_at)
    .execute(db)
    .await
    .context("insert pairing token")?;
    Ok(PairingCode {
        code,
        id,
        expires_at,
    })
}

/// State of the code with `id` (its hash).
///
/// # Errors
/// `NotFound` for an unknown id.
pub async fn status(
    db: &SqlitePool,
    tenant_id: &str,
    id: &str,
    now_ms: i64,
) -> Result<PairingStatus> {
    let row: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT p.expires_at, d.name, d.platform FROM pairing_tokens p
         LEFT JOIN devices d ON d.id = p.device_id
         WHERE p.token_hash = $1 AND p.tenant_id = $2",
    )
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .context("load pairing token")?;
    Ok(match row.ok_or(BerthError::NotFound)? {
        (_, Some(device_name), Some(platform)) => PairingStatus::Connected {
            device_name,
            platform,
        },
        (expires_at, ..) if expires_at > now_ms => PairingStatus::Waiting { expires_at },
        _ => PairingStatus::Expired,
    })
}

fn validate_device(device_name: &str, platform: &str) -> Result<(String, String)> {
    let platform = platform.trim().to_ascii_lowercase();
    if platform.is_empty()
        || platform.len() > 16
        || !platform.bytes().all(|b| b.is_ascii_lowercase())
    {
        return Err(BerthError::Validation("platform_invalid".into()));
    }
    let name: String = device_name.trim().chars().take(MAX_DEVICE_NAME).collect();
    let name = if name.is_empty() {
        platform.clone()
    } else {
        name
    };
    Ok((name, platform))
}

/// Exchanges a pairing code for a new device with the `listen` scope and its first tokens.
///
/// # Errors
/// `Auth` with `PairingTokenInvalid`, `PairingTokenUsed` or `PairingTokenExpired`;
/// `Validation("platform_invalid")`.
pub async fn pair(
    db: &SqlitePool,
    secret: &SecretKey,
    code: &str,
    device_name: &str,
    platform: &str,
    now_ms: i64,
) -> Result<Paired> {
    let (device_name, platform) = validate_device(device_name, platform)?;
    let code = normalize(code);
    if code.len() != CODE_LEN {
        return Err(AuthError::PairingTokenInvalid.into());
    }
    let hash = token_hash(&code);

    // Write first: the conditional UPDATE lets exactly one of two concurrent exchanges win.
    let mut tx = db.begin().await.context("begin")?;
    let claimed: Option<(String, Option<String>)> = sqlx::query_as(
        "UPDATE pairing_tokens SET used_at = $2
         WHERE token_hash = $1 AND flow_type = 'qr' AND used_at IS NULL AND expires_at > $2
         RETURNING tenant_id, user_id",
    )
    .bind(&hash)
    .bind(now_ms)
    .fetch_optional(&mut *tx)
    .await
    .context("claim pairing token")?;
    let Some((tenant_id, Some(user_id))) = claimed else {
        tx.rollback().await.context("rollback")?;
        let row: Option<(i64, Option<i64>)> =
            sqlx::query_as("SELECT expires_at, used_at FROM pairing_tokens WHERE token_hash = $1")
                .bind(&hash)
                .fetch_optional(db)
                .await
                .context("load pairing token")?;
        return Err(match row {
            Some((_, Some(_))) => AuthError::PairingTokenUsed,
            Some((expires_at, None)) if expires_at <= now_ms => AuthError::PairingTokenExpired,
            _ => AuthError::PairingTokenInvalid,
        }
        .into());
    };

    let device_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO devices (id, tenant_id, user_id, name, platform, paired_at, last_seen_at)
         VALUES ($1, $2, $3, $4, $5, $6, $6)",
    )
    .bind(&device_id)
    .bind(&tenant_id)
    .bind(&user_id)
    .bind(&device_name)
    .bind(&platform)
    .bind(now_ms)
    .execute(&mut *tx)
    .await
    .context("insert device")?;
    sqlx::query(
        "INSERT INTO device_scopes (tenant_id, device_id, scope) VALUES ($1, $2, 'listen')",
    )
    .bind(&tenant_id)
    .bind(&device_id)
    .execute(&mut *tx)
    .await
    .context("insert scope")?;
    sqlx::query("UPDATE pairing_tokens SET device_id = $2 WHERE token_hash = $1")
        .bind(&hash)
        .bind(&device_id)
        .execute(&mut *tx)
        .await
        .context("link device")?;
    let tokens = tokens::issue(&mut tx, secret, &tenant_id, &user_id, &device_id, now_ms).await?;
    tx.commit().await.context("commit pairing")?;
    Ok(Paired { device_id, tokens })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_unambiguous_alphabet() {
        for _ in 0..200 {
            let code = generate_code().unwrap();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
            assert!(!code.contains(['0', 'O', '1', 'I', 'L']));
        }
        assert_eq!(normalize(" k7p2-9qxm "), "K7P29QXM");
    }

    #[test]
    fn validates_devices() {
        assert_eq!(
            validate_device("  Pixel 9 ", "Android").unwrap(),
            ("Pixel 9".to_owned(), "android".to_owned())
        );
        assert_eq!(validate_device("", "ios").unwrap().0, "ios");
        assert_eq!(
            validate_device(&"x".repeat(100), "ios").unwrap().0.len(),
            64
        );
        for bad in ["", "andr0id", "a-b", &"a".repeat(17)] {
            assert!(validate_device("x", bad).is_err(), "{bad}");
        }
    }
}
