//! Hourly removal of dead tokens and sessions (policy: comment in `0002_auth.sql`).

use std::time::Duration;

use sqlx::SqlitePool;

use crate::db::now_ms;

const INTERVAL: Duration = Duration::from_hours(1);
const USED_PAIRING_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Starts the cleanup task; it runs right away and then every hour.
pub fn spawn(db: SqlitePool) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = purge_expired(&db, now_ms()).await {
                tracing::warn!(error = ?err, "token cleanup failed");
            }
        }
    });
}

/// Deletes expired refresh tokens, web sessions and pairing tokens, used pairing tokens
/// older than a day and web devices without a session. Idempotent.
pub(crate) async fn purge_expired(db: &SqlitePool, now: i64) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM refresh_tokens WHERE expires_at < $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM web_sessions WHERE expires_at < $1")
        .bind(now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM pairing_tokens WHERE expires_at < $1 OR used_at < $2")
        .bind(now)
        .bind(now - USED_PAIRING_TTL_MS)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM device_scopes WHERE device_id IN (
             SELECT id FROM devices d WHERE d.platform = 'web'
             AND NOT EXISTS (SELECT 1 FROM web_sessions s WHERE s.device_id = d.id))",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM devices WHERE platform = 'web'
         AND NOT EXISTS (SELECT 1 FROM web_sessions s WHERE s.device_id = devices.id)",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000_000;
    const DAY: i64 = USED_PAIRING_TTL_MS;

    async fn count(db: &SqlitePool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
    }

    #[tokio::test]
    async fn removes_only_dead_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::connect(&dir.path().join("t.sqlite"))
            .await
            .unwrap();
        for sql in [
            "INSERT INTO tenants VALUES ('t', 'T', 0)",
            "INSERT INTO users (id, tenant_id, name, role, created_at) VALUES ('u', 't', 'U', 'owner', 0)",
            "INSERT INTO devices (id, tenant_id, user_id, name, platform, paired_at)
             VALUES ('phone', 't', 'u', 'Phone', 'android', 0),
                    ('web-live', 't', 'u', 'Browser', 'web', 0),
                    ('web-dead', 't', 'u', 'Browser', 'web', 0)",
            "INSERT INTO device_scopes VALUES ('t', 'web-live', 'admin'), ('t', 'web-dead', 'admin')",
        ] {
            sqlx::query(sql).execute(&db).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO refresh_tokens VALUES ('old', 't', 'phone', 0, $1, NULL), ('new', 't', 'phone', 0, $2, NULL)",
        )
        .bind(NOW - 1)
        .bind(NOW + DAY)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO web_sessions VALUES ('dead', 't', 'web-dead', 0, $1), ('live', 't', 'web-live', 0, $2)")
            .bind(NOW - 1)
            .bind(NOW + DAY)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO pairing_tokens VALUES
                ('expired', 't', 'u', 'qr', $1, NULL),
                ('used-long-ago', 't', 'u', 'qr', $2, $3),
                ('used-recently', 't', 'u', 'qr', $2, $4),
                ('live', 't', 'u', 'qr', $2, NULL)",
        )
        .bind(NOW - 1)
        .bind(NOW + DAY)
        .bind(NOW - 2 * DAY)
        .bind(NOW - 1)
        .execute(&db)
        .await
        .unwrap();

        purge_expired(&db, NOW).await.unwrap();
        purge_expired(&db, NOW).await.unwrap();

        let refresh: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM refresh_tokens")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(refresh, ["new"]);
        let pairing: Vec<String> =
            sqlx::query_scalar("SELECT token_hash FROM pairing_tokens ORDER BY token_hash")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(pairing, ["live", "used-recently"]);
        let devices: Vec<String> = sqlx::query_scalar("SELECT id FROM devices ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(devices, ["phone", "web-live"]);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM web_sessions").await, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM device_scopes").await, 1);
    }
}
