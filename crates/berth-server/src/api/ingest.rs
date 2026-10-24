//! Book uploads for devices with the `upload` or `admin` scope: thin wrappers over
//! `crate::service::ingest`.

use axum::{
    Json,
    body::Body,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::auth::{Authenticated, Body as JsonBody};
use crate::{
    AppState,
    error::{ApiError, Problem},
    service::{
        books::BookEdits,
        ingest::{self, Created, NewFile, Upload},
        tokens::DeviceAuth,
    },
};

fn may_upload(device: &DeviceAuth) -> Result<(), ApiError> {
    if device.require("upload").is_ok() {
        return Ok(());
    }
    Ok(device.require("admin")?)
}

/// A file about to be uploaded.
#[derive(Deserialize, ToSchema)]
pub(crate) struct UploadFile {
    /// Path inside the dropped folder, e.g. `Book/01.mp3`.
    name: String,
    /// Bytes.
    size: u64,
    /// Hex sha256; required when the storage takes presigned uploads.
    sha256: Option<String>,
}

/// `POST /v1/ingest/uploads` request.
#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateUpload {
    /// The dropped folder or file name; the title is guessed from it when tags are empty.
    name: String,
    /// Audio files (mp3, m4b, m4a, aac, ogg, opus, flac, wav) and cover images (jpg, png, webp).
    files: Vec<UploadFile>,
}

/// Start an upload of one book. Each file gets a target: `server` (PUT it to
/// `/v1/ingest/uploads/{id}/files/{idx}`), `presigned` (PUT it to `url` with `headers`; S3
/// rejects other bytes) or `stored` (already on the server).
#[utoipa::path(
    post,
    path = "/v1/ingest/uploads",
    tag = "ingest",
    request_body = CreateUpload,
    responses(
        (status = CREATED, description = "Upload and file targets", body = Created),
        (status = BAD_REQUEST, description = "`validation`; `detail`: `upload_empty`, `upload_too_many`, `upload_file_unsupported`, `upload_no_audio`, `upload_hash_invalid`, `upload_hash_required` or `invalid_body`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: no `upload` or `admin` scope", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn create_upload(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    JsonBody(req): JsonBody<CreateUpload>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    may_upload(&device)?;
    let files: Vec<NewFile> = req
        .files
        .into_iter()
        .map(|f| NewFile {
            name: f.name,
            size: f.size,
            sha256: f.sha256,
        })
        .collect();
    let created = ingest::create(&state, &device.tenant_id, &req.name, &files).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Send one file through the server (target `server`). The body is the file.
#[utoipa::path(
    put,
    path = "/v1/ingest/uploads/{id}/files/{idx}",
    tag = "ingest",
    params(
        ("id" = String, Path, description = "Upload id"),
        ("idx" = i64, Path, description = "File index in the create request"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = NO_CONTENT, description = "Stored"),
        (status = BAD_REQUEST, description = "`validation`; `detail`: `upload_size_mismatch` or `upload_hash_mismatch`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: no `upload` or `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
        (status = CONFLICT, description = "`conflict`; `detail`: `upload_closed`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn put_file(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path((id, idx)): Path<(String, i64)>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    may_upload(&device)?;
    ingest::put_file(&state, &device.tenant_id, &id, idx, body.into_data_stream()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// All files are sent: the server checks presigned objects and starts reading the files.
/// Poll the upload until `ready`, then save it.
#[utoipa::path(
    post,
    path = "/v1/ingest/uploads/{id}/complete",
    tag = "ingest",
    params(("id" = String, Path, description = "Upload id")),
    responses(
        (status = ACCEPTED, description = "Being analysed", body = Upload),
        (status = BAD_REQUEST, description = "`validation`; `detail`: `upload_file_missing`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: no `upload` or `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
        (status = CONFLICT, description = "`conflict`; `detail`: `upload_closed`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn complete_upload(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Upload>), ApiError> {
    may_upload(&device)?;
    ingest::complete(&state, &device.tenant_id, &id).await?;
    let upload = ingest::get(&state.db, &device.tenant_id, &id).await?;
    Ok((StatusCode::ACCEPTED, Json(upload)))
}

/// An upload: its status and, once `ready`, the proposed book.
#[utoipa::path(
    get,
    path = "/v1/ingest/uploads/{id}",
    tag = "ingest",
    params(("id" = String, Path, description = "Upload id")),
    responses(
        (status = OK, description = "Upload", body = Upload),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: no `upload` or `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn get_upload(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<Upload>, ApiError> {
    may_upload(&device)?;
    Ok(Json(ingest::get(&state.db, &device.tenant_id, &id).await?))
}

/// The book metadata to save; starts from the proposal.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveBook {
    /// Title.
    title: String,
    /// Authors in order.
    #[serde(default)]
    authors: Vec<String>,
    /// Narrators in order.
    #[serde(default)]
    narrators: Vec<String>,
    /// Series name.
    #[serde(default)]
    series: String,
    /// Number in the series.
    series_no: Option<f64>,
    /// Age rating: 0, 6, 12, 16 or 18.
    rating: Option<i64>,
    /// Section ids; omitted, the sections the proposal matched.
    category_ids: Option<Vec<String>>,
    /// Language code (`""` for none); omitted, the language of the proposal.
    language: Option<String>,
}

/// The created book.
#[derive(Serialize, ToSchema)]
pub(crate) struct SavedBook {
    /// Book id.
    book_id: String,
}

/// Create the book from a `ready` upload.
#[utoipa::path(
    post,
    path = "/v1/ingest/uploads/{id}/save",
    tag = "ingest",
    params(("id" = String, Path, description = "Upload id")),
    request_body = SaveBook,
    responses(
        (status = CREATED, description = "Book created", body = SavedBook),
        (status = BAD_REQUEST, description = "`validation`; `detail`: `book_title_required`, `book_title_too_long`, `book_rating_invalid`, `book_language_invalid` or `invalid_body`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
        (status = FORBIDDEN, description = "`forbidden`: no `upload` or `admin` scope", body = Problem, content_type = "application/problem+json"),
        (status = NOT_FOUND, description = "`not_found`", body = Problem, content_type = "application/problem+json"),
        (status = CONFLICT, description = "`conflict`; `detail`: `upload_not_ready`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn save_upload(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    Path(id): Path<String>,
    JsonBody(req): JsonBody<SaveBook>,
) -> Result<(StatusCode, Json<SavedBook>), ApiError> {
    may_upload(&device)?;
    let draft = ingest::get(&state.db, &device.tenant_id, &id)
        .await?
        .draft
        .unwrap_or_default();
    let category_ids = req.category_ids.unwrap_or(draft.category_ids);
    let language = req.language.or(draft.language);
    let edits = BookEdits {
        title: req.title,
        authors: req.authors,
        narrators: req.narrators,
        series: req.series,
        series_no: req.series_no,
        rating: req.rating,
        category_ids,
        language,
    };
    let book_id = ingest::save(&state, &device.tenant_id, &id, &edits).await?;
    Ok((StatusCode::CREATED, Json(SavedBook { book_id })))
}
