//! Admin sign-in to the web with the phone (mvp-spec §5): pairing in reverse. The browser
//! shows a code and its QR and keeps a random claim in a cookie; the administrator's paired
//! phone (scope `admin`) sees which browser asks and approves; the browser holding the claim
//! then gets a session for the phone's user. A code seen over someone's shoulder is useless
//! without the claim, and the phone shows the browser before approving.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    auth::{browser_name, create_session},
    pairing::{CODE_LEN, generate_code, normalize},
    random_token, token_hash,
    tokens::DeviceAuth,
};

/// How long a code waits for the phone.
pub const TTL_MS: i64 = 120_000;
/// How long after approval the browser may still pick the session up.
const CLAIM_GRACE_MS: i64 = 120_000;

/// A sign-in the browser waits on.
#[derive(Debug, Clone)]
pub struct Started {
    /// Code for the phone (also in the QR).
    pub code: String,
    /// Secret of this browser, kept in a cookie.
    pub claim: String,
    /// Expiry, epoch milliseconds.
    pub expires_at: i64,
}

/// What the phone shows before approving.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct WebLoginRequest {
    /// The browser asking, e.g. "Chrome on Windows".
    pub browser: String,
    /// When it asked, epoch milliseconds.
    pub created_at: i64,
    /// When the code expires, epoch milliseconds.
    pub expires_at: i64,
}

/// Where a browser's sign-in stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not approved yet.
    Waiting {
        /// Expiry, epoch milliseconds.
        expires_at: i64,
    },
    /// Expired, unknown or already used: start again.
    Expired,
    /// Approved: the session token for the cookie.
    SignedIn(String),
}

/// Opens a sign-in for a browser.
///
/// # Errors
/// `NotFound` before setup.
pub async fn start(db: &SqlitePool, user_agent: &str, now: i64) -> Result<Started> {
    let tenant_id: Option<String> = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_optional(db)
        .await
        .context("load tenant")?;
    let tenant_id = tenant_id.ok_or(BerthError::NotFound)?;
    let code = generate_code()?;
    let claim = random_token()?;
    let expires_at = now + TTL_MS;
    sqlx::query(
        "INSERT INTO web_login_requests (code_hash, claim_hash, tenant_id, browser, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(token_hash(&code))
    .bind(token_hash(&claim))
    .bind(&tenant_id)
    .bind(browser_name(user_agent))
    .bind(now)
    .bind(expires_at)
    .execute(db)
    .await
    .context("insert web login")?;
    Ok(Started {
        code,
        claim,
        expires_at,
    })
}

fn code_hash(code: &str) -> Result<String> {
    let code = normalize(code);
    if code.len() == CODE_LEN {
        Ok(token_hash(&code))
    } else {
        Err(BerthError::Validation("web_login_invalid".into()))
    }
}

/// An open sign-in of the device's tenant.
async fn open_request(
    db: &SqlitePool,
    device: &DeviceAuth,
    code: &str,
    now: i64,
) -> Result<(String, WebLoginRequest)> {
    device.require("admin")?;
    let hash = code_hash(code)?;
    let row: Option<(String, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT browser, created_at, expires_at, approved_by FROM web_login_requests
         WHERE code_hash = $1 AND tenant_id = $2",
    )
    .bind(&hash)
    .bind(&device.tenant_id)
    .fetch_optional(db)
    .await
    .context("load web login")?;
    let Some((browser, created_at, expires_at, approved_by)) = row else {
        return Err(BerthError::Validation("web_login_invalid".into()));
    };
    if approved_by.is_some() {
        return Err(BerthError::Validation("web_login_used".into()));
    }
    if expires_at <= now {
        return Err(BerthError::Validation("web_login_expired".into()));
    }
    Ok((
        hash,
        WebLoginRequest {
            browser,
            created_at,
            expires_at,
        },
    ))
}

/// The browser behind a code, for the phone to show before approving.
///
/// # Errors
/// `Forbidden` without the `admin` scope; `Validation` with `web_login_invalid`,
/// `web_login_expired` or `web_login_used`.
pub async fn describe(
    db: &SqlitePool,
    device: &DeviceAuth,
    code: &str,
    now: i64,
) -> Result<WebLoginRequest> {
    Ok(open_request(db, device, code, now).await?.1)
}

/// Approves a sign-in: the browser gets a session for this device's user.
///
/// # Errors
/// As [`describe`].
pub async fn approve(db: &SqlitePool, device: &DeviceAuth, code: &str, now: i64) -> Result<()> {
    let (hash, _) = open_request(db, device, code, now).await?;
    let updated = sqlx::query(
        "UPDATE web_login_requests SET approved_by = $2, approved_at = $3
         WHERE code_hash = $1 AND approved_by IS NULL AND expires_at > $3",
    )
    .bind(&hash)
    .bind(&device.device_id)
    .bind(now)
    .execute(db)
    .await
    .context("approve web login")?
    .rows_affected();
    if updated == 0 {
        return Err(BerthError::Validation("web_login_used".into()));
    }
    Ok(())
}

/// The browser asks how its sign-in stands; once approved it gets the session (only once).
///
/// # Errors
/// Database failures.
pub async fn claim(
    db: &SqlitePool,
    code: &str,
    claim: &str,
    user_agent: &str,
    now: i64,
) -> Result<Status> {
    let Ok(hash) = code_hash(code) else {
        return Ok(Status::Expired);
    };
    let mut tx = db.begin().await.context("begin")?;
    let approved: Option<(String, String)> = sqlx::query_as(
        "UPDATE web_login_requests SET used_at = $3
         WHERE code_hash = $1 AND claim_hash = $2 AND approved_by IS NOT NULL
           AND used_at IS NULL AND approved_at > $4
         RETURNING tenant_id, approved_by",
    )
    .bind(&hash)
    .bind(token_hash(claim))
    .bind(now)
    .bind(now - CLAIM_GRACE_MS)
    .fetch_optional(&mut *tx)
    .await
    .context("claim web login")?;
    if let Some((tenant_id, device_id)) = approved {
        // The approving phone may have been revoked meanwhile.
        let user_id: Option<String> = sqlx::query_scalar(
            "SELECT user_id FROM devices WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
        )
        .bind(&device_id)
        .bind(&tenant_id)
        .fetch_optional(&mut *tx)
        .await
        .context("load approving device")?;
        let Some(user_id) = user_id else {
            tx.rollback().await.context("rollback")?;
            return Ok(Status::Expired);
        };
        let token = create_session(&mut tx, &tenant_id, &user_id, user_agent).await?;
        tx.commit().await.context("commit")?;
        return Ok(Status::SignedIn(token));
    }
    tx.rollback().await.context("rollback")?;
    let waiting: Option<i64> = sqlx::query_scalar(
        "SELECT expires_at FROM web_login_requests
         WHERE code_hash = $1 AND claim_hash = $2 AND approved_by IS NULL AND expires_at > $3",
    )
    .bind(&hash)
    .bind(token_hash(claim))
    .bind(now)
    .fetch_optional(db)
    .await
    .context("load web login")?;
    Ok(waiting.map_or(Status::Expired, |expires_at| Status::Waiting { expires_at }))
}
