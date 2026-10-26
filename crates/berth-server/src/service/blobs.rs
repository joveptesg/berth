//! Content-addressed blobs of a tenant: which bytes exist (`blobs`) and in which storage
//! profiles (`blob_locations`, design doc §22.B10).

use anyhow::Context;
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use super::storage as profiles;
use crate::{
    AppState,
    db::now_ms,
    storage::{BlobStore, ByteStream, ContentHash},
};

/// Records a blob and its location; an existing location moves to `state`.
pub(crate) async fn register(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    hash: &str,
    size: u64,
    codec: Option<&str>,
    storage_id: &str,
    state: &str,
) -> sqlx::Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO blobs (tenant_id, hash, size, codec, created_at) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, hash) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(hash)
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .bind(codec)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO blob_locations (tenant_id, hash, storage_id, state, updated_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, hash, storage_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
    )
    .bind(tenant_id)
    .bind(hash)
    .bind(storage_id)
    .bind(state)
    .bind(now)
    .execute(conn)
    .await?;
    Ok(())
}

/// Whether the blob is stored (active) in the profile.
pub(crate) async fn is_active(
    db: impl sqlx::SqliteExecutor<'_>,
    tenant_id: &str,
    hash: &str,
    storage_id: &str,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM blob_locations
          WHERE tenant_id = $1 AND hash = $2 AND storage_id = $3 AND state = 'active')",
    )
    .bind(tenant_id)
    .bind(hash)
    .bind(storage_id)
    .fetch_one(db)
    .await
}

/// Hex sha256.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Restarts the collection grace period of blobs that just lost a reference.
pub(crate) async fn touch(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    hashes: &[String],
) -> sqlx::Result<()> {
    let now = now_ms();
    for hash in hashes {
        sqlx::query(
            "UPDATE blob_locations SET updated_at = $3
             WHERE tenant_id = $1 AND hash = $2 AND state = 'active'",
        )
        .bind(tenant_id)
        .bind(hash)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Stores small bytes (a cover) in the profile unless they are already there; returns the hash.
/// The blob is registered again either way, which keeps it from collection until the caller
/// references it.
pub(crate) async fn store_bytes(
    state: &AppState,
    tenant_id: &str,
    storage_id: &str,
    data: Vec<u8>,
    codec: &str,
) -> anyhow::Result<String> {
    let hash = sha256_hex(&data);
    let size = u64::try_from(data.len())?;
    let _writing = state.blob_writes.read().await;
    if !is_active(&state.db, tenant_id, &hash, storage_id).await? {
        let store = profiles::open(state, tenant_id, storage_id).await?;
        let content = ContentHash::parse(&hash).context("sha256 hex")?;
        store
            .put(tenant_id, &content, Box::new(std::io::Cursor::new(data)))
            .await
            .context("store blob")?;
    }
    let mut conn = state.db.acquire().await?;
    register(
        &mut conn,
        tenant_id,
        &hash,
        size,
        Some(codec),
        storage_id,
        "active",
    )
    .await?;
    Ok(hash)
}

/// The bytes of a blob from any profile that has it, with its media type.
pub(crate) async fn read(
    state: &AppState,
    tenant_id: &str,
    hash: &str,
) -> anyhow::Result<Option<(ByteStream, Option<String>, u64)>> {
    let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT l.storage_id, b.codec, b.size FROM blob_locations l
         JOIN blobs b ON b.tenant_id = l.tenant_id AND b.hash = l.hash
         WHERE l.tenant_id = $1 AND l.hash = $2 AND l.state = 'active' LIMIT 1",
    )
    .bind(tenant_id)
    .bind(hash)
    .fetch_optional(&state.db)
    .await?;
    let Some((storage_id, codec, size)) = row else {
        return Ok(None);
    };
    let content = ContentHash::parse(hash).context("sha256 hex")?;
    let store = profiles::open(state, tenant_id, &storage_id).await?;
    let stream = store.get(tenant_id, &content).await.context("read blob")?;
    Ok(Some((stream, codec, u64::try_from(size).unwrap_or(0))))
}
