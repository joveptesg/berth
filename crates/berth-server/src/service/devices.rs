//! Paired devices of the tenant and their revocation.
//!
//! Admin browsers are rows in `devices` too (`platform = 'web'` with the `admin` scope), but they
//! are sign-in sessions, not devices: they are neither listed nor revoked here.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

/// A device in the admin list.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Device {
    /// Device id.
    pub id: String,
    /// Name the device reported, e.g. "Pixel 9".
    pub name: String,
    /// `android`, `ios`, … (`web` for a browser build of the app).
    pub platform: String,
    /// Owner's name.
    pub user_name: String,
    /// Pairing time, epoch milliseconds.
    pub paired_at: i64,
    /// Last token refresh, epoch milliseconds.
    pub last_seen_at: Option<i64>,
}

/// Active paired devices of the tenant, most recently seen first. Revoked rows stay in the
/// database (their tokens must keep answering `device_revoked`) but are not listed.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Device>> {
    Ok(sqlx::query_as(
        "SELECT d.id, d.name, d.platform, u.name AS user_name, d.paired_at, d.last_seen_at
         FROM devices d JOIN users u ON u.id = d.user_id
         WHERE d.tenant_id = $1 AND NOT (d.platform = 'web' AND EXISTS (
               SELECT 1 FROM device_scopes s WHERE s.device_id = d.id AND s.scope = 'admin'))
           AND d.revoked_at IS NULL
         ORDER BY COALESCE(d.last_seen_at, d.paired_at) DESC",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Revokes a paired device: its refresh chain goes at once; its next request gets
/// `device_revoked` (an access token still in flight is rejected by the revocation check).
/// Idempotent.
///
/// # Errors
/// `NotFound` for an unknown device or an admin browser.
pub async fn revoke(db: &SqlitePool, tenant_id: &str, device_id: &str, now_ms: i64) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    let found = sqlx::query(
        "UPDATE devices SET revoked_at = COALESCE(revoked_at, $3)
         WHERE id = $1 AND tenant_id = $2 AND NOT (platform = 'web' AND EXISTS (
             SELECT 1 FROM device_scopes s WHERE s.device_id = devices.id AND s.scope = 'admin'))",
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
