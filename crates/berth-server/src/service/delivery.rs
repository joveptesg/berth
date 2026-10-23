//! Delivery of audio bytes (design doc §7, §22.A7): a device asks for a book file and gets a
//! short-lived URL that works without authorization headers. Presigned profiles hand out a
//! presigned S3 GET, CDN profiles a URL under their CDN address (signed when the CDN requires
//! it); everything else a signed `/v1/blobs/{token}` that the server streams with range
//! requests.
//!
//! A blob token is `base64url(tenant \n hash \n device \n exp) . base64url(HMAC)`. It lives
//! longer than a presigned URL because players keep one URL for hours of range requests;
//! revocation still cuts it off at once, since every request checks the device.

use std::time::Duration;

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use berth_core::{AuthError, BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    books::{BookFile, Chapter, PolicyFilter},
    storage as profiles,
};
use crate::{
    AppState,
    db::now_ms,
    secret::SecretKey,
    storage::{self, BlobStore, ContentHash, S3Config},
};

const TOKEN_LABEL: &[u8] = b"berth blob token v1";
/// Lifetime of a presigned S3 URL (design doc §7: 5–15 min).
pub const PRESIGNED_TTL: Duration = Duration::from_mins(15);
/// Lifetime of a proxy token: about one long listening session.
pub const PROXY_TTL: Duration = Duration::from_hours(6);

/// Where to fetch a file's bytes.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct StreamLink {
    /// Absolute (presigned) or server-relative (`/v1/blobs/...`) URL; a relative one resolves
    /// against the server address the client is using. Send no authorization header.
    pub url: String,
    /// When the URL stops working, epoch milliseconds: ask for a new one then.
    pub expires_at: i64,
}

/// A file of the download manifest.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ManifestFile {
    /// The file.
    #[serde(flatten)]
    pub file: BookFile,
    /// Its bytes; for a resumed download ask `/stream` again instead of reusing this.
    #[serde(flatten)]
    pub link: StreamLink,
}

/// Everything to keep a book on the phone.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct DownloadManifest {
    /// Book id.
    pub book_id: String,
    /// Sum of file sizes.
    pub size: i64,
    /// Files in order; verify each by its `blob_hash` (sha256) before use.
    pub files: Vec<ManifestFile>,
    /// Chapters on the book timeline.
    pub chapters: Vec<Chapter>,
}

/// The device a blob token was issued to and what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobGrant {
    /// Tenant id.
    pub tenant_id: String,
    /// Blob hash.
    pub hash: String,
    /// Device id.
    pub device_id: String,
    /// Expiry, epoch milliseconds.
    pub expires_at: i64,
}

fn mac(secret: &SecretKey, payload: &[u8]) -> [u8; 32] {
    secret.hmac(&[TOKEN_LABEL, payload])
}

/// Signs a blob token.
#[must_use]
pub fn sign(secret: &SecretKey, grant: &BlobGrant) -> String {
    let payload = format!(
        "{}\n{}\n{}\n{}",
        grant.tenant_id, grant.hash, grant.device_id, grant.expires_at
    );
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload.as_bytes()),
        URL_SAFE_NO_PAD.encode(mac(secret, payload.as_bytes()))
    )
}

/// Checks a blob token's signature and expiry; the device is checked by [`open_token`].
#[must_use]
pub fn verify(secret: &SecretKey, token: &str, now: i64) -> Option<BlobGrant> {
    let (payload, signature) = token.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let expected = mac(secret, &payload);
    if !constant_time_eq(&expected, &signature) {
        return None;
    }
    let text = String::from_utf8(payload).ok()?;
    let mut parts = text.split('\n');
    let grant = BlobGrant {
        tenant_id: parts.next()?.to_owned(),
        hash: parts.next()?.to_owned(),
        device_id: parts.next()?.to_owned(),
        expires_at: parts.next()?.parse().ok()?,
    };
    (parts.next().is_none() && grant.expires_at > now).then_some(grant)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A valid token of a device that is still paired.
///
/// # Errors
/// `unauthenticated` for a bad or expired token, `device_revoked` for a revoked device.
pub async fn open_token(db: &SqlitePool, secret: &SecretKey, token: &str) -> Result<BlobGrant> {
    let grant =
        verify(secret, token, now_ms()).ok_or(BerthError::Auth(AuthError::Unauthenticated))?;
    let revoked: Option<Option<i64>> =
        sqlx::query_scalar("SELECT revoked_at FROM devices WHERE tenant_id = $1 AND id = $2")
            .bind(&grant.tenant_id)
            .bind(&grant.device_id)
            .fetch_optional(db)
            .await
            .context("load device")?;
    match revoked {
        None => Err(BerthError::Auth(AuthError::Unauthenticated)),
        Some(Some(_)) => Err(BerthError::Auth(AuthError::DeviceRevoked)),
        Some(None) => Ok(grant),
    }
}

/// Files of a visible book.
async fn files(
    db: &SqlitePool,
    tenant_id: &str,
    policy: &PolicyFilter,
    book_id: &str,
) -> Result<Vec<BookFile>> {
    if !policy.allows(book_id) {
        return Err(BerthError::NotFound);
    }
    let files: Vec<BookFile> = sqlx::query_as(
        "SELECT f.idx, f.blob_hash, bl.size, f.offset_ms, f.duration_ms FROM book_files f
         JOIN books b ON b.id = f.book_id AND b.tenant_id = f.tenant_id
         JOIN blobs bl ON bl.tenant_id = f.tenant_id AND bl.hash = f.blob_hash
         WHERE f.tenant_id = $1 AND f.book_id = $2 AND b.deleted_at IS NULL ORDER BY f.idx",
    )
    .bind(tenant_id)
    .bind(book_id)
    .fetch_all(db)
    .await
    .context("load book files")?;
    if files.is_empty() {
        return Err(BerthError::NotFound);
    }
    Ok(files)
}

/// A URL for a blob: public under the CDN address or presigned when its profile delivers
/// that way, a proxy token otherwise.
async fn link(
    state: &AppState,
    tenant_id: &str,
    device_id: &str,
    hash: &str,
) -> Result<StreamLink> {
    let location: Option<(String, String, String)> = sqlx::query_as(
        "SELECT p.id, p.delivery_mode, p.config FROM blob_locations l
         JOIN storage_profiles p ON p.id = l.storage_id
         WHERE l.tenant_id = $1 AND l.hash = $2 AND l.state = 'active'
         ORDER BY CASE p.delivery_mode WHEN 'cdn' THEN 0 WHEN 'presigned' THEN 1 ELSE 2 END
         LIMIT 1",
    )
    .bind(tenant_id)
    .bind(hash)
    .fetch_optional(&state.db)
    .await
    .context("find blob")?;
    let Some((storage_id, mode, config)) = location else {
        return Err(BerthError::NotFound);
    };
    if mode == "cdn" {
        let config: S3Config = serde_json::from_str(&config).context("s3 config")?;
        let content = ContentHash::parse(hash).context("sha256 hex")?;
        let expires_at = now_ms() + ms(PROXY_TTL);
        // An unsigned URL never expires; the client still refreshes it like a proxy link.
        let url = if config.cdn_signing.is_none() {
            storage::cdn_link(&config.cdn_url, tenant_id, &content)
        } else {
            let expires = u64::try_from(expires_at / 1000).unwrap_or_default();
            profiles::cdn_link(state, tenant_id, &storage_id, &content, expires).await?
        };
        return Ok(StreamLink { url, expires_at });
    }
    if mode == "presigned" {
        let store = profiles::open(state, tenant_id, &storage_id).await?;
        let content = ContentHash::parse(hash).context("sha256 hex")?;
        if let Some(url) = store
            .presign_get(tenant_id, &content, PRESIGNED_TTL)
            .await
            .context("presign")?
        {
            return Ok(StreamLink {
                url: url.into(),
                expires_at: now_ms() + ms(PRESIGNED_TTL),
            });
        }
    }
    let grant = BlobGrant {
        tenant_id: tenant_id.to_owned(),
        hash: hash.to_owned(),
        device_id: device_id.to_owned(),
        expires_at: now_ms() + ms(PROXY_TTL),
    };
    Ok(StreamLink {
        url: format!("/v1/blobs/{}", sign(&state.secret, &grant)),
        expires_at: grant.expires_at,
    })
}

fn ms(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

/// A URL for file `idx` of a book.
///
/// # Errors
/// `NotFound` for a hidden book, a missing file or bytes in no storage.
pub async fn stream(
    state: &AppState,
    tenant_id: &str,
    device_id: &str,
    policy: &PolicyFilter,
    book_id: &str,
    idx: i64,
) -> Result<StreamLink> {
    let files = files(&state.db, tenant_id, policy, book_id).await?;
    let file = files
        .iter()
        .find(|f| f.idx == idx)
        .ok_or(BerthError::NotFound)?;
    link(state, tenant_id, device_id, &file.blob_hash).await
}

/// Files with URLs and chapters of a book for an offline download.
///
/// # Errors
/// `NotFound` for a hidden book or one without files.
pub async fn manifest(
    state: &AppState,
    tenant_id: &str,
    device_id: &str,
    policy: &PolicyFilter,
    book_id: &str,
) -> Result<DownloadManifest> {
    let files = files(&state.db, tenant_id, policy, book_id).await?;
    let chapters: Vec<Chapter> = sqlx::query_as(
        "SELECT idx, title, start_ms, end_ms FROM chapters
         WHERE tenant_id = $1 AND book_id = $2 ORDER BY idx",
    )
    .bind(tenant_id)
    .bind(book_id)
    .fetch_all(&state.db)
    .await
    .context("load chapters")?;
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let link = link(state, tenant_id, device_id, &file.blob_hash).await?;
        out.push(ManifestFile { file, link });
    }
    Ok(DownloadManifest {
        book_id: book_id.to_owned(),
        size: out.iter().map(|f| f.file.size).sum(),
        files: out,
        chapters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_sign_and_expire() {
        let secret = SecretKey::from_bytes([7; 32]);
        let grant = BlobGrant {
            tenant_id: "t".into(),
            hash: "ab".repeat(32),
            device_id: "d".into(),
            expires_at: 1_000,
        };
        let token = sign(&secret, &grant);
        assert_eq!(verify(&secret, &token, 999), Some(grant.clone()));
        assert_eq!(verify(&secret, &token, 1_000), None, "expired");
        assert_eq!(
            verify(&SecretKey::from_bytes([8; 32]), &token, 0),
            None,
            "other key"
        );
        let forged = BlobGrant {
            device_id: "x".into(),
            ..grant
        };
        let (_, signature) = token.split_once('.').unwrap();
        let forged = sign(&secret, &forged);
        let (payload, _) = forged.split_once('.').unwrap();
        assert_eq!(verify(&secret, &format!("{payload}.{signature}"), 0), None);
        assert_eq!(verify(&secret, "garbage", 0), None);
    }
}
