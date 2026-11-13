//! Bookmarks: places in a book the user marked, with an optional note. They sync like progress
//! ([`super::progress`]): a device keeps its changes while offline and sends them in a batch;
//! each change is the whole bookmark with the device time it was made, corrected for the device
//! clock. The latest change wins, and a deletion is final: a deleted bookmark stays as a
//! tombstone, so a late edit from another device does not bring it back.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    db::now_ms,
    service::{books::PolicyFilter, tokens::DeviceAuth},
};

/// Most changes in one batch; a longer queue goes in several.
pub const MAX_BATCH: usize = 500;

/// Longest note, in characters.
pub const MAX_NOTE: usize = 500;

/// A bookmark as the device has it now.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct BookmarkChange {
    /// `UUIDv7` made by the device when the bookmark was set.
    pub id: String,
    /// Book id.
    pub book_id: String,
    /// Position on the book timeline.
    pub position_ms: i64,
    /// The user's note; empty for none.
    #[serde(default)]
    pub note: String,
    /// The bookmark was deleted.
    #[serde(default)]
    pub deleted: bool,
    /// Device clock when the bookmark was set, epoch milliseconds.
    pub created_at: i64,
    /// Device clock of this change, epoch milliseconds.
    pub client_ts: i64,
}

/// `POST /v1/bookmarks/batch` body.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct BookmarkBatch {
    /// Device clock at sending, epoch milliseconds: the server corrects the times by it.
    pub client_now: i64,
    /// Changes, oldest first.
    pub changes: Vec<BookmarkChange>,
}

/// A bookmark as the server keeps it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Bookmark {
    /// Id made by the device that set it.
    pub id: String,
    /// Book id.
    pub book_id: String,
    /// Position on the book timeline.
    pub position_ms: i64,
    /// The user's note; empty for none.
    pub note: String,
    /// When it was set, server clock, epoch milliseconds.
    pub created_at: i64,
    /// When it last changed, server clock, epoch milliseconds.
    pub updated_at: i64,
    /// Deleted: the device drops its copy.
    pub deleted: bool,
}

/// The user's bookmarks.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Bookmarks {
    /// Server clock, epoch milliseconds: `t - server_now + device_now` is device time.
    pub server_now: i64,
    /// Every bookmark of the user in books they may see, deleted ones included.
    pub bookmarks: Vec<Bookmark>,
}

/// Records a batch; the whole batch is accepted or rejected, so the device can drop it after 200.
///
/// # Errors
/// `Validation` for a too long batch or a malformed change.
pub async fn record(db: &SqlitePool, device: &DeviceAuth, batch: &BookmarkBatch) -> Result<()> {
    if batch.changes.len() > MAX_BATCH {
        return Err(BerthError::Validation("bookmark_batch_too_long".into()));
    }
    for change in &batch.changes {
        let valid = uuid::Uuid::parse_str(&change.id).is_ok()
            && change.position_ms >= 0
            && change.note.chars().count() <= MAX_NOTE;
        if !valid {
            return Err(BerthError::Validation("bookmark_invalid".into()));
        }
    }
    let now = now_ms();
    let skew = now - batch.client_now;
    // A clock running ahead cannot claim the future.
    let at = |ts: i64| (ts + skew).min(now);
    let mut tx = db.begin().await.context("begin")?;
    for change in &batch.changes {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM books WHERE tenant_id = $1 AND id = $2)",
        )
        .bind(&device.tenant_id)
        .bind(&change.book_id)
        .fetch_one(&mut *tx)
        .await
        .context("check book")?;
        if !known {
            // A book that never existed here: nothing to keep, but no reason to fail the batch.
            continue;
        }
        // Another user's id is left alone; a deleted bookmark stays deleted.
        sqlx::query(
            "INSERT INTO bookmarks (id, tenant_id, user_id, book_id, position_ms, note,
                created_at, updated_at, deleted)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET
                position_ms = excluded.position_ms, note = excluded.note,
                updated_at = excluded.updated_at, deleted = excluded.deleted
             WHERE bookmarks.tenant_id = excluded.tenant_id
                AND bookmarks.user_id = excluded.user_id
                AND NOT bookmarks.deleted
                AND excluded.updated_at > bookmarks.updated_at",
        )
        .bind(&change.id)
        .bind(&device.tenant_id)
        .bind(&device.user_id)
        .bind(&change.book_id)
        .bind(change.position_ms)
        .bind(&change.note)
        .bind(at(change.created_at))
        .bind(at(change.client_ts))
        .bind(change.deleted)
        .execute(&mut *tx)
        .await
        .context("save bookmark")?;
    }
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Bookmarks of the user in books that still exist and that the user may see.
///
/// # Errors
/// Database failures.
pub async fn list(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
    policy: &PolicyFilter,
) -> anyhow::Result<Bookmarks> {
    let bookmarks: Vec<Bookmark> = sqlx::query_as(
        "SELECT m.id, m.book_id, m.position_ms, m.note, m.created_at, m.updated_at, m.deleted
         FROM bookmarks m JOIN books b ON b.id = m.book_id
         WHERE m.tenant_id = $1 AND m.user_id = $2 AND b.deleted_at IS NULL
         ORDER BY m.book_id, m.position_ms",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(Bookmarks {
        server_now: now_ms(),
        bookmarks: bookmarks
            .into_iter()
            .filter(|m| policy.allows(&m.book_id))
            .collect(),
    })
}
