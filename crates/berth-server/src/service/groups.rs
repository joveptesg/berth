//! Groups of listeners (Adults, Children, Work…). A group keeps an access rule of the same
//! shape as a listener's own: the whole library or picked books, sections given, languages.
//! A listener in groups sees what any of their groups sees; see `books::PolicyFilter`.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

use super::categories::touch_policies;
use crate::db::now_ms;

const MAX_NAME_LEN: usize = 64;

/// A group as the admin edits it.
#[derive(Debug, Clone, Serialize)]
pub struct Group {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// `all` or `selected`.
    pub book_access: String,
    /// Books the group sees: picked ones, those of the sections given, in its languages.
    pub books: i64,
    /// Languages the group sees; empty for all.
    pub languages: Vec<String>,
    /// Ids of the sections given to the group.
    pub categories: Vec<String>,
    /// Ids of the listeners in the group.
    pub members: Vec<String>,
}

/// What the admin sets on a group.
#[derive(Debug, Clone, Default)]
pub struct GroupInput {
    /// Name.
    pub name: String,
    /// `None` for the whole library, else the picked book ids.
    pub books: Option<Vec<String>>,
    /// Ids of the sections given.
    pub categories: Vec<String>,
    /// Language codes; empty for all (unknown codes are dropped).
    pub languages: Vec<String>,
    /// Ids of the listeners in the group; ids that are not listeners are dropped.
    pub members: Vec<String>,
}

/// Groups of the tenant, in the order they were added.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Group>> {
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT g.id, g.name, g.book_access, g.languages
         FROM user_groups g WHERE g.tenant_id = $1 ORDER BY g.position, g.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    let alive = super::books::alive_ids(db, tenant_id).await?;
    let mut seen = Vec::with_capacity(rows.len());
    for (id, _, access, languages) in &rows {
        let books = super::books::group_view(db, tenant_id, id, access, languages).await?;
        seen.push(super::books::seen_count(books.as_ref(), &alive));
    }
    let given: Vec<(String, String)> =
        sqlx::query_as("SELECT group_id, category_id FROM group_categories WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(db)
            .await?;
    let members: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.group_id, m.user_id FROM group_members m JOIN users u ON u.id = m.user_id
         WHERE m.tenant_id = $1 AND u.deleted_at IS NULL ORDER BY u.name COLLATE NOCASE",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    let of = |pairs: &[(String, String)], id: &str| -> Vec<String> {
        pairs
            .iter()
            .filter(|(g, _)| g == id)
            .map(|(_, v)| v.clone())
            .collect()
    };
    Ok(rows
        .into_iter()
        .zip(seen)
        .map(|((id, name, book_access, languages), books)| Group {
            categories: of(&given, &id),
            members: of(&members, &id),
            languages: super::languages::split(&languages),
            id,
            name,
            book_access,
            books,
        })
        .collect())
}

/// Books picked for a group.
///
/// # Errors
/// Database failures.
pub async fn picked(db: &SqlitePool, tenant_id: &str, id: &str) -> anyhow::Result<Vec<String>> {
    Ok(
        sqlx::query_scalar(
            "SELECT book_id FROM group_books WHERE tenant_id = $1 AND group_id = $2",
        )
        .bind(tenant_id)
        .bind(id)
        .fetch_all(db)
        .await?,
    )
}

/// Ids of the groups a listener is in.
///
/// # Errors
/// Database failures.
pub async fn of_user(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT group_id FROM group_members WHERE tenant_id = $1 AND user_id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

fn key(name: &str) -> String {
    name.trim().to_lowercase().replace('ё', "е")
}

/// Checks the name and that no other group has it (case and «ё» aside).
async fn validate(db: &SqlitePool, tenant_id: &str, id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("group_name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("group_name_too_long".into()));
    }
    let taken = list(db, tenant_id)
        .await
        .context("load groups")?
        .into_iter()
        .any(|g| g.id != id && key(&g.name) == key(name));
    if taken {
        return Err(BerthError::Validation("group_name_taken".into()));
    }
    Ok(name.to_owned())
}

/// Puts `members` (listeners of the tenant) in the group instead of whom it had.
pub(crate) async fn set_members(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    id: &str,
    members: &[String],
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM group_members WHERE tenant_id = $1 AND group_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for user in members {
        sqlx::query(
            "INSERT OR IGNORE INTO group_members (tenant_id, group_id, user_id)
             SELECT $1, $2, id FROM users
             WHERE tenant_id = $1 AND id = $3 AND role = 'member' AND deleted_at IS NULL",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(user)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Puts the listener `user_id` in `groups` (ids of the tenant's groups) instead of the ones
/// they were in.
pub(crate) async fn join(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    user_id: &str,
    groups: &[String],
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM group_members WHERE tenant_id = $1 AND user_id = $2")
        .bind(tenant_id)
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    for group in groups {
        sqlx::query(
            "INSERT OR IGNORE INTO group_members (tenant_id, group_id, user_id)
             SELECT $1, id, $2 FROM user_groups WHERE tenant_id = $1 AND id = $3",
        )
        .bind(tenant_id)
        .bind(user_id)
        .bind(group)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Saves the rule and the members of the group `id` (already inserted).
async fn save(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    id: &str,
    input: &GroupInput,
    languages: &[String],
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE user_groups SET book_access = $3, languages = $4, updated_at = $5
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(if input.books.is_some() {
        "selected"
    } else {
        "all"
    })
    .bind(languages.join(","))
    .bind(now_ms())
    .execute(&mut *conn)
    .await?;
    for sql in [
        "DELETE FROM group_books WHERE tenant_id = $1 AND group_id = $2",
        "DELETE FROM group_categories WHERE tenant_id = $1 AND group_id = $2",
    ] {
        sqlx::query(sql)
            .bind(tenant_id)
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    for book in input.books.as_deref().unwrap_or_default() {
        sqlx::query(
            "INSERT OR IGNORE INTO group_books (tenant_id, group_id, book_id)
             SELECT $1, $2, id FROM books WHERE tenant_id = $1 AND id = $3",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(book)
        .execute(&mut *conn)
        .await?;
    }
    for category in &input.categories {
        sqlx::query(
            "INSERT OR IGNORE INTO group_categories (tenant_id, group_id, category_id)
             SELECT $1, $2, id FROM categories WHERE tenant_id = $1 AND id = $3",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(category)
        .execute(&mut *conn)
        .await?;
    }
    set_members(conn, tenant_id, id, &input.members).await?;
    touch_policies(conn, tenant_id).await
}

/// Adds a group at the end of the list.
///
/// # Errors
/// `Validation` with `group_name_required`, `group_name_too_long` or `group_name_taken`.
pub async fn create(db: &SqlitePool, tenant_id: &str, input: &GroupInput) -> Result<String> {
    let name = validate(db, tenant_id, "", &input.name).await?;
    let known = super::languages::list(db, tenant_id).await?;
    let languages = super::languages::pick(&known, &input.languages.join(","));
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(
        "INSERT INTO user_groups (id, tenant_id, name, position, updated_at)
         SELECT $1, $2, $3, COALESCE(MAX(position) + 1, 0), $4
         FROM user_groups WHERE tenant_id = $2",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(&name)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("insert group")?;
    save(&mut tx, tenant_id, &id, input, &languages)
        .await
        .context("save group")?;
    tx.commit().await.context("commit group")?;
    tracing::info!(group = %id, name = ?name, members = input.members.len(), "group created");
    Ok(id)
}

/// Renames a group and sets its rule and members.
///
/// # Errors
/// `NotFound`, the `Validation` codes of [`create`].
pub async fn update(db: &SqlitePool, tenant_id: &str, id: &str, input: &GroupInput) -> Result<()> {
    let name = validate(db, tenant_id, id, &input.name).await?;
    let known = super::languages::list(db, tenant_id).await?;
    let languages = super::languages::pick(&known, &input.languages.join(","));
    let mut tx = db.begin().await.context("begin")?;
    let renamed = sqlx::query("UPDATE user_groups SET name = $3 WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .bind(&name)
        .execute(&mut *tx)
        .await
        .context("rename group")?
        .rows_affected();
    if renamed == 0 {
        return Err(BerthError::NotFound);
    }
    save(&mut tx, tenant_id, id, input, &languages)
        .await
        .context("save group")?;
    tx.commit().await.context("commit group")?;
    let selected = input.books.as_ref().map(Vec::len);
    tracing::info!(group = %id, ?selected, sections = input.categories.len(), ?languages, members = input.members.len(), "group changed (None = whole library)");
    Ok(())
}

/// Removes a group; its listeners keep their own rule, used again if they are in no group.
///
/// # Errors
/// `NotFound`.
pub async fn delete(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    for sql in [
        "DELETE FROM group_members WHERE tenant_id = $1 AND group_id = $2",
        "DELETE FROM group_books WHERE tenant_id = $1 AND group_id = $2",
        "DELETE FROM group_categories WHERE tenant_id = $1 AND group_id = $2",
    ] {
        sqlx::query(sql)
            .bind(tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("unlink group")?;
    }
    let deleted = sqlx::query("DELETE FROM user_groups WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete group")?
        .rows_affected();
    if deleted == 0 {
        return Err(BerthError::NotFound);
    }
    touch_policies(&mut tx, tenant_id)
        .await
        .context("policies")?;
    tx.commit().await.context("commit delete")?;
    tracing::info!(group = %id, "group deleted");
    Ok(())
}
