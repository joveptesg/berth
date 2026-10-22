//! Storage profiles page: thin wrappers over `crate::service::storage`.

use axum::{
    Extension, Form,
    extract::{Path, State},
    http::HeaderMap,
};
use berth_core::BerthError;
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    pages::{Page, render, render_form, shell},
    redirect,
    session::AdminSession,
};
use crate::{
    AppState,
    error::ApiError,
    service::{
        r2, settings,
        storage::{self, Profile, ProfileInput},
    },
    storage::{LocalConfig, local_root},
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

/// Storage fields of the `/storage` dialogs; the setup wizard sends them with `storage_`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct StorageForm {
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) path: String,
    #[serde(default)]
    pub(super) endpoint: String,
    #[serde(default)]
    pub(super) bucket: String,
    #[serde(default)]
    pub(super) region: String,
    /// Checkbox: `on` when ticked, absent otherwise.
    #[serde(default)]
    pub(super) path_style: Option<String>,
    #[serde(default)]
    pub(super) delivery_mode: String,
    #[serde(default)]
    pub(super) cdn_url: String,
    #[serde(default, skip_serializing)]
    pub(super) access_key_id: String,
    #[serde(default, skip_serializing)]
    pub(super) secret_access_key: String,
}

impl StorageForm {
    pub(super) fn input(&self) -> ProfileInput {
        ProfileInput {
            name: self.name.clone(),
            kind: self.kind.clone(),
            path: self.path.clone(),
            endpoint: self.endpoint.clone(),
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            path_style: self.path_style.is_some(),
            delivery_mode: self.delivery_mode.clone(),
            cdn_url: self.cdn_url.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
        }
    }

    /// The input to show again after an error: everything but the keys.
    pub(super) fn view(&self, id: &str) -> Value {
        context! {
            id, path_style => self.path_style.is_some(), ..serde(self)
        }
    }
}

/// A profile row with where its bytes are, as people read it.
#[derive(Serialize)]
struct Row {
    #[serde(flatten)]
    profile: Profile,
    location: String,
}

fn location(profile: &Profile, data_dir: &std::path::Path) -> String {
    if profile.kind == "local" {
        let config = LocalConfig {
            path: profile.path.clone(),
        };
        return local_root(&config, data_dir).display().to_string();
    }
    let host = url::Url::parse(&profile.endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| format!("AWS {}", profile.region));
    format!("{} @ {host}", profile.bucket)
}

/// Context of the section: profiles plus an open dialog or a refused action.
async fn section(
    state: &AppState,
    tenant_id: &str,
    form: Option<Value>,
    error: Option<String>,
    notice: Option<String>,
) -> Result<Value, ApiError> {
    let data_dir = state.config.load().data_dir.clone();
    let rows: Vec<Row> = storage::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .map(|profile| Row {
            location: location(&profile, &data_dir),
            profile,
        })
        .collect();
    Ok(context! {
        profiles => serde(rows), form, error, notice,
        data_dir => data_dir.display().to_string(),
        r2_token_page => r2::TOKEN_PAGE,
        ..shell(state, "storage")
    })
}

/// Fields of the Cloudflare R2 dialog.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct R2Form {
    #[serde(default)]
    name: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    bucket: String,
}

/// The R2 wizard: bucket, keys and CORS from one token, then the connection check.
pub(crate) async fn connect_r2(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<R2Form>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let input = r2::R2Input {
        name: form.name.clone(),
        account_id: form.account_id.clone(),
        token: form.token.clone(),
        bucket: form.bucket.clone(),
    };
    // Uploads from the admin go straight to the bucket: its addresses may use CORS.
    let origins = settings::server_urls(&state.config.load());
    let ctx = match r2::connect(&state, tenant_id, &input, r2::API, origins).await {
        Ok(id) => {
            storage::verify(&state, tenant_id, &id).await?;
            if !super::is_htmx(&headers) {
                return Ok(redirect(&headers, "/storage"));
            }
            section(&state, tenant_id, None, None, None).await?
        }
        Err(err) => {
            let error = shown(err)?;
            let ctx = section(&state, tenant_id, None, None, None).await?;
            // The token is not sent back to the page.
            context! {
                r2_error => error,
                r2_form => context! {
                    account_id => form.account_id, bucket => form.bucket, name => form.name,
                },
                ..ctx
            }
        }
    };
    reply(&state, &headers, ctx)
}

fn reply(state: &AppState, headers: &HeaderMap, ctx: Value) -> Page {
    render_form(
        state,
        headers,
        "storage.html",
        "partials/storage_section.html",
        ctx,
    )
}

/// Validation and refused actions are shown on the page; anything else is an error.
fn shown(err: BerthError) -> Result<String, ApiError> {
    match err {
        BerthError::Validation(code) | BerthError::Conflict(code) => Ok(code),
        err => Err(err.into()),
    }
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let ctx = section(&state, &session.0.tenant_id, None, None, None).await?;
    render(&state, "storage.html", ctx)
}

pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<StorageForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let ctx = match storage::create(&state, tenant_id, &form.input()).await {
        Ok(id) => {
            // A new profile is checked right away: the list shows whether it works.
            storage::verify(&state, tenant_id, &id).await?;
            if !super::is_htmx(&headers) {
                return Ok(redirect(&headers, "/storage"));
            }
            section(&state, tenant_id, None, None, None).await?
        }
        Err(err) => {
            let error = shown(err)?;
            section(&state, tenant_id, Some(form.view("")), Some(error), None).await?
        }
    };
    reply(&state, &headers, ctx)
}

pub(crate) async fn update(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<StorageForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let ctx = match storage::update(&state, tenant_id, &id, &form.input()).await {
        Ok(()) => {
            if !super::is_htmx(&headers) {
                return Ok(redirect(&headers, "/storage"));
            }
            section(&state, tenant_id, None, None, None).await?
        }
        Err(err) => {
            let error = shown(err)?;
            section(&state, tenant_id, Some(form.view(&id)), Some(error), None).await?
        }
    };
    reply(&state, &headers, ctx)
}

pub(crate) async fn verify(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    storage::verify(&state, tenant_id, &id).await?;
    action_done(&state, &headers, tenant_id, None).await
}

pub(crate) async fn make_default(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    storage::set_default(&state.db, tenant_id, &id).await?;
    action_done(&state, &headers, tenant_id, None).await
}

pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let notice = match storage::delete(&state.db, tenant_id, &id).await {
        Ok(()) => None,
        Err(err) => Some(shown(err)?),
    };
    action_done(&state, &headers, tenant_id, notice).await
}

async fn action_done(
    state: &AppState,
    headers: &HeaderMap,
    tenant_id: &str,
    notice: Option<String>,
) -> Page {
    if !super::is_htmx(headers) && notice.is_none() {
        return Ok(redirect(headers, "/storage"));
    }
    let ctx = section(state, tenant_id, None, None, notice).await?;
    reply(state, headers, ctx)
}
