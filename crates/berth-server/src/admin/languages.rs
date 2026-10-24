//! The languages tab of the settings: thin wrappers over `crate::service::languages`. A
//! language is added and renamed in a dialog; every action answers with the whole list, and
//! a form that did not save comes back open with its error.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::HeaderMap,
};
use berth_core::Result;
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    pages::{Page, backup_ctx, form_error, render, render_form},
    session::AdminSession,
};
use crate::{AppState, service::languages};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

#[derive(Deserialize)]
pub(crate) struct LanguageForm {
    #[serde(default)]
    code: String,
    #[serde(default)]
    name: String,
}

/// The settings page on the languages tab; `dialog` stays open when a form did not save.
async fn list(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    dialog: Option<Value>,
) -> Page {
    let entries = languages::entries(&state.db, tenant_id).await?;
    let ctx = context! {
        ..backup_ctx(state, "languages"),
        ..context! { languages => serde(entries), dialog },
    };
    render_form(
        state,
        headers,
        "settings.html",
        "partials/settings_languages.html",
        ctx,
    )
}

async fn answer(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    editing: Option<&str>,
    form: &LanguageForm,
    result: Result<()>,
) -> Page {
    match result {
        Ok(()) => list(state, headers, tenant_id, None).await,
        Err(err) => {
            let error = form_error(err)?;
            let dialog = context! {
                editing, code => editing.unwrap_or(&form.code), name => form.name.clone(), error,
            };
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

/// The empty dialog of a new language.
pub(crate) async fn new_dialog(State(state): State<AppState>) -> Page {
    let dialog = context! { editing => (), code => "", name => "" };
    render(&state, "partials/language_dialog.html", context! { dialog })
}

/// The dialog of a language as saved.
pub(crate) async fn edit_dialog(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Path(code): Path<String>,
) -> Page {
    let language = languages::list(&state.db, &session.0.tenant_id)
        .await?
        .into_iter()
        .find(|l| l.code == code)
        .ok_or(berth_core::BerthError::NotFound)?;
    let dialog =
        context! { editing => language.code.clone(), code => language.code, name => language.name };
    render(&state, "partials/language_dialog.html", context! { dialog })
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<LanguageForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = languages::create(&state.db, tenant_id, &form.code, &form.name).await;
    answer(&state, &headers, tenant_id, None, &form, result).await
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Form(form): Form<LanguageForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = languages::update(&state.db, tenant_id, &code, &form.name).await;
    answer(&state, &headers, tenant_id, Some(&code), &form, result).await
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    // In use: the list comes back unchanged, the button there is off for such a language.
    match languages::delete(&state.db, tenant_id, &code).await {
        Ok(()) | Err(berth_core::BerthError::Validation(_)) => {
            list(&state, &headers, tenant_id, None).await
        }
        Err(err) => Err(err.into()),
    }
}
