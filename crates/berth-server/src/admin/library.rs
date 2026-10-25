//! Library and book pages: thin wrappers over `crate::service::books`.

use axum::{
    Extension, Form,
    extract::{Multipart, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect},
};
use berth_core::BerthError;
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    pages::{Page, form_error, render, render_form, shell},
    redirect,
    session::AdminSession,
};
use crate::{
    AppState,
    api::library::serve_image,
    error::ApiError,
    service::{
        ai,
        books::{self, BookEdits, BookQuery, PolicyFilter, RATINGS},
        categories, languages,
    },
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

/// `/library` opens the default library.
pub(crate) async fn default_library(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let id = books::default_library_id(&state.db, &session.0.tenant_id).await?;
    Ok(Redirect::to(&format!("/library/{id}")).into_response())
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct LibraryQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    series: String,
    #[serde(default)]
    rating: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    language: String,
    #[serde(default)]
    view: String,
}

pub(crate) async fn library(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<LibraryQuery>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let filter = BookQuery {
        search: query.q.clone(),
        author: query.author.clone(),
        series: query.series.clone(),
        category: query.category.clone(),
        language: query.language.clone(),
        rating: query.rating.parse().ok(),
    };
    let list = books::list(&state.db, tenant_id, &PolicyFilter::admin(), &id, &filter).await?;
    let (authors, series, languages) = books::facets(&state.db, tenant_id, &id).await?;
    let categories = categories::list(&state.db, tenant_id).await?;
    let filtered = !(query.q.is_empty()
        && query.author.is_empty()
        && query.series.is_empty()
        && query.category.is_empty()
        && query.language.is_empty()
        && query.rating.is_empty());
    let ctx = context! {
        library_id => id, books => serde(&list), authors, series, ratings => RATINGS.to_vec(),
        categories => serde(categories), languages => serde(languages),
        query => serde(&query), filtered, list_view => query.view == "list",
        ..shell(&state, "library")
    };
    render_form(
        &state,
        &headers,
        "library.html",
        "partials/book_grid.html",
        ctx,
    )
}

pub(super) async fn book_ctx(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    form: Option<Value>,
    error: Option<String>,
    saved: bool,
) -> Result<Value, ApiError> {
    let detail = books::get(&state.db, tenant_id, &PolicyFilter::admin(), id).await?;
    let categories = categories::list(&state.db, tenant_id).await?;
    Ok(context! {
        book => serde(&detail), form, error, saved, ratings => RATINGS.to_vec(),
        categories => serde(categories),
        languages => serde(languages::list(&state.db, tenant_id).await?),
        ai_ready => ai::settings(&state.db, tenant_id).await?.ready(),
        ..shell(state, "library")
    })
}

pub(crate) async fn book(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let ctx = book_ctx(&state, &session.0.tenant_id, &id, None, None, false).await?;
    render(&state, "book.html", ctx)
}

/// The "about the book" dialog opened from a library card.
pub(crate) async fn book_about(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let detail = books::get(&state.db, &session.0.tenant_id, &PolicyFilter::admin(), &id).await?;
    render(
        &state,
        "partials/book_about_dialog.html",
        context! { book => serde(&detail.book) },
    )
}

/// Book metadata as the forms send it (here and on upload cards).
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct BookForm {
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) authors: String,
    #[serde(default)]
    pub(super) narrators: String,
    #[serde(default)]
    pub(super) series: String,
    #[serde(default)]
    pub(super) series_no: String,
    #[serde(default)]
    pub(super) rating: String,
    /// Section ids, comma-separated.
    #[serde(default)]
    pub(super) category_ids: String,
    /// Language code; empty for none.
    #[serde(default)]
    pub(super) language: String,
    #[serde(default)]
    pub(super) description: String,
    #[serde(default)]
    pub(super) year: String,
}

impl BookForm {
    pub(super) fn edits(&self) -> Result<BookEdits, BerthError> {
        let series_no = match self.series_no.trim().replace(',', ".") {
            no if no.is_empty() => None,
            no => Some(
                no.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(|| BerthError::Validation("book_series_no_invalid".into()))?,
            ),
        };
        let year = match self.year.trim() {
            "" => None,
            year => Some(
                year.parse::<i64>()
                    .map_err(|_| BerthError::Validation("book_year_invalid".into()))?,
            ),
        };
        Ok(BookEdits {
            title: self.title.clone(),
            authors: books::split_names(&self.authors),
            narrators: books::split_names(&self.narrators),
            series: self.series.clone(),
            series_no,
            rating: self.rating.parse().ok(),
            category_ids: books::split_names(&self.category_ids),
            language: Some(self.language.clone()),
            description: Some(self.description.clone()),
            year,
        })
    }
}

pub(crate) async fn book_save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<BookForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = match form.edits() {
        Ok(edits) => books::update(&state.db, tenant_id, &id, &edits).await,
        Err(err) => Err(err),
    };
    let ctx = match result {
        Ok(()) => book_ctx(&state, tenant_id, &id, None, None, true).await?,
        Err(err) => {
            let error = form_error(err)?;
            book_ctx(
                &state,
                tenant_id,
                &id,
                Some(serde(&form)),
                Some(error),
                false,
            )
            .await?
        }
    };
    render_form(
        &state,
        &headers,
        "book.html",
        "partials/book_form.html",
        ctx,
    )
}

pub(crate) async fn book_cover_upload(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| BerthError::Validation("cover_invalid".into()))?
    {
        if field.name() == Some("cover") {
            let bytes = field
                .bytes()
                .await
                .map_err(|_| BerthError::Validation("cover_too_large".into()))?;
            data = Some(bytes.to_vec());
        }
    }
    let result = match data {
        Some(data) if !data.is_empty() => books::set_cover(&state, tenant_id, &id, data).await,
        _ => Err(BerthError::Validation("cover_invalid".into())),
    };
    let error = match result {
        Ok(()) => None,
        Err(err) => Some(form_error(err)?),
    };
    if error.is_none() && !super::is_htmx(&headers) {
        return Ok(redirect(&headers, &format!("/book/{id}")));
    }
    let ctx = book_ctx(&state, tenant_id, &id, None, None, false).await?;
    let ctx = context! { cover_error => error, ..ctx };
    render_form(
        &state,
        &headers,
        "book.html",
        "partials/book_cover.html",
        ctx,
    )
}

pub(crate) async fn book_cover(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let hash = books::cover_hash(&state.db, tenant_id, &PolicyFilter::admin(), &id).await?;
    serve_image(&state, tenant_id, &hash, &headers).await
}

pub(crate) async fn book_delete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let library = books::get(&state.db, tenant_id, &PolicyFilter::admin(), &id)
        .await?
        .book
        .library_id;
    books::delete(&state.db, tenant_id, &id).await?;
    Ok(redirect(&headers, &format!("/library/{library}")))
}
