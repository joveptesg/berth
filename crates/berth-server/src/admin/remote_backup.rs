//! Backups in a storage bucket: the card on the backup tab and the one on the restore tab,
//! loaded by the tabs themselves. Thin wrappers over `crate::remote_backup`.

use axum::{Extension, Form, extract::State, http::HeaderMap};
use minijinja::{Value, context};
use serde::{Deserialize, Serialize};

use super::{
    pages::{Page, form_error, render, restored},
    session::AdminSession,
};
use crate::{
    AppState, db,
    remote_backup::{self, KEEP_DAYS, PREFIX},
    service::storage,
};

fn serde(value: impl Serialize) -> Value {
    Value::from(minijinja::value::Serde(value))
}

/// S3 profiles of the tenant: only a bucket can hold the copies.
async fn buckets(state: &AppState, tenant_id: &str) -> anyhow::Result<Value> {
    let profiles: Vec<Value> = storage::list(&state.db, tenant_id)
        .await?
        .into_iter()
        .filter(|p| p.kind == "s3")
        .map(|p| context! { id => p.id, name => p.name, bucket => p.bucket })
        .collect();
    Ok(Value::from(profiles))
}

async fn card(state: &AppState, tenant_id: &str, error: Option<String>, saved: bool) -> Page {
    let ctx = context! {
        remote => context! {
            config => serde(remote_backup::config(&state.db, tenant_id).await?),
            last => serde(remote_backup::last(&state.db, tenant_id).await?),
            buckets => buckets(state, tenant_id).await?,
            prefix => PREFIX, keep_days => KEEP_DAYS, error, saved,
        },
    };
    render(state, "partials/remote_backup.html", ctx)
}

pub(crate) async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    card(&state, &session.0.tenant_id, None, false).await
}

#[derive(Deserialize)]
pub(crate) struct RemoteForm {
    /// Checkbox: `on` makes daily copies.
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    storage_id: String,
}

pub(crate) async fn save(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Form(form): Form<RemoteForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    match remote_backup::save(&state, tenant_id, form.enabled.is_some(), &form.storage_id).await {
        Ok(()) => card(&state, tenant_id, None, true).await,
        Err(err) => card(&state, tenant_id, Some(form_error(err)?), false).await,
    }
}

/// A copy right now; the outcome shows as the last copy.
pub(crate) async fn now(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let _ = remote_backup::run(&state, tenant_id).await;
    card(&state, tenant_id, None, false).await
}

#[derive(Deserialize, Default)]
pub(crate) struct RestoreForm {
    #[serde(default)]
    storage_id: String,
    /// Path of the copy in the bucket.
    #[serde(default)]
    key: String,
    /// Hex `secret.key` of the server that made the copy; empty when not at hand.
    #[serde(default)]
    old_secret: String,
}

async fn restore_card(
    state: &AppState,
    tenant_id: &str,
    storage_id: &str,
    list: Option<Result<Vec<remote_backup::RemoteBackup>, String>>,
    error: Option<&str>,
) -> Page {
    let server_id = db::server_id(&state.db).await?;
    let (backups, list_error) = match list {
        Some(Ok(backups)) => (Some(serde(backups)), None),
        Some(Err(message)) => (None, Some(message)),
        None => (None, None),
    };
    let ctx = context! {
        remote => context! {
            buckets => buckets(state, tenant_id).await?,
            storage_id, backups, list_error, error, server_id, prefix => PREFIX,
        },
    };
    render(state, "partials/remote_restore.html", ctx)
}

pub(crate) async fn restore_page(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
) -> Page {
    restore_card(&state, &session.0.tenant_id, "", None, None).await
}

/// The copies in the chosen bucket.
pub(crate) async fn restore_list(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    Form(form): Form<RestoreForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let list = remote_backup::list(&state, tenant_id, &form.storage_id)
        .await
        .map_err(|e| format!("{e:#}"));
    restore_card(&state, tenant_id, &form.storage_id, Some(list), None).await
}

/// Restores a copy from the bucket; then the restore tab shows the outcome.
pub(crate) async fn restore(
    State(state): State<AppState>,
    Extension(session): Extension<AdminSession>,
    headers: HeaderMap,
    Form(form): Form<RestoreForm>,
) -> Page {
    let tenant_id = &session.0.tenant_id;
    let result = remote_backup::restore(
        &state,
        tenant_id,
        &form.storage_id,
        &form.key,
        &form.old_secret,
    )
    .await;
    if let Err(code) = result {
        let list = remote_backup::list(&state, tenant_id, &form.storage_id)
            .await
            .map_err(|e| format!("{e:#}"));
        return restore_card(&state, tenant_id, &form.storage_id, Some(list), Some(code)).await;
    }
    restored(&state, &headers, Ok(())).await
}
