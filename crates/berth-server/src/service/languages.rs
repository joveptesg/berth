//! Languages of books: the list the admin keeps (an ISO 639 code and the name in the
//! language), and the codes the tags of files give.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

/// The list a new server starts with.
pub const DEFAULTS: [(&str, &str); 8] = [
    ("ru", "Русский"),
    ("en", "English"),
    ("de", "Deutsch"),
    ("fr", "Français"),
    ("es", "Español"),
    ("it", "Italiano"),
    ("zh", "中文"),
    ("ja", "日本語"),
];
const MAX_NAME_LEN: usize = 40;

/// A language as forms and devices show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Language {
    /// ISO 639 code: `ru`, `en`…
    pub code: String,
    /// Name in the language itself: «Русский», «English».
    pub name: String,
}

/// A language with its number of books, for the settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LanguageEntry {
    /// Code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Books in it.
    pub books: i64,
}

/// Languages in their order.
///
/// # Errors
/// Database failures.
pub async fn list(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<Language>> {
    Ok(sqlx::query_as(
        "SELECT code, name FROM languages WHERE tenant_id = $1 ORDER BY position, code",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Languages with their book counts.
///
/// # Errors
/// Database failures.
pub async fn entries(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<Vec<LanguageEntry>> {
    Ok(sqlx::query_as(
        "SELECT l.code, l.name,
                (SELECT COUNT(*) FROM books b WHERE b.tenant_id = l.tenant_id
                 AND b.language = l.code AND b.deleted_at IS NULL) AS books
         FROM languages l WHERE l.tenant_id = $1 ORDER BY l.position, l.code",
    )
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// The starting list of a new tenant.
pub(crate) async fn seed(conn: &mut SqliteConnection, tenant_id: &str) -> sqlx::Result<()> {
    for (position, (code, name)) in DEFAULTS.iter().enumerate() {
        sqlx::query(
            "INSERT INTO languages (tenant_id, code, name, position) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant_id)
        .bind(code)
        .bind(name)
        .bind(i64::try_from(position).unwrap_or(i64::MAX))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn valid_code(code: &str) -> bool {
    (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("language_name_required".into()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(BerthError::Validation("language_name_too_long".into()));
    }
    Ok(name.to_owned())
}

/// Adds a language at the end of the list.
///
/// # Errors
/// `Validation` with `language_code_invalid`, `language_code_taken`, `language_name_required`
/// or `language_name_too_long`.
pub async fn create(db: &SqlitePool, tenant_id: &str, code: &str, name: &str) -> Result<()> {
    let code = code.trim().to_lowercase();
    if !valid_code(&code) {
        return Err(BerthError::Validation("language_code_invalid".into()));
    }
    let name = valid_name(name)?;
    let added = sqlx::query(
        "INSERT INTO languages (tenant_id, code, name, position)
         SELECT $1, $2, $3, COALESCE(MAX(position) + 1, 0) FROM languages WHERE tenant_id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(tenant_id)
    .bind(&code)
    .bind(&name)
    .execute(db)
    .await
    .context("insert language")?
    .rows_affected();
    if added == 0 {
        return Err(BerthError::Validation("language_code_taken".into()));
    }
    tracing::info!(code, name = ?name, "language added");
    Ok(())
}

/// Renames a language; its code stays, books keep it.
///
/// # Errors
/// `NotFound`, `Validation` with `language_name_required` or `language_name_too_long`.
pub async fn update(db: &SqlitePool, tenant_id: &str, code: &str, name: &str) -> Result<()> {
    let name = valid_name(name)?;
    let updated = sqlx::query("UPDATE languages SET name = $3 WHERE tenant_id = $1 AND code = $2")
        .bind(tenant_id)
        .bind(code)
        .bind(&name)
        .execute(db)
        .await
        .context("update language")?
        .rows_affected();
    if updated == 0 {
        return Err(BerthError::NotFound);
    }
    Ok(())
}

/// Removes a language no book has.
///
/// # Errors
/// `NotFound`, `Validation("language_in_use")`.
pub async fn delete(db: &SqlitePool, tenant_id: &str, code: &str) -> Result<()> {
    let books: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM books WHERE tenant_id = $1 AND language = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(code)
    .fetch_one(db)
    .await
    .context("count books")?;
    if books > 0 {
        return Err(BerthError::Validation("language_in_use".into()));
    }
    let deleted = sqlx::query("DELETE FROM languages WHERE tenant_id = $1 AND code = $2")
        .bind(tenant_id)
        .bind(code)
        .execute(db)
        .await
        .context("delete language")?
        .rows_affected();
    if deleted == 0 {
        return Err(BerthError::NotFound);
    }
    tracing::info!(code, "language deleted");
    Ok(())
}

/// Checks that `code` (or none) is in the list.
///
/// # Errors
/// `Validation("book_language_invalid")`; database failures.
pub async fn check(db: &SqlitePool, tenant_id: &str, code: Option<&str>) -> Result<()> {
    let Some(code) = code else { return Ok(()) };
    let known = list(db, tenant_id).await?.iter().any(|l| l.code == code);
    if known {
        Ok(())
    } else {
        Err(BerthError::Validation("book_language_invalid".into()))
    }
}

/// The ISO 639-1 code of a common spelling: `rus`, `Russian`, `русский`…
#[must_use]
pub fn alias(value: &str) -> Option<&'static str> {
    let value = value.trim().to_lowercase();
    let code = match value.as_str() {
        "rus" | "russian" | "русский" => "ru",
        "eng" | "english" | "английский" => "en",
        "ukr" | "ukrainian" | "українська" | "украинский" => "uk",
        "bel" | "belarusian" | "беларуская" | "белорусский" => "be",
        "kaz" | "kazakh" | "қазақ" | "қазақша" | "казахский" => "kk",
        "deu" | "ger" | "german" | "deutsch" | "немецкий" => "de",
        "fra" | "fre" | "french" | "français" | "французский" => "fr",
        "spa" | "spanish" | "español" | "испанский" => "es",
        "ita" | "italian" | "italiano" | "итальянский" => "it",
        "pol" | "polish" | "polski" | "польский" => "pl",
        "zho" | "chi" | "chinese" | "中文" | "китайский" => "zh",
        "jpn" | "japanese" | "日本語" | "японский" => "ja",
        _ => return None,
    };
    Some(code)
}

/// The code in `known` a tag value names: its code, its name, or a common spelling of it;
/// `None` for a language the admin does not keep.
#[must_use]
pub fn resolve(known: &[Language], value: &str) -> Option<String> {
    let value = value.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    let code = alias(&value).map_or(value.clone(), str::to_owned);
    known
        .iter()
        .find(|l| l.code == code || l.name.to_lowercase() == value)
        .map(|l| l.code.clone())
}

/// A guess when the files say nothing: Cyrillic in the title or names is Russian.
#[must_use]
pub fn guess<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    texts
        .into_iter()
        .any(|t| t.chars().any(|c| ('\u{400}'..='\u{4ff}').contains(&c)))
        .then_some("ru")
}

/// The codes of `list` ("en,ru") that are in `known`, in its order.
#[must_use]
pub fn pick(known: &[Language], list: &str) -> Vec<String> {
    let picked: Vec<&str> = list.split(',').map(str::trim).collect();
    known
        .iter()
        .filter(|l| picked.contains(&l.code.as_str()))
        .map(|l| l.code.clone())
        .collect()
}

/// The codes of a saved list, as they are.
#[must_use]
pub fn split(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<Language> {
        DEFAULTS
            .iter()
            .map(|(code, name)| Language {
                code: (*code).into(),
                name: (*name).into(),
            })
            .collect()
    }

    #[test]
    fn codes_from_tags_and_guesses() {
        let known = known();
        assert_eq!(resolve(&known, " RUS ").as_deref(), Some("ru"));
        assert_eq!(resolve(&known, "English").as_deref(), Some("en"));
        assert_eq!(resolve(&known, "ger").as_deref(), Some("de"));
        assert_eq!(resolve(&known, "Deutsch").as_deref(), Some("de"));
        assert_eq!(resolve(&known, "und"), None);
        assert_eq!(resolve(&known[..1], "en"), None, "not kept by the admin");
        assert_eq!(guess(["Основание", "Айзек Азимов"]), Some("ru"));
        assert_eq!(guess(["Foundation"]), None);
        assert_eq!(pick(&known, "ru, xx,en,ru"), ["ru", "en"]);
        assert!(valid_code("fr") && valid_code("chu") && !valid_code("FR") && !valid_code("f"));
    }
}
