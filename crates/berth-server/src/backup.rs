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
