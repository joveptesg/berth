//! Book upload page: thin wrappers over `crate::service::ingest`.
//!
//! The browser sends the files itself (progress bars need script): it asks `/upload/new`
//! for the targets, then PUTs each file here or to its presigned URL, then completes the
//! upload. Cards are server-rendered fragments that poll while the files are analysed.

use axum::{
    Extension, Form, Json,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use berth_core::BerthError;
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    library::BookForm,
    pages::{Page, form_error, render, shell},
    session::AdminSession,
};
use crate::{
    AppState,
    api::library::serve_image,
    error::ApiError,
    service::{
        books::RATINGS,
        categories,
        ingest::{self, Created, NewFile, Upload},
        languages, storage,
    },
    storage::BlobStore,
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let uploads = ingest::recent(&state.db, tenant_id, 30).await?;
    // S3 takes the files straight from the browser, signed with their sha256.
    let presigned = match storage::default_id(&state.db, tenant_id).await? {
        Some(id) => storage::open(&state, tenant_id, &id)
            .await
            .is_ok_and(|store| store.supports_presigned()),
        None => false,
    };
    let categories = categories::list(&state.db, tenant_id).await?;
    render(
        &state,
        "upload.html",
        context! {
            uploads => serde(uploads), presigned, ratings => RATINGS.to_vec(),
            categories => serde(categories), languages => languages::LANGUAGES.to_vec(),
            ..shell(&state, "upload")
        },
    )
}

#[derive(Deserialize)]
pub(crate) struct NewUploadFile {
    name: String,
    size: u64,
    sha256: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct NewUpload {
    name: String,
    files: Vec<NewUploadFile>,
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Json(req): Json<NewUpload>,
) -> Result<Json<Created>, ApiError> {
    let files: Vec<NewFile> = req
        .files
        .into_iter()
        .map(|f| NewFile {
            name: f.name,
            size: f.size,
            sha256: f.sha256,
        })
        .collect();
    Ok(Json(
        ingest::create(&state, &session.0.tenant_id, &req.name, &files).await?,
    ))
}

pub(crate) async fn put_file(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path((id, idx)): Path<(String, i64)>,
    body: Body,
) -> Result<StatusCode, ApiError> {
    ingest::put_file(
        &state,
        &session.0.tenant_id,
        &id,
        idx,
        body.into_data_stream(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn card(
    state: &AppState,
    tenant_id: &str,
    upload: &Upload,
    form: Option<Value>,
    error: Option<String>,
) -> Page {
    let categories = categories::list(&state.db, tenant_id).await?;
    render(
        state,
        "partials/upload_card.html",
        context! {
            upload => serde(upload), form, error, ratings => RATINGS.to_vec(),
            categories => serde(categories), languages => languages::LANGUAGES.to_vec(),
        },
    )
}

pub(crate) async fn complete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let error = match ingest::complete(&state, tenant_id, &id).await {
        Ok(()) => None,
        Err(BerthError::Validation(code) | BerthError::Conflict(code)) => Some(code),
        Err(err) => return Err(err.into()),
    };
    let upload = ingest::get(&state.db, tenant_id, &id).await?;
    card(&state, tenant_id, &upload, None, error).await
}

pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let upload = ingest::get(&state.db, tenant_id, &id).await?;
    card(&state, tenant_id, &upload, None, None).await
}

pub(crate) async fn save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
    Form(form): Form<BookForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = match form.edits() {
        Ok(edits) => ingest::save(&state, tenant_id, &id, &edits)
            .await
            .map(|_| ()),
        Err(err) => Err(err),
    };
    let upload = ingest::get(&state.db, tenant_id, &id).await?;
    match result {
        Ok(()) => card(&state, tenant_id, &upload, None, None).await,
        Err(err) => {
            let error = match err {
                BerthError::Conflict(code) => code,
                err => form_error(err)?,
            };
            card(&state, tenant_id, &upload, Some(serde(&form)), Some(error)).await
        }
    }
}

pub(crate) async fn discard(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    ingest::discard(&state.db, &session.0.tenant_id, &id).await?;
    // The card is swapped for nothing.
    Ok(().into_response())
}

pub(crate) async fn cover(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let upload = ingest::get(&state.db, tenant_id, &id).await?;
    let hash = upload
        .draft
        .and_then(|d| d.cover_hash)
        .ok_or(BerthError::NotFound)?;
    serve_image(&state, tenant_id, &hash, &headers).await
}
