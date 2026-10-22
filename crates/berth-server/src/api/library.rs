//! Library for devices: the sync delta and book covers.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use berth_core::BerthError;
use serde::Deserialize;
use utoipa::IntoParams;

use super::auth::Authenticated;
use crate::{
    AppState,
    error::{ApiError, Problem},
    service::{
        blobs,
        books::{self, PolicyFilter, SyncPage},
    },
};

/// `GET /v1/sync` query.
#[derive(Deserialize, IntoParams)]
pub(crate) struct SyncQuery {
    /// `cursor` of the previous page; absent for a full slice.
    cursor: Option<String>,
}

/// Library changes since `cursor`: new and changed books with their files and chapters,
/// ids of removed ones. Without a cursor, or when the reader's access changed, the reply is
/// a full slice with `reset`: drop every book these pages do not bring. Repeat while
/// `has_more`. Responses are gzip-compressed for clients that accept it.
#[utoipa::path(
    get,
    path = "/v1/sync",
    tag = "library",
    params(SyncQuery),
    responses(
        (status = OK, description = "One page of the delta", body = SyncPage),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn sync(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Query(query): Query<SyncQuery>,
) -> Result<Json<SyncPage>, ApiError> {
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    let page = books::sync(
        &state.db,
        &device.tenant_id,
        &policy,
        query.cursor.as_deref(),
    )
    .await?;
    Ok(Json(page))
}

/// The cover image of a book. The URL changes with the cover (`cover_hash` in the sync), so
/// it can be cached forever.
#[utoipa::path(
    get,
    path = "/v1/books/{id}/cover",
    tag = "library",
    params(("id" = String, Path, description = "Book id")),
    responses(
        (status = OK, description = "JPEG, PNG or WebP", content_type = "image/*"),
        (status = NOT_MODIFIED, description = "`If-None-Match` matches"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: no such book or no cover", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn cover(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    let hash = books::cover_hash(&state.db, &device.tenant_id, &policy, &id).await?;
    serve_image(&state, &device.tenant_id, &hash, &headers).await
}

/// An image blob with caching headers; shared with the admin.
pub(crate) async fn serve_image(
    state: &AppState,
    tenant_id: &str,
    hash: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let etag = format!("\"{hash}\"");
    let cache = [
        (
            header::ETAG,
            HeaderValue::from_str(&etag).map_err(anyhow::Error::from)?,
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=31536000, immutable"),
        ),
    ];
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        return Ok((StatusCode::NOT_MODIFIED, cache).into_response());
    }
    let Some((stream, codec, size)) = blobs::read(state, tenant_id, hash).await? else {
        return Err(BerthError::NotFound.into());
    };
    let mime = codec
        .filter(|c| c.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".into());
    Ok((
        cache,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&mime).map_err(anyhow::Error::from)?,
            ),
            (header::CONTENT_LENGTH, HeaderValue::from(size)),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}
