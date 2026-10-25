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

#[derive(Deserialize, Default)]
pub(crate) struct PageQuery {
    /// `history` for the saved uploads, else the drop zone and the queue.
    #[serde(default)]
    view: String,
    #[serde(default)]
    page: Option<i64>,
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    axum::extract::Query(query): axum::extract::Query<PageQuery>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    if query.view == "history" {
        let mut page = query.page.unwrap_or(1).max(1);
        let (mut uploads, total) = ingest::history(&state.db, tenant_id, page).await?;
        let pages = ((total + ingest::HISTORY_PAGE - 1) / ingest::HISTORY_PAGE).max(1);
        // A page past the end (uploads went since) shows the last one.
        if page > pages {
            page = pages;
            uploads = ingest::history(&state.db, tenant_id, page).await?.0;
        }
        return render(
            &state,
            "upload.html",
            context! {
                view => "history", uploads => serde(uploads), total, page, pages,
                ..shell(&state, "upload")
            },
        );
    }
    let uploads = ingest::queue(&state.db, tenant_id).await?;
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
            view => "upload", uploads => serde(uploads), presigned, ratings => RATINGS.to_vec(),
            categories => serde(categories),
            languages => serde(languages::list(&state.db, tenant_id).await?),
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
            categories => serde(categories),
            languages => serde(languages::list(&state.db, tenant_id).await?),
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

/// What the browser reports when a file did not get through.
#[derive(Deserialize)]
pub(crate) struct SendFailure {
    file: i64,
    /// The file went to its presigned URL, not to the server.
    presigned: bool,
    reason: String,
}

pub(crate) async fn fail(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
    Json(req): Json<SendFailure>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    ingest::send_failed(
        &state.db,
        tenant_id,
        &id,
        req.file,
        req.presigned,
        &req.reason,
    )
    .await?;
    let upload = ingest::get(&state.db, tenant_id, &id).await?;
    card(&state, tenant_id, &upload, None, None).await
}

pub(crate) async fn retry(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let error = match ingest::retry(&state, tenant_id, &id).await {
        Ok(()) => None,
        Err(BerthError::Conflict(code)) => Some(code),
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
