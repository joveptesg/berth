//! Devices of the tenant: paired apps and admin browsers, and their revocation.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

/// A device in the admin list.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Device {
    /// Device id.
    pub id: String,
    /// Name the device reported, e.g. "Pixel 9" or "Chrome on Windows".
    pub name: String,
    /// `android`, `ios`, … or `web` for an admin browser.
    pub platform: String,
    /// Owner's name.
    pub user_name: String,
    /// Pairing or sign-in time, epoch milliseconds.
    pub paired_at: i64,
    /// Last token refresh or sign-in, epoch milliseconds.
    pub last_seen_at: Option<i64>,
    /// Revocation time, epoch milliseconds.
    pub revoked_at: Option<i64>,
}

/// Devices of the tenant: active first, most recently seen first.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Device>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.name, d.platform, u.name AS user_name, d.paired_at, d.last_seen_at,
                d.revoked_at
         FROM devices d JOIN users u ON u.id = d.user_id
         WHERE d.tenant_id = $1
         ORDER BY d.revoked_at IS NOT NULL, COALESCE(d.last_seen_at, d.paired_at) DESC",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// One device of the tenant.
///
/// # Errors
/// `NotFound` for an unknown device.
pub async fn get(db: &SqlitePool, tenant_id: &str, device_id: &str) -> Result<Device> {
    list(db, tenant_id)
        .await?
        .into_iter()
        .find(|d| d.id == device_id)
        .ok_or(BerthError::NotFound)
}

/// Revokes a device: its refresh chain and browser session go at once; its next request gets
/// `device_revoked` (an access token still in flight is rejected by the revocation check).
/// Idempotent.
///
/// # Errors
/// `NotFound` for an unknown device.
pub async fn revoke(db: &SqlitePool, tenant_id: &str, device_id: &str, now_ms: i64) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    let found = sqlx::query(
        "UPDATE devices SET revoked_at = COALESCE(revoked_at, $3) WHERE id = $1 AND tenant_id = $2",
    )
    .bind(device_id)
    .bind(tenant_id)
    .bind(now_ms)
    .execute(&mut *tx)
    .await
    .context("revoke device")?;
    if found.rows_affected() == 0 {
        return Err(BerthError::NotFound);
    }
    for sql in [
        "DELETE FROM refresh_tokens WHERE device_id = $1",
        "DELETE FROM web_sessions WHERE device_id = $1",
    ] {
        sqlx::query(sql)
            .bind(device_id)
            .execute(&mut *tx)
            .await
            .context("delete device tokens")?;
    }
    tx.commit().await.context("commit revoke")?;
    tracing::info!(device_id, "device revoked");
    Ok(())
}
