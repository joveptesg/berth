//! Storage profiles for devices with the `admin` scope: thin wrappers over
//! `crate::service::storage`. Keys are write-only.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::auth::{Authenticated, Body};
use crate::{
    AppState,
    error::{ApiError, Problem},
    service::storage::{self, Profile, ProfileInput},
};

/// Where the bytes are.
#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StorageKind {
    /// Folder on the server.
    Local,
    /// S3-compatible bucket.
    S3,
}

/// How clients get the bytes.
#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DeliveryMode {
    /// Through the server (`/v1/blobs/{token}`); the only mode of local storage.
    Proxy,
    /// Presigned URLs straight to S3.
    Presigned,
    /// Public URLs under `cdn_url` (a CDN or a public bucket); they need no signature.
    Cdn,
}

/// Create or replace a storage profile.
#[derive(Deserialize, ToSchema)]
pub(crate) struct StorageProfileRequest {
    /// Display name, unique within the server (64 characters at most).
    name: String,
    /// Type; cannot change on update.
    kind: StorageKind,
    /// `local`: folder, absolute or relative to the data directory.
    #[serde(default)]
    path: String,
    /// `s3`: service address; empty or absent means AWS.
    #[serde(default)]
    endpoint: String,
    /// `s3`: bucket.
    #[serde(default)]
    bucket: String,
    /// `s3`: region; empty means `us-east-1` (`auto` for R2).
    #[serde(default)]
    region: String,
    /// `s3`: `endpoint/bucket/key` addressing (usually `MinIO`).
    #[serde(default)]
    path_style: bool,
    /// `s3`: `presigned` (default), `proxy` or `cdn`; local storage is always `proxy`.
    delivery_mode: Option<DeliveryMode>,
    /// `s3`: public http(s) address of the bucket root, required for `cdn`; the blob at key
    /// `k` must be readable at `cdn_url/k` without signing.
    #[serde(default)]
    cdn_url: String,
    /// `s3`: access key id; required on create, absent on update keeps the saved keys.
    access_key_id: Option<String>,
    /// `s3`: secret access key; sent together with `access_key_id`. Never returned.
    secret_access_key: Option<String>,
}

impl StorageProfileRequest {
    fn input(self) -> ProfileInput {
        let delivery_mode = match self.delivery_mode {
            None => String::new(),
            Some(DeliveryMode::Proxy) => "proxy".into(),
            Some(DeliveryMode::Presigned) => "presigned".into(),
            Some(DeliveryMode::Cdn) => "cdn".into(),
        };
        ProfileInput {
            name: self.name,
            kind: match self.kind {
                StorageKind::Local => "local",
                StorageKind::S3 => "s3",
            }
            .into(),
            path: self.path,
            endpoint: self.endpoint,
            bucket: self.bucket,
            region: self.region,
            path_style: self.path_style,
            delivery_mode,
            cdn_url: self.cdn_url,
            access_key_id: self.access_key_id.unwrap_or_default(),
            secret_access_key: self.secret_access_key.unwrap_or_default(),
        }
    }
}

/// A storage profile; keys are never returned.
#[derive(Serialize, ToSchema)]
pub(crate) struct StorageProfile {
    /// Profile id.
    id: String,
    /// Display name.
    name: String,
    /// Type.
    #[schema(value_type = StorageKind)]
    kind: String,
    /// How clients get the bytes.
    #[schema(value_type = DeliveryMode)]
    delivery_mode: String,
    /// `local`: folder as entered.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// `s3`: service address; empty means AWS.
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// `s3`: bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    bucket: Option<String>,
    /// `s3`: region.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    /// `s3`: path-style addressing.
    #[serde(skip_serializing_if = "Option::is_none")]
    path_style: Option<bool>,
    /// `s3`: public address of the bucket root; empty when unused.
    #[serde(skip_serializing_if = "Option::is_none")]
    cdn_url: Option<String>,
    /// New books go here.
    is_default: bool,
    /// When the last connection check passed, epoch milliseconds.
    verified_at: Option<i64>,
    /// Code of the last failed check: `storage_auth_failed`, `storage_access_denied`,
    /// `storage_bucket_not_found`, `storage_unreachable`, `storage_mismatch`,
    /// `storage_local_unwritable`, `storage_endpoint_invalid`, `storage_cdn_unreachable`,
    /// `storage_cdn_mismatch` or `storage_failed`.
    verify_error: Option<String>,
    /// Creation time, epoch milliseconds.
    created_at: i64,
}

impl From<Profile> for StorageProfile {
    fn from(p: Profile) -> Self {
        let local = p.kind == "local";
        let s3 = |value: String| (!local).then_some(value);
        Self {
            path: local.then_some(p.path),
            endpoint: s3(p.endpoint),
            bucket: s3(p.bucket),
            region: s3(p.region),
            path_style: (!local).then_some(p.path_style),
            cdn_url: s3(p.cdn_url),
            id: p.id,
            name: p.name,
            kind: p.kind,
            delivery_mode: p.delivery_mode,
            is_default: p.is_default,
            verified_at: p.verified_at,
            verify_error: p.verify_error,
            created_at: p.created_at,
        }
    }
}

/// List storage profiles, oldest first.
#[utoipa::path(
    get,
    path = "/v1/admin/storage-profiles",
    tag = "admin",
    responses(
        (status = OK, description = "Profiles", body = Vec<StorageProfile>),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn list_storage_profiles(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
) -> Result<Json<Vec<StorageProfile>>, ApiError> {
    device.require("admin")?;
    let profiles = storage::list(&state.db, &device.tenant_id).await?;
    Ok(Json(profiles.into_iter().map(Into::into).collect()))
}

/// Add a storage profile. It is not checked: call `verify` next.
#[utoipa::path(
    post,
    path = "/v1/admin/storage-profiles",
    tag = "admin",
    request_body = StorageProfileRequest,
    responses(
        (status = CREATED, description = "Created profile", body = StorageProfile),
        (status = BAD_REQUEST, description = "`validation`; `detail`: `storage_name_required`, `storage_name_too_long`, `storage_name_taken`, `storage_path_required`, `storage_bucket_required`, `storage_endpoint_invalid`, `storage_delivery_invalid`, `storage_keys_required` or `invalid_body`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn create_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Body(req): Body<StorageProfileRequest>,
) -> Result<(StatusCode, Json<StorageProfile>), ApiError> {
    device.require("admin")?;
    let id = storage::create(&state, &device.tenant_id, &req.input()).await?;
    let profile = storage::get(&state.db, &device.tenant_id, &id).await?;
    Ok((StatusCode::CREATED, Json(profile.into())))
}

/// One storage profile.
#[utoipa::path(
    get,
    path = "/v1/admin/storage-profiles/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Profile id")),
    responses(
        (status = OK, description = "Profile", body = StorageProfile),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<StorageProfile>, ApiError> {
    device.require("admin")?;
    Ok(Json(
        storage::get(&state.db, &device.tenant_id, &id)
            .await?
            .into(),
    ))
}

/// Replace a storage profile; absent keys keep the saved ones. A new location or new keys
/// clear the last check.
#[utoipa::path(
    put,
    path = "/v1/admin/storage-profiles/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Profile id")),
    request_body = StorageProfileRequest,
    responses(
        (status = OK, description = "Updated profile", body = StorageProfile),
        (status = BAD_REQUEST, description = "`validation`; `detail` as for create, or `storage_kind_invalid` when the type changes", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
        (status = CONFLICT, description = "`conflict`; `detail`: `storage_in_use` (blobs are stored at the old location)", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn update_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
    Body(req): Body<StorageProfileRequest>,
) -> Result<Json<StorageProfile>, ApiError> {
    device.require("admin")?;
    storage::update(&state, &device.tenant_id, &id, &req.input()).await?;
    Ok(Json(
        storage::get(&state.db, &device.tenant_id, &id)
            .await?
            .into(),
    ))
}

/// Delete a storage profile that holds nothing and is not the default.
#[utoipa::path(
    delete,
    path = "/v1/admin/storage-profiles/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "Profile id")),
    responses(
        (status = NO_CONTENT, description = "Deleted"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
        (status = CONFLICT, description = "`conflict`; `detail`: `storage_is_default` or `storage_in_use`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn delete_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    device.require("admin")?;
    storage::delete(&state.db, &device.tenant_id, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Check the connection: write, read back and delete a probe object. The result is in
/// `verified_at` / `verify_error`; a failed check is not an HTTP error.
#[utoipa::path(
    post,
    path = "/v1/admin/storage-profiles/{id}/verify",
    tag = "admin",
    params(("id" = String, Path, description = "Profile id")),
    responses(
        (status = OK, description = "Profile with the check result", body = StorageProfile),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn verify_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<StorageProfile>, ApiError> {
    device.require("admin")?;
    Ok(Json(
        storage::verify(&state, &device.tenant_id, &id)
            .await?
            .into(),
    ))
}

/// Make the profile the default: new books go there.
#[utoipa::path(
    post,
    path = "/v1/admin/storage-profiles/{id}/default",
    tag = "admin",
    params(("id" = String, Path, description = "Profile id")),
    responses(
        (status = OK, description = "The new default profile", body = StorageProfile),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: the device has no `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn default_storage_profile(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<StorageProfile>, ApiError> {
    device.require("admin")?;
    storage::set_default(&state.db, &device.tenant_id, &id).await?;
    Ok(Json(
        storage::get(&state.db, &device.tenant_id, &id)
            .await?
            .into(),
    ))
}
