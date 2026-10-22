//! Users of the tenant: the owner and listeners. Listeners have no password; their phones
//! pair with codes from `/connect`.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db::now_ms;

const MAX_NAME_LEN: usize = 64;

/// A user in the admin list.
#[derive(Debug, Clone, Serialize)]
pub struct User {
    /// User id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `owner`, `admin`, `member` or `restricted`.
    pub role: String,
    /// Paired devices that are not revoked (admin browsers not counted).
    pub devices: i64,
    /// Creation time, epoch milliseconds.
    pub created_at: i64,
}

/// Users of the tenant, owner first, then by name.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<User>> {
    let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT u.id, u.name, u.role, u.created_at,
                (SELECT COUNT(*) FROM devices d WHERE d.user_id = u.id AND d.revoked_at IS NULL
                 AND NOT (d.platform = 'web' AND EXISTS (
                     SELECT 1 FROM device_scopes s WHERE s.device_id = d.id AND s.scope = 'admin')))
         FROM users u WHERE u.tenant_id = $1 AND u.deleted_at IS NULL
         ORDER BY u.role <> 'owner', u.name COLLATE NOCASE",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, role, created_at, devices)| User {
            id,
            name,
            role,
            devices,
            created_at,
        })
        .collect())
}

/// Creates a listener (role `member`) with the default, unrestricted access policy.
///
/// # Errors
/// `Validation` with `user_name_required` or `user_name_too_long`.
pub async fn create_listener(db: &SqlitePool, tenant_id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("user_name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("user_name_too_long".into()));
    }
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(
        "INSERT INTO users (id, tenant_id, name, role, created_at) VALUES ($1, $2, $3, 'member', $4)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(name)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("insert user")?;
    sqlx::query("INSERT INTO access_policies (tenant_id, user_id) VALUES ($1, $2)")
        .bind(tenant_id)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .context("insert policy")?;
    tx.commit().await.context("commit user")?;
    Ok(id)
}
