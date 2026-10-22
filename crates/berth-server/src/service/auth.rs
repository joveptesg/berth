//! Admin password (argon2id) and web sessions.
//!
//! The admin's browser is a device (`platform = 'web'`, scope `admin`) with one row in
//! `web_sessions`; ending the session removes the device.

use anyhow::Context;
use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use berth_core::{BerthError, Result};
use sqlx::{SqliteConnection, SqlitePool};

use super::{random_token, token_hash};
use crate::db::now_ms;

/// Minimum admin password length.
pub const MIN_PASSWORD_LEN: usize = 10;
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// An authenticated admin browser.
#[derive(Debug, Clone)]
pub struct Session {
    /// Tenant of the admin.
    pub tenant_id: String,
    /// The admin user.
    pub user_id: String,
    /// The browser's device row.
    pub device_id: String,
}

/// Checks a new password and its confirmation.
///
/// # Errors
/// `Validation` with `password_too_short` or `password_mismatch`.
pub fn validate_new_password(password: &str, confirm: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(BerthError::Validation("password_too_short".into()));
    }
    if password != confirm {
        return Err(BerthError::Validation("password_mismatch".into()));
    }
    Ok(())
}

/// Hashes a password with argon2id (default parameters) off the async runtime.
pub(crate) async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|e| anyhow::anyhow!("argon2: {e}"))
    })
    .await?
}

async fn verify_password(hash: String, password: String) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || {
        let hash = PasswordHash::new(&hash).map_err(|e| anyhow::anyhow!("corrupt hash: {e}"))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok())
    })
    .await?
}

/// Password login of the owner; returns the session cookie token.
///
/// # Errors
/// `Validation("password_wrong")` when the password does not match or the server is not set up.
pub async fn login(db: &SqlitePool, password: &str, user_agent: &str) -> Result<String> {
    let owner: Option<(String, String, String)> = sqlx::query_as(
        "SELECT tenant_id, id, password_hash FROM users
         WHERE role = 'owner' AND password_hash IS NOT NULL AND deleted_at IS NULL",
    )
    .fetch_optional(db)
    .await
    .context("load owner")?;
    let Some((tenant_id, user_id, hash)) = owner else {
        return Err(BerthError::Validation("password_wrong".into()));
    };
    if !verify_password(hash, password.to_owned()).await? {
        return Err(BerthError::Validation("password_wrong".into()));
    }
    let mut conn = db.acquire().await.context("acquire")?;
    Ok(create_session(&mut conn, &tenant_id, &user_id, user_agent).await?)
}

/// Registers the browser as a web device with the `admin` scope and opens a session.
pub(crate) async fn create_session(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    user_id: &str,
    user_agent: &str,
) -> anyhow::Result<String> {
    let token = random_token()?;
    let device_id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO devices (id, tenant_id, user_id, name, platform, paired_at, last_seen_at)
         VALUES ($1, $2, $3, $4, 'web', $5, $5)",
    )
    .bind(&device_id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(browser_name(user_agent))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO device_scopes (tenant_id, device_id, scope) VALUES ($1, $2, 'admin')")
        .bind(tenant_id)
        .bind(&device_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO web_sessions (token_hash, tenant_id, device_id, created_at, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(token_hash(&token))
    .bind(tenant_id)
    .bind(&device_id)
    .bind(now)
    .bind(now + SESSION_TTL_MS)
    .execute(&mut *conn)
    .await?;
    Ok(token)
}

/// Resolves a session cookie token.
///
/// # Errors
/// Database failures.
pub async fn session(db: &SqlitePool, token: &str) -> anyhow::Result<Option<Session>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT s.tenant_id, d.user_id, d.id FROM web_sessions s
         JOIN devices d ON d.id = s.device_id
         WHERE s.token_hash = $1 AND s.expires_at > $2 AND d.revoked_at IS NULL",
    )
    .bind(token_hash(token))
    .bind(now_ms())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(tenant_id, user_id, device_id)| Session {
        tenant_id,
        user_id,
        device_id,
    }))
}

/// Ends the session of the token and removes its web device.
///
/// # Errors
/// Database failures.
pub async fn logout(db: &SqlitePool, token: &str) -> anyhow::Result<()> {
    let device: Option<String> =
        sqlx::query_scalar("SELECT device_id FROM web_sessions WHERE token_hash = $1")
            .bind(token_hash(token))
            .fetch_optional(db)
            .await?;
    if let Some(device_id) = device {
        let mut tx = db.begin().await?;
        delete_web_device(&mut tx, &device_id).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Changes the admin password and signs out the admin's other browsers.
///
/// # Errors
/// `Validation` with `password_wrong`, `password_too_short` or `password_mismatch`.
pub async fn change_password(
    db: &SqlitePool,
    session: &Session,
    current: &str,
    password: &str,
    confirm: &str,
) -> Result<()> {
    let hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1 AND tenant_id = $2")
            .bind(&session.user_id)
            .bind(&session.tenant_id)
            .fetch_optional(db)
            .await
            .context("load password")?
            .flatten();
    let matches = match hash {
        Some(hash) => verify_password(hash, current.to_owned()).await?,
        None => false,
    };
    if !matches {
        return Err(BerthError::Validation("password_wrong".into()));
    }
    validate_new_password(password, confirm)?;
    let hash = hash_password(password.to_owned()).await?;

    let mut tx = db.begin().await.context("begin")?;
    set_password(&mut tx, &session.user_id, &hash, Some(&session.device_id)).await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Sets a new owner password from the server console and signs out every browser.
///
/// # Errors
/// `Validation` for a weak password, `NotFound` when the server is not set up.
pub async fn reset_password(db: &SqlitePool, password: &str, confirm: &str) -> Result<()> {
    validate_new_password(password, confirm)?;
    let owner: Option<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE role = 'owner' AND deleted_at IS NULL")
            .fetch_optional(db)
            .await
            .context("load owner")?;
    let owner = owner.ok_or(BerthError::NotFound)?;
    let hash = hash_password(password.to_owned()).await?;

    let mut tx = db.begin().await.context("begin")?;
    set_password(&mut tx, &owner, &hash, None).await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Stores the hash and removes the user's web devices except `keep`.
async fn set_password(
    conn: &mut SqliteConnection,
    user_id: &str,
    hash: &str,
    keep: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(hash)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    let devices: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM devices WHERE user_id = $1 AND platform = 'web' AND id <> $2",
    )
    .bind(user_id)
    .bind(keep.unwrap_or(""))
    .fetch_all(&mut *conn)
    .await?;
    for device_id in devices {
        delete_web_device(conn, &device_id).await?;
    }
    Ok(())
}

async fn delete_web_device(conn: &mut SqliteConnection, device_id: &str) -> sqlx::Result<()> {
    for sql in [
        "DELETE FROM web_sessions WHERE device_id = $1",
        "DELETE FROM device_scopes WHERE device_id = $1",
        "DELETE FROM devices WHERE id = $1 AND platform = 'web'",
    ] {
        sqlx::query(sql).bind(device_id).execute(&mut *conn).await?;
    }
    Ok(())
}

/// Short device name for the devices list, e.g. "Chrome on Windows".
fn browser_name(user_agent: &str) -> String {
    // Order matters: Edge and Chrome UAs also mention Chrome and Safari.
    let browser = ["Edg", "Firefox", "Chrome", "Safari"]
        .into_iter()
        .find(|b| user_agent.contains(b))
        .map(|b| if b == "Edg" { "Edge" } else { b });
    let os = [
        ("Android", "Android"),
        ("iPhone", "iOS"),
        ("iPad", "iPadOS"),
        ("Windows", "Windows"),
        ("Mac OS", "macOS"),
        ("Linux", "Linux"),
    ]
    .into_iter()
    .find(|(needle, _)| user_agent.contains(needle))
    .map(|(_, name)| name);
    match (browser, os) {
        (Some(browser), Some(os)) => format!("{browser} on {os}"),
        (Some(name), None) | (None, Some(name)) => name.to_owned(),
        (None, None) => "Browser".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn argon2id_round_trip() {
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(
            verify_password(hash.clone(), "correct horse".into())
                .await
                .unwrap()
        );
        assert!(!verify_password(hash, "wrong horse".into()).await.unwrap());
    }

    #[test]
    fn password_rules() {
        assert!(validate_new_password("short", "short").is_err());
        assert!(validate_new_password("long enough!", "long enough?").is_err());
        assert!(validate_new_password("long enough!", "long enough!").is_ok());
    }

    #[test]
    fn names_browsers() {
        let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0 Safari/537.36";
        assert_eq!(browser_name(chrome), "Chrome on Windows");
        let safari = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1";
        assert_eq!(browser_name(safari), "Safari on iOS");
        assert_eq!(browser_name("curl/8.0"), "Browser");
    }
}
