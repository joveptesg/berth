//! S3 storage against a real `MinIO` in Docker (testcontainers): blob I/O, the connection
//! check and presigned URLs, including a presigned PUT that must reject foreign bytes
//! (design doc §22.A5), and database backups in a bucket restored on a new server. Skipped
//! with a message when `MinIO` cannot start (no Docker or no image).

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use berth_server::storage::{
    self, Backend, BlobStore, CdnSigning, ContentHash, ObjectBlobStore, S3Config, S3Keys,
    put_headers,
};
use bytes::Bytes;
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use testcontainers_modules::{minio::MinIO, testcontainers::runners::AsyncRunner};

const USER: &str = "minioadmin";

fn keys(secret: &str) -> S3Keys {
    S3Keys {
        access_key_id: USER.into(),
        secret_access_key: secret.into(),
        cdn_token_key: String::new(),
    }
}

fn store(endpoint: &str, bucket: &str, secret: &str) -> ObjectBlobStore {
    let config = S3Config {
        endpoint: endpoint.into(),
        bucket: bucket.into(),
        region: "us-east-1".into(),
        path_style: true,
        cdn_url: String::new(),
        cdn_signing: CdnSigning::None,
    };
    storage::open(&Backend::S3(config, keys(secret)), Path::new(".")).unwrap()
}

/// `PUT /<bucket>`, signed in headers: buckets are made outside Berth.
async fn create_bucket(client: &reqwest::Client, endpoint: &str, bucket: &str) {
    let url = format!("{endpoint}/{bucket}");
    let identity = Credentials::new(USER, USER, None, None, "test").into();
    let mut settings = SigningSettings::default();
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region("us-east-1")
        .name("s3")
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .unwrap()
        .into();
    let request =
        SignableRequest::new("PUT", &url, std::iter::empty(), SignableBody::Bytes(b"")).unwrap();
    let (instructions, _) = sign(request, &params).unwrap().into_parts();
    let mut put = client.put(&url);
    for (name, value) in instructions.headers() {
        put = put.header(name, value);
    }
    let reply = put.send().await.unwrap();
    assert!(
        reply.status().is_success(),
        "{}",
        reply.text().await.unwrap()
    );
}

#[tokio::test]
async fn s3_store_on_minio() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let container = match MinIO::default().start().await {
        Ok(container) => container,
        Err(err) => {
            eprintln!(
                "skipped: MinIO did not start (no Docker, or the image cannot be pulled): {err}"
            );
            return;
        }
    };
    let port = container.get_host_port_ipv4(9000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    create_bucket(&client, &endpoint, "books").await;

    // The connection check and its error codes.
    let s3 = store(&endpoint, "books", USER);
    assert_eq!(s3.verify("t1").await, Ok(()));
    assert_eq!(
        store(&endpoint, "books", "wrong-secret").verify("t1").await,
        Err("storage_auth_failed")
    );
    assert_eq!(
        store(&endpoint, "missing", USER).verify("t1").await,
        Err("storage_bucket_not_found")
    );

    // Blob I/O at <tenant>/blobs/sha256/ab/cd/<hash>.
    let body = b"chapter one of a long book".to_vec();
    let digest: [u8; 32] = Sha256::digest(&body).into();
    let hash = ContentHash::parse(&hex::encode(digest)).unwrap();
    assert!(!s3.exists("t1", &hash).await.unwrap());
    let blob = s3
        .put("t1", &hash, Box::new(std::io::Cursor::new(body.clone())))
        .await
        .unwrap();
    assert_eq!(blob.size, body.len() as u64);
    assert!(s3.exists("t1", &hash).await.unwrap());
    assert!(!s3.exists("t2", &hash).await.unwrap(), "per tenant");
    let read: Vec<Bytes> = s3
        .get("t1", &hash)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(read.concat(), body);

    // Presigned GET serves the bytes without other credentials.
    let ttl = Duration::from_mins(5);
    let get = s3.presign_get("t1", &hash, ttl).await.unwrap().unwrap();
    let fetched = client.get(get).send().await.unwrap();
    assert!(fetched.status().is_success());
    assert_eq!(fetched.bytes().await.unwrap().as_ref(), body.as_slice());
    s3.delete("t1", &hash).await.unwrap();
    assert!(!s3.exists("t1", &hash).await.unwrap());

    // Presigned PUT takes exactly the declared bytes.
    let size = body.len() as u64;
    let put = s3
        .presign_put("t2", &hash, size, ttl)
        .await
        .unwrap()
        .unwrap();
    let upload = |bytes: Vec<u8>| {
        let mut req = client.put(put.clone()).body(bytes);
        for (name, value) in put_headers(&digest, size) {
            if name != "content-length" {
                req = req.header(name, value);
            }
        }
        req.send()
    };
    let mut forged = body.clone();
    forged[0] ^= 1;
    let rejected = upload(forged).await.unwrap();
    assert!(
        rejected.status().is_client_error(),
        "same size, other bytes"
    );
    assert!(!s3.exists("t2", &hash).await.unwrap());
    let longer = upload([body.as_slice(), b"!"].concat()).await.unwrap();
    assert!(longer.status().is_client_error(), "other size");
    let accepted = upload(body.clone()).await.unwrap();
    assert!(
        accepted.status().is_success(),
        "{}",
        accepted.text().await.unwrap()
    );
    assert!(s3.exists("t2", &hash).await.unwrap());
}

async fn server(dir: &Path) -> berth_server::AppState {
    let config = dir.join("berth.toml");
    std::fs::write(
        &config,
        format!("data_dir = '{}'\n", dir.join("data").display()),
    )
    .unwrap();
    berth_server::bootstrap(Some(&config)).await.unwrap()
}

#[tokio::test]
async fn database_backups_in_the_bucket_restore_on_a_new_server() {
    use berth_server::{
        remote_backup,
        service::{ai, setup, storage as profiles},
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let container = match MinIO::default().start().await {
        Ok(container) => container,
        Err(err) => {
            eprintln!("skipped: MinIO did not start: {err}");
            return;
        }
    };
    let port = container.get_host_port_ipv4(9000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    create_bucket(&reqwest::Client::new(), &endpoint, "library").await;
    let bucket = profiles::ProfileInput {
        name: "MinIO".into(),
        kind: "s3".into(),
        endpoint,
        bucket: "library".into(),
        region: "us-east-1".into(),
        path_style: true,
        access_key_id: USER.into(),
        secret_access_key: USER.into(),
        ..profiles::ProfileInput::default()
    };
    let password = "harbour lights";

    // The old server copies its database into the bucket once a day.
    let old_dir = tempfile::tempdir().unwrap();
    let old = server(old_dir.path()).await;
    let owner = setup::create_owner(&old, "Old", password, password, &bucket, None)
        .await
        .unwrap();
    let old_storage = profiles::default_id(&old.db, &owner.tenant_id)
        .await
        .unwrap()
        .unwrap();
    let ai_settings = ai::AiInput {
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "m".into(),
        api_key: "sk-old".into(),
        ..ai::AiInput::default()
    };
    ai::save(&old, &owner.tenant_id, &ai_settings)
        .await
        .unwrap();
    remote_backup::save(&old, &owner.tenant_id, true, &old_storage)
        .await
        .unwrap();
    remote_backup::run_due(&old).await.unwrap();
    remote_backup::run_due(&old).await.unwrap();
    let last = remote_backup::last(&old.db, &owner.tenant_id)
        .await
        .unwrap();
    assert!(last.ok_at.is_some() && last.error.is_none(), "{last:?}");

    // A new server connects the same bucket and finds one copy (the second run was not due).
    let new_dir = tempfile::tempdir().unwrap();
    let new = server(new_dir.path()).await;
    let fresh = setup::create_owner(&new, "New", password, password, &bucket, None)
        .await
        .unwrap();
    let new_storage = profiles::default_id(&new.db, &fresh.tenant_id)
        .await
        .unwrap()
        .unwrap();
    let copies = remote_backup::list(&new, &fresh.tenant_id, &new_storage)
        .await
        .unwrap();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].server_id, old.server_id.as_ref());

    // Without the old secret.key: the library and this bucket's keys come back, the AI key
    // cannot.
    assert_eq!(
        remote_backup::restore(&new, &fresh.tenant_id, &new_storage, &copies[0].key, "zz").await,
        Err("restore_secret_invalid")
    );
    remote_backup::restore(&new, &fresh.tenant_id, &new_storage, &copies[0].key, "")
        .await
        .unwrap();
    let tenants: Vec<String> = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_all(&new.db)
        .await
        .unwrap();
    assert_eq!(tenants, [owner.tenant_id.as_str()]);
    let profile = profiles::verify(&new, &owner.tenant_id, &old_storage)
        .await
        .unwrap();
    assert_eq!(profile.verify_error, None, "the bucket keys work again");
    assert!(!ai::has_key(&new, &owner.tenant_id).await.unwrap());

    // With it, every secret comes back.
    remote_backup::restore(
        &new,
        &owner.tenant_id,
        &old_storage,
        &copies[0].key,
        &hex::encode(old.secret.as_bytes()),
    )
    .await
    .unwrap();
    assert!(ai::has_key(&new, &owner.tenant_id).await.unwrap());
}
