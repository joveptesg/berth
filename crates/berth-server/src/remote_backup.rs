//! Daily copies of the database in a storage bucket, next to the audio: a new server connects
//! the same bucket and restores the library from there. Copies live at
//! `berth-backups/<server id>/berth-<epoch ms>.sqlite`; each server keeps its last
//! [`KEEP_DAYS`] days (never fewer than one). The local automatic backups stay as they are.

use std::time::Duration;

use anyhow::Context;
use berth_core::BerthError;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    AppState, SecretKey,
    backup::{self, RestoreError},
    db::{self, now_ms},
    service::storage as profiles,
};

/// Folder of the copies in a bucket.
pub const PREFIX: &str = "berth-backups";
/// Days of copies a server keeps.
pub const KEEP_DAYS: i64 = 7;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// After a failed copy the next try waits this long.
const RETRY_MS: i64 = 60 * 60 * 1000;
const CHECK: Duration = Duration::from_mins(10);
const CONFIG_KEY: &str = "remote_backup";
const LAST_KEY: &str = "remote_backup_last";
const NAME_PREFIX: &str = "berth-";
const EXT: &str = ".sqlite";

/// What the admin set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Copies are made.
    pub enabled: bool,
    /// The S3 profile they go to.
    pub storage_id: String,
}

/// The last copy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastRun {
    /// When tried.
    pub at: i64,
    /// When the last copy went through.
    pub ok_at: Option<i64>,
    /// Why the last try failed.
    pub error: Option<String>,
}

/// A copy in a bucket.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteBackup {
    /// Path in the bucket, also its id in forms.
    pub key: String,
    /// The server that made it.
    pub server_id: String,
    /// When made, epoch milliseconds.
    pub created_at: i64,
    /// Bytes.
    pub size: u64,
}

/// The settings.
///
/// # Errors
/// Database failures.
pub async fn config(db: &sqlx::SqlitePool, tenant_id: &str) -> anyhow::Result<RemoteConfig> {
    Ok(db::tenant_setting(db, tenant_id, CONFIG_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

/// The last copy.
///
/// # Errors
/// Database failures.
pub async fn last(db: &sqlx::SqlitePool, tenant_id: &str) -> anyhow::Result<LastRun> {
    Ok(db::tenant_setting(db, tenant_id, LAST_KEY)
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

/// Saves the settings.
///
/// # Errors
/// `Validation("remote_backup_storage_invalid")` when switched on without an S3 profile.
pub async fn save(
    state: &AppState,
    tenant_id: &str,
    enabled: bool,
    storage_id: &str,
) -> Result<(), BerthError> {
    if enabled
        && profiles::s3_settings(state, tenant_id, storage_id)
            .await
            .is_err()
    {
        return Err(BerthError::Validation(
            "remote_backup_storage_invalid".into(),
        ));
    }
    let config = RemoteConfig {
        enabled,
        storage_id: storage_id.to_owned(),
    };
    let mut conn = state.db.acquire().await.context("acquire")?;
    db::put_tenant_setting(&mut conn, tenant_id, CONFIG_KEY, &json!(config))
        .await
        .context("save remote backup")?;
    Ok(())
}

/// The copies of this server live under this folder.
fn folder(server_id: &str) -> String {
    format!("{PREFIX}/{server_id}/")
}

/// Server id and time of a copy from its path.
fn parse_key(key: &str) -> Option<(String, i64)> {
    let rest = key.strip_prefix(PREFIX)?.strip_prefix('/')?;
    let (server, name) = rest.split_once('/')?;
    let valid_server = !server.is_empty()
        && server
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let at = name
        .strip_prefix(NAME_PREFIX)?
        .strip_suffix(EXT)?
        .parse()
        .ok()?;
    valid_server.then(|| (server.to_owned(), at))
}

/// Copies the database to the bucket now and drops this server's copies older than
/// [`KEEP_DAYS`]. The outcome is kept for the settings page.
///
/// # Errors
/// Not set up, database, file or bucket failures.
pub async fn run(state: &AppState, tenant_id: &str) -> anyhow::Result<()> {
    let result = upload(state, tenant_id).await;
    let mut last = last(&state.db, tenant_id).await?;
    last.at = now_ms();
    match &result {
        Ok(()) => {
            last.ok_at = Some(last.at);
            last.error = None;
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "remote backup failed");
            last.error = Some(format!("{err:#}"));
        }
    }
    let mut conn = state.db.acquire().await?;
    db::put_tenant_setting(&mut conn, tenant_id, LAST_KEY, &json!(last)).await?;
    result
}

async fn upload(state: &AppState, tenant_id: &str) -> anyhow::Result<()> {
    let config = config(&state.db, tenant_id).await?;
    anyhow::ensure!(!config.storage_id.is_empty(), "no storage chosen");
    let store = profiles::open(state, tenant_id, &config.storage_id).await?;
    // The id in the database, not the one this process started with: after a restore the
    // copies go on in the restored server's folder.
    let server_id = db::server_id(&state.db).await?;
    let now = now_ms();
    let tmp = state.config.load().data_dir.join("tmp");
    tokio::fs::create_dir_all(&tmp).await?;
    let file = tmp.join(format!("remote-{now}{EXT}"));
    backup::snapshot(&state.db, &file).await?;
    let key = format!("{}{NAME_PREFIX}{now}{EXT}", folder(&server_id));
    let sent = store.put_file(&key, &file).await;
    // The copy has the storage keys (encrypted): it must not linger in tmp.
    let _ = tokio::fs::remove_file(&file).await;
    sent.with_context(|| format!("upload {key}"))?;
    tracing::info!(key = %key, "backed up the database to the bucket");

    let mut own: Vec<(String, i64)> = store
        .list_files(&folder(&server_id))
        .await?
        .into_iter()
        .filter_map(|(path, _, _)| parse_key(&path).map(|(_, at)| (path, at)))
        .collect();
    own.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
    for (path, at) in own.into_iter().skip(1) {
        if at < now - KEEP_DAYS * DAY_MS
            && let Err(err) = store.delete_file(&path).await
        {
            tracing::warn!(error = %err, key = %path, "cannot remove an old remote backup");
        }
    }
    Ok(())
}

/// Copies in the bucket of `storage_id`, of any server, newest first.
///
/// # Errors
/// Not an S3 profile, bucket failures.
pub async fn list(
    state: &AppState,
    tenant_id: &str,
    storage_id: &str,
) -> anyhow::Result<Vec<RemoteBackup>> {
    profiles::s3_settings(state, tenant_id, storage_id).await?;
    let store = profiles::open(state, tenant_id, storage_id).await?;
    let mut out: Vec<RemoteBackup> = store
        .list_files(PREFIX)
        .await?
        .into_iter()
        .filter_map(|(key, size, _)| {
            let (server_id, created_at) = parse_key(&key)?;
            Some(RemoteBackup {
                key,
                server_id,
                created_at,
                size,
            })
        })
        .collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    Ok(out)
}

/// Restores the database from copy `key` in the bucket of `storage_id`. `old_secret` is the
/// hex `secret.key` of the server that made it: with it every stored key is read again;
/// without it only the keys of this bucket are (see `rekey_after_restore`).
///
/// # Errors
/// `restore_secret_invalid`, `restore_download_failed` and the codes of [`backup::restore`].
pub async fn restore(
    state: &AppState,
    tenant_id: &str,
    storage_id: &str,
    key: &str,
    old_secret: &str,
) -> Result<(), RestoreError> {
    if parse_key(key).is_none() {
        return Err("restore_not_berth");
    }
    let old = match old_secret.trim() {
        "" => None,
        hex => Some(SecretKey::from_hex(hex).map_err(|_| "restore_secret_invalid")?),
    };
    let source = profiles::s3_settings(state, tenant_id, storage_id)
        .await
        .map_err(|_| "restore_download_failed")?;
    let data_dir = state.config.load().data_dir.clone();
    let tmp = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|_| "restore_failed")?;
    let file = tmp.join(format!("remote-restore-{}{EXT}", now_ms()));
    let downloaded = match profiles::open(state, tenant_id, storage_id).await {
        Ok(store) => store.get_file(key, &file).await,
        Err(err) => Err(err),
    };
    if let Err(err) = downloaded {
        tracing::warn!(error = %format!("{err:#}"), key, "cannot download the remote backup");
        let _ = std::fs::remove_file(&file);
        return Err("restore_download_failed");
    }
    let result = backup::restore(&state.db, &data_dir, &file).await;
    let _ = std::fs::remove_file(&file);
    result?;
    profiles::rekey_after_restore(state, old.as_ref(), Some((&source.0, &source.1)))
        .await
        .map_err(|err| {
            tracing::warn!(error = %format!("{err:#}"), "cannot re-encrypt the restored keys");
            "restore_failed"
        })
}

/// Starts the scheduler: every few minutes, a copy for each tenant that wants one and has
/// none from the last day (after a failure, an hour later).
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CHECK);
        loop {
            interval.tick().await;
            if let Err(err) = run_due(&state).await {
                tracing::warn!(error = ?err, "remote backup scheduler failed");
            }
        }
    });
}

/// Makes the copies that are due.
///
/// # Errors
/// Database failures (a failed copy is recorded, not returned).
pub async fn run_due(state: &AppState) -> anyhow::Result<()> {
    let scopes: Vec<String> =
        sqlx::query_scalar("SELECT scope FROM settings WHERE key = $1 AND scope LIKE 'tenant:%'")
            .bind(CONFIG_KEY)
            .fetch_all(&state.db)
            .await?;
    let now = now_ms();
    for scope in scopes {
        let tenant_id = scope.trim_start_matches("tenant:");
        if !config(&state.db, tenant_id).await?.enabled {
            continue;
        }
        let last = last(&state.db, tenant_id).await?;
        let fresh = last.ok_at.is_some_and(|at| now - at < DAY_MS);
        let failed_recently = last.error.is_some() && now - last.at < RETRY_MS;
        if !fresh && !failed_recently {
            let _ = run(state, tenant_id).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(
            parse_key("berth-backups/01a1-ff/berth-1792665056275.sqlite"),
            Some(("01a1-ff".into(), 1_792_665_056_275))
        );
        for bad in [
            "berth-backups/berth-1.sqlite",
            "berth-backups/../x/berth-1.sqlite",
            "berth-backups/a/berth-x.sqlite",
            "other/a/berth-1.sqlite",
            "berth-backups/a/b/berth-1.sqlite",
        ] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
    }
}
