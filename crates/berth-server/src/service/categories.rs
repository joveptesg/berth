//! Sections of the library: a flat list the admin keeps in order. A book may be in several;
//! genres from file tags pick sections by name or alias when a book is uploaded. A section
//! can be given to listeners, and a closed one only they (and the owner) see.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

use super::books::next_revision;
use crate::db::now_ms;

const MAX_NAME_LEN: usize = 64;

/// A section as devices sync it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Category {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Place in the list, from 0.
    pub position: i64,
}

/// A section as the admin edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CategoryEntry {
    /// Id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Other names of the genre, one per line.
    pub aliases: String,
    /// Place in the list.
    pub position: i64,
    /// Books in it.
    pub books: i64,
    /// Only the owner and the listeners in `users` see its books.
    pub closed: bool,
    /// Listeners the section is given to.
    pub users: Vec<String>,
}

/// What the admin sets on a section.
#[derive(Debug, Clone, Default)]
pub struct CategoryInput {
    /// Name.
    pub name: String,
    /// Other names of the genre: comma-, semicolon- or line-separated.
    pub aliases: String,
    /// Closed to everyone it is not given to.
    pub closed: bool,
    /// Listeners it is given to; ids that are not listeners are dropped.
    pub users: Vec<String>,
}

/// Sections in their order.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Category>> {
    Ok(sqlx::query_as(
        "SELECT id, name, position FROM categories WHERE tenant_id = $1 ORDER BY position, name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Sections with their aliases and book counts.
///
/// # Errors
/// Database failures.
pub async fn entries(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<CategoryEntry>> {
    let rows: Vec<(String, String, String, i64, i64, bool)> = sqlx::query_as(
        "SELECT c.id, c.name, c.aliases, c.position,
                (SELECT COUNT(*) FROM book_categories bc JOIN books b ON b.id = bc.book_id
                 WHERE bc.category_id = c.id AND b.deleted_at IS NULL) AS books,
                c.closed
         FROM categories c WHERE c.tenant_id = $1 ORDER BY c.position, c.name",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?;
    let grants: Vec<(String, String)> =
        sqlx::query_as("SELECT category_id, user_id FROM user_categories WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, aliases, position, books, closed)| {
            let users = grants
                .iter()
                .filter(|(c, _)| *c == id)
                .map(|(_, u)| u.clone())
                .collect();
            CategoryEntry {
                id,
                name,
                aliases,
                position,
                books,
                closed,
                users,
            }
        })
        .collect())
}

/// Sections given to a user.
///
/// # Errors
/// Database failures.
pub async fn given(db: &SqlitePool, tenant_id: &str, user_id: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT category_id FROM user_categories WHERE tenant_id = $1 AND user_id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

async fn grantees(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    id: &str,
) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT user_id FROM user_categories WHERE tenant_id = $1 AND category_id = $2
         ORDER BY user_id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(&mut *conn)
    .await
}

/// Gives the section to `users` (listeners of the tenant) instead of whom it had; tells
/// whether anything changed.
async fn give(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    id: &str,
    users: &[String],
) -> sqlx::Result<bool> {
    let before = grantees(conn, tenant_id, id).await?;
    sqlx::query("DELETE FROM user_categories WHERE tenant_id = $1 AND category_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for user in users {
        sqlx::query(
            "INSERT OR IGNORE INTO user_categories (tenant_id, user_id, category_id)
             SELECT $1, id, $2 FROM users
             WHERE tenant_id = $1 AND id = $3 AND role = 'member' AND deleted_at IS NULL",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(user)
        .execute(&mut *conn)
        .await?;
    }
    Ok(before != grantees(conn, tenant_id, id).await?)
}

/// A new policy version for every user of the tenant: who sees what changed, so the next
/// sync of every phone is a full slice.
pub(crate) async fn touch_policies(
    conn: &mut SqliteConnection,
    tenant_id: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO access_policies (tenant_id, user_id)
         SELECT tenant_id, id FROM users WHERE tenant_id = $1 AND deleted_at IS NULL
         ON CONFLICT (tenant_id, user_id) DO UPDATE SET policy_version = policy_version + 1",
    )
    .bind(tenant_id)
    .execute(conn)
    .await?;
    Ok(())
}

fn key(name: &str) -> String {
    name.trim().to_lowercase().replace('ё', "е")
}

fn clean_aliases(aliases: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for alias in aliases.split(['\n', ',', ';']).map(str::trim) {
        if !alias.is_empty() && !out.iter().any(|o| key(o) == key(alias)) {
            out.push(alias);
        }
    }
    out.join("\n")
}

/// Checks the name and that no other section has it (case and «ё» aside).
async fn validate(db: &SqlitePool, tenant_id: &str, id: &str, name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("category_name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("category_name_too_long".into()));
    }
    let taken = list(db, tenant_id)
        .await
        .context("load categories")?
        .into_iter()
        .any(|c| c.id != id && key(&c.name) == key(name));
    if taken {
        return Err(BerthError::Validation("category_name_taken".into()));
    }
    Ok(name.to_owned())
}

/// Adds a section at the end of the list.
///
/// # Errors
/// `Validation` with `category_name_required`, `category_name_too_long` or
/// `category_name_taken`.
pub async fn create(db: &SqlitePool, tenant_id: &str, input: &CategoryInput) -> Result<String> {
    let name = validate(db, tenant_id, "", &input.name).await?;
    let id = uuid::Uuid::now_v7().to_string();
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(
        "INSERT INTO categories (id, tenant_id, name, aliases, position, updated_at, closed)
         SELECT $1, $2, $3, $4, COALESCE(MAX(position) + 1, 0), $5, $6
         FROM categories WHERE tenant_id = $2",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(&name)
    .bind(clean_aliases(&input.aliases))
    .bind(now_ms())
    .bind(input.closed)
    .execute(&mut *tx)
    .await
    .context("insert category")?;
    if give(&mut tx, tenant_id, &id, &input.users)
        .await
        .context("give category")?
    {
        touch_policies(&mut tx, tenant_id)
            .await
            .context("policies")?;
    }
    tx.commit().await.context("commit category")?;
    tracing::info!(category = %id, name = ?name, closed = input.closed, users = input.users.len(), "category created");
    Ok(id)
}

/// Renames a section and sets its aliases. Books keep it: they refer to the id.
///
/// # Errors
/// `NotFound`, the `Validation` codes of [`create`].
pub async fn update(
    db: &SqlitePool,
    tenant_id: &str,
    id: &str,
    input: &CategoryInput,
) -> Result<()> {
    let name = validate(db, tenant_id, id, &input.name).await?;
    let mut tx = db.begin().await.context("begin")?;
    let was_closed: Option<bool> =
        sqlx::query_scalar("SELECT closed FROM categories WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .context("load category")?;
    let was_closed = was_closed.ok_or(BerthError::NotFound)?;
    sqlx::query(
        "UPDATE categories SET name = $3, aliases = $4, updated_at = $5, closed = $6
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&name)
    .bind(clean_aliases(&input.aliases))
    .bind(now_ms())
    .bind(input.closed)
    .execute(&mut *tx)
    .await
    .context("update category")?;
    let regiven = give(&mut tx, tenant_id, id, &input.users)
        .await
        .context("give category")?;
    if regiven || was_closed != input.closed {
        touch_policies(&mut tx, tenant_id)
            .await
            .context("policies")?;
        tracing::info!(category = %id, closed = input.closed, users = input.users.len(), "category access changed");
    }
    tx.commit().await.context("commit category")?;
    Ok(())
}

/// Swaps a section with its neighbour above (`up`) or below.
///
/// # Errors
/// `NotFound`.
pub async fn shift(db: &SqlitePool, tenant_id: &str, id: &str, up: bool) -> Result<()> {
    let mut order: Vec<String> = list(db, tenant_id)
        .await
        .context("load categories")?
        .into_iter()
        .map(|c| c.id)
        .collect();
    let at = order
        .iter()
        .position(|c| c == id)
        .ok_or(BerthError::NotFound)?;
    let other = if up { at.checked_sub(1) } else { Some(at + 1) };
    let Some(other) = other.filter(|&o| o < order.len()) else {
        return Ok(());
    };
    order.swap(at, other);
    let mut tx = db.begin().await.context("begin")?;
    for (position, id) in order.iter().enumerate() {
        sqlx::query("UPDATE categories SET position = $3 WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_id)
            .bind(id)
            .bind(i64::try_from(position).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .context("reorder")?;
    }
    tx.commit().await.context("commit order")?;
    Ok(())
}

/// Removes a section; its books stay, and phones learn the change on the next sync.
///
/// # Errors
/// `NotFound`.
pub async fn delete(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    let mut tx = db.begin().await.context("begin")?;
    let books: Vec<String> = sqlx::query_scalar(
        "SELECT book_id FROM book_categories WHERE tenant_id = $1 AND category_id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .context("load books")?;
    sqlx::query("DELETE FROM book_categories WHERE tenant_id = $1 AND category_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("unlink books")?;
    let gated: bool = sqlx::query_scalar(
        "SELECT closed OR EXISTS (SELECT 1 FROM user_categories u WHERE u.category_id = c.id)
         FROM categories c WHERE c.tenant_id = $1 AND c.id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .context("load category")?
    .unwrap_or(false);
    sqlx::query("DELETE FROM user_categories WHERE tenant_id = $1 AND category_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("ungive")?;
    if gated {
        touch_policies(&mut tx, tenant_id)
            .await
            .context("policies")?;
    }
    let deleted = sqlx::query("DELETE FROM categories WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete category")?
        .rows_affected();
    if deleted == 0 {
        return Err(BerthError::NotFound);
    }
    if !books.is_empty() {
        let revision = next_revision(&mut tx, tenant_id)
            .await
            .context("revision")?;
        for book in &books {
            sqlx::query("UPDATE books SET revision = $3 WHERE tenant_id = $1 AND id = $2")
                .bind(tenant_id)
                .bind(book)
                .bind(revision)
                .execute(&mut *tx)
                .await
                .context("touch book")?;
        }
    }
    tx.commit().await.context("commit delete")?;
    tracing::info!(category = %id, books = books.len(), "category deleted");
    Ok(())
}

/// Sections whose name or an alias is one of `genres`, in list order.
///
/// # Errors
/// Database failures.
pub async fn match_genres(
    db: &SqlitePool,
    tenant_id: &str,
    genres: &[String],
) -> anyhow::Result<Vec<String>> {
    if genres.is_empty() {
        return Ok(Vec::new());
    }
    let genres: Vec<String> = genres.iter().map(|g| key(g)).collect();
    Ok(entries(db, tenant_id)
        .await?
        .into_iter()
        .filter(|c| {
            std::iter::once(c.name.as_str())
                .chain(c.aliases.lines())
                .any(|name| genres.contains(&key(name)))
        })
        .map(|c| c.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_aliases() {
        assert_eq!(key(" Ёлки "), "елки");
        assert_eq!(
            clean_aliases("Sci-Fi, sci-fi;\n Фантастика \n\n"),
            "Sci-Fi\nФантастика"
        );
    }
}
