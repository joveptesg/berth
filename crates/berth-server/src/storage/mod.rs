//! Content-addressed blob storage over `object_store`: a local folder or any S3-compatible
//! service (AWS, R2, B2, `MinIO`, Yandex, ...).
//!
//! Blobs live at `<tenant>/blobs/sha256/ab/cd/<hash>`: deduplication only within a tenant
//! (design doc §22.A6). Local storage is delivered through the server (proxy); S3 can hand out
//! presigned URLs.

mod presign;

use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream::BoxStream};
use object_store::{
    GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutPayload, RetryConfig,
    aws::AmazonS3Builder, buffered::BufWriter, local::LocalFileSystem, path::Path,
};
use presign::Presigner;
pub use presign::put_headers;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWriteExt};
use url::Url;

/// Lowercase hex sha256 of a blob's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHash(String);

impl ContentHash {
    /// Accepts exactly 64 lowercase hex digits.
    #[must_use]
    pub fn parse(hex: &str) -> Option<Self> {
        let valid = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(hex.to_owned()))
    }

    /// The hex form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn digest(&self) -> [u8; 32] {
        let mut out = [0; 32];
        hex::decode_to_slice(&self.0, &mut out).expect("validated on parse");
        out
    }
}

/// A stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    /// Content hash.
    pub hash: ContentHash,
    /// Size in bytes.
    pub size: u64,
}

/// Bytes of a blob as they arrive.
pub type ByteStream = BoxStream<'static, object_store::Result<Bytes>>;

/// Where a tenant's blob is stored inside a profile.
#[must_use]
pub fn blob_path(tenant: &str, hash: &ContentHash) -> Path {
    let h = hash.as_str();
    Path::from(format!(
        "{tenant}/blobs/sha256/{}/{}/{h}",
        &h[..2],
        &h[2..4]
    ))
}

/// Blob storage of one profile (M0 plan §2.1).
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Streams `data` into the blob; returns its size.
    async fn put(
        &self,
        tenant: &str,
        hash: &ContentHash,
        data: Box<dyn AsyncRead + Send + Unpin>,
    ) -> object_store::Result<BlobRef>;
    /// Blob bytes, for proxy delivery.
    async fn get(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<ByteStream>;
    /// Whether the blob exists, for reconciliation.
    async fn exists(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<bool>;
    /// Deletes the blob; deleting a missing blob succeeds.
    async fn delete(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<()>;
    /// Whether clients can be given presigned URLs.
    fn supports_presigned(&self) -> bool;
    /// Presigned download URL, `None` without presigning.
    async fn presign_get(
        &self,
        tenant: &str,
        hash: &ContentHash,
        ttl: Duration,
    ) -> object_store::Result<Option<Url>>;
    /// Presigned upload URL that only accepts `size` bytes hashing to `hash`: the client sends
    /// [`put_headers`] with it. `None` without presigning.
    async fn presign_put(
        &self,
        tenant: &str,
        hash: &ContentHash,
        size: u64,
        ttl: Duration,
    ) -> object_store::Result<Option<Url>>;
}

/// Non-secret settings of a local profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalConfig {
    /// Folder; relative paths are inside the data directory.
    pub path: String,
}

/// Non-secret settings of an S3 profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// Service address; empty means AWS in `region`.
    #[serde(default)]
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// Region (`auto` for R2, `us-east-1` for most `MinIO` setups).
    pub region: String,
    /// `endpoint/bucket/key` addressing instead of `bucket.endpoint/key`.
    #[serde(default)]
    pub path_style: bool,
    /// Public address of the bucket root, usually a CDN in front of it; `cdn` delivery hands
    /// out `cdn_url/<blob path>`. Empty when unused.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cdn_url: String,
    /// How links under `cdn_url` are signed.
    #[serde(default, skip_serializing_if = "CdnSigning::is_none")]
    pub cdn_signing: CdnSigning,
}

/// Signing of CDN links.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CdnSigning {
    /// Public URLs that never expire.
    #[default]
    None,
    /// Bunny CDN token authentication (HS256) with the pull zone's key.
    Bunny,
}

impl CdnSigning {
    /// No signature.
    #[must_use]
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// The public URL of a blob behind `cdn_url` (no trailing slash).
#[must_use]
pub fn cdn_link(cdn_url: &str, tenant: &str, hash: &ContentHash) -> String {
    format!("{cdn_url}/{}", blob_path(tenant, hash))
}

/// `url` (without a query) signed for Bunny CDN token authentication until `expires`, unix
/// seconds: `?token=HS256-<base64url HMAC-SHA256(key, path + expires)>&expires=<expires>`.
///
/// # Errors
/// A URL that does not parse.
pub fn bunny_sign(url: &str, key: &str, expires: u64) -> anyhow::Result<String> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let path = Url::parse(url)
        .context("invalid CDN URL")?
        .path()
        .to_owned();
    let expires = expires.to_string();
    let mac = crate::secret::hmac_sha256(key.as_bytes(), &[path.as_bytes(), expires.as_bytes()]);
    Ok(format!(
        "{url}?token=HS256-{}&expires={expires}",
        URL_SAFE_NO_PAD.encode(mac)
    ))
}

/// Access keys of an S3 profile: stored encrypted, never shown again.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Keys {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Token authentication key of the CDN when it signs links.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cdn_token_key: String,
}

/// A profile ready to open.
pub enum Backend {
    /// Folder on this server.
    Local(LocalConfig),
    /// S3-compatible bucket.
    S3(S3Config, S3Keys),
}

/// Bucket address: `endpoint/bucket` (path style) or `bucket.endpoint` (virtual-hosted).
///
/// # Errors
/// An endpoint that is not an http(s) URL.
pub fn bucket_url(config: &S3Config) -> anyhow::Result<Url> {
    let mut url = endpoint_url(config)?;
    if config.path_style {
        let path = format!("{}/{}", url.path().trim_end_matches('/'), config.bucket);
        url.set_path(&path);
    } else {
        let host = format!("{}.{}", config.bucket, url.host_str().unwrap_or_default());
        url.set_host(Some(&host)).context("invalid bucket host")?;
    }
    Ok(url)
}

/// Whether the endpoint only works with path-style addressing: an IP address or `localhost`.
#[must_use]
pub fn needs_path_style(endpoint: &str) -> bool {
    Url::parse(endpoint).is_ok_and(|url| {
        matches!(
            url.host(),
            Some(url::Host::Ipv4(_) | url::Host::Ipv6(_) | url::Host::Domain("localhost"))
        )
    })
}

/// Service address; AWS in the profile's region when empty.
fn endpoint_url(config: &S3Config) -> anyhow::Result<Url> {
    let endpoint = if config.endpoint.trim().is_empty() {
        format!("https://s3.{}.amazonaws.com", config.region)
    } else {
        config.endpoint.trim().to_owned()
    };
    let url = Url::parse(&endpoint).context("invalid endpoint")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.has_host(),
        "endpoint must be an http(s) URL"
    );
    Ok(url)
}

/// Blob store on top of an `object_store` backend.
pub struct ObjectBlobStore {
    store: Arc<dyn ObjectStore>,
    presigner: Option<Presigner>,
    /// Folder of a local store: files can be read in place.
    local: Option<Arc<LocalFileSystem>>,
}

/// Opens the store of a profile; a local folder is created if missing.
///
/// # Errors
/// Invalid settings or a local folder that cannot be created.
pub fn open(backend: &Backend, data_dir: &FsPath) -> anyhow::Result<ObjectBlobStore> {
    match backend {
        Backend::Local(config) => {
            let root = local_root(config, data_dir);
            std::fs::create_dir_all(&root)
                .with_context(|| format!("cannot create {}", root.display()))?;
            // Deleting the last blob of a `ab/cd` folder removes the empty folders too.
            let store =
                Arc::new(LocalFileSystem::new_with_prefix(&root)?.with_automatic_cleanup(true));
            Ok(ObjectBlobStore {
                store: store.clone(),
                presigner: None,
                local: Some(store),
            })
        }
        Backend::S3(config, keys) => {
            let base = bucket_url(config)?;
            // A real endpoint answers in seconds: a wrong one should fail the check quickly.
            let retry = RetryConfig {
                max_retries: 2,
                retry_timeout: Duration::from_secs(10),
                ..RetryConfig::default()
            };
            // object_store wants the bucket in the endpoint for virtual-hosted addressing.
            let endpoint = if config.path_style {
                endpoint_url(config)?
            } else {
                base.clone()
            };
            let store = AmazonS3Builder::new()
                .with_bucket_name(&config.bucket)
                .with_region(&config.region)
                .with_endpoint(endpoint.as_str().trim_end_matches('/'))
                .with_allow_http(base.scheme() == "http")
                .with_virtual_hosted_style_request(!config.path_style)
                .with_access_key_id(&keys.access_key_id)
                .with_secret_access_key(&keys.secret_access_key)
                .with_retry(retry)
                .build()?;
            Ok(ObjectBlobStore {
                store: Arc::new(store),
                local: None,
                presigner: Some(Presigner {
                    base,
                    region: config.region.clone(),
                    access_key_id: keys.access_key_id.clone(),
                    secret_access_key: keys.secret_access_key.clone(),
                }),
            })
        }
    }
}

/// Folder of a local profile: relative paths are inside the data directory.
#[must_use]
pub fn local_root(config: &LocalConfig, data_dir: &FsPath) -> PathBuf {
    data_dir.join(config.path.trim())
}

/// Why a connection check failed: a stable code the admin translates as `errors.<code>`.
pub type VerifyError = &'static str;

impl ObjectBlobStore {
    /// Where a blob of a local store is on disk; `None` for remote stores.
    #[must_use]
    pub fn local_path(&self, tenant: &str, hash: &ContentHash) -> Option<PathBuf> {
        self.local
            .as_ref()?
            .path_to_filesystem(&blob_path(tenant, hash))
            .ok()
    }

    /// Size of a stored blob, `None` when it is missing.
    ///
    /// # Errors
    /// Backend failures other than a missing object.
    pub async fn size(
        &self,
        tenant: &str,
        hash: &ContentHash,
    ) -> object_store::Result<Option<u64>> {
        match self.store.head(&blob_path(tenant, hash)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Bytes `range` of a blob (all of it for `None`) with the range actually returned and the
    /// blob size, for range requests of players and resumed downloads.
    ///
    /// # Errors
    /// A missing blob, a range past its end, or backend failures.
    pub async fn get_range(
        &self,
        tenant: &str,
        hash: &ContentHash,
        range: Option<GetRange>,
    ) -> object_store::Result<(ByteStream, std::ops::Range<u64>, u64)> {
        let options = GetOptions::default().with_range(range);
        let result = self
            .store
            .get_opts(&blob_path(tenant, hash), options)
            .await?;
        let (range, size) = (result.range.clone(), result.meta.size);
        Ok((
            result.into_stream().map_err(Into::into).boxed(),
            range,
            size,
        ))
    }

    /// Writes, reads back and deletes a probe object under `prefix`.
    ///
    /// # Errors
    /// `storage_auth_failed`, `storage_access_denied`, `storage_bucket_not_found`,
    /// `storage_unreachable`, `storage_mismatch`, `storage_local_unwritable` or `storage_failed`.
    pub async fn verify(&self, prefix: &str) -> Result<(), VerifyError> {
        let mut probe = [0; 16];
        getrandom::fill(&mut probe).map_err(|_| "storage_failed")?;
        let path = Path::from(format!("{prefix}/.berth-verify/{}", hex::encode(probe)));
        let local = self.presigner.is_none();
        let classify = |err: object_store::Error| {
            tracing::info!(error = %err, "storage check failed");
            error_code(&err, local)
        };
        self.store
            .put(&path, PutPayload::from(probe.to_vec()))
            .await
            .map_err(classify)?;
        let read = self.store.get(&path).await.map_err(classify)?;
        let bytes = read.bytes().await.map_err(classify)?;
        // Best effort: a failed delete leaves 16 bytes behind, the check itself passed.
        if let Err(err) = self.store.delete(&path).await {
            tracing::warn!(error = %err, "storage check could not delete its probe");
        }
        if bytes.as_ref() == probe {
            Ok(())
        } else {
            Err("storage_mismatch")
        }
    }

    /// Writes a probe under `prefix`, reads it back through `cdn_url` (signed with
    /// `token_key` when the CDN signs links), then deletes it.
    ///
    /// # Errors
    /// The codes of [`Self::verify`] for the write, `storage_cdn_unreachable` when the CDN does
    /// not answer 2xx (403: not public or a wrong token key; 404: the address is not the bucket
    /// root), `storage_cdn_mismatch` for other bytes and `storage_cdn_not_protected` when a
    /// signing CDN also serves the probe without a token.
    pub async fn verify_cdn(
        &self,
        prefix: &str,
        cdn_url: &str,
        token_key: Option<&str>,
        client: &reqwest::Client,
    ) -> Result<(), VerifyError> {
        let mut probe = [0; 16];
        getrandom::fill(&mut probe).map_err(|_| "storage_failed")?;
        let path = Path::from(format!("{prefix}/.berth-verify/{}", hex::encode(probe)));
        let url = format!("{cdn_url}/{path}");
        let signed = match token_key {
            Some(key) => bunny_sign(&url, key, unix_now() + 300).map_err(|_| "storage_failed")?,
            None => url.clone(),
        };
        let local = self.presigner.is_none();
        self.store
            .put(&path, PutPayload::from(probe.to_vec()))
            .await
            .map_err(|err| error_code(&err, local))?;
        let fetch = |url: String| async move {
            let response = client.get(url).send().await?.error_for_status()?;
            response.bytes().await
        };
        let fetched = fetch(signed).await;
        let open = token_key.is_some() && fetch(url).await.is_ok();
        if let Err(err) = self.store.delete(&path).await {
            tracing::warn!(error = %err, "CDN check could not delete its probe");
        }
        match fetched {
            Ok(_) if open => Err("storage_cdn_not_protected"),
            Ok(bytes) if bytes.as_ref() == probe => Ok(()),
            Ok(_) => Err("storage_cdn_mismatch"),
            Err(err) => {
                tracing::info!(error = %err, "CDN check failed");
                Err("storage_cdn_unreachable")
            }
        }
    }

    /// Asks the bucket, as a browser does before a presigned upload, whether a page at
    /// `origin` may PUT there (a CORS preflight). Stores without presigned uploads pass.
    ///
    /// # Errors
    /// `storage_cors_missing` when the bucket does not allow the origin, `storage_unreachable`
    /// when it does not answer.
    pub async fn verify_cors(
        &self,
        prefix: &str,
        origin: &str,
        client: &reqwest::Client,
    ) -> Result<(), VerifyError> {
        let Some(presigner) = &self.presigner else {
            return Ok(());
        };
        let key = format!("{prefix}/.berth-verify/cors");
        let url = presigner
            .put(
                &key,
                &[0; 32],
                0,
                Duration::from_secs(300),
                SystemTime::now(),
            )
            .map_err(|_| "storage_failed")?;
        // A browser sends the file's type and the signed checksum.
        let response = client
            .request(reqwest::Method::OPTIONS, url)
            .header("origin", origin)
            .header("access-control-request-method", "PUT")
            .header(
                "access-control-request-headers",
                "content-type,x-amz-checksum-sha256",
            )
            .send()
            .await
            .map_err(|err| {
                tracing::info!(error = %err, "CORS check failed");
                "storage_unreachable"
            })?;
        let allowed = response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "*" || v == origin);
        if response.status().is_success() && allowed {
            Ok(())
        } else {
            tracing::info!(status = %response.status(), origin, "bucket CORS does not allow uploads");
            Err("storage_cors_missing")
        }
    }
}

/// Seconds since the unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn error_code(err: &object_store::Error, local: bool) -> VerifyError {
    // S3 answers a wrong key with 403 too; its error code tells it from a missing permission.
    let text = err.to_string();
    let bad_keys = [
        "InvalidAccessKeyId",
        "SignatureDoesNotMatch",
        "InvalidSecurity",
    ]
    .iter()
    .any(|code| text.contains(code));
    match err {
        object_store::Error::Unauthenticated { .. } => "storage_auth_failed",
        object_store::Error::PermissionDenied { .. } if bad_keys => "storage_auth_failed",
        object_store::Error::PermissionDenied { .. } => "storage_access_denied",
        object_store::Error::NotFound { .. } if !local => "storage_bucket_not_found",
        _ if local => "storage_local_unwritable",
        _ if unreachable(err) => "storage_unreachable",
        _ => "storage_failed",
    }
}

/// Connection, DNS and timeout failures anywhere in the error chain.
fn unreachable(err: &object_store::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(http) = e.downcast_ref::<object_store::client::HttpError>()
            && matches!(
                http.kind(),
                object_store::client::HttpErrorKind::Connect
                    | object_store::client::HttpErrorKind::Timeout
            )
        {
            return true;
        }
        source = e.source();
    }
    false
}

#[async_trait]
impl BlobStore for ObjectBlobStore {
    async fn put(
        &self,
        tenant: &str,
        hash: &ContentHash,
        mut data: Box<dyn AsyncRead + Send + Unpin>,
    ) -> object_store::Result<BlobRef> {
        let mut writer = BufWriter::new(self.store.clone(), blob_path(tenant, hash));
        let copied = tokio::io::copy(&mut data, &mut writer).await;
        let size = match copied {
            Ok(size) => size,
            Err(err) => {
                let _ = writer.abort().await;
                return Err(object_store::Error::Generic {
                    store: "berth",
                    source: Box::new(err),
                });
            }
        };
        writer
            .shutdown()
            .await
            .map_err(|err| object_store::Error::Generic {
                store: "berth",
                source: Box::new(err),
            })?;
        Ok(BlobRef {
            hash: hash.clone(),
            size,
        })
    }

    async fn get(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<ByteStream> {
        let result = self.store.get(&blob_path(tenant, hash)).await?;
        Ok(result.into_stream().map_err(Into::into).boxed())
    }

    async fn exists(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<bool> {
        match self.store.head(&blob_path(tenant, hash)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn delete(&self, tenant: &str, hash: &ContentHash) -> object_store::Result<()> {
        match self.store.delete(&blob_path(tenant, hash)).await {
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            other => other,
        }
    }

    fn supports_presigned(&self) -> bool {
        self.presigner.is_some()
    }

    async fn presign_get(
        &self,
        tenant: &str,
        hash: &ContentHash,
        ttl: Duration,
    ) -> object_store::Result<Option<Url>> {
        let Some(presigner) = &self.presigner else {
            return Ok(None);
        };
        presigner
            .get(blob_path(tenant, hash).as_ref(), ttl, SystemTime::now())
            .map(Some)
            .map_err(signing_error)
    }

    async fn presign_put(
        &self,
        tenant: &str,
        hash: &ContentHash,
        size: u64,
        ttl: Duration,
    ) -> object_store::Result<Option<Url>> {
        let Some(presigner) = &self.presigner else {
            return Ok(None);
        };
        presigner
            .put(
                blob_path(tenant, hash).as_ref(),
                &hash.digest(),
                size,
                ttl,
                SystemTime::now(),
            )
            .map(Some)
            .map_err(signing_error)
    }
}

fn signing_error(err: anyhow::Error) -> object_store::Error {
    object_store::Error::Generic {
        store: "berth",
        source: err.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "abcd0123456789abcdef0123456789abcdef0123456789abcdef0123456789ef";

    #[test]
    fn hashes_and_paths() {
        let hash = ContentHash::parse(HASH).unwrap();
        assert_eq!(
            blob_path("t1", &hash).as_ref(),
            format!("t1/blobs/sha256/ab/cd/{HASH}")
        );
        assert_eq!(
            cdn_link("https://cdn.example.com/books", "t1", &hash),
            format!("https://cdn.example.com/books/t1/blobs/sha256/ab/cd/{HASH}")
        );
        assert!(ContentHash::parse(&HASH.to_uppercase()).is_none());
        assert!(ContentHash::parse(&HASH[1..]).is_none());
        assert!(ContentHash::parse(&HASH.replace('a', "g")).is_none());
    }

    #[test]
    fn bucket_urls() {
        let mut config = S3Config {
            endpoint: "http://127.0.0.1:9000".into(),
            bucket: "books".into(),
            region: "us-east-1".into(),
            path_style: true,
            cdn_url: String::new(),
            cdn_signing: CdnSigning::None,
        };
        assert_eq!(
            bucket_url(&config).unwrap().as_str(),
            "http://127.0.0.1:9000/books"
        );
        config.endpoint = "https://storage.yandexcloud.net".into();
        config.path_style = false;
        assert_eq!(
            bucket_url(&config).unwrap().as_str(),
            "https://books.storage.yandexcloud.net/"
        );
        config.endpoint = String::new();
        config.region = "eu-central-1".into();
        assert_eq!(
            bucket_url(&config).unwrap().as_str(),
            "https://books.s3.eu-central-1.amazonaws.com/"
        );
        config.endpoint = "ftp://host".into();
        assert!(bucket_url(&config).is_err());
    }

    #[test]
    fn bunny_tokens() {
        // Vector of the reference implementation (BunnyWay/BunnyCDN.TokenAuthentication).
        assert_eq!(
            bunny_sign(
                "https://token-tester.b-cdn.net/abc/",
                "SecurityKey",
                1_598_024_587
            )
            .unwrap(),
            "https://token-tester.b-cdn.net/abc/\
             ?token=HS256-bTMv4RVOkjx2UXLfVDl-JIygaxfSIQP8UCnCy7CILuY&expires=1598024587"
        );
    }

    #[tokio::test]
    async fn local_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(
            &Backend::Local(LocalConfig {
                path: "blobs".into(),
            }),
            dir.path(),
        )
        .unwrap();
        let hash = ContentHash::parse(HASH).unwrap();
        assert!(!store.exists("t1", &hash).await.unwrap());
        let blob = store
            .put("t1", &hash, Box::new(&b"chapter one"[..]))
            .await
            .unwrap();
        assert_eq!(blob.size, 11);
        assert!(
            dir.path()
                .join(format!("blobs/t1/blobs/sha256/ab/cd/{HASH}"))
                .is_file()
        );
        let bytes: Vec<Bytes> = store
            .get("t1", &hash)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        assert_eq!(bytes.concat(), b"chapter one");
        assert!(!store.supports_presigned());
        assert!(
            store
                .presign_put("t1", &hash, 11, Duration::from_secs(60))
                .await
                .unwrap()
                .is_none()
        );
        assert!(!store.exists("t2", &hash).await.unwrap(), "per tenant");
        store.delete("t1", &hash).await.unwrap();
        store.delete("t1", &hash).await.unwrap();
        assert!(!store.exists("t1", &hash).await.unwrap());
        assert_eq!(store.verify("t1").await, Ok(()));
        assert!(
            !dir.path().join("blobs/t1/.berth-verify").exists(),
            "empty folders are removed"
        );
    }

    /// How the fake CDN lets requests through.
    #[derive(Clone, Copy)]
    enum Access {
        Public,
        Private,
        /// Bunny token authentication with this key.
        Token(&'static str),
    }

    /// A CDN in front of the folder: it serves its files, or refuses with 403.
    async fn fake_cdn(root: std::path::PathBuf, access: Access) -> String {
        use std::collections::HashMap;

        use axum::{
            extract::{Path as UrlPath, Query},
            http::{StatusCode, Uri},
            routing::get,
        };
        let app = axum::Router::new().route(
            "/{*key}",
            get(
                move |UrlPath(key): UrlPath<String>,
                      Query(query): Query<HashMap<String, String>>,
                      uri: Uri| {
                    let file = root.join(key);
                    async move {
                        let allowed = match access {
                            Access::Public => true,
                            Access::Private => false,
                            Access::Token(secret) => {
                                let expires: u64 = query
                                    .get("expires")
                                    .and_then(|e| e.parse().ok())
                                    .unwrap_or_default();
                                let expected = bunny_sign(
                                    &format!("http://cdn{}", uri.path()),
                                    secret,
                                    expires,
                                )
                                .unwrap();
                                expires > unix_now()
                                    && query.get("token").is_some_and(|token| {
                                        expected.contains(&format!("token={token}&"))
                                    })
                            }
                        };
                        if !allowed {
                            return Err(StatusCode::FORBIDDEN);
                        }
                        std::fs::read(file).map_err(|_| StatusCode::NOT_FOUND)
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn cdn_check_reads_the_probe_through_the_cdn() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(
            &Backend::Local(LocalConfig {
                path: "blobs".into(),
            }),
            dir.path(),
        )
        .unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let root = dir.path().join("blobs");
        let public = fake_cdn(root.clone(), Access::Public).await;
        assert_eq!(store.verify_cdn("t1", &public, None, &client).await, Ok(()));
        let private = fake_cdn(root.clone(), Access::Private).await;
        assert_eq!(
            store.verify_cdn("t1", &private, None, &client).await,
            Err("storage_cdn_unreachable")
        );
        assert_eq!(
            store
                .verify_cdn("t1", &format!("{public}/elsewhere"), None, &client)
                .await,
            Err("storage_cdn_unreachable"),
            "404: not the bucket root"
        );
        let signing = fake_cdn(root, Access::Token("zone-key")).await;
        assert_eq!(
            store
                .verify_cdn("t1", &signing, Some("zone-key"), &client)
                .await,
            Ok(())
        );
        assert_eq!(
            store
                .verify_cdn("t1", &signing, Some("wrong-key"), &client)
                .await,
            Err("storage_cdn_unreachable")
        );
        assert_eq!(
            store
                .verify_cdn("t1", &public, Some("zone-key"), &client)
                .await,
            Err("storage_cdn_not_protected"),
            "a signing profile in front of a public CDN"
        );
        assert!(
            !dir.path().join("blobs/t1/.berth-verify").exists(),
            "probes are removed"
        );
    }

    /// A bucket that answers CORS preflights for `allowed` only (none: no CORS rules).
    async fn fake_bucket(allowed: Option<&'static str>) -> String {
        use axum::http::{HeaderMap, StatusCode};
        let app = axum::Router::new().fallback(move |headers: HeaderMap| async move {
            let origin = headers.get("origin").and_then(|v| v.to_str().ok());
            match allowed {
                Some(allowed) if allowed == "*" || origin == Some(allowed) => {
                    Ok([("access-control-allow-origin", allowed)])
                }
                _ => Err(StatusCode::FORBIDDEN),
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn s3_store(endpoint: &str) -> ObjectBlobStore {
        let config = S3Config {
            endpoint: endpoint.into(),
            bucket: "books".into(),
            region: "us-east-1".into(),
            path_style: true,
            cdn_url: String::new(),
            cdn_signing: CdnSigning::None,
        };
        let keys = S3Keys {
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            cdn_token_key: String::new(),
        };
        open(&Backend::S3(config, keys), FsPath::new(".")).unwrap()
    }

    #[tokio::test]
    async fn cors_check_asks_the_bucket_like_a_browser() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::new();
        let admin = "https://berth.example.com";
        let open_bucket = s3_store(&fake_bucket(Some("*")).await);
        assert_eq!(open_bucket.verify_cors("t1", admin, &client).await, Ok(()));
        let ours = s3_store(&fake_bucket(Some(admin)).await);
        assert_eq!(ours.verify_cors("t1", admin, &client).await, Ok(()));
        assert_eq!(
            ours.verify_cors("t1", "https://other.example.com", &client)
                .await,
            Err("storage_cors_missing")
        );
        let closed = s3_store(&fake_bucket(None).await);
        assert_eq!(
            closed.verify_cors("t1", admin, &client).await,
            Err("storage_cors_missing")
        );
        let dir = tempfile::tempdir().unwrap();
        let local = open(
            &Backend::Local(LocalConfig {
                path: "blobs".into(),
            }),
            dir.path(),
        )
        .unwrap();
        assert_eq!(local.verify_cors("t1", admin, &client).await, Ok(()));
    }
}
