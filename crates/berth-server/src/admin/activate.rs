//! `/activate`: an administrator allows a desktop app's device-code sign-in for a user.
//!
//! The app opens `/activate?code=…` in the browser; without a session the page sends the
//! browser to sign in and back here with the code.

use axum::{
    Extension, Form,
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Redirect},
};
use berth_core::BerthError;
use minijinja::{Value, context};
use serde::Deserialize;

use super::{
    pages::{Page, form_error, render, render_form, shell},
    session::{self, AdminSession},
};
use crate::{
    AppState,
    db::now_ms,
    error::ApiError,
    service::{auth, device_auth, pairing::normalize, setup, users},
};

#[derive(Deserialize)]
pub(crate) struct CodeForm {
    #[serde(default)]
    code: String,
}

#[derive(Deserialize)]
pub(crate) struct ApproveForm {
    code: String,
    user_id: String,
}

/// The code form, empty or with an error.
fn enter(code: &str, error: Option<String>) -> Value {
    context! { state => "enter", code, error }
}

/// The device behind `code` with the users to give it to, or the code form with an error.
async fn lookup(state: &AppState, session: &auth::Session, code: &str) -> Result<Value, ApiError> {
    let code = normalize(code);
    match device_auth::describe(&state.db, &session.tenant_id, &code, now_ms()).await {
        Ok(request) => {
            let users: Vec<Value> = users::list(&state.db, &session.tenant_id)
                .await?
                .into_iter()
                .map(|u| context! { id => u.id, name => u.name })
                .collect();
            Ok(context! {
                state => "confirm", code, users, selected => session.user_id.clone(),
                device_name => request.device_name, platform => request.platform,
            })
        }
        Err(err) => Ok(enter(&code, Some(form_error(err)?))),
    }
}

fn page_ctx(state: &AppState, activate: Value) -> Value {
    context! { activate, ..shell(state, "devices") }
}

/// The page, opened by the app with `?code=`.
pub(crate) async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CodeForm>,
) -> Page {
    let code = normalize(&query.code);
    let signed_in = match session::token(&headers) {
        Some(token) => auth::session(&state.db, token).await?,
        None => None,
    };
    let Some(session) = signed_in else {
        if !setup::is_configured(&state.db).await? {
            return Ok(Redirect::to("/setup").into_response());
        }
        let back = if code.is_empty() {
            "/activate".to_owned()
        } else {
            format!("/activate?code={code}")
        };
        let next: String = url::form_urlencoded::byte_serialize(back.as_bytes()).collect();
        return Ok(Redirect::to(&format!("/login?next={next}")).into_response());
    };
    let activate = if code.is_empty() {
        enter("", None)
    } else {
        lookup(&state, &session, &code).await?
    };
    render(&state, "activate.html", page_ctx(&state, activate))
}

/// The code typed on the page.
pub(crate) async fn submit(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Page {
    let activate = lookup(&state, &session.0, &form.code).await?;
    render_form(
        &state,
        &headers,
        "activate.html",
        "partials/activate.html",
        page_ctx(&state, activate),
    )
}

/// Allows the sign-in for the chosen user.
pub(crate) async fn approve(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let code = normalize(&form.code);
    let request = device_auth::describe(&state.db, tenant_id, &code, now_ms()).await;
    let result = match request {
        Ok(request) => device_auth::approve(
            &state.db,
            tenant_id,
            &session.0.user_id,
            &code,
            &form.user_id,
            now_ms(),
        )
        .await
        .map(|()| request),
        Err(err) => Err(err),
    };
    let activate = match result {
        Ok(request) => {
            let user_name = users::list(&state.db, tenant_id)
                .await?
                .into_iter()
                .find(|u| u.id == form.user_id)
                .map(|u| u.name)
                .ok_or(BerthError::NotFound)?;
            context! {
                state => "allowed", device_name => request.device_name,
                platform => request.platform, user_name,
            }
        }
        Err(err) => enter(&code, Some(form_error(err)?)),
    };
    render_form(
        &state,
        &headers,
        "activate.html",
        "partials/activate.html",
        page_ctx(&state, activate),
    )
}

/// Refuses the sign-in.
pub(crate) async fn deny(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Page {
    let code = normalize(&form.code);
    let activate = match device_auth::deny(&state.db, &session.0.tenant_id, &code, now_ms()).await {
        Ok(()) => context! { state => "denied" },
        Err(err) => enter(&code, Some(form_error(err)?)),
    };
    render_form(
        &state,
        &headers,
        "activate.html",
        "partials/activate.html",
        page_ctx(&state, activate),
    )
}
