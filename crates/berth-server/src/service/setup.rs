//! First start: the one-time setup code and creation of the tenant with its owner.

use std::sync::Mutex;

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;
use url::Url;

use super::{
    auth, books, pairing, settings,
    storage::{self, ProfileInput},
};
use crate::{AppState, db, db::now_ms};

/// One-time code from the server log that guards `/setup` until the server is set up
/// (design doc §22.A2): without it the first visitor of a fresh server would become its admin.
/// 8 characters like a pairing code, short enough to type; `POST /setup` is rate limited.
/// It lives only in memory: a restart before setup prints a new one.
pub struct SetupCode(Mutex<Option<String>>);

impl SetupCode {
    pub(crate) fn new(configured: bool) -> anyhow::Result<Self> {
        let code = if configured {
            None
        } else {
            Some(pairing::generate_code()?)
        };
        Ok(Self(Mutex::new(code)))
    }

    /// The code while setup is pending.
    #[must_use]
    pub fn get(&self) -> Option<String> {
        self.0.lock().ok()?.clone()
    }

    /// Constant-time comparison of a typed code (any case, dashes and spaces ignored);
    /// always false once setup is done.
    pub(crate) fn matches(&self, typed: &str) -> bool {
        let typed = pairing::normalize(typed);
        self.get()
            .is_some_and(|code| bool::from(code.as_bytes().ct_eq(typed.as_bytes())))
    }

    fn consume(&self) {
        if let Ok(mut code) = self.0.lock() {
            *code = None;
        }
    }
}

/// The created owner.
#[derive(Debug)]
pub struct Owner {
    /// The new tenant.
    pub tenant_id: String,
    /// The owner user.
    pub user_id: String,
}

/// Whether the server has a tenant.
///
/// # Errors
/// Database failures.
pub async fn is_configured(db: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants)")
        .fetch_one(db)
        .await?)
}

/// Creates the tenant, its owner with the password, the default access policy, the server
/// name setting and the default storage profile in one transaction. The storage is checked
/// (a probe is written, read and deleted) before anything is created.
///
/// `opened_at` is the address the wizard was opened at; it becomes the home or internet
/// address for pairing QR codes (see [`settings::address_setting`]).
///
/// # Errors
/// `Validation` for a bad name, password or storage (including a failed check),
/// `Conflict("already_set_up")` when a tenant exists.
pub async fn create_owner(
    state: &AppState,
    name: &str,
    password: &str,
    confirm: &str,
    storage: &ProfileInput,
    opened_at: Option<&Url>,
) -> Result<Owner> {
    let name = settings::validate_name(name)?;
    auth::validate_new_password(password, confirm)?;
    let tenant_id = uuid::Uuid::now_v7().to_string();
    let first_storage = storage::prepare_first(state, &tenant_id, storage).await?;
    let hash = auth::hash_password(password.to_owned()).await?;
    let user_id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();

    let mut tx = state.db.begin().await.context("begin")?;
    // Conditional insert: of two concurrent wizards only one creates the tenant.
    let created = sqlx::query(
        "INSERT INTO tenants (id, name, created_at)
         SELECT $1, $2, $3 WHERE NOT EXISTS (SELECT 1 FROM tenants)",
    )
    .bind(&tenant_id)
    .bind(&name)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("insert tenant")?;
    if created.rows_affected() == 0 {
        return Err(BerthError::Conflict("already_set_up".into()));
    }
    sqlx::query("INSERT INTO revisions (tenant_id, v) VALUES ($1, 0)")
        .bind(&tenant_id)
        .execute(&mut *tx)
        .await
        .context("insert revisions")?;
    sqlx::query(
        "INSERT INTO users (id, tenant_id, name, role, password_hash, created_at)
         VALUES ($1, $2, 'Admin', 'owner', $3, $4)",
    )
    .bind(&user_id)
    .bind(&tenant_id)
    .bind(&hash)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("insert owner")?;
    sqlx::query("INSERT INTO access_policies (tenant_id, user_id) VALUES ($1, $2)")
        .bind(&tenant_id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await
        .context("insert policy")?;
    storage::insert_first(&mut tx, &state.secret, &tenant_id, &first_storage).await?;
    super::languages::seed(&mut tx, &tenant_id)
        .await
        .context("seed languages")?;
    books::default_library(&mut tx, &tenant_id)
        .await
        .context("create library")?;
    let locked = settings::locked(state);
    if !locked.name {
        db::put_server_setting(&mut tx, "name", Some(&Value::String(name)))
            .await
            .context("save name")?;
    }
    if let Some(url) = opened_at
        && let Some(key) = settings::address_setting(url)
        && !(if key == "lan_url" {
            locked.lan_url
        } else {
            locked.wan_url
        })
    {
        let value = Value::String(settings::display_url(url));
        db::put_server_setting(&mut tx, key, Some(&value))
            .await
            .context("save address")?;
    }
    tx.commit().await.context("commit setup")?;

    state.setup.consume();
    settings::reload(state).await?;
    tracing::info!(tenant = %tenant_id, "server set up");
    Ok(Owner { tenant_id, user_id })
}
