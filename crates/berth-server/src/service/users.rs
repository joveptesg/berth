//! Users of the tenant: the owner and listeners. Listeners pair their phones with codes from
//! `/connect`; a listener may also get a login and password for phones that cannot scan one
//! (a store reviewer's). Each user sees the whole library or only the books picked for them.

use std::collections::HashSet;

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::auth;
use crate::db::now_ms;

const MAX_NAME_LEN: usize = 64;
const MIN_LOGIN_LEN: usize = 3;
const MAX_LOGIN_LEN: usize = 64;
/// Short on purpose: a store demo account is often demo/demo. The admin is told to pick a
/// longer one for real people.
pub const MIN_APP_PASSWORD_LEN: usize = 4;

/// A user in the admin list.
#[derive(Debug, Clone, Serialize)]
pub struct User {
    /// User id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `owner`, `admin`, `member` or `restricted`.
    pub role: String,
    /// Paired devices that are not revoked (admin browsers not counted).
    pub devices: i64,
    /// Creation time, epoch milliseconds.
    pub created_at: i64,
    /// Login for apps, if the user has one.
    pub login: Option<String>,
    /// `all` or `selected`.
    pub book_access: String,
    /// Books picked for a `selected` user.
    pub books: i64,
}

/// Users of the tenant, owner first, then by name.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<User>> {
    type Row = (
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
        String,
        i64,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT u.id, u.name, u.role, u.created_at,
                (SELECT COUNT(*) FROM devices d WHERE d.user_id = u.id AND d.revoked_at IS NULL
                 AND NOT (d.platform = 'web' AND EXISTS (
                     SELECT 1 FROM device_scopes s WHERE s.device_id = d.id AND s.scope = 'admin'))),
                u.login, COALESCE(p.book_access, 'all'),
                (SELECT COUNT(*) FROM user_books ub JOIN books b ON b.id = ub.book_id
                 WHERE ub.user_id = u.id AND b.deleted_at IS NULL)
         FROM users u
         LEFT JOIN access_policies p ON p.tenant_id = u.tenant_id AND p.user_id = u.id
         WHERE u.tenant_id = $1 AND u.deleted_at IS NULL
         ORDER BY u.role <> 'owner', u.name COLLATE NOCASE",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, name, role, created_at, devices, login, book_access, books)| User {
                id,
                name,
                role,
                devices,
                created_at,
                login,
                book_access,
                books,
            },
        )
        .collect())
}

/// Creates a listener (role `member`) with the default, unrestricted access policy.
///
/// # Errors
/// `Validation` with `user_name_required` or `user_name_too_long`.
pub async fn create_listener(db: &SqlitePool, tenant_id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("user_name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("user_name_too_long".into()));
    }
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(
        "INSERT INTO users (id, tenant_id, name, role, created_at) VALUES ($1, $2, $3, 'member', $4)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(name)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("insert user")?;
    sqlx::query("INSERT INTO access_policies (tenant_id, user_id) VALUES ($1, $2)")
        .bind(tenant_id)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .context("insert policy")?;
    tx.commit().await.context("commit user")?;
    Ok(id)
}

/// Whether some listener can sign in with a password (`server-info` offers it then).
///
/// # Errors
/// Database failures.
pub async fn any_login(db: &SqlitePool) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE login IS NOT NULL AND password_hash IS NOT NULL
             AND role = 'member' AND deleted_at IS NULL)",
    )
    .fetch_one(db)
    .await?)
}

fn valid_login(login: &str) -> bool {
    (MIN_LOGIN_LEN..=MAX_LOGIN_LEN).contains(&login.chars().count())
        && login
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

async fn listener(db: &SqlitePool, tenant_id: &str, user_id: &str) -> Result<()> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM users WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_optional(db)
    .await
    .context("load user")?;
    match role.as_deref() {
        None => Err(BerthError::NotFound),
        Some("member") => Ok(()),
        // The owner signs in to the admin; a password here would open it to apps.
        Some(_) => Err(BerthError::Validation("login_admin".into())),
    }
}

/// Gives a listener a login and password for apps; an empty `password` keeps the saved one.
///
/// # Errors
/// `NotFound`; `Validation` with `login_admin` (not a listener), `login_invalid`,
/// `login_taken`, `login_password_short` or `login_password_required` (a new login without
/// a password).
pub async fn set_login(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
    login: &str,
    password: &str,
) -> Result<()> {
    listener(db, tenant_id, user_id).await?;
    let login = login.trim();
    if !valid_login(login) {
        return Err(BerthError::Validation("login_invalid".into()));
    }
    let hash = if password.is_empty() {
        let saved: Option<String> = sqlx::query_scalar(
            "SELECT password_hash FROM users WHERE id = $1 AND login IS NOT NULL",
        )
        .bind(user_id)
        .fetch_optional(db)
        .await
        .context("load password")?
        .flatten();
        saved.ok_or_else(|| BerthError::Validation("login_password_required".into()))?
    } else if password.chars().count() < MIN_APP_PASSWORD_LEN {
        return Err(BerthError::Validation("login_password_short".into()));
    } else {
        auth::hash_password(password.to_owned()).await?
    };
    sqlx::query("UPDATE users SET login = $3, password_hash = $4 WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(user_id)
        .bind(login)
        .bind(hash)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => {
                BerthError::Validation("login_taken".into())
            }
            _ => BerthError::Internal(anyhow::Error::new(e).context("save login")),
        })?;
    Ok(())
}

/// Turns a listener's login off; signed-in devices stay until revoked.
///
/// # Errors
/// `NotFound`; `Validation("login_admin")`.
pub async fn clear_login(db: &SqlitePool, tenant_id: &str, user_id: &str) -> Result<()> {
    listener(db, tenant_id, user_id).await?;
    sqlx::query(
        "UPDATE users SET login = NULL, password_hash = NULL WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .execute(db)
    .await
    .context("clear login")?;
    Ok(())
}

/// Books a user sees: `None` for the whole library, else the ids picked for them.
///
/// # Errors
/// Database failures.
pub async fn access(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
) -> anyhow::Result<Option<HashSet<String>>> {
    Ok(super::books::PolicyFilter::for_user(db, tenant_id, user_id)
        .await?
        .books)
}

/// Sets what a listener sees: the whole library (`None`) or only `books`. Changes the policy
/// version, so the next sync of their phones is a full slice without the hidden books.
///
/// # Errors
/// `NotFound`; `Validation("login_admin")` for the owner, who always sees everything.
pub async fn set_access(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
    books: Option<&[String]>,
) -> Result<()> {
    listener(db, tenant_id, user_id).await?;
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query("DELETE FROM user_books WHERE tenant_id = $1 AND user_id = $2")
        .bind(tenant_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("clear books")?;
    for book in books.unwrap_or_default() {
        sqlx::query(
            "INSERT OR IGNORE INTO user_books (tenant_id, user_id, book_id)
             SELECT $1, $2, id FROM books WHERE tenant_id = $1 AND id = $3",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(book)
        .execute(&mut *tx)
        .await
        .context("pick book")?;
    }
    sqlx::query(
        "INSERT INTO access_policies (tenant_id, user_id, book_access) VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, user_id) DO UPDATE SET
            book_access = excluded.book_access, policy_version = policy_version + 1",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(if books.is_some() { "selected" } else { "all" })
    .execute(&mut *tx)
    .await
    .context("save access")?;
    tx.commit().await.context("commit access")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins() {
        assert!(valid_login("demo"));
        assert!(valid_login("anna.k@home"));
        assert!(valid_login("Мама"));
        assert!(!valid_login("ab"));
        assert!(!valid_login("with space"));
        assert!(!valid_login(&"x".repeat(65)));
    }
}
