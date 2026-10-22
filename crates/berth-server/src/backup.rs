//! Database backups (mvp-spec §7): a consistent copy made with SQLite `VACUUM INTO`, on demand
//! from the admin and automatically before schema migrations (the last [`KEEP`] are kept in
//! `<data_dir>/backups`). `secret.key` is not in a backup: it is kept apart on purpose
//! (design doc §22.C), without it the storage keys in the copy cannot be read.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db::now_ms;

/// Folder of automatic backups inside the data directory.
pub const DIR: &str = "backups";
/// Automatic backups kept.
pub const KEEP: usize = 3;
const AUTO_PREFIX: &str = "auto-";
const EXT: &str = ".sqlite";

/// An automatic backup on disk.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Backup {
    /// File name, also its id in URLs.
    pub name: String,
    /// When it was made, epoch milliseconds.
    pub created_at: i64,
    /// Bytes.
    pub size: u64,
}

/// Writes a consistent copy of the database to `target` (which must not exist).
///
/// # Errors
/// Database or file system failures.
pub async fn snapshot(db: &SqlitePool, target: &Path) -> anyhow::Result<()> {
    sqlx::query("VACUUM INTO $1")
        .bind(target.to_string_lossy().into_owned())
        .execute(db)
        .await
        .with_context(|| format!("backup to {}", target.display()))?;
    Ok(())
}

/// Backs the database up if `pending` migrations are about to change a database that already
/// has data, then keeps only the newest [`KEEP`] automatic backups.
///
/// # Errors
/// A failed backup: migrations must not run without one.
pub async fn before_migrations(
    db: &SqlitePool,
    data_dir: &Path,
    pending: bool,
) -> anyhow::Result<Option<PathBuf>> {
    if !pending {
        return Ok(None);
    }
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let target = dir.join(format!("{AUTO_PREFIX}{}{EXT}", now_ms()));
    snapshot(db, &target).await?;
    tracing::info!(path = %target.display(), "backed up the database before migrations");
    for old in list(data_dir).into_iter().skip(KEEP) {
        if let Err(err) = std::fs::remove_file(dir.join(&old.name)) {
            tracing::warn!(error = %err, name = old.name, "cannot remove an old backup");
        }
    }
    Ok(Some(target))
}

/// Why a file cannot be restored: a stable code the admin translates as `errors.<code>`.
pub type RestoreError = &'static str;

/// Restores the database from the backup at `source`, while the server runs.
///
/// The backup is copied into `<data_dir>/tmp`, checked to be a Berth database not newer than
/// this server, and brought to the current schema there. The current database is backed up
/// first (it shows among the automatic backups), then every table is replaced from the copy
/// in one transaction: either all of the backup is in place or nothing changed. Sessions and
/// devices come back as they were in the backup, so browsers may have to sign in again.
///
/// # Errors
/// `restore_not_berth` (not a Berth database), `restore_newer` (made by a newer server) or
/// `restore_failed`.
pub async fn restore(db: &SqlitePool, data_dir: &Path, source: &Path) -> Result<(), RestoreError> {
    let tmp = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|_| "restore_failed")?;
    let copy = tmp.join(format!("restore-{}.sqlite", now_ms()));
    let result = restore_from_copy(db, data_dir, source, &copy).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", copy.display()));
    }
    result
}

async fn restore_from_copy(
    db: &SqlitePool,
    data_dir: &Path,
    source: &Path,
    copy: &Path,
) -> Result<(), RestoreError> {
    let header = std::fs::read(source)
        .ok()
        .and_then(|b| b.get(..16).map(<[u8]>::to_vec));
    if header.as_deref() != Some(b"SQLite format 3\0".as_slice()) {
        return Err("restore_not_berth");
    }
    std::fs::copy(source, copy).map_err(|_| "restore_failed")?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(copy)
        .foreign_keys(false);
    let backup = sqlx::SqlitePool::connect_with(options)
        .await
        .map_err(|_| "restore_not_berth")?;
    let migrator = sqlx::migrate!();
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = TRUE")
            .fetch_all(&backup)
            .await
            .map_err(|_| "restore_not_berth")?;
    let has_tenants: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'tenants')",
    )
    .fetch_one(&backup)
    .await
    .map_err(|_| "restore_not_berth")?;
    if applied.is_empty() || !has_tenants {
        return Err("restore_not_berth");
    }
    if applied
        .iter()
        .any(|v| !migrator.iter().any(|m| m.version == *v))
    {
        return Err("restore_newer");
    }
    migrator.run(&backup).await.map_err(|err| {
        tracing::warn!(error = %err, "cannot bring the backup to the current schema");
        "restore_failed"
    })?;
    backup.close().await;

    // The current state is kept: a wrong restore can be undone the same way.
    let dir = data_dir.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|_| "restore_failed")?;
    snapshot(db, &dir.join(format!("{AUTO_PREFIX}{}{EXT}", now_ms())))
        .await
        .map_err(|_| "restore_failed")?;

    replace_tables(db, copy).await.map_err(|err| {
        tracing::warn!(error = %err, "restore failed");
        "restore_failed"
    })?;
    tracing::info!(source = %source.display(), "restored the database from a backup");
    Ok(())
}

/// Replaces every table of the live database with the same table of `copy` (same schema).
async fn replace_tables(db: &SqlitePool, copy: &Path) -> anyhow::Result<()> {
    use sqlx::{AssertSqlSafe, Row};

    let mut conn = db.acquire().await?;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await?;
    sqlx::query("ATTACH DATABASE $1 AS restored")
        .bind(copy.to_string_lossy().into_owned())
        .execute(&mut *conn)
        .await?;
    let result = async {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM main.sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
        )
        .fetch_all(&mut *conn)
        .await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        for table in &tables {
            // Names come from sqlite_master, not from the user; quoted all the same.
            let quoted = format!("\"{}\"", table.replace('"', "\"\""));
            let columns: Vec<String> = sqlx::query(AssertSqlSafe(format!(
                "SELECT name FROM pragma_table_info('{}', 'main')",
                table.replace('\'', "''")
            )))
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|row| format!("\"{}\"", row.get::<String, _>(0).replace('"', "\"\"")))
            .collect();
            let columns = columns.join(", ");
            sqlx::query(AssertSqlSafe(format!("DELETE FROM main.{quoted}")))
                .execute(&mut *conn)
                .await?;
            sqlx::query(AssertSqlSafe(format!(
                "INSERT INTO main.{quoted} ({columns}) SELECT {columns} FROM restored.{quoted}"
            )))
            .execute(&mut *conn)
            .await?;
        }
        let broken: Vec<String> =
            sqlx::query_scalar("SELECT \"table\" FROM pragma_foreign_key_check")
                .fetch_all(&mut *conn)
                .await?;
        anyhow::ensure!(broken.is_empty(), "foreign keys broken in {broken:?}");
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        anyhow::Ok(())
    }
    .await;
    if result.is_err() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
    }
    let _ = sqlx::query("DETACH DATABASE restored")
        .execute(&mut *conn)
        .await;
    let _ = sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await;
    result
}

/// Automatic backups, newest first.
#[must_use]
pub fn list(data_dir: &Path) -> Vec<Backup> {
    let Ok(entries) = std::fs::read_dir(data_dir.join(DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<Backup> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let created_at = parse_name(&name)?;
            let size = entry.metadata().ok()?.len();
            Some(Backup {
                name,
                created_at,
                size,
            })
        })
        .collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    out
}

/// The path of automatic backup `name`, if it is one (no path tricks).
#[must_use]
pub fn path_of(data_dir: &Path, name: &str) -> Option<PathBuf> {
    parse_name(name)?;
    let path = data_dir.join(DIR).join(name);
    path.is_file().then_some(path)
}

fn parse_name(name: &str) -> Option<i64> {
    name.strip_prefix(AUTO_PREFIX)?
        .strip_suffix(EXT)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            parse_name("auto-1792665056275.sqlite"),
            Some(1_792_665_056_275)
        );
        for bad in [
            "auto-.sqlite",
            "auto-1.db",
            "x-1.sqlite",
            "auto-../1.sqlite",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
        assert_eq!(path_of(Path::new("."), "../berth.sqlite"), None);
    }
}
