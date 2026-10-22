use std::{path::Path, str::FromStr, time::Duration};

use anyhow::Context;
use serde_json::{Map, Value};
use sqlx::{
    SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

pub(crate) const FILE_NAME: &str = "berth.sqlite";

const SERVER_SCOPE: &str = "server";
const SERVER_ID_KEY: &str = "server_id";

/// Opens (creating if needed) the database and applies migrations.
pub(crate) async fn connect(path: &Path) -> anyhow::Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite:")?
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .with_context(|| format!("failed to open database {}", path.display()))?;
    sqlx::migrate!()
        .run(&pool)
        .await
        .context("failed to apply migrations")?;
    Ok(pool)
}

/// Returns the server id, generating it on first start.
pub(crate) async fn server_id(pool: &SqlitePool) -> anyhow::Result<String> {
    let generated = Value::String(uuid::Uuid::now_v7().to_string()).to_string();
    sqlx::query(
        "INSERT INTO settings (scope, key, value, updated_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (scope, key) DO NOTHING",
    )
    .bind(SERVER_SCOPE)
    .bind(SERVER_ID_KEY)
    .bind(generated)
    .bind(now_ms())
    .execute(pool)
    .await?;

    let value: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE scope = $1 AND key = $2")
            .bind(SERVER_SCOPE)
            .bind(SERVER_ID_KEY)
            .fetch_one(pool)
            .await?;
    serde_json::from_str(&value).context("corrupt server_id setting")
}

/// Server-scope plain settings as a configuration layer (values are JSON).
pub(crate) async fn server_settings(pool: &SqlitePool) -> anyhow::Result<Map<String, Value>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, value FROM settings WHERE scope = $1 AND key <> $2 AND value IS NOT NULL",
    )
    .bind(SERVER_SCOPE)
    .bind(SERVER_ID_KEY)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(key, value)| {
            let value =
                serde_json::from_str(&value).with_context(|| format!("corrupt setting {key}"))?;
            Ok((key, value))
        })
        .collect()
}

/// Writes a server-scope plain setting; `None` removes it.
pub(crate) async fn put_server_setting(
    conn: &mut SqliteConnection,
    key: &str,
    value: Option<&Value>,
) -> sqlx::Result<()> {
    let Some(value) = value else {
        sqlx::query("DELETE FROM settings WHERE scope = $1 AND key = $2")
            .bind(SERVER_SCOPE)
            .bind(key)
            .execute(conn)
            .await?;
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO settings (scope, key, value, updated_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (scope, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(SERVER_SCOPE)
    .bind(key)
    .bind(value.to_string())
    .bind(now_ms())
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
}
