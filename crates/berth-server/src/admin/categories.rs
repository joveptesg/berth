//! The sections page: thin wrappers over `crate::service::categories`. Every action answers
//! with the whole list, as it is short.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::HeaderMap,
};
use berth_core::{BerthError, Result};
use minijinja::{Value, context};
use serde::Deserialize;

use super::{
    pages::{Page, form_error, render_form, shell},
    session::AdminSession,
};
use crate::{AppState, service::categories};

#[derive(Deserialize)]
pub(crate) struct CategoryForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    aliases: String,
}

/// The list, with the error of the form that failed (`""` for the new one).
async fn list(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    failed: Option<(&str, &CategoryForm, String)>,
) -> Page {
    let entries = categories::entries(&state.db, tenant_id).await?;
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|c| {
            let aliases = c.aliases.replace('\n', ", ");
            context! { id => c.id, name => c.name, aliases, books => c.books }
        })
        .collect();
    let failed = failed.map(|(id, form, error)| {
        context! { id, name => form.name.clone(), aliases => form.aliases.clone(), error }
    });
    render_form(
        state,
        headers,
        "categories.html",
        "partials/category_list.html",
        context! { categories => entries, failed, ..shell(state, "categories") },
    )
}

/// Shows the list again, or the form error next to the form that sent it.
async fn answer(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    id: &str,
    form: &CategoryForm,
    result: Result<()>,
) -> Page {
    match result {
        Ok(()) => list(state, headers, tenant_id, None).await,
        Err(err) => {
            let error = form_error(err)?;
            list(state, headers, tenant_id, Some((id, form, error))).await
        }
    }
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
) -> Page {
    list(&state, &headers, &session.0.tenant_id, None).await
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<CategoryForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = categories::create(&state.db, tenant_id, &form.name, &form.aliases)
        .await
        .map(|_| ());
    answer(&state, &headers, tenant_id, "", &form, result).await
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<CategoryForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = categories::update(&state.db, tenant_id, &id, &form.name, &form.aliases).await;
    answer(&state, &headers, tenant_id, &id, &form, result).await
}

pub(crate) async fn shift(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path((id, direction)): Path<(String, String)>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let up = match direction.as_str() {
        "up" => true,
        "down" => false,
        _ => return Err(BerthError::NotFound.into()),
    };
    categories::shift(&state.db, tenant_id, &id, up).await?;
    list(&state, &headers, tenant_id, None).await
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    categories::delete(&state.db, tenant_id, &id).await?;
    list(&state, &headers, tenant_id, None).await
}
