//! Audio delivery and listening progress for devices.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use berth_core::BerthError;
use object_store::GetRange;
use serde::Deserialize;
use utoipa::IntoParams;

use super::auth::{Authenticated, Body as JsonBody};
use crate::{
    AppState,
    error::{ApiError, Problem},
    service::{
        books::PolicyFilter,
        delivery::{self, DownloadManifest, StreamLink},
        progress::{self, Positions, ProgressBatch},
        storage as profiles,
    },
    storage::ContentHash,
};

/// `GET /v1/books/{id}/files/{idx}/stream` query.
#[derive(Deserialize, IntoParams)]
pub(crate) struct StreamQuery {
    /// `1`: answer with a 302 to the URL instead of JSON, for simple clients.
    #[serde(default)]
    redirect: u8,
}

/// A short-lived URL for file `idx` of a book, playable without authorization headers: a
/// presigned S3 GET or a signed `/v1/blobs/{token}` (relative to the server address). Do not
/// keep it: ask again when it expires or fails.
#[utoipa::path(
    get,
    path = "/v1/books/{id}/files/{idx}/stream",
    tag = "delivery",
    params(
        ("id" = String, Path, description = "Book id"),
        ("idx" = i64, Path, description = "File index"),
        StreamQuery,
    ),
    responses(
        (status = OK, description = "Where to fetch the file", body = StreamLink),
        (status = FOUND, description = "With `redirect=1`: `Location` is the URL"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: no such book or file", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn stream(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path((id, idx)): Path<(String, i64)>,
    Query(query): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    let link = delivery::stream(
        &state,
        &device.tenant_id,
        &device.device_id,
        &policy,
        &id,
        idx,
    )
    .await?;
    Ok(if query.redirect == 1 {
        (StatusCode::FOUND, [(header::LOCATION, link.url)]).into_response()
    } else {
        Json(link).into_response()
    })
}

/// Files with URLs, sizes, hashes and chapters to keep a book on the device. Verify each file
/// by its sha256; to resume a download whose URL expired, ask `/stream` for that file.
#[utoipa::path(
    get,
    path = "/v1/books/{id}/download-manifest",
    tag = "delivery",
    params(("id" = String, Path, description = "Book id")),
    responses(
        (status = OK, description = "The manifest", body = DownloadManifest),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: no such book", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn download_manifest(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<DownloadManifest>, ApiError> {
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    let manifest =
        delivery::manifest(&state, &device.tenant_id, &device.device_id, &policy, &id).await?;
    Ok(Json(manifest))
}

/// Bytes of a blob by a signed token from `/stream`, with range requests; no authorization
/// header. Stops working when the token expires or the device is revoked.
#[utoipa::path(
    get,
    path = "/v1/blobs/{token}",
    tag = "delivery",
    security(()),
    params(("token" = String, Path, description = "Signed token from a stream URL")),
    responses(
        (status = OK, description = "The whole file", content_type = "audio/*"),
        (status = PARTIAL_CONTENT, description = "The requested `Range`", content_type = "audio/*"),
        (status = RANGE_NOT_SATISFIABLE, description = "The range starts past the end"),
        (status = UNAUTHORIZED, description = "`unauthenticated` (bad or expired token) or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`: the bytes are gone", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn blob(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let grant = delivery::open_token(&state.db, &state.secret, &token).await?;
    let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT l.storage_id, b.codec, b.size FROM blob_locations l
         JOIN blobs b ON b.tenant_id = l.tenant_id AND b.hash = l.hash
         WHERE l.tenant_id = $1 AND l.hash = $2 AND l.state = 'active' LIMIT 1",
    )
    .bind(&grant.tenant_id)
    .bind(&grant.hash)
    .fetch_optional(&state.db)
    .await
    .map_err(anyhow::Error::from)?;
    let (storage_id, codec, size) = row.ok_or(BerthError::NotFound)?;
    let size = u64::try_from(size).unwrap_or(0);
    let requested = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map_or(Ranged::Whole, |v| parse_range(v, size));
    let range = match requested {
        Ranged::Whole => None,
        Ranged::Part(range) => Some(range),
        Ranged::Unsatisfiable => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response());
        }
    };
    let store = profiles::open(&state, &grant.tenant_id, &storage_id).await?;
    let hash = ContentHash::parse(&grant.hash).ok_or(BerthError::NotFound)?;
    let (stream, got, _) = store
        .get_range(
            &grant.tenant_id,
            &hash,
            range.clone().map(GetRange::Bounded),
        )
        .await
        .map_err(|err| match err {
            object_store::Error::NotFound { .. } => BerthError::NotFound.into(),
            err => ApiError::from(anyhow::Error::from(err)),
        })?;
    let mime = HeaderValue::from_static(mime(codec.as_deref()));
    let mut response = (
        [
            (header::CONTENT_TYPE, mime),
            (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
            (
                header::CONTENT_LENGTH,
                HeaderValue::from(got.end - got.start),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=3600"),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response();
    if range.is_some() {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{size}", got.start, got.end - 1))
                .map_err(anyhow::Error::from)?,
        );
    }
    Ok(response)
}

/// What a `Range` header asks for.
#[derive(Debug, PartialEq, Eq)]
enum Ranged {
    /// No usable range (malformed, or several ranges): the whole file.
    Whole,
    /// One range inside the file.
    Part(std::ops::Range<u64>),
    /// Starts past the end.
    Unsatisfiable,
}

/// A single `bytes=` range of a `size`-byte file.
fn parse_range(header: &str, size: u64) -> Ranged {
    parse_spec(header, size).unwrap_or(Ranged::Whole)
}

fn parse_spec(header: &str, size: u64) -> Option<Ranged> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    let range = if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 {
            return Some(Ranged::Unsatisfiable);
        }
        size.saturating_sub(suffix)..size
    } else {
        let start: u64 = start.parse().ok()?;
        let end = if end.is_empty() {
            size
        } else {
            let last: u64 = end.parse().ok()?;
            if last < start {
                return None;
            }
            (last + 1).min(size)
        };
        start..end
    };
    Some(if range.start < size {
        Ranged::Part(range)
    } else {
        Ranged::Unsatisfiable
    })
}

/// Media type by the stored file extension.
fn mime(codec: Option<&str>) -> &'static str {
    match codec.unwrap_or_default() {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" | "mp4" | "aac" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Sends events of the device's offline progress queue. The batch is accepted as a whole:
/// drop it from the queue after 200. Resending an event (same `event_id`) is a no-op. The
/// reply has the user's current positions.
#[utoipa::path(
    post,
    path = "/v1/progress/batch",
    tag = "progress",
    request_body = ProgressBatch,
    responses(
        (status = OK, description = "Accepted; current positions", body = Positions),
        (status = BAD_REQUEST, description = "`invalid_body`, `progress_batch_too_long` or `progress_event_invalid`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn progress_batch(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    JsonBody(batch): JsonBody<ProgressBatch>,
) -> Result<Json<Positions>, ApiError> {
    progress::record(&state.db, &device, &batch).await?;
    Ok(Json(
        progress::list(&state.db, &device.tenant_id, &device.user_id).await?,
    ))
}

/// The user's current position in every book, as other devices left it. A device applies a
/// position only when it is newer than its own (compare in one clock with `server_now`).
#[utoipa::path(
    get,
    path = "/v1/progress",
    tag = "progress",
    responses(
        (status = OK, description = "Current positions", body = Positions),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn progress(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
) -> Result<Json<Positions>, ApiError> {
    Ok(Json(
        progress::list(&state.db, &device.tenant_id, &device.user_id).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Ranged::Part(0..100));
        assert_eq!(parse_range("bytes=900-", 1000), Ranged::Part(900..1000));
        assert_eq!(parse_range("bytes=-100", 1000), Ranged::Part(900..1000));
        assert_eq!(parse_range("bytes=990-2000", 1000), Ranged::Part(990..1000));
        assert_eq!(parse_range("bytes=1000-", 1000), Ranged::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), Ranged::Whole);
        assert_eq!(parse_range("items=0-1", 1000), Ranged::Whole);
        assert_eq!(parse_range("bytes=9-1", 1000), Ranged::Whole);
    }
}
