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
    ObjectStore, ObjectStoreExt, PutPayload, RetryConfig, aws::AmazonS3Builder,
    buffered::BufWriter, local::LocalFileSystem, path::Path,
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
}

/// Access keys of an S3 profile: stored encrypted, never shown again.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Keys {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
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
            let store = LocalFileSystem::new_with_prefix(&root)?.with_automatic_cleanup(true);
            Ok(ObjectBlobStore {
                store: Arc::new(store),
                presigner: None,
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
}
