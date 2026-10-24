//! The sections page: thin wrappers over `crate::service::categories`. A section is added
//! and edited in a dialog; every action answers with the whole list, as it is short, and a
//! form that did not save comes back open with its error.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::HeaderMap,
};
use berth_core::{BerthError, Result};
use minijinja::{Value, context};
use serde::Deserialize;

use super::{
    pages::{Page, form_error, render, render_form, shell},
    session::AdminSession,
};
use crate::{
    AppState,
    service::{
        categories::{self, CategoryInput},
        users,
    },
};

#[derive(Deserialize)]
pub(crate) struct CategoryForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    aliases: String,
    /// Checkbox: `on` when the section is closed.
    #[serde(default)]
    closed: Option<String>,
    /// Ids of the listeners it is given to, comma-separated.
    #[serde(default)]
    users: String,
}

impl CategoryForm {
    fn input(&self) -> CategoryInput {
        CategoryInput {
            name: self.name.clone(),
            aliases: self.aliases.clone(),
            closed: self.closed.is_some(),
            users: self
                .users
                .split(',')
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }
}

/// The dialog of a section: `id` empty for a new one.
async fn dialog_ctx(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    form: Option<&CategoryForm>,
    error: Option<String>,
) -> Result<Value> {
    let saved = if id.is_empty() {
        None
    } else {
        let entry = categories::entries(&state.db, tenant_id)
            .await?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or(BerthError::NotFound)?;
        Some(entry)
    };
    let listeners: Vec<Value> = users::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .filter(|u| u.role == "member")
        .map(|u| context! { id => u.id, name => u.name })
        .collect();
    let (name, aliases, closed, given, books) = match (form, &saved) {
        (Some(f), _) => {
            let input = f.input();
            let books = saved.as_ref().map_or(0, |s| s.books);
            (input.name, input.aliases, input.closed, input.users, books)
        }
        (None, Some(s)) => (
            s.name.clone(),
            s.aliases.replace('\n', ", "),
            s.closed,
            s.users.clone(),
            s.books,
        ),
        (None, None) => (String::new(), String::new(), false, Vec::new(), 0),
    };
    Ok(context! { id, name, aliases, closed, given, books, listeners, error })
}

/// The list; `dialog` stays open over it when a form did not save.
async fn list(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    dialog: Option<Value>,
) -> Page {
    let listeners = users::list(&state.db, tenant_id).await?;
    let entries: Vec<Value> = categories::entries(&state.db, tenant_id)
        .await?
        .into_iter()
        .map(|c| {
            let aliases = c.aliases.replace('\n', ", ");
            let names: Vec<String> = listeners
                .iter()
                .filter(|u| c.users.contains(&u.id))
                .map(|u| u.name.clone())
                .collect();
            context! {
                id => c.id, name => c.name, aliases, books => c.books, closed => c.closed,
                users => names,
            }
        })
        .collect();
    render_form(
        state,
        headers,
        "categories.html",
        "partials/category_list.html",
        context! { categories => entries, dialog, ..shell(state, "categories") },
    )
}

/// The list after a save, or the dialog again with the form error.
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
            let dialog = dialog_ctx(state, tenant_id, id, Some(form), Some(error)).await?;
            list(state, headers, tenant_id, Some(dialog)).await
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

/// The empty dialog of a new section.
pub(crate) async fn new_dialog(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let dialog = dialog_ctx(&state, &session.0.tenant_id, "", None, None).await?;
    render(&state, "partials/category_dialog.html", context! { dialog })
}

/// The dialog of a section as saved.
pub(crate) async fn edit_dialog(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(id): Path<String>,
) -> Page {
    let dialog = dialog_ctx(&state, &session.0.tenant_id, &id, None, None).await?;
    render(&state, "partials/category_dialog.html", context! { dialog })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<CategoryForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = categories::create(&state.db, tenant_id, &form.input())
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
    let result = categories::update(&state.db, tenant_id, &id, &form.input()).await;
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
