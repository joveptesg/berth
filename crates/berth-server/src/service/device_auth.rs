//! Sign-in of a desktop app with a device code (clients-spec §1, after RFC 8628). A computer has
//! no camera for the pairing QR: the app asks for a code pair, shows the short user code and
//! opens `/activate?code=…` in the browser; an administrator signed in to the web admin sees
//! which device asks and allows it for a user. The app polls with its secret device code and,
//! once allowed, gets a new device of that user with its first tokens, as pairing gives.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    pairing::{self, CODE_LEN, Paired, generate_code, normalize},
    random_token, token_hash,
};
use crate::SecretKey;

/// How long a code waits for the administrator.
pub const TTL_MS: i64 = 600_000;
/// How often the app should poll, seconds.
pub const INTERVAL_S: i64 = 5;
/// How long after approval the app may still pick its tokens up.
const CLAIM_GRACE_MS: i64 = 120_000;

/// A sign-in the app waits on.
#[derive(Debug, Clone)]
pub struct Started {
    /// Secret of the app, sent with every poll.
    pub device_code: String,
    /// Short code the app shows and the administrator confirms.
    pub user_code: String,
    /// Expiry, epoch milliseconds.
    pub expires_at: i64,
}

/// What `/activate` shows before allowing.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceRequest {
    /// Name the app reported, e.g. "Alex's laptop".
    pub device_name: String,
    /// Its platform: `windows`, `macos`, `linux`.
    pub platform: String,
    /// When the code expires, epoch milliseconds.
    pub expires_at: i64,
}

/// Where an app's sign-in stands.
#[derive(Debug)]
pub enum Status {
    /// Not decided yet.
    Waiting {
        /// Expiry, epoch milliseconds.
        expires_at: i64,
    },
    /// The administrator refused it.
    Denied,
    /// Expired, unknown or already used: start again.
    Expired,
    /// Allowed: the new device and its tokens (only once).
    SignedIn(Paired),
}

/// Opens a sign-in for an app.
///
/// # Errors
/// `NotFound` before setup; `Validation("platform_invalid")`.
pub async fn start(
    db: &SqlitePool,
    device_name: &str,
    platform: &str,
    now: i64,
) -> Result<Started> {
    let (device_name, platform) = pairing::validate_device(device_name, platform)?;
    let tenant_id: Option<String> = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_optional(db)
        .await
        .context("load tenant")?;
    let tenant_id = tenant_id.ok_or(BerthError::NotFound)?;
    let device_code = random_token()?;
    let user_code = generate_code()?;
    let expires_at = now + TTL_MS;
    sqlx::query(
        "INSERT INTO device_auth_requests
           (device_code_hash, user_code_hash, tenant_id, device_name, platform, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(token_hash(&device_code))
    .bind(token_hash(&user_code))
    .bind(&tenant_id)
    .bind(&device_name)
    .bind(&platform)
    .bind(now)
    .bind(expires_at)
    .execute(db)
    .await
    .context("insert device auth")?;
    tracing::info!(name = ?device_name, platform, "device code sign-in started");
    Ok(Started {
        device_code,
        user_code,
        expires_at,
    })
}

fn user_code_hash(code: &str) -> Result<String> {
    let code = normalize(code);
    if code.len() == CODE_LEN {
        Ok(token_hash(&code))
    } else {
        Err(BerthError::Validation("device_code_invalid".into()))
    }
}

/// An open sign-in of the tenant: its hash and what to show.
async fn open_request(
    db: &SqlitePool,
    tenant_id: &str,
    user_code: &str,
    now: i64,
) -> Result<(String, DeviceRequest)> {
    let hash = user_code_hash(user_code)?;
    let row: Option<(String, String, i64, bool)> = sqlx::query_as(
        "SELECT device_name, platform, expires_at,
                approved_at IS NOT NULL OR denied_at IS NOT NULL
         FROM device_auth_requests WHERE user_code_hash = $1 AND tenant_id = $2",
    )
    .bind(&hash)
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .context("load device auth")?;
    let Some((device_name, platform, expires_at, decided)) = row else {
        return Err(BerthError::Validation("device_code_invalid".into()));
    };
    if decided {
        return Err(BerthError::Validation("device_code_used".into()));
    }
    if expires_at <= now {
        return Err(BerthError::Validation("device_code_expired".into()));
    }
    Ok((
        hash,
        DeviceRequest {
            device_name,
            platform,
            expires_at,
        },
    ))
}

/// The device behind a user code, for `/activate` to show before allowing.
///
/// # Errors
/// `Validation` with `device_code_invalid`, `device_code_expired` or `device_code_used`.
pub async fn describe(
    db: &SqlitePool,
    tenant_id: &str,
    user_code: &str,
    now: i64,
) -> Result<DeviceRequest> {
    Ok(open_request(db, tenant_id, user_code, now).await?.1)
}

/// Allows the sign-in: the app becomes a device of `user_id`.
///
/// # Errors
/// As [`describe`]; `NotFound` for a user not in the tenant.
pub async fn approve(
    db: &SqlitePool,
    tenant_id: &str,
    approved_by: &str,
    user_code: &str,
    user_id: &str,
    now: i64,
) -> Result<()> {
    let (hash, _) = open_request(db, tenant_id, user_code, now).await?;
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
    let updated = sqlx::query(
        "UPDATE device_auth_requests SET user_id = $2, approved_by = $3, approved_at = $4
         WHERE user_code_hash = $1 AND approved_at IS NULL AND denied_at IS NULL AND expires_at > $4",
    )
    .bind(&hash)
    .bind(user_id)
    .bind(approved_by)
    .bind(now)
    .execute(db)
    .await
    .context("approve device auth")?
    .rows_affected();
    if updated == 0 {
        return Err(BerthError::Validation("device_code_used".into()));
    }
    tracing::info!(user = %user_id, approved_by, "device code sign-in allowed");
    Ok(())
}

/// Refuses the sign-in: the app stops polling and says so.
///
/// # Errors
/// As [`describe`].
pub async fn deny(db: &SqlitePool, tenant_id: &str, user_code: &str, now: i64) -> Result<()> {
    let (hash, _) = open_request(db, tenant_id, user_code, now).await?;
    sqlx::query(
        "UPDATE device_auth_requests SET denied_at = $2
         WHERE user_code_hash = $1 AND approved_at IS NULL AND denied_at IS NULL",
    )
    .bind(&hash)
    .bind(now)
    .execute(db)
    .await
    .context("deny device auth")?;
    tracing::info!("device code sign-in denied");
    Ok(())
}

/// The app asks how its sign-in stands; once allowed it gets a new device (only once).
///
/// # Errors
/// Database failures.
pub async fn poll(
    db: &SqlitePool,
    secret: &SecretKey,
    device_code: &str,
    now: i64,
) -> Result<Status> {
    let hash = token_hash(device_code.trim());
    let mut tx = db.begin().await.context("begin")?;
    // Write first: the conditional UPDATE lets exactly one of two concurrent polls win.
    let claimed: Option<(String, String, String, String)> = sqlx::query_as(
        "UPDATE device_auth_requests SET used_at = $2
         WHERE device_code_hash = $1 AND user_id IS NOT NULL AND used_at IS NULL
           AND approved_at > $3
         RETURNING tenant_id, user_id, device_name, platform",
    )
    .bind(&hash)
    .bind(now)
    .bind(now - CLAIM_GRACE_MS)
    .fetch_optional(&mut *tx)
    .await
    .context("claim device auth")?;
    if let Some((tenant_id, user_id, device_name, platform)) = claimed {
        // The user may have been deleted meanwhile.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND tenant_id = $2 AND deleted_at IS NULL)",
        )
        .bind(&user_id)
        .bind(&tenant_id)
        .fetch_one(&mut *tx)
        .await
        .context("load user")?;
        if !exists {
            tx.rollback().await.context("rollback")?;
            return Ok(Status::Expired);
        }
        let paired = pairing::create_device(
            &mut tx,
            secret,
            (&tenant_id, &user_id),
            (&device_name, &platform),
            now,
        )
        .await?;
        sqlx::query("UPDATE device_auth_requests SET device_id = $2 WHERE device_code_hash = $1")
            .bind(&hash)
            .bind(&paired.device_id)
            .execute(&mut *tx)
            .await
            .context("link device")?;
        tx.commit().await.context("commit")?;
        tracing::info!(device = %paired.device_id, name = ?device_name, platform, user = %user_id, "device signed in with a device code");
        return Ok(Status::SignedIn(paired));
    }
    tx.rollback().await.context("rollback")?;
    let row: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT expires_at, approved_at, denied_at FROM device_auth_requests
         WHERE device_code_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await
    .context("load device auth")?;
    Ok(match row {
        Some((_, _, Some(_))) => Status::Denied,
        Some((expires_at, None, None)) if expires_at > now => Status::Waiting { expires_at },
        _ => Status::Expired,
    })
}
