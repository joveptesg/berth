//! The catalog: libraries and books, as the admin edits them and devices sync them.
//!
//! Every read of books takes a [`PolicyFilter`]; writes bump the tenant revision
//! (`UPDATE revisions … RETURNING v` in the same transaction), which drives `/v1/sync`.

use std::collections::{HashMap, HashSet};

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::Serialize;
use sqlx::{AssertSqlSafe, SqliteConnection, SqlitePool};

use super::{blobs, storage};
use crate::{AppState, db::now_ms, media};

/// Age ratings: the minimum listener age.
pub const RATINGS: [i64; 5] = [0, 6, 12, 16, 18];
/// From this age a listener has no rating ceiling.
const ADULT_AGE: i64 = 18;
const DAY_MS: i64 = 86_400_000;
const MAX_TITLE_LEN: usize = 300;
/// Longest description, in characters.
pub const MAX_DESCRIPTION_LEN: usize = 4000;
/// Years a book may carry: ancient works have small numbers, the future none.
pub const YEARS: std::ops::RangeInclusive<i64> = 1..=2100;
const MAX_COVER_BYTES: usize = 10 << 20;
/// Books per sync page.
pub const SYNC_PAGE: i64 = 200;

/// What a reader may see: the whole library, or only the books picked for them. Every book
/// query below takes the filter, so none can forget it.
#[derive(Debug, Clone)]
pub struct PolicyFilter {
    /// `access_policies.policy_version` of the reader: part of the sync cursor.
    pub version: i64,
    /// The only books the reader may see; `None` for the whole library.
    pub books: Option<HashSet<String>>,
    /// Closed sections not given to the reader: their books and names stay hidden.
    pub hidden_categories: HashSet<String>,
}

impl PolicyFilter {
    /// The admin sees everything.
    #[must_use]
    pub fn admin() -> Self {
        Self {
            version: 0,
            books: None,
            hidden_categories: HashSet::new(),
        }
    }

    /// The filter of a user. The owner sees everything. A listener in groups sees what any of
    /// their groups sees; one outside groups, what their own rule gives (see [`Rule`]). A
    /// child (with a date of birth) then sees only the books rated up to their age, and books
    /// without a rating if allowed.
    ///
    /// # Errors
    /// Database failures.
    pub async fn for_user(db: &SqlitePool, tenant_id: &str, user_id: &str) -> anyhow::Result<Self> {
        type Policy = (
            i64,
            String,
            String,
            String,
            Option<String>,
            Option<i64>,
            String,
        );
        let policy: Option<Policy> = sqlx::query_as(
            "SELECT p.policy_version, p.book_access, p.languages, u.role, p.birth_date,
                    p.max_rating, p.unrated_behavior
             FROM access_policies p JOIN users u ON u.id = p.user_id
             WHERE p.tenant_id = $1 AND p.user_id = $2",
        )
        .bind(tenant_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?;
        let (mut version, access, languages, role, birth_date, applied, unrated) = policy
            .unwrap_or_else(|| {
                let all = "all".to_owned();
                (
                    1,
                    all,
                    String::new(),
                    "member".into(),
                    None,
                    None,
                    "allowed".into(),
                )
            });
        if role == "owner" {
            return Ok(Self {
                version,
                ..Self::admin()
            });
        }
        let (mut books, hidden_categories) = match groups_view(db, tenant_id, user_id).await? {
            Some(view) => view,
            None => {
                Rule::User(user_id)
                    .view(db, tenant_id, &access, &languages)
                    .await?
            }
        };
        if let Some(birth) = birth_date.as_deref() {
            let ceiling = rating_ceiling(birth, now_ms());
            if ceiling != applied {
                // A birthday lifted the ceiling: a new version, so phones get a full slice.
                sqlx::query(
                    "UPDATE access_policies SET max_rating = $3, policy_version = policy_version + 1
                     WHERE tenant_id = $1 AND user_id = $2 AND max_rating IS NOT $3",
                )
                .bind(tenant_id)
                .bind(user_id)
                .bind(ceiling)
                .execute(db)
                .await?;
                version = sqlx::query_scalar(
                    "SELECT policy_version FROM access_policies WHERE tenant_id = $1 AND user_id = $2",
                )
                .bind(tenant_id)
                .bind(user_id)
                .fetch_one(db)
                .await?;
                tracing::info!(user = %user_id, ?ceiling, "rating ceiling changed with age");
            }
            if let Some(ceiling) = ceiling {
                let fit: HashSet<String> = sqlx::query_scalar(
                    "SELECT id FROM books WHERE tenant_id = $1
                     AND (rating <= $2 OR (rating IS NULL AND $3 = 'allowed'))",
                )
                .bind(tenant_id)
                .bind(ceiling)
                .bind(&unrated)
                .fetch_all(db)
                .await?
                .into_iter()
                .collect();
                books = Some(narrow(books, &fit));
            }
        }
        Ok(Self {
            version,
            books,
            hidden_categories,
        })
    }

    /// The books picked for a user; `None` when they see the whole library.
    ///
    /// # Errors
    /// Database failures.
    pub async fn picked(
        db: &SqlitePool,
        tenant_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Option<HashSet<String>>> {
        let access: Option<String> = sqlx::query_scalar(
            "SELECT book_access FROM access_policies WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(tenant_id)
        .bind(user_id)
        .fetch_optional(db)
        .await?;
        if access.as_deref() != Some("selected") {
            return Ok(None);
        }
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT book_id FROM user_books WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(tenant_id)
        .bind(user_id)
        .fetch_all(db)
        .await?;
        Ok(Some(ids.into_iter().collect()))
    }

    /// Whether the reader may see book `id`.
    #[must_use]
    pub fn allows(&self, id: &str) -> bool {
        self.books.as_ref().is_none_or(|books| books.contains(id))
    }
}

/// What the groups of a listener show together: the books any of them sees (`None` for
/// all) and the closed sections none of them opens; `None` when the listener is in no group.
async fn groups_view(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
) -> anyhow::Result<Option<(Option<HashSet<String>>, HashSet<String>)>> {
    let groups: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT g.id, g.book_access, g.languages FROM group_members m
         JOIN user_groups g ON g.id = m.group_id
         WHERE m.tenant_id = $1 AND m.user_id = $2",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    if groups.is_empty() {
        return Ok(None);
    }
    let mut books = Some(HashSet::new());
    let mut hidden: Option<HashSet<String>> = None;
    for (id, access, languages) in &groups {
        let (seen, closed) = Rule::Group(id)
            .view(db, tenant_id, access, languages)
            .await?;
        books = match (books, seen) {
            (Some(mut all), Some(seen)) => {
                all.extend(seen);
                Some(all)
            }
            _ => None,
        };
        hidden = Some(match hidden {
            Some(hidden) => hidden.intersection(&closed).cloned().collect(),
            None => closed,
        });
    }
    Ok(Some((books, hidden.unwrap_or_default())))
}

/// The books a group's rule shows; `None` for the whole library.
///
/// # Errors
/// Database failures.
pub async fn group_view(
    db: &SqlitePool,
    tenant_id: &str,
    group_id: &str,
    access: &str,
    languages: &str,
) -> anyhow::Result<Option<HashSet<String>>> {
    Ok(Rule::Group(group_id)
        .view(db, tenant_id, access, languages)
        .await?
        .0)
}

/// Ids of the tenant's books that are not deleted.
///
/// # Errors
/// Database failures.
pub async fn alive_ids(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<HashSet<String>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM books WHERE tenant_id = $1 AND deleted_at IS NULL")
            .bind(tenant_id)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect(),
    )
}

/// How many of `alive` books `books` shows (`None` for all).
#[must_use]
pub fn seen_count<S: std::hash::BuildHasher, T: std::hash::BuildHasher>(
    books: Option<&HashSet<String, S>>,
    alive: &HashSet<String, T>,
) -> i64 {
    let n = books.map_or(alive.len(), |books| {
        books.iter().filter(|id| alive.contains(*id)).count()
    });
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `books` (`None` for all) narrowed to `fit`.
fn narrow(books: Option<HashSet<String>>, fit: &HashSet<String>) -> HashSet<String> {
    match books {
        Some(books) => books.intersection(fit).cloned().collect(),
        None => fit.clone(),
    }
}

/// Whose access rule: a listener's own or a group's. A rule gives the whole library, or the
/// picked books plus the books of the sections given; then without the books of closed
/// sections not given, and only the books in its languages or without a language.
#[derive(Debug, Clone, Copy)]
enum Rule<'a> {
    User(&'a str),
    Group(&'a str),
}

impl Rule<'_> {
    /// Tables of the picked books and given sections, their owner column, the owner id.
    fn tables(self) -> (&'static str, &'static str, &'static str, String) {
        match self {
            Rule::User(id) => ("user_books", "user_categories", "user_id", id.to_owned()),
            Rule::Group(id) => ("group_books", "group_categories", "group_id", id.to_owned()),
        }
    }

    /// The books the rule shows (`None` for all) and the closed sections it hides.
    async fn view(
        self,
        db: &SqlitePool,
        tenant_id: &str,
        access: &str,
        languages: &str,
    ) -> anyhow::Result<(Option<HashSet<String>>, HashSet<String>)> {
        // Table and column names come from the fixed list in `tables`.
        let (picked_table, given_table, owner, id) = self.tables();
        let mut books = None;
        if access == "selected" {
            let mut picked: HashSet<String> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT book_id FROM {picked_table} WHERE tenant_id = $1 AND {owner} = $2"
            )))
            .bind(tenant_id)
            .bind(&id)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
            let shelved: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT bc.book_id FROM book_categories bc JOIN {given_table} uc
                   ON uc.tenant_id = bc.tenant_id AND uc.category_id = bc.category_id
                 WHERE bc.tenant_id = $1 AND uc.{owner} = $2"
            )))
            .bind(tenant_id)
            .bind(&id)
            .fetch_all(db)
            .await?;
            picked.extend(shelved);
            books = Some(picked);
        }
        let hidden: HashSet<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT id FROM categories c WHERE c.tenant_id = $1 AND c.closed
             AND NOT EXISTS (SELECT 1 FROM {given_table} uc
                 WHERE uc.category_id = c.id AND uc.{owner} = $2)"
        )))
        .bind(tenant_id)
        .bind(&id)
        .fetch_all(db)
        .await?
        .into_iter()
        .collect();
        if !hidden.is_empty() {
            let open: HashSet<String> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT id FROM books b WHERE b.tenant_id = $1 AND NOT EXISTS (
                     SELECT 1 FROM book_categories bc JOIN categories c ON c.id = bc.category_id
                     WHERE bc.book_id = b.id AND c.closed AND NOT EXISTS (
                         SELECT 1 FROM {given_table} uc
                         WHERE uc.category_id = c.id AND uc.{owner} = $2))"
            )))
            .bind(tenant_id)
            .bind(&id)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
            books = Some(narrow(books, &open));
        }
        let languages = super::languages::split(languages);
        if !languages.is_empty() {
            let spoken: HashSet<String> = sqlx::query_scalar(
                "SELECT id FROM books WHERE tenant_id = $1
                 AND (language IS NULL OR instr(',' || $2 || ',', ',' || language || ',') > 0)",
            )
            .bind(tenant_id)
            .bind(languages.join(","))
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
            books = Some(narrow(books, &spoken));
        }
        Ok((books, hidden))
    }
}

/// Parses a date of birth, `YYYY-MM-DD`: days since 1970-01-01, or `None` for no such date.
#[must_use]
pub fn birth_days(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let days = days_from_civil(y, m, d);
    (civil_from_days(days) == (y, m, d)).then_some(days)
}

/// The rating ceiling of a child born on `birth` (`YYYY-MM-DD`) at `now_ms`: the highest of
/// [`RATINGS`] not above their age; `None` for an adult or a date that is not one.
#[must_use]
pub fn rating_ceiling(birth: &str, now_ms: i64) -> Option<i64> {
    let born = civil_from_days(birth_days(birth)?);
    let today = civil_from_days(now_ms.div_euclid(DAY_MS));
    let age = today.0 - born.0 - i64::from((today.1, today.2) < (born.1, born.2));
    if age >= ADULT_AGE {
        return None;
    }
    Some(
        RATINGS
            .iter()
            .copied()
            .filter(|r| *r <= age)
            .max()
            .unwrap_or(0),
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date (year, month, day) of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
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
    /// Sections (`SyncPage.categories`) in their order.
    pub category_ids: Vec<String>,
    /// Series name.
    pub series: Option<String>,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Age rating: 0, 6, 12, 16, 18.
    pub rating: Option<i64>,
    /// Language, ISO 639-1; `None` when not known.
    pub language: Option<String>,
    /// What the book is about, a few sentences.
    pub description: Option<String>,
    /// Year the work was first published.
    pub year: Option<i64>,
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
    language: Option<String>,
    description: Option<String>,
    year: Option<i64>,
    duration_ms: i64,
    cover_hash: Option<String>,
    revision: i64,
    created_at: i64,
    deleted_at: Option<i64>,
}

impl BookRow {
    fn into_book(self, links: &mut HashMap<String, Links>) -> Book {
        let Links {
            authors,
            narrators,
            categories,
        } = links.remove(&self.id).unwrap_or_default();
        Book {
            id: self.id,
            library_id: self.library_id,
            title: self.title,
            authors,
            narrators,
            category_ids: categories,
            series: self.series,
            series_no: self.series_no,
            rating: self.rating,
            language: self.language,
            description: self.description,
            year: self.year,
            duration_ms: self.duration_ms,
            cover_hash: self.cover_hash,
            revision: self.revision,
            created_at: self.created_at,
        }
    }
}

/// What a book refers to besides its own row.
#[derive(Default)]
struct Links {
    authors: Vec<String>,
    narrators: Vec<String>,
    categories: Vec<String>,
}

/// People and sections of the tenant's books with revision in `(from, to]`, by book.
async fn links(
    db: &SqlitePool,
    tenant_id: &str,
    from: i64,
    to: i64,
) -> sqlx::Result<HashMap<String, Links>> {
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
    let mut out: HashMap<String, Links> = HashMap::new();
    for (book, role, person) in rows {
        let entry = out.entry(book).or_default();
        if role == "author" {
            entry.authors.push(person);
        } else {
            entry.narrators.push(person);
        }
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT bc.book_id, bc.category_id FROM book_categories bc
         JOIN books b ON b.id = bc.book_id
         JOIN categories c ON c.id = bc.category_id
         WHERE bc.tenant_id = $1 AND b.revision > $2 AND b.revision <= $3
         ORDER BY bc.book_id, c.position, c.name",
    )
    .bind(tenant_id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;
    for (book, category) in rows {
        out.entry(book).or_default().categories.push(category);
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
    /// Section id.
    pub category: String,
    /// Language code.
    pub language: String,
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
    policy: &PolicyFilter,
    library_id: &str,
    query: &BookQuery,
) -> anyhow::Result<Vec<Book>> {
    let search = format!("%{}%", query.search.trim().to_lowercase());
    let rows: Vec<BookRow> = sqlx::query_as(
        "SELECT id, library_id, title, series, series_no, rating, language, description, year, duration_ms, cover_hash, revision,
                created_at, deleted_at
         FROM books b
         WHERE tenant_id = $1 AND library_id = $2 AND deleted_at IS NULL
           AND search_text LIKE $3
           AND ($4 = '' OR EXISTS (SELECT 1 FROM book_people p
                 WHERE p.book_id = b.id AND p.role = 'author' AND p.person = $4))
           AND ($5 = '' OR series = $5)
           AND ($6 IS NULL OR rating = $6)
           AND ($7 = '' OR EXISTS (SELECT 1 FROM book_categories bc
                 WHERE bc.book_id = b.id AND bc.category_id = $7))
           AND ($8 = '' OR language = $8)
         ORDER BY CASE WHEN series IS NULL THEN title ELSE series END, series_no, title",
    )
    .bind(tenant_id)
    .bind(library_id)
    .bind(search)
    .bind(query.author.trim())
    .bind(query.series.trim())
    .bind(query.rating)
    .bind(query.category.trim())
    .bind(query.language.trim())
    .fetch_all(db)
    .await?;
    let mut links = links(db, tenant_id, 0, i64::MAX).await?;
    Ok(rows
        .into_iter()
        .filter(|r| policy.allows(&r.id))
        .map(|r| r.into_book(&mut links))
        .collect())
}

/// Authors, series and languages of a library, for the filters.
///
/// # Errors
/// Database failures.
pub async fn facets(
    db: &SqlitePool,
    tenant_id: &str,
    library_id: &str,
) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<super::languages::Language>)> {
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
    let languages = sqlx::query_as(
        "SELECT l.code, l.name FROM languages l
         WHERE l.tenant_id = $1 AND EXISTS (SELECT 1 FROM books b WHERE b.tenant_id = l.tenant_id
             AND b.library_id = $2 AND b.deleted_at IS NULL AND b.language = l.code)
         ORDER BY l.position, l.code",
    )
    .bind(tenant_id)
    .bind(library_id)
    .fetch_all(db)
    .await?;
    Ok((authors, series, languages))
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
        "SELECT id, library_id, title, series, series_no, rating, language, description, year, duration_ms, cover_hash, revision,
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
    policy: &PolicyFilter,
    id: &str,
) -> Result<BookDetail> {
    if !policy.allows(id) {
        return Err(BerthError::NotFound);
    }
    let book = row(db, tenant_id, id).await?;
    let mut links = links(db, tenant_id, book.revision - 1, book.revision)
        .await
        .context("links")?;
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
        book: book.into_book(&mut links),
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
    /// Sections; ids that do not exist are dropped on save.
    pub category_ids: Vec<String>,
    /// Language code; `None` when not known.
    pub language: Option<String>,
    /// What the book is about; `None` or empty for none.
    pub description: Option<String>,
    /// Year of first publication.
    pub year: Option<i64>,
}

impl BookEdits {
    /// Validated and trimmed.
    ///
    /// # Errors
    /// `Validation` with `book_title_required`, `book_title_too_long`, `book_rating_invalid`,
    /// `book_description_too_long` or `book_year_invalid`.
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
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty());
        if description.is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_LEN) {
            return Err(BerthError::Validation("book_description_too_long".into()));
        }
        if self.year.is_some_and(|y| !YEARS.contains(&y)) {
            return Err(BerthError::Validation("book_year_invalid".into()));
        }
        let language = self
            .language
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty());
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
            category_ids: names(&self.category_ids),
            language: language.map(str::to_owned),
            description: description.map(str::to_owned),
            year: self.year,
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
            revision = $8, updated_at = $9, language = $10, description = $11, year = $12
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
    .bind(&edits.language)
    .bind(&edits.description)
    .bind(edits.year)
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
    sqlx::query("DELETE FROM book_categories WHERE tenant_id = $1 AND book_id = $2")
        .bind(tenant_id)
        .bind(book_id)
        .execute(&mut *conn)
        .await?;
    for category in &edits.category_ids {
        sqlx::query(
            "INSERT INTO book_categories (tenant_id, book_id, category_id)
             SELECT $1, $2, id FROM categories WHERE tenant_id = $1 AND id = $3",
        )
        .bind(tenant_id)
        .bind(book_id)
        .bind(category)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Saves the admin's edits.
///
/// # Errors
/// `NotFound`, the `Validation` codes of [`BookEdits::validated`].
pub async fn update(db: &SqlitePool, tenant_id: &str, id: &str, edits: &BookEdits) -> Result<()> {
    let edits = edits.validated()?;
    super::languages::check(db, tenant_id, edits.language.as_deref()).await?;
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

/// Fills the description and year a book does not have yet; values already set stay.
/// Returns whether anything changed.
///
/// # Errors
/// Database failures.
pub async fn fill_about(
    db: &SqlitePool,
    tenant_id: &str,
    id: &str,
    description: Option<&str>,
    year: Option<i64>,
) -> anyhow::Result<bool> {
    let mut tx = db.begin().await.context("begin")?;
    let current: Option<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT description, year FROM books WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((has_description, has_year)) = current else {
        return Ok(false);
    };
    let description = description.filter(|_| has_description.is_none());
    let year = year.filter(|_| has_year.is_none());
    if description.is_none() && year.is_none() {
        return Ok(false);
    }
    let revision = next_revision(&mut tx, tenant_id)
        .await
        .context("revision")?;
    sqlx::query(
        "UPDATE books SET description = COALESCE(description, $3), year = COALESCE(year, $4),
            revision = $5, updated_at = $6
         WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(description)
    .bind(year)
    .bind(revision)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("fill book")?;
    tx.commit().await.context("commit book")?;
    Ok(true)
}

/// The next book after `after` (by id) that lacks a description or a year.
///
/// # Errors
/// Database failures.
pub async fn next_without_about(
    db: &SqlitePool,
    tenant_id: &str,
    after: &str,
) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM books WHERE tenant_id = $1 AND deleted_at IS NULL
           AND (description IS NULL OR year IS NULL) AND id > $2
         ORDER BY id LIMIT 1",
    )
    .bind(tenant_id)
    .bind(after)
    .fetch_optional(db)
    .await?)
}

/// How many books lack a description or a year.
///
/// # Errors
/// Database failures.
pub async fn count_without_about(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM books WHERE tenant_id = $1 AND deleted_at IS NULL
           AND (description IS NULL OR year IS NULL)",
    )
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
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
    policy: &PolicyFilter,
    id: &str,
) -> Result<String> {
    if !policy.allows(id) {
        return Err(BerthError::NotFound);
    }
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
    /// The sections the reader sees, in their order; the list replaces the one the device
    /// has.
    pub categories: Vec<super::categories::Category>,
    /// Languages the admin keeps, in their order; the list replaces the one the device has.
    pub languages: Vec<super::languages::Language>,
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
        "SELECT id, library_id, title, series, series_no, rating, language, description, year, duration_ms, cover_hash, revision,
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
    let mut links = links(db, tenant_id, from, to).await?;
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
        // A hidden book goes as removed: a full slice drops it anyway, a delta takes it away.
        if row.deleted_at.is_some() || !policy.allows(&row.id) {
            removed.push(row.id);
            continue;
        }
        let id = row.id.clone();
        books.push(SyncBook {
            book: row.into_book(&mut links),
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
        categories: super::categories::list(db, tenant_id)
            .await?
            .into_iter()
            .filter(|c| !policy.hidden_categories.contains(&c.id))
            .collect(),
        languages: super::languages::list(db, tenant_id).await?,
        // The API layer fills it from the settings.
        server_urls: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noon UTC of a date, epoch milliseconds.
    fn at(date: &str) -> i64 {
        birth_days(date).unwrap() * DAY_MS + DAY_MS / 2
    }

    #[test]
    fn dates_and_ceilings() {
        assert_eq!(birth_days("1970-01-01"), Some(0));
        assert_eq!(birth_days("2000-03-01"), Some(11_017));
        for bad in [
            "",
            "2020-02-30",
            "2021-02-29",
            "2020-13-01",
            "20-01-01",
            "2020-1-01",
            "2020-01-01-1",
        ] {
            assert_eq!(birth_days(bad), None, "{bad}");
        }
        for days in [-1, 0, 59, 11_016, 20_000, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        let born = "2012-05-20";
        assert_eq!(rating_ceiling(born, at("2014-05-20")), Some(0));
        assert_eq!(rating_ceiling(born, at("2018-05-19")), Some(0));
        assert_eq!(rating_ceiling(born, at("2018-05-20")), Some(6));
        assert_eq!(rating_ceiling(born, at("2024-05-19")), Some(6));
        assert_eq!(rating_ceiling(born, at("2024-05-20")), Some(12));
        assert_eq!(rating_ceiling(born, at("2028-05-20")), Some(16));
        assert_eq!(rating_ceiling(born, at("2030-05-19")), Some(16));
        assert_eq!(rating_ceiling(born, at("2030-05-20")), None);
        // Born on 29 February: a year older on 1 March when the year has no 29th.
        assert_eq!(rating_ceiling("2012-02-29", at("2018-02-28")), Some(0));
        assert_eq!(rating_ceiling("2012-02-29", at("2018-03-01")), Some(6));
        assert_eq!(rating_ceiling("not a date", at("2018-03-01")), None);
    }

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
