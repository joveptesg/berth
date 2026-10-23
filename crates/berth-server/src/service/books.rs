//! The catalog: libraries and books, as the admin edits them and devices sync them.
//!
//! Every read of books takes a [`PolicyFilter`]; writes bump the tenant revision
//! (`UPDATE revisions … RETURNING v` in the same transaction), which drives `/v1/sync`.

use std::collections::HashMap;

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};

use super::{blobs, storage};
use crate::{AppState, db::now_ms, media};

/// Ratings of the 436-FZ scale.
pub const RATINGS: [i64; 5] = [0, 6, 12, 16, 18];
const MAX_TITLE_LEN: usize = 300;
const MAX_COVER_BYTES: usize = 10 << 20;
/// Books per sync page.
pub const SYNC_PAGE: i64 = 200;

/// What a reader may see. In the MVP every book is visible; access rules plug in here and
/// every book query below takes the filter, so none can forget it.
#[derive(Debug, Clone)]
pub struct PolicyFilter {
    /// `access_policies.policy_version` of the reader: part of the sync cursor.
    pub version: i64,
}

impl PolicyFilter {
    /// The admin sees everything.
    #[must_use]
    pub fn admin() -> Self {
        Self { version: 0 }
    }

    /// The filter of a user.
    ///
    /// # Errors
    /// Database failures.
    pub async fn for_user(db: &SqlitePool, tenant_id: &str, user_id: &str) -> anyhow::Result<Self> {
        let version: Option<i64> = sqlx::query_scalar(
            "SELECT policy_version FROM access_policies WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(tenant_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?;
        Ok(Self {
            version: version.unwrap_or(1),
        })
    }
}

/// Next tenant revision; call inside the write transaction.
pub(crate) async fn next_revision(
    conn: &mut SqliteConnection,
    tenant_id: &str,
) -> sqlx::Result<i64> {
    sqlx::query_scalar("UPDATE revisions SET v = v + 1 WHERE tenant_id = $1 RETURNING v")
        .bind(tenant_id)
        .fetch_one(conn)
        .await
}

/// The tenant's first library, created when missing.
pub(crate) async fn default_library(
    conn: &mut SqliteConnection,
    tenant_id: &str,
) -> anyhow::Result<String> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM libraries WHERE tenant_id = $1 AND deleted_at IS NULL ORDER BY id LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = id {
        return Ok(id);
    }
    let id = uuid::Uuid::now_v7().to_string();
    let revision = next_revision(conn, tenant_id).await?;
    sqlx::query(
        "INSERT INTO libraries (id, tenant_id, name, revision, updated_at) VALUES ($1, $2, 'Library', $3, $4)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(revision)
    .bind(now_ms())
    .execute(conn)
    .await?;
    Ok(id)
}

/// The id of the default library (creating it).
///
/// # Errors
/// Database failures.
pub async fn default_library_id(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<String> {
    let mut conn = db.acquire().await?;
    default_library(&mut conn, tenant_id).await
}

/// A file of a book on the book timeline.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct BookFile {
    /// Order in the book.
    pub idx: i64,
    /// Content hash.
    pub blob_hash: String,
    /// Bytes.
    pub size: i64,
    /// Start on the book timeline.
    pub offset_ms: i64,
    /// Playing time.
    pub duration_ms: i64,
}

/// A chapter on the book timeline.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Chapter {
    /// Order.
    pub idx: i64,
    /// Title.
    pub title: String,
    /// Start.
    pub start_ms: i64,
    /// End.
    pub end_ms: i64,
}

/// A book with its people.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Book {
    /// Id.
    pub id: String,
    /// Library.
    pub library_id: String,
    /// Title.
    pub title: String,
    /// Authors in order.
    pub authors: Vec<String>,
    /// Narrators in order.
    pub narrators: Vec<String>,
    /// Series name.
    pub series: Option<String>,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Age rating: 0, 6, 12, 16, 18.
    pub rating: Option<i64>,
    /// Playing time.
    pub duration_ms: i64,
    /// Cover blob.
    pub cover_hash: Option<String>,
    /// Revision of the last change.
    pub revision: i64,
    /// When added.
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct BookRow {
    id: String,
    library_id: String,
    title: String,
    series: Option<String>,
    series_no: Option<f64>,
    rating: Option<i64>,
    duration_ms: i64,
    cover_hash: Option<String>,
    revision: i64,
    created_at: i64,
    deleted_at: Option<i64>,
}

impl BookRow {
    fn into_book(self, people: &mut HashMap<String, (Vec<String>, Vec<String>)>) -> Book {
        let (authors, narrators) = people.remove(&self.id).unwrap_or_default();
        Book {
            id: self.id,
            library_id: self.library_id,
            title: self.title,
            authors,
            narrators,
            series: self.series,
            series_no: self.series_no,
            rating: self.rating,
            duration_ms: self.duration_ms,
            cover_hash: self.cover_hash,
            revision: self.revision,
            created_at: self.created_at,
        }
    }
}

/// People of the tenant's books with revision in `(from, to]`, by book.
async fn people(
    db: &SqlitePool,
    tenant_id: &str,
    from: i64,
    to: i64,
) -> sqlx::Result<HashMap<String, (Vec<String>, Vec<String>)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT p.book_id, p.role, p.person FROM book_people p
         JOIN books b ON b.id = p.book_id
         WHERE p.tenant_id = $1 AND b.revision > $2 AND b.revision <= $3
         ORDER BY p.book_id, p.role, p.idx",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for (book, role, person) in rows {
        let entry = out.entry(book).or_default();
        if role == "author" {
            entry.0.push(person);
        } else {
            entry.1.push(person);
        }
    }
    Ok(out)
}

/// Admin list filters; empty strings mean "any".
#[derive(Debug, Default, Clone)]
pub struct BookQuery {
    /// Words in title, people or series.
    pub search: String,
    /// Exact author.
    pub author: String,
    /// Exact series.
    pub series: String,
    /// Rating, or empty.
    pub rating: Option<i64>,
}

/// Books of a library: series together by number, then by title.
///
/// # Errors
/// Database failures.
pub async fn list(
    db: &SqlitePool,
    tenant_id: &str,
    _policy: &PolicyFilter,
    library_id: &str,
    query: &BookQuery,
) -> anyhow::Result<Vec<Book>> {
    let search = format!("%{}%", query.search.trim().to_lowercase());
    let rows: Vec<BookRow> = sqlx::query_as(
        "SELECT id, library_id, title, series, series_no, rating, duration_ms, cover_hash, revision,
                created_at, deleted_at
         FROM books b
         WHERE tenant_id = $1 AND library_id = $2 AND deleted_at IS NULL
           AND search_text LIKE $3
           AND ($4 = '' OR EXISTS (SELECT 1 FROM book_people p
                 WHERE p.book_id = b.id AND p.role = 'author' AND p.person = $4))
           AND ($5 = '' OR series = $5)
           AND ($6 IS NULL OR rating = $6)
         ORDER BY CASE WHEN series IS NULL THEN title ELSE series END, series_no, title",
    )
    .bind(tenant_id)
    .bind(library_id)
    .bind(search)
    .bind(query.author.trim())
    .bind(query.series.trim())
    .bind(query.rating)
    .fetch_all(db)
    .await?;
    let mut people = people(db, tenant_id, 0, i64::MAX).await?;
    Ok(rows.into_iter().map(|r| r.into_book(&mut people)).collect())
}

/// Authors and series of a library, for the filters.
///
/// # Errors
/// Database failures.
pub async fn facets(
    db: &SqlitePool,
    tenant_id: &str,
    library_id: &str,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let authors = sqlx::query_scalar(
        "SELECT DISTINCT p.person FROM book_people p JOIN books b ON b.id = p.book_id
         WHERE p.tenant_id = $1 AND b.library_id = $2 AND b.deleted_at IS NULL AND p.role = 'author'
         ORDER BY p.person",
    )
    .bind(tenant_id)
    .bind(library_id)
    .fetch_all(db)
    .await?;
    let series = sqlx::query_scalar(
        "SELECT DISTINCT series FROM books
         WHERE tenant_id = $1 AND library_id = $2 AND deleted_at IS NULL AND series IS NOT NULL
         ORDER BY series",
    )
    .bind(tenant_id)
    .bind(library_id)
    .fetch_all(db)
    .await?;
    Ok((authors, series))
}

/// A book with its files, chapters and storage.
#[derive(Debug, Clone, Serialize)]
pub struct BookDetail {
    /// The book.
    #[serde(flatten)]
    pub book: Book,
    /// Files in order.
    pub files: Vec<BookFile>,
    /// Chapters in order.
    pub chapters: Vec<Chapter>,
    /// Name of the storage profile holding the files.
    pub storage: Option<String>,
}

async fn row(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<BookRow> {
    sqlx::query_as(
        "SELECT id, library_id, title, series, series_no, rating, duration_ms, cover_hash, revision,
                created_at, deleted_at
         FROM books WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .context("load book")?
    .ok_or(BerthError::NotFound)
}

/// One book.
///
/// # Errors
/// `NotFound`.
pub async fn get(
    db: &SqlitePool,
    tenant_id: &str,
    _policy: &PolicyFilter,
    id: &str,
) -> Result<BookDetail> {
    let book = row(db, tenant_id, id).await?;
    let mut people = people(db, tenant_id, book.revision - 1, book.revision)
        .await
        .context("people")?;
    let files = files_of(db, tenant_id, id).await.context("files")?;
    let chapters = chapters_of(db, tenant_id, id).await.context("chapters")?;
    let storage: Option<String> = sqlx::query_scalar(
        "SELECT s.name FROM book_files f
         JOIN blob_locations l ON l.tenant_id = f.tenant_id AND l.hash = f.blob_hash AND l.state = 'active'
         JOIN storage_profiles s ON s.id = l.storage_id
         WHERE f.tenant_id = $1 AND f.book_id = $2 LIMIT 1",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .context("storage")?;
    Ok(BookDetail {
        book: book.into_book(&mut people),
        files,
        chapters,
        storage,
    })
}

async fn files_of(db: &SqlitePool, tenant_id: &str, book_id: &str) -> sqlx::Result<Vec<BookFile>> {
    sqlx::query_as(
        "SELECT f.idx, f.blob_hash, b.size, f.offset_ms, f.duration_ms FROM book_files f
         JOIN blobs b ON b.tenant_id = f.tenant_id AND b.hash = f.blob_hash
         WHERE f.tenant_id = $1 AND f.book_id = $2 ORDER BY f.idx",
    )
    .bind(tenant_id)
    .bind(book_id)
    .fetch_all(db)
    .await
}

async fn chapters_of(
    db: &SqlitePool,
    tenant_id: &str,
    book_id: &str,
) -> sqlx::Result<Vec<Chapter>> {
    sqlx::query_as(
        "SELECT idx, title, start_ms, end_ms FROM chapters
         WHERE tenant_id = $1 AND book_id = $2 ORDER BY idx",
    )
    .bind(tenant_id)
    .bind(book_id)
    .fetch_all(db)
    .await
}

/// Metadata the admin edits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookEdits {
    /// Title.
    pub title: String,
    /// Authors in order.
    pub authors: Vec<String>,
    /// Narrators in order.
    pub narrators: Vec<String>,
    /// Series name; empty for none.
    pub series: String,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Age rating.
    pub rating: Option<i64>,
}

impl BookEdits {
    /// Validated and trimmed.
    ///
    /// # Errors
    /// `Validation` with `book_title_required`, `book_title_too_long` or `book_rating_invalid`.
    pub fn validated(&self) -> Result<Self> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err(BerthError::Validation("book_title_required".into()));
        }
        if title.chars().count() > MAX_TITLE_LEN {
            return Err(BerthError::Validation("book_title_too_long".into()));
        }
        if self.rating.is_some_and(|r| !RATINGS.contains(&r)) {
            return Err(BerthError::Validation("book_rating_invalid".into()));
        }
        let names = |list: &[String]| {
            let mut out: Vec<String> = Vec::new();
            for name in list.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
                if !out.iter().any(|o| o == name) {
                    out.push(name.to_owned());
                }
            }
            out
        };
        let series = self.series.trim().to_owned();
        Ok(Self {
            title: title.to_owned(),
            authors: names(&self.authors),
            narrators: names(&self.narrators),
            series_no: self.series_no.filter(|_| !series.is_empty()),
            series,
            rating: self.rating,
        })
    }

    fn search_text(&self) -> String {
        [self.title.as_str(), &self.series]
            .into_iter()
            .chain(self.authors.iter().map(String::as_str))
            .chain(self.narrators.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
}

/// Splits "A, B; C" into names.
#[must_use]
pub fn split_names(text: &str) -> Vec<String> {
    text.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Writes the edited fields and people; the caller bumps the revision.
pub(crate) async fn write_edits(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    book_id: &str,
    edits: &BookEdits,
    revision: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE books SET title = $3, series = $4, series_no = $5, rating = $6, search_text = $7,
            revision = $8, updated_at = $9
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(book_id)
    .bind(&edits.title)
    .bind((!edits.series.is_empty()).then_some(&edits.series))
    .bind(edits.series_no)
    .bind(edits.rating)
    .bind(edits.search_text())
    .bind(revision)
    .bind(now_ms())
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM book_people WHERE tenant_id = $1 AND book_id = $2")
        .bind(tenant_id)
        .bind(book_id)
        .execute(&mut *conn)
        .await?;
    for (role, names) in [("author", &edits.authors), ("narrator", &edits.narrators)] {
        for (idx, person) in names.iter().enumerate() {
            sqlx::query(
                "INSERT INTO book_people (tenant_id, book_id, role, idx, person) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(tenant_id)
            .bind(book_id)
            .bind(role)
            .bind(i64::try_from(idx).unwrap_or(i64::MAX))
            .bind(person)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Saves the admin's edits.
///
/// # Errors
/// `NotFound`, the `Validation` codes of [`BookEdits::validated`].
pub async fn update(db: &SqlitePool, tenant_id: &str, id: &str, edits: &BookEdits) -> Result<()> {
    let edits = edits.validated()?;
    row(db, tenant_id, id).await?;
    let mut tx = db.begin().await.context("begin")?;
    let revision = next_revision(&mut tx, tenant_id)
        .await
        .context("revision")?;
    write_edits(&mut tx, tenant_id, id, &edits, revision)
        .await
        .context("save book")?;
    tx.commit().await.context("commit book")?;
    Ok(())
}

/// Replaces the cover with an uploaded image (JPEG, PNG or WebP up to 10 MB).
///
/// # Errors
/// `NotFound`, `Validation` with `cover_invalid` or `cover_too_large`.
pub async fn set_cover(state: &AppState, tenant_id: &str, id: &str, data: Vec<u8>) -> Result<()> {
    row(&state.db, tenant_id, id).await?;
    if data.len() > MAX_COVER_BYTES {
        return Err(BerthError::Validation("cover_too_large".into()));
    }
    let mime =
        media::image_type(&data).ok_or_else(|| BerthError::Validation("cover_invalid".into()))?;
    let storage_id = storage::default_id(&state.db, tenant_id)
        .await?
        .context("no default storage")?;
    let hash = blobs::store_bytes(state, tenant_id, &storage_id, data, mime).await?;
    let mut tx = state.db.begin().await.context("begin")?;
    let revision = next_revision(&mut tx, tenant_id)
        .await
        .context("revision")?;
    sqlx::query(
        "UPDATE books SET cover_hash = $3, revision = $4, updated_at = $5 WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(&hash)
    .bind(revision)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("save cover")?;
    tx.commit().await.context("commit cover")?;
    Ok(())
}

/// Removes a book: devices drop it on the next sync. Its blobs stay (collected later).
///
/// # Errors
/// `NotFound`.
pub async fn delete(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    row(db, tenant_id, id).await?;
    let mut tx = db.begin().await.context("begin")?;
    let revision = next_revision(&mut tx, tenant_id)
        .await
        .context("revision")?;
    sqlx::query(
        "UPDATE books SET deleted_at = $3, revision = $4, updated_at = $3 WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(now_ms())
    .bind(revision)
    .execute(&mut *tx)
    .await
    .context("delete book")?;
    tx.commit().await.context("commit delete")?;
    tracing::info!(book = %id, "book deleted");
    Ok(())
}

/// The cover hash of a book the reader may see.
///
/// # Errors
/// `NotFound` when the book is hidden or has no cover.
pub async fn cover_hash(
    db: &SqlitePool,
    tenant_id: &str,
    _policy: &PolicyFilter,
    id: &str,
) -> Result<String> {
    row(db, tenant_id, id)
        .await?
        .cover_hash
        .ok_or(BerthError::NotFound)
}

// ── Sync ─────────────────────────────────────────────────

/// A book as devices replicate it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SyncBook {
    /// The book.
    #[serde(flatten)]
    pub book: Book,
    /// Files in order.
    pub files: Vec<BookFile>,
    /// Chapters in order.
    pub chapters: Vec<Chapter>,
}

/// One page of the delta.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SyncPage {
    /// Pass back as `cursor` for the next page or the next sync.
    pub cursor: String,
    /// The client must drop every book that is not in this sync's pages.
    pub reset: bool,
    /// More pages follow right away.
    pub has_more: bool,
    /// New and changed books.
    pub books: Vec<SyncBook>,
    /// Ids of books removed or no longer visible.
    pub removed: Vec<String>,
    /// The server's addresses as the admin set them, home network first: a paired phone
    /// learns new ones here and switches between them without pairing again. Empty when
    /// none are set.
    pub server_urls: Vec<String>,
}

fn parse_cursor(cursor: &str) -> Option<(i64, i64)> {
    let mut parts = cursor.strip_prefix("v1.")?.split('.');
    let rev = parts.next()?.parse().ok()?;
    let policy = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((rev, policy))
}

/// Changes after `cursor` (none or unusable: a full slice with `reset`).
///
/// # Errors
/// Database failures.
pub async fn sync(
    db: &SqlitePool,
    tenant_id: &str,
    policy: &PolicyFilter,
    cursor: Option<&str>,
) -> anyhow::Result<SyncPage> {
    let (from, reset) = match cursor.and_then(parse_cursor) {
        Some((rev, version)) if version == policy.version => (rev, false),
        _ => (0, true),
    };
    let rows: Vec<BookRow> = sqlx::query_as(
        "SELECT id, library_id, title, series, series_no, rating, duration_ms, cover_hash, revision,
                created_at, deleted_at
         FROM books WHERE tenant_id = $1 AND revision > $2
           AND ($3 = 0 OR deleted_at IS NULL)
         ORDER BY revision LIMIT $4",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(i64::from(reset))
    .bind(SYNC_PAGE + 1)
    .fetch_all(db)
    .await?;
    let has_more = rows.len() > usize::try_from(SYNC_PAGE)?;
    let rows: Vec<BookRow> = rows.into_iter().take(usize::try_from(SYNC_PAGE)?).collect();
    let to = match rows.last() {
        Some(last) => last.revision,
        None => {
            sqlx::query_scalar("SELECT v FROM revisions WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(db)
                .await?
        }
    };
    let mut people = people(db, tenant_id, from, to).await?;
    let mut files: HashMap<String, Vec<BookFile>> = HashMap::new();
    let file_rows: Vec<(String, i64, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT f.book_id, f.idx, f.blob_hash, bl.size, f.offset_ms, f.duration_ms FROM book_files f
         JOIN books b ON b.id = f.book_id
         JOIN blobs bl ON bl.tenant_id = f.tenant_id AND bl.hash = f.blob_hash
         WHERE f.tenant_id = $1 AND b.revision > $2 AND b.revision <= $3 ORDER BY f.book_id, f.idx",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;
    for (book, idx, blob_hash, size, offset_ms, duration_ms) in file_rows {
        files.entry(book).or_default().push(BookFile {
            idx,
            blob_hash,
            size,
            offset_ms,
            duration_ms,
        });
    }
    let mut chapters: HashMap<String, Vec<Chapter>> = HashMap::new();
    let chapter_rows: Vec<(String, i64, String, i64, i64)> = sqlx::query_as(
        "SELECT c.book_id, c.idx, c.title, c.start_ms, c.end_ms FROM chapters c
         JOIN books b ON b.id = c.book_id
         WHERE c.tenant_id = $1 AND b.revision > $2 AND b.revision <= $3 ORDER BY c.book_id, c.idx",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;
    for (book, idx, title, start_ms, end_ms) in chapter_rows {
        chapters.entry(book).or_default().push(Chapter {
            idx,
            title,
            start_ms,
            end_ms,
        });
    }

    let mut books = Vec::new();
    let mut removed = Vec::new();
    for row in rows {
        if row.deleted_at.is_some() {
            removed.push(row.id);
            continue;
        }
        let id = row.id.clone();
        books.push(SyncBook {
            book: row.into_book(&mut people),
            files: files.remove(&id).unwrap_or_default(),
            chapters: chapters.remove(&id).unwrap_or_default(),
        });
    }
    Ok(SyncPage {
        cursor: format!("v1.{to}.{}", policy.version),
        reset,
        has_more,
        books,
        removed,
        // The API layer fills it from the settings.
        server_urls: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_and_edits() {
        assert_eq!(parse_cursor("v1.42.3"), Some((42, 3)));
        for bad in ["", "v1.42", "v2.1.1", "v1.x.1", "v1.1.1.1"] {
            assert_eq!(parse_cursor(bad), None, "{bad}");
        }
        assert_eq!(split_names(" Анна ,Борис; ;"), ["Анна", "Борис"]);
        let edits = BookEdits {
            title: " Книга ".into(),
            authors: vec!["А".into(), " А ".into(), String::new()],
            series_no: Some(2.0),
            ..BookEdits::default()
        }
        .validated()
        .unwrap();
        assert_eq!((edits.title.as_str(), edits.authors.len()), ("Книга", 1));
        assert_eq!(edits.series_no, None, "no number without a series");
        let bad = BookEdits {
            title: "x".into(),
            rating: Some(7),
            ..BookEdits::default()
        };
        assert!(bad.validated().is_err());
    }
}
