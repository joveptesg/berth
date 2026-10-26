//! Removal of blobs nothing uses any more: files of deleted books and discarded uploads,
//! replaced covers, and objects of uploads that never finished. A blob goes a day after it
//! lost its last reference, so issued links expire first and a recent backup still finds it.

use std::time::Duration;

use anyhow::Context;
use sqlx::AssertSqlSafe;

use crate::{
    AppState,
    db::now_ms,
    service::storage as profiles,
    storage::{BlobStore, ContentHash},
};

const INTERVAL: Duration = Duration::from_hours(1);
/// The buckets are listed on every this many collections (once a day).
const COLLECTIONS_PER_LISTING: u32 = 24;
/// How long an unused blob, or an object no blob row knows, is kept (longer than a presigned
/// upload and a proxy link live).
pub const GRACE_MS: i64 = 24 * 60 * 60 * 1000;

/// Matches a `blob_locations l` row whose hash no book or open upload uses. Saved uploads
/// do not count: their book does.
const UNREFERENCED: &str = "NOT EXISTS (SELECT 1 FROM book_files f
        JOIN books b ON b.tenant_id = f.tenant_id AND b.id = f.book_id
        WHERE f.tenant_id = l.tenant_id AND f.blob_hash = l.hash AND b.deleted_at IS NULL)
    AND NOT EXISTS (SELECT 1 FROM books b
        WHERE b.tenant_id = l.tenant_id AND b.cover_hash = l.hash AND b.deleted_at IS NULL)
    AND NOT EXISTS (SELECT 1 FROM upload_files uf JOIN uploads u ON u.id = uf.upload_id
        WHERE uf.tenant_id = l.tenant_id AND uf.blob_hash = l.hash AND u.status <> 'saved')
    AND NOT EXISTS (SELECT 1 FROM uploads u
        WHERE u.tenant_id = l.tenant_id AND u.status <> 'saved'
        AND json_extract(u.draft, '$.cover_hash') = l.hash)";

/// Starts the collector; it runs right away and then every hour, listing the buckets daily.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        let mut tick = 0u32;
        loop {
            interval.tick().await;
            if let Err(err) = collect_unreferenced(&state, now_ms()).await {
                tracing::warn!(error = ?err, "blob collection failed");
            }
            if tick.is_multiple_of(COLLECTIONS_PER_LISTING)
                && let Err(err) = sweep_buckets(&state, now_ms()).await
            {
                tracing::warn!(error = ?err, "bucket sweep failed");
            }
            tick = tick.wrapping_add(1);
        }
    });
}

/// Deletes blobs unused for [`GRACE_MS`] and retries deletions that failed before; returns
/// how many locations it removed. Idempotent.
///
/// # Errors
/// Database failures; a blob that cannot be deleted is logged and tried again next time.
pub async fn collect_unreferenced(state: &AppState, now: i64) -> anyhow::Result<usize> {
    let cutoff = now - GRACE_MS;
    let candidates: Vec<(String, String, String)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT tenant_id, hash, storage_id FROM blob_locations l
         WHERE (state = 'deleting' OR (state = 'active' AND updated_at < $1)) AND {UNREFERENCED}"
    )))
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .context("unused blobs")?;
    let mut removed = 0;
    for (tenant_id, hash, storage_id) in candidates {
        match remove(state, &tenant_id, &hash, &storage_id, cutoff).await {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(error = ?err, %hash, storage = %storage_id, "could not remove blob");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "unused blobs removed");
    }
    Ok(removed)
}

/// Marks the location deleting while it is still unused, deletes the object, then the rows.
/// A failed delete leaves the location `deleting`: it no longer counts as stored.
async fn remove(
    state: &AppState,
    tenant_id: &str,
    hash: &str,
    storage_id: &str,
    cutoff: i64,
) -> anyhow::Result<bool> {
    let content = ContentHash::parse(hash).context("sha256 hex")?;
    let _collecting = state.blob_writes.write().await;
    let marked = sqlx::query(AssertSqlSafe(format!(
        "UPDATE blob_locations AS l SET state = 'deleting'
         WHERE tenant_id = $1 AND hash = $2 AND storage_id = $3
         AND (state = 'deleting' OR (state = 'active' AND updated_at < $4)) AND {UNREFERENCED}"
    )))
    .bind(tenant_id)
    .bind(hash)
    .bind(storage_id)
    .bind(cutoff)
    .execute(&state.db)
    .await
    .context("mark deleting")?
    .rows_affected();
    if marked == 0 {
        return Ok(false);
    }
    let store = profiles::open(state, tenant_id, storage_id).await?;
    store
        .delete(tenant_id, &content)
        .await
        .context("delete object")?;
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(
        "DELETE FROM blob_locations
         WHERE tenant_id = $1 AND hash = $2 AND storage_id = $3 AND state = 'deleting'",
    )
    .bind(tenant_id)
    .bind(hash)
    .bind(storage_id)
    .execute(&mut *tx)
    .await
    .context("delete location")?;
    // Files of deleted books keep their rows for sync, and those rows keep the blob row.
    sqlx::query(
        "DELETE FROM blobs WHERE tenant_id = $1 AND hash = $2
         AND NOT EXISTS (SELECT 1 FROM blob_locations l
                         WHERE l.tenant_id = blobs.tenant_id AND l.hash = blobs.hash)
         AND NOT EXISTS (SELECT 1 FROM book_files f
                         WHERE f.tenant_id = blobs.tenant_id AND f.blob_hash = blobs.hash)",
    )
    .bind(tenant_id)
    .bind(hash)
    .execute(&mut *tx)
    .await
    .context("delete blob")?;
    tx.commit().await.context("commit")?;
    Ok(true)
}

/// Deletes objects under each profile's blob folder that no location row knows and no open
/// upload expects, once they are older than [`GRACE_MS`]; returns how many. Idempotent.
///
/// # Errors
/// Database failures; a profile that cannot be listed is logged and skipped.
pub async fn sweep_buckets(state: &AppState, now: i64) -> anyhow::Result<usize> {
    let cutoff = now - GRACE_MS;
    let profiles: Vec<(String, String)> =
        sqlx::query_as("SELECT tenant_id, id FROM storage_profiles WHERE deleted_at IS NULL")
            .fetch_all(&state.db)
            .await
            .context("storage profiles")?;
    let mut removed = 0;
    for (tenant_id, storage_id) in profiles {
        match sweep_profile(state, &tenant_id, &storage_id, cutoff).await {
            Ok(count) => removed += count,
            Err(err) => {
                tracing::warn!(error = ?err, storage = %storage_id, "could not sweep storage");
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "stray objects removed");
    }
    Ok(removed)
}

async fn sweep_profile(
    state: &AppState,
    tenant_id: &str,
    storage_id: &str,
    cutoff: i64,
) -> anyhow::Result<usize> {
    let store = profiles::open(state, tenant_id, storage_id).await?;
    let objects = store
        .list_files(&format!("{tenant_id}/blobs/sha256"))
        .await
        .context("list blobs")?;
    let mut removed = 0;
    for (path, _, modified) in objects {
        let Some(hash) = path
            .rsplit('/')
            .next()
            .filter(|h| ContentHash::parse(h).is_some())
        else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        let _collecting = state.blob_writes.write().await;
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM blob_locations
                             WHERE tenant_id = $1 AND hash = $2 AND storage_id = $3)
                 OR EXISTS (SELECT 1 FROM upload_files uf JOIN uploads u ON u.id = uf.upload_id
                             WHERE uf.tenant_id = $1 AND uf.blob_hash = $2 AND u.status <> 'saved')",
        )
        .bind(tenant_id)
        .bind(hash)
        .bind(storage_id)
        .fetch_one(&state.db)
        .await
        .context("known blob")?;
        if !known {
            store.delete_file(&path).await.context("delete object")?;
            removed += 1;
        }
    }
    Ok(removed)
}
