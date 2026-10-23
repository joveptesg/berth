//! Storage profiles: where a tenant's blobs live (a local folder or an S3 bucket).
//!
//! Keys are stored encrypted (`crate::crypto`) and never returned. The tenant's default
//! profile, the end of the `book → collection → library → default` cascade, is the
//! `default_storage_id` setting in scope `tenant:<id>`.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};

use crate::{
    AppState, SecretKey, crypto,
    db::{self, now_ms},
    storage::{self, Backend, CdnSigning, LocalConfig, S3Config, S3Keys},
};

/// Setting key of the tenant's default profile.
const DEFAULT_KEY: &str = "default_storage_id";
const MAX_NAME_LEN: usize = 64;
const DEFAULT_REGION: &str = "us-east-1";

/// A profile as the admin and the API show it: no keys.
#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    /// Profile id.
    pub id: String,
    /// Display name, unique within the tenant.
    pub name: String,
    /// `local` or `s3`.
    pub kind: String,
    /// `proxy` (through the server), `presigned` (straight from S3) or `cdn` (public URLs
    /// under `cdn_url`).
    pub delivery_mode: String,
    /// Local folder as entered (relative to the data directory or absolute).
    pub path: String,
    /// S3 endpoint; empty means AWS.
    pub endpoint: String,
    /// S3 bucket.
    pub bucket: String,
    /// S3 region.
    pub region: String,
    /// S3 path-style addressing.
    pub path_style: bool,
    /// Public address of the bucket root for `cdn` delivery; empty when unused.
    pub cdn_url: String,
    /// How CDN links are signed: `none` or `bunny` (its key is never shown).
    pub cdn_signing: String,
    /// Whether new books go here.
    pub is_default: bool,
    /// When the last connection check passed, epoch milliseconds.
    pub verified_at: Option<i64>,
    /// Error code of the last failed check.
    pub verify_error: Option<String>,
    /// Creation time, epoch milliseconds.
    pub created_at: i64,
}

/// Form or API input. Empty keys on update keep the stored ones.
#[derive(Debug, Clone, Default)]
pub struct ProfileInput {
    /// Display name.
    pub name: String,
    /// `local` or `s3`.
    pub kind: String,
    /// Local folder.
    pub path: String,
    /// S3 endpoint; empty means AWS.
    pub endpoint: String,
    /// S3 bucket.
    pub bucket: String,
    /// S3 region; empty means `us-east-1`.
    pub region: String,
    /// S3 path-style addressing.
    pub path_style: bool,
    /// S3 delivery: `presigned` (default), `proxy` or `cdn`.
    pub delivery_mode: String,
    /// Public address of the bucket root, required for `cdn`.
    pub cdn_url: String,
    /// CDN link signing: `none` (default) or `bunny`.
    pub cdn_signing: String,
    /// Bunny CDN token authentication key; empty on update keeps the stored one.
    pub cdn_token_key: String,
    /// S3 access key id.
    pub access_key_id: String,
    /// S3 secret access key.
    pub secret_access_key: String,
}

impl ProfileInput {
    /// The local profile made by setup: `<data_dir>/blobs`.
    #[must_use]
    pub fn local_default() -> Self {
        Self {
            name: "Local".into(),
            kind: "local".into(),
            path: "blobs".into(),
            ..Self::default()
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    name: String,
    kind: String,
    config: String,
    secrets_encrypted: Option<String>,
    delivery_mode: String,
    verified_at: Option<i64>,
    verify_error: Option<String>,
    created_at: i64,
}

/// Validated input.
struct Prepared {
    name: String,
    kind: &'static str,
    config: Value,
    delivery_mode: &'static str,
    /// New keys; `None` keeps the stored ones.
    keys: Option<S3Keys>,
    /// New CDN token key; `None` keeps the stored one.
    cdn_token_key: Option<String>,
}

fn prepare(input: &ProfileInput) -> Result<Prepared> {
    let invalid = |code: &str| BerthError::Validation(code.into());
    let name = input.name.trim();
    if name.is_empty() {
        return Err(invalid("storage_name_required"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(invalid("storage_name_too_long"));
    }
    let name = name.to_owned();
    match input.kind.as_str() {
        "local" => {
            let path = input.path.trim();
            if path.is_empty() {
                return Err(invalid("storage_path_required"));
            }
            Ok(Prepared {
                name,
                kind: "local",
                config: serde_json::to_value(LocalConfig { path: path.into() })
                    .context("local config")?,
                delivery_mode: "proxy",
                keys: None,
                cdn_token_key: None,
            })
        }
        "s3" => {
            let bucket = input.bucket.trim();
            if bucket.is_empty() {
                return Err(invalid("storage_bucket_required"));
            }
            let region = match input.region.trim() {
                "" => DEFAULT_REGION,
                region => region,
            };
            let endpoint = input.endpoint.trim().trim_end_matches('/');
            let cdn_url = input.cdn_url.trim().trim_end_matches('/');
            if !cdn_url.is_empty() && !public_base(cdn_url) {
                return Err(invalid("storage_cdn_url_invalid"));
            }
            let cdn_signing = match input.cdn_signing.as_str() {
                "" | "none" => CdnSigning::None,
                "bunny" => CdnSigning::Bunny,
                _ => return Err(invalid("storage_cdn_signing_invalid")),
            };
            if !cdn_signing.is_none() && cdn_url.is_empty() {
                return Err(invalid("storage_cdn_url_required"));
            }
            let config = S3Config {
                endpoint: endpoint.into(),
                bucket: bucket.into(),
                region: region.into(),
                // A bucket cannot be a subdomain of an IP address or localhost.
                path_style: input.path_style || storage::needs_path_style(endpoint),
                cdn_url: cdn_url.into(),
                cdn_signing,
            };
            if storage::bucket_url(&config).is_err() {
                return Err(invalid("storage_endpoint_invalid"));
            }
            let delivery_mode = match input.delivery_mode.as_str() {
                "" | "presigned" => "presigned",
                "proxy" => "proxy",
                "cdn" if cdn_url.is_empty() => return Err(invalid("storage_cdn_url_required")),
                "cdn" => "cdn",
                _ => return Err(invalid("storage_delivery_invalid")),
            };
            let (id, key) = (input.access_key_id.trim(), input.secret_access_key.trim());
            let keys = match (id.is_empty(), key.is_empty()) {
                (true, true) => None,
                (false, false) => Some(S3Keys {
                    access_key_id: id.into(),
                    secret_access_key: key.into(),
                    cdn_token_key: String::new(),
                }),
                _ => return Err(invalid("storage_keys_required")),
            };
            Ok(Prepared {
                name,
                kind: "s3",
                config: serde_json::to_value(config).context("s3 config")?,
                delivery_mode,
                keys,
                cdn_token_key: match input.cdn_token_key.trim() {
                    "" => None,
                    key => Some(key.into()),
                },
            })
        }
        _ => Err(invalid("storage_kind_invalid")),
    }
}

/// An http(s) address with a host and no query or fragment, to put blob paths under.
fn public_base(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.host().is_some()
            && u.query().is_none()
            && u.fragment().is_none()
    })
}

fn backend(secret: &SecretKey, row: &Row) -> anyhow::Result<Backend> {
    Ok(match row.kind.as_str() {
        "local" => Backend::Local(serde_json::from_str(&row.config).context("local config")?),
        "s3" => Backend::S3(
            serde_json::from_str(&row.config).context("s3 config")?,
            decrypt_keys(secret, row)?.context("s3 profile without keys")?,
        ),
        kind => anyhow::bail!("unsupported storage kind {kind}"),
    })
}

fn encrypt_keys(secret: &SecretKey, id: &str, keys: &S3Keys) -> anyhow::Result<String> {
    crypto::encrypt(secret, id, &serde_json::to_vec(keys)?)
}

fn decrypt_keys(secret: &SecretKey, row: &Row) -> anyhow::Result<Option<S3Keys>> {
    row.secrets_encrypted
        .as_deref()
        .map(|stored| {
            let keys = crypto::decrypt(secret, &row.id, stored)?;
            serde_json::from_slice(&keys).context("s3 keys")
        })
        .transpose()
}

impl Prepared {
    /// Keys to store for an S3 profile: the new ones or `stored`, with the new CDN token key;
    /// none when links are not signed.
    ///
    /// # Errors
    /// `storage_keys_required` without access keys, `storage_cdn_token_required` when links
    /// are signed but there is no token key.
    fn keys(&self, stored: Option<S3Keys>) -> Result<S3Keys> {
        let invalid = |code: &str| BerthError::Validation(code.into());
        let mut keys = match (&self.keys, stored) {
            (Some(new), stored) => S3Keys {
                cdn_token_key: stored.map(|s| s.cdn_token_key).unwrap_or_default(),
                ..new.clone()
            },
            (None, Some(stored)) => stored,
            (None, None) => return Err(invalid("storage_keys_required")),
        };
        if let Some(key) = &self.cdn_token_key {
            keys.cdn_token_key.clone_from(key);
        }
        // `cdn_signing` is absent from the config when links are not signed.
        if self.config.get("cdn_signing").is_none() {
            keys.cdn_token_key.clear();
        } else if keys.cdn_token_key.is_empty() {
            return Err(invalid("storage_cdn_token_required"));
        }
        Ok(keys)
    }
}

fn profile(row: Row, default_id: Option<&str>) -> anyhow::Result<Profile> {
    let mut out = Profile {
        is_default: default_id == Some(row.id.as_str()),
        id: row.id,
        name: row.name,
        kind: row.kind,
        delivery_mode: row.delivery_mode,
        path: String::new(),
        endpoint: String::new(),
        bucket: String::new(),
        region: String::new(),
        path_style: false,
        cdn_url: String::new(),
        cdn_signing: "none".into(),
        verified_at: row.verified_at,
        verify_error: row.verify_error,
        created_at: row.created_at,
    };
    match out.kind.as_str() {
        "local" => {
            let config: LocalConfig = serde_json::from_str(&row.config).context("local config")?;
            out.path = config.path;
        }
        "s3" => {
            let config: S3Config = serde_json::from_str(&row.config).context("s3 config")?;
            out.endpoint = config.endpoint;
            out.bucket = config.bucket;
            out.region = config.region;
            out.path_style = config.path_style;
            out.cdn_url = config.cdn_url;
            if config.cdn_signing == CdnSigning::Bunny {
                out.cdn_signing = "bunny".into();
            }
        }
        _ => {}
    }
    Ok(out)
}

async fn row(db: impl sqlx::SqliteExecutor<'_>, tenant_id: &str, id: &str) -> Result<Row> {
    sqlx::query_as::<_, Row>(
        "SELECT id, name, kind, config, secrets_encrypted, delivery_mode, verified_at,
            verify_error, created_at FROM storage_profiles
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .context("load storage profile")?
    .ok_or(BerthError::NotFound)
}

/// The tenant's default profile id.
///
/// # Errors
/// Database failures.
pub async fn default_id(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Option<String>> {
    Ok(db::tenant_setting(db, tenant_id, DEFAULT_KEY)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

/// Profiles of the tenant, oldest first.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Profile>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, name, kind, config, secrets_encrypted, delivery_mode, verified_at,
            verify_error, created_at FROM storage_profiles
         WHERE tenant_id = $1 AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    let default_id = default_id(db, tenant_id).await?;
    rows.into_iter()
        .map(|row| profile(row, default_id.as_deref()))
        .collect()
}

/// One profile.
///
/// # Errors
/// `NotFound`.
pub async fn get(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<Profile> {
    let row = row(db, tenant_id, id).await?;
    let default_id = default_id(db, tenant_id).await?;
    Ok(profile(row, default_id.as_deref())?)
}

fn name_taken(err: sqlx::Error) -> BerthError {
    match &err {
        sqlx::Error::Database(e) if e.is_unique_violation() => {
            BerthError::Validation("storage_name_taken".into())
        }
        _ => BerthError::Internal(anyhow::Error::from(err).context("save storage profile")),
    }
}

/// Inserts a validated profile; `verified_at` when it was just checked.
async fn insert(
    conn: &mut SqliteConnection,
    secret: &SecretKey,
    tenant_id: &str,
    id: &str,
    prepared: &Prepared,
    verified_at: Option<i64>,
) -> Result<()> {
    let secrets = match prepared.kind {
        "s3" => Some(encrypt_keys(secret, id, &prepared.keys(None)?)?),
        _ => None,
    };
    let now = now_ms();
    sqlx::query(
        "INSERT INTO storage_profiles
            (id, tenant_id, name, kind, config, secrets_encrypted, delivery_mode, verified_at,
             created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&prepared.name)
    .bind(prepared.kind)
    .bind(prepared.config.to_string())
    .bind(secrets)
    .bind(prepared.delivery_mode)
    .bind(verified_at)
    .bind(now)
    .execute(conn)
    .await
    .map_err(name_taken)?;
    Ok(())
}

/// Adds a profile (not checked yet).
///
/// # Errors
/// `Validation` with `storage_name_required`, `storage_name_too_long`, `storage_name_taken`,
/// `storage_kind_invalid`, `storage_path_required`, `storage_bucket_required`,
/// `storage_endpoint_invalid`, `storage_delivery_invalid`, `storage_cdn_url_required`,
/// `storage_cdn_url_invalid`, `storage_cdn_signing_invalid`, `storage_cdn_token_required` or
/// `storage_keys_required`.
pub async fn create(state: &AppState, tenant_id: &str, input: &ProfileInput) -> Result<String> {
    let prepared = prepare(input)?;
    let id = uuid::Uuid::now_v7().to_string();
    let mut conn = state.db.acquire().await.context("acquire")?;
    insert(&mut conn, &state.secret, tenant_id, &id, &prepared, None).await?;
    tracing::info!(profile = %id, kind = prepared.kind, "storage profile created");
    Ok(id)
}

/// Changes a profile; its kind stays. A new location or new keys reset the check.
///
/// # Errors
/// `NotFound`, the `Validation` codes of [`create`], `Conflict("storage_in_use")` when the
/// location changes while blobs are stored there.
pub async fn update(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    input: &ProfileInput,
) -> Result<()> {
    let prepared = prepare(input)?;
    let mut tx = state.db.begin().await.context("begin")?;
    let current = row(&mut *tx, tenant_id, id).await?;
    if current.kind != prepared.kind {
        return Err(BerthError::Validation("storage_kind_invalid".into()));
    }
    let current_config: Value =
        serde_json::from_str(&current.config).context("stored storage config")?;
    // The CDN is how clients read, not where the bytes are.
    let location = |config: &Value| {
        let mut config = config.clone();
        if let Some(map) = config.as_object_mut() {
            map.remove("cdn_url");
            map.remove("cdn_signing");
        }
        config
    };
    let moved = location(&current_config) != location(&prepared.config);
    if moved && has_blobs(&mut tx, tenant_id, id).await? {
        return Err(BerthError::Conflict("storage_in_use".into()));
    }
    let secrets = match prepared.kind {
        "s3" => {
            let stored = decrypt_keys(&state.secret, &current)?;
            let keys = prepared.keys(stored.clone())?;
            if stored.as_ref() == Some(&keys) {
                current.secrets_encrypted.clone()
            } else {
                Some(encrypt_keys(&state.secret, id, &keys)?)
            }
        }
        _ => current.secrets_encrypted.clone(),
    };
    let recheck = moved
        || prepared.keys.is_some()
        || prepared.cdn_token_key.is_some()
        || current_config != prepared.config
        || current.delivery_mode != prepared.delivery_mode;
    sqlx::query(
        "UPDATE storage_profiles SET name = $3, config = $4, secrets_encrypted = $5,
            delivery_mode = $6, updated_at = $7,
            verified_at = CASE WHEN $8 THEN NULL ELSE verified_at END,
            verify_error = CASE WHEN $8 THEN NULL ELSE verify_error END
         WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&prepared.name)
    .bind(prepared.config.to_string())
    .bind(secrets)
    .bind(prepared.delivery_mode)
    .bind(now_ms())
    .bind(recheck)
    .execute(&mut *tx)
    .await
    .map_err(name_taken)?;
    tx.commit().await.context("commit storage profile")?;
    Ok(())
}

async fn has_blobs(conn: &mut SqliteConnection, tenant_id: &str, id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM blob_locations WHERE tenant_id = $1 AND storage_id = $2)",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_one(conn)
    .await
    .context("count blobs")?)
}

/// Removes a profile that holds nothing and is not the default.
///
/// # Errors
/// `NotFound`, `Conflict("storage_is_default")`, `Conflict("storage_in_use")` when blobs,
/// libraries, collections or books use it.
pub async fn delete(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    row(&mut *tx, tenant_id, id).await?;
    if default_id(db, tenant_id).await?.as_deref() == Some(id) {
        return Err(BerthError::Conflict("storage_is_default".into()));
    }
    let referenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM libraries WHERE tenant_id = $1 AND default_storage_id = $2)
             OR EXISTS (SELECT 1 FROM collections WHERE tenant_id = $1 AND storage_id = $2)
             OR EXISTS (SELECT 1 FROM books WHERE tenant_id = $1 AND storage_id = $2)",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .context("count references")?;
    if referenced || has_blobs(&mut tx, tenant_id, id).await? {
        return Err(BerthError::Conflict("storage_in_use".into()));
    }
    sqlx::query("UPDATE storage_profiles SET deleted_at = $3 WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(now_ms())
        .execute(&mut *tx)
        .await
        .context("delete storage profile")?;
    tx.commit().await.context("commit storage profile")?;
    tracing::info!(profile = %id, "storage profile deleted");
    Ok(())
}

/// Makes the profile the tenant's default.
///
/// # Errors
/// `NotFound`.
pub async fn set_default(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    row(&mut *tx, tenant_id, id).await?;
    db::put_tenant_setting(&mut tx, tenant_id, DEFAULT_KEY, &Value::String(id.into()))
        .await
        .context("save default storage")?;
    tx.commit().await.context("commit default storage")?;
    Ok(())
}

/// A profile ready to open (a deleted profile still opens: its blobs may remain).
async fn load_backend(state: &AppState, tenant_id: &str, id: &str) -> anyhow::Result<Backend> {
    let row: Row = sqlx::query_as(
        "SELECT id, name, kind, config, secrets_encrypted, delivery_mode, verified_at,
            verify_error, created_at FROM storage_profiles WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_one(&state.db)
    .await
    .context("load storage profile")?;
    backend(&state.secret, &row)
}

/// Opens the blob store of a profile (a deleted profile still opens: its blobs may remain).
pub(crate) async fn open(
    state: &AppState,
    tenant_id: &str,
    id: &str,
) -> anyhow::Result<storage::ObjectBlobStore> {
    storage::open(
        &load_backend(state, tenant_id, id).await?,
        &state.config.load().data_dir,
    )
}

/// The address of a blob under the profile's CDN, signed until `expires` (unix seconds) when
/// the CDN signs links.
pub(crate) async fn cdn_link(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    hash: &storage::ContentHash,
    expires: u64,
) -> anyhow::Result<String> {
    let Backend::S3(config, keys) = load_backend(state, tenant_id, id).await? else {
        anyhow::bail!("only S3 profiles have a CDN");
    };
    let url = storage::cdn_link(&config.cdn_url, tenant_id, hash);
    match config.cdn_signing {
        CdnSigning::None => Ok(url),
        CdnSigning::Bunny => storage::bunny_sign(&url, &keys.cdn_token_key, expires),
    }
}

/// Checks the connection (write, read back, delete a probe) and stores the result.
///
/// # Errors
/// `NotFound`; a failed check is not an error, it is in `verify_error`.
pub async fn verify(state: &AppState, tenant_id: &str, id: &str) -> Result<Profile> {
    let row = row(&state.db, tenant_id, id).await?;
    let result = check(
        &backend(&state.secret, &row)?,
        &state.config.load().data_dir,
        tenant_id,
        row.delivery_mode == "cdn",
    )
    .await;
    let now = now_ms();
    sqlx::query(
        "UPDATE storage_profiles SET verified_at = $3, verify_error = $4
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(result.is_ok().then_some(now))
    .bind(result.err())
    .execute(&state.db)
    .await
    .context("save storage check")?;
    get(&state.db, tenant_id, id).await
}

async fn check(
    backend: &Backend,
    data_dir: &std::path::Path,
    tenant_id: &str,
    cdn: bool,
) -> std::result::Result<(), &'static str> {
    let store = storage::open(backend, data_dir).map_err(|err| {
        tracing::info!(error = %err, "storage cannot be opened");
        match backend {
            Backend::Local(_) => "storage_local_unwritable",
            Backend::S3(..) => "storage_endpoint_invalid",
        }
    })?;
    store.verify(tenant_id).await?;
    match backend {
        Backend::S3(config, keys) if cdn => {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|_| "storage_failed")?;
            let token_key = (!config.cdn_signing.is_none()).then_some(keys.cdn_token_key.as_str());
            store
                .verify_cdn(tenant_id, &config.cdn_url, token_key, &client)
                .await
        }
        _ => Ok(()),
    }
}

/// Setup: validates and checks the first profile before the tenant exists.
///
/// # Errors
/// The `Validation` codes of [`create`], or the check's error code as `Validation`.
pub(crate) async fn prepare_first(
    state: &AppState,
    tenant_id: &str,
    input: &ProfileInput,
) -> Result<FirstProfile> {
    let prepared = prepare(input)?;
    let backend = match prepared.kind {
        "s3" => Backend::S3(
            serde_json::from_value(prepared.config.clone()).context("s3 config")?,
            prepared.keys(None)?,
        ),
        _ => {
            Backend::Local(serde_json::from_value(prepared.config.clone()).context("local config")?)
        }
    };
    let cdn = prepared.delivery_mode == "cdn";
    check(&backend, &state.config.load().data_dir, tenant_id, cdn)
        .await
        .map_err(|code| BerthError::Validation(code.into()))?;
    Ok(FirstProfile(prepared))
}

/// A checked profile for setup.
pub(crate) struct FirstProfile(Prepared);

/// Setup: inserts the checked profile as the tenant's default inside the setup transaction.
pub(crate) async fn insert_first(
    conn: &mut SqliteConnection,
    secret: &SecretKey,
    tenant_id: &str,
    first: &FirstProfile,
) -> Result<()> {
    let id = uuid::Uuid::now_v7().to_string();
    insert(&mut *conn, secret, tenant_id, &id, &first.0, Some(now_ms())).await?;
    db::put_tenant_setting(conn, tenant_id, DEFAULT_KEY, &Value::String(id))
        .await
        .context("save default storage")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3() -> ProfileInput {
        ProfileInput {
            name: "Bucket".into(),
            kind: "s3".into(),
            endpoint: "http://127.0.0.1:9000/".into(),
            bucket: "books".into(),
            access_key_id: "id".into(),
            secret_access_key: "key".into(),
            ..ProfileInput::default()
        }
    }

    fn code(input: &ProfileInput) -> String {
        match prepare(input) {
            Err(BerthError::Validation(code)) => code,
            _ => panic!("accepted"),
        }
    }

    #[test]
    fn validates_input() {
        let local = prepare(&ProfileInput::local_default()).unwrap();
        assert_eq!((local.kind, local.delivery_mode), ("local", "proxy"));
        assert_eq!(local.config["path"], "blobs");

        let ok = prepare(&s3()).unwrap();
        assert_eq!(ok.delivery_mode, "presigned");
        assert_eq!(ok.config["region"], "us-east-1");
        assert_eq!(ok.config["endpoint"], "http://127.0.0.1:9000");
        assert_eq!(
            ok.config["path_style"], true,
            "an IP endpoint needs path style"
        );
        assert!(
            ok.config.get("access_key_id").is_none(),
            "keys stay out of config"
        );

        assert_eq!(
            code(&ProfileInput {
                name: " ".into(),
                ..s3()
            }),
            "storage_name_required"
        );
        assert_eq!(
            code(&ProfileInput {
                kind: "gdrive".into(),
                ..s3()
            }),
            "storage_kind_invalid"
        );
        assert_eq!(
            code(&ProfileInput {
                bucket: String::new(),
                ..s3()
            }),
            "storage_bucket_required"
        );
        assert_eq!(
            code(&ProfileInput {
                endpoint: "storage.local".into(),
                ..s3()
            }),
            "storage_endpoint_invalid"
        );
        assert_eq!(
            code(&ProfileInput {
                secret_access_key: String::new(),
                ..s3()
            }),
            "storage_keys_required"
        );
        assert_eq!(
            code(&ProfileInput {
                delivery_mode: "cdn".into(),
                ..s3()
            }),
            "storage_cdn_url_required"
        );
        assert_eq!(
            code(&ProfileInput {
                delivery_mode: "cdn".into(),
                cdn_url: "https://cdn.example.com/?x=1".into(),
                ..s3()
            }),
            "storage_cdn_url_invalid"
        );
        assert_eq!(
            code(&ProfileInput {
                delivery_mode: "ftp".into(),
                ..s3()
            }),
            "storage_delivery_invalid"
        );
        assert_eq!(
            code(&ProfileInput {
                path: " ".into(),
                ..ProfileInput::local_default()
            }),
            "storage_path_required"
        );
        assert_eq!(
            code(&ProfileInput {
                cdn_signing: "akamai".into(),
                ..s3()
            }),
            "storage_cdn_signing_invalid"
        );
        assert_eq!(
            code(&ProfileInput {
                cdn_signing: "bunny".into(),
                ..s3()
            }),
            "storage_cdn_url_required"
        );
    }

    fn key_code(prepared: &Prepared, stored: Option<S3Keys>) -> String {
        match prepared.keys(stored) {
            Err(BerthError::Validation(code)) => code,
            _ => panic!("accepted"),
        }
    }

    #[test]
    fn merges_cdn_token_keys() {
        let bunny = ProfileInput {
            delivery_mode: "cdn".into(),
            cdn_url: "https://zone.b-cdn.net".into(),
            cdn_signing: "bunny".into(),
            ..s3()
        };
        let unsigned = prepare(&s3()).unwrap();
        assert!(
            unsigned.config.get("cdn_signing").is_none(),
            "old configs stay the same"
        );

        let new = prepare(&ProfileInput {
            cdn_token_key: " zone-key ".into(),
            ..bunny.clone()
        })
        .unwrap();
        assert_eq!(new.config["cdn_signing"], "bunny");
        let stored = new.keys(None).unwrap();
        assert_eq!(stored.cdn_token_key, "zone-key");
        assert_eq!(
            key_code(&prepare(&bunny).unwrap(), None),
            "storage_cdn_token_required"
        );

        // Editing: empty keys keep the stored ones, new access keys keep the token key.
        let keep = prepare(&ProfileInput {
            access_key_id: String::new(),
            secret_access_key: String::new(),
            ..bunny.clone()
        })
        .unwrap();
        assert!(keep.keys(Some(stored.clone())).unwrap() == stored);
        let rotated = prepare(&ProfileInput {
            access_key_id: "id2".into(),
            secret_access_key: "key2".into(),
            ..bunny
        })
        .unwrap()
        .keys(Some(stored.clone()))
        .unwrap();
        assert_eq!(
            (
                rotated.access_key_id.as_str(),
                rotated.cdn_token_key.as_str()
            ),
            ("id2", "zone-key")
        );
        assert!(
            unsigned
                .keys(Some(stored))
                .unwrap()
                .cdn_token_key
                .is_empty(),
            "turning signing off drops the key"
        );
    }
}
