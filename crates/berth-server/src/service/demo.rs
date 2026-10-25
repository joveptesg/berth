//! The demo library: public-domain `LibriVox` audiobooks the server downloads from archive.org
//! on the admin's request, with covers Berth ships, into a section of their own. The admin
//! picks the books (whole sets or some) and the length (first chapters or whole books) and may
//! change them later: [`apply`] brings the library to the pick, a `demo_book` job loads each
//! book, [`remove`] takes it all away.

use std::time::Duration;

use anyhow::Context;
use berth_core::{BerthError, Result};
use bytes::Bytes;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use url::Url;

use super::{
    books::{self, BookEdits},
    categories::{self, CategoryInput},
    ingest::{self, NewFile},
    languages, users,
};
use crate::{
    AppState,
    db::{self, now_ms},
    jobs,
};

/// Sets of the catalog, in the order the form shows them.
pub const LOCALES: [&str; 2] = ["ru", "en"];
const CATEGORY_KEY: &str = "demo.category";
const REVIEWER_KEY: &str = "demo.reviewer";
/// Recordings in this format: small enough for phones, good enough for speech.
const FORMAT: &str = "64Kbps MP3";

#[derive(RustEmbed)]
#[folder = "assets/demo/"]
struct DemoAssets;

/// A book of the catalog.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CatalogBook {
    /// Stable id, also the cover's file name.
    pub slot: String,
    /// `ru` or `en`.
    pub locale: String,
    /// archive.org item.
    pub archive: String,
    /// Files of the short version.
    pub chapters: usize,
    /// Title.
    pub title: String,
    /// Authors.
    pub authors: Vec<String>,
    /// Series.
    pub series: Option<String>,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Files of the whole recording.
    pub files: usize,
    /// Length of the whole recording.
    pub hours: f64,
    /// Megabytes of the short version.
    pub mb_short: u64,
    /// Megabytes of the whole recording.
    pub mb_full: u64,
}

#[derive(Deserialize)]
struct Catalog {
    books: Vec<CatalogBook>,
}

/// The books the server can load.
#[must_use]
pub fn catalog() -> Vec<CatalogBook> {
    DemoAssets::get("catalog.json")
        .and_then(|file| serde_json::from_slice::<Catalog>(&file.data).ok())
        .map(|c| c.books)
        .unwrap_or_default()
}

fn cover(slot: &str) -> Option<Vec<u8>> {
    DemoAssets::get(&format!("covers/{slot}.jpg")).map(|f| f.data.into_owned())
}

/// A listener for store review: signs in with a login, sees only the demo section.
#[derive(Debug, Clone, Default)]
pub struct Reviewer {
    /// Name in the users list.
    pub name: String,
    /// Login for apps.
    pub login: String,
    /// Password; empty keeps the saved one.
    pub password: String,
}

/// What the admin picks.
#[derive(Debug, Clone, Default)]
pub struct DemoPlan {
    /// Books of the catalog, by slot.
    pub slots: Vec<String>,
    /// Whole books rather than their first chapters.
    pub full: bool,
    /// The demo section is closed.
    pub closed: bool,
    /// Listeners the section is open to.
    pub users: Vec<String>,
    /// The review listener, or none.
    pub reviewer: Option<Reviewer>,
}

/// A book of the demo as the settings show it.
#[derive(Debug, Clone, Serialize)]
pub struct DemoBook {
    /// Catalog entry.
    #[serde(flatten)]
    pub book: CatalogBook,
    /// `queued`, `downloading`, `ready` or `failed`.
    pub status: String,
    /// Whole recording.
    pub full: bool,
    /// Last error.
    pub error: Option<String>,
}

/// The demo library as it stands.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DemoState {
    /// Books picked, in catalog order.
    pub books: Vec<DemoBook>,
    /// Sets picked.
    pub locales: Vec<String>,
    /// Whole books.
    pub full: bool,
    /// The demo section, if it exists.
    pub category: Option<categories::CategoryEntry>,
    /// The review listener: id, name, login (none when their login is off).
    pub reviewer: Option<(String, String, Option<String>)>,
}

#[derive(sqlx::FromRow)]
struct Row {
    slot: String,
    full: bool,
    status: String,
    book_id: Option<String>,
    error: Option<String>,
}

async fn rows(db: &SqlitePool, tenant_id: &str) -> sqlx::Result<Vec<Row>> {
    sqlx::query_as("SELECT slot, full, status, book_id, error FROM demo_books WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(db)
        .await
}

async fn setting_id(db: &SqlitePool, tenant_id: &str, key: &str) -> anyhow::Result<Option<String>> {
    Ok(db::tenant_setting(db, tenant_id, key)
        .await?
        .and_then(|v| v.as_str().map(str::to_owned)))
}

async fn put_setting(
    db: &SqlitePool,
    tenant_id: &str,
    key: &str,
    value: &Value,
) -> anyhow::Result<()> {
    let mut conn = db.acquire().await?;
    db::put_tenant_setting(&mut conn, tenant_id, key, value).await?;
    Ok(())
}

/// The demo section, if it is still there.
async fn category(
    db: &SqlitePool,
    tenant_id: &str,
) -> anyhow::Result<Option<categories::CategoryEntry>> {
    let Some(id) = setting_id(db, tenant_id, CATEGORY_KEY).await? else {
        return Ok(None);
    };
    Ok(categories::entries(db, tenant_id)
        .await?
        .into_iter()
        .find(|c| c.id == id))
}

/// The demo library as it stands.
///
/// # Errors
/// Database failures.
pub async fn state(db: &SqlitePool, tenant_id: &str) -> anyhow::Result<DemoState> {
    let rows = rows(db, tenant_id).await.context("load demo books")?;
    let mut books = Vec::new();
    for book in catalog() {
        if let Some(row) = rows.iter().find(|r| r.slot == book.slot) {
            books.push(DemoBook {
                book,
                status: row.status.clone(),
                full: row.full,
                error: row.error.clone(),
            });
        }
    }
    let mut locales: Vec<String> = Vec::new();
    for book in &books {
        if !locales.contains(&book.book.locale) {
            locales.push(book.book.locale.clone());
        }
    }
    let reviewer = match setting_id(db, tenant_id, REVIEWER_KEY).await? {
        Some(id) => users::list(db, tenant_id)
            .await?
            .into_iter()
            .find(|u| u.id == id)
            .map(|u| (u.id, u.name, u.login)),
        None => None,
    };
    Ok(DemoState {
        full: books.iter().any(|b| b.full),
        books,
        locales,
        category: category(db, tenant_id).await?,
        reviewer,
    })
}

/// Brings the demo library to `plan`: the section (named `section_name` when it is made),
/// its access, the review listener, and the books (new ones and ones of the other length
/// are queued, the ones of sets no longer picked are removed).
///
/// # Errors
/// `Validation` with `demo_books_required`, `demo_reviewer_name_required` or the codes of
/// [`users::set_login`] and [`categories::create`].
pub async fn apply(
    state: &AppState,
    tenant_id: &str,
    plan: &DemoPlan,
    section_name: &str,
) -> Result<()> {
    let db = &state.db;
    let slots: Vec<String> = catalog()
        .into_iter()
        .filter(|b| plan.slots.contains(&b.slot))
        .map(|b| b.slot)
        .collect();
    if slots.is_empty() {
        return Err(BerthError::Validation("demo_books_required".into()));
    }
    // The review listener first: the section is given to them.
    let reviewer = apply_reviewer(db, tenant_id, plan.reviewer.as_ref()).await?;
    let mut open_to = plan.users.clone();
    if let Some(id) = reviewer.as_ref().filter(|id| !open_to.contains(id)) {
        open_to.push(id.clone());
    }
    let category_id = apply_category(db, tenant_id, plan.closed, open_to, section_name).await?;
    if let Some(id) = &reviewer {
        users::set_access(
            db,
            tenant_id,
            id,
            Some(&[]),
            std::slice::from_ref(&category_id),
            &[],
        )
        .await?;
    }
    let queued = queue_books(db, tenant_id, &slots, plan.full).await?;
    state.wake_jobs();
    tracing::info!(
        books = slots.len(),
        full = plan.full,
        closed = plan.closed,
        queued,
        reviewer = reviewer.is_some(),
        "demo library applied"
    );
    Ok(())
}

/// Creates or updates the review listener, or turns their login off; their id when on.
async fn apply_reviewer(
    db: &SqlitePool,
    tenant_id: &str,
    reviewer: Option<&Reviewer>,
) -> Result<Option<String>> {
    let known = users::list(db, tenant_id).await?;
    let saved = setting_id(db, tenant_id, REVIEWER_KEY)
        .await?
        .filter(|id| known.iter().any(|u| &u.id == id));
    let Some(r) = reviewer else {
        if let Some(id) = &saved {
            users::clear_login(db, tenant_id, id).await?;
        }
        return Ok(None);
    };
    let name = r.name.trim();
    if name.is_empty() {
        return Err(BerthError::Validation("demo_reviewer_name_required".into()));
    }
    let id = match &saved {
        Some(id) => id.clone(),
        None => users::create_listener(db, tenant_id, name).await?,
    };
    if let Err(err) = users::set_login(db, tenant_id, &id, &r.login, &r.password).await {
        if saved.is_none() {
            // A new listener whose login failed would stay as a stray user.
            let _ =
                sqlx::query("UPDATE users SET deleted_at = $3 WHERE tenant_id = $1 AND id = $2")
                    .bind(tenant_id)
                    .bind(&id)
                    .bind(now_ms())
                    .execute(db)
                    .await;
        }
        return Err(err);
    }
    put_setting(db, tenant_id, REVIEWER_KEY, &json!(id)).await?;
    Ok(Some(id))
}

/// Creates the demo section or updates its access; its id.
async fn apply_category(
    db: &SqlitePool,
    tenant_id: &str,
    closed: bool,
    users: Vec<String>,
    section_name: &str,
) -> Result<String> {
    if let Some(entry) = category(db, tenant_id).await? {
        let input = CategoryInput {
            name: entry.name,
            aliases: entry.aliases,
            closed,
            users,
        };
        categories::update(db, tenant_id, &entry.id, &input).await?;
        return Ok(entry.id);
    }
    // A section of that name made by hand becomes the demo one rather than a clash.
    let same_name = categories::entries(db, tenant_id)
        .await?
        .into_iter()
        .find(|c| c.name.to_lowercase() == section_name.to_lowercase());
    let id = if let Some(entry) = same_name {
        let input = CategoryInput {
            name: entry.name,
            aliases: entry.aliases,
            closed,
            users,
        };
        categories::update(db, tenant_id, &entry.id, &input).await?;
        entry.id
    } else {
        let input = CategoryInput {
            name: section_name.to_owned(),
            aliases: String::new(),
            closed,
            users,
        };
        categories::create(db, tenant_id, &input).await?
    };
    put_setting(db, tenant_id, CATEGORY_KEY, &json!(id)).await?;
    Ok(id)
}

/// Queues the books of `slots` that are missing or of the other length, and removes the
/// ones no longer picked; how many were queued.
async fn queue_books(
    db: &SqlitePool,
    tenant_id: &str,
    slots: &[String],
    full: bool,
) -> anyhow::Result<usize> {
    let rows = rows(db, tenant_id).await.context("load demo books")?;
    let mut queued = 0;
    let mut conn = db.acquire().await.context("acquire")?;
    for book in catalog() {
        let row = rows.iter().find(|r| r.slot == book.slot);
        if !slots.contains(&book.slot) {
            if let Some(row) = row {
                if let Some(book_id) = &row.book_id {
                    remove_book(db, tenant_id, book_id).await;
                }
                sqlx::query("DELETE FROM demo_books WHERE tenant_id = $1 AND slot = $2")
                    .bind(tenant_id)
                    .bind(&row.slot)
                    .execute(&mut *conn)
                    .await
                    .context("drop demo book")?;
            }
            continue;
        }
        if row.is_some_and(|r| r.full == full && r.status != "failed") {
            continue;
        }
        sqlx::query(
            "INSERT INTO demo_books (tenant_id, slot, full, status, updated_at)
             VALUES ($1, $2, $3, 'queued', $4)
             ON CONFLICT (tenant_id, slot) DO UPDATE SET
                full = excluded.full, status = 'queued', error = NULL, updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(&book.slot)
        .bind(full)
        .bind(now_ms())
        .execute(&mut *conn)
        .await
        .context("queue demo book")?;
        jobs::enqueue(
            &mut conn,
            tenant_id,
            jobs::DEMO_BOOK,
            &json!({ "slot": book.slot, "full": full }),
        )
        .await
        .context("enqueue")?;
        queued += 1;
    }
    Ok(queued)
}

async fn remove_book(db: &SqlitePool, tenant_id: &str, book_id: &str) {
    match books::delete(db, tenant_id, book_id).await {
        Ok(()) | Err(BerthError::NotFound) => {}
        Err(err) => tracing::warn!(error = %err, book = %book_id, "could not remove a demo book"),
    }
}

/// Takes the demo library away: its books, its section, and the review listener's login.
///
/// # Errors
/// Database failures.
pub async fn remove(state: &AppState, tenant_id: &str) -> Result<()> {
    let db = &state.db;
    for row in rows(db, tenant_id).await.context("load demo books")? {
        if let Some(book_id) = &row.book_id {
            remove_book(db, tenant_id, book_id).await;
        }
    }
    sqlx::query("DELETE FROM demo_books WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(db)
        .await
        .context("drop demo books")?;
    // The section goes with its books; one that holds other books too stays.
    if let Some(entry) = category(db, tenant_id).await?
        && entry.books == 0
    {
        categories::delete(db, tenant_id, &entry.id).await?;
    }
    put_setting(db, tenant_id, CATEGORY_KEY, &Value::Null).await?;
    if let Some(id) = setting_id(db, tenant_id, REVIEWER_KEY).await? {
        match users::clear_login(db, tenant_id, &id).await {
            Ok(()) | Err(BerthError::NotFound) => {}
            Err(err) => return Err(err),
        }
    }
    tracing::info!("demo library removed");
    Ok(())
}

async fn set_status(
    db: &SqlitePool,
    tenant_id: &str,
    slot: &str,
    status: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE demo_books SET status = $3, error = $4, updated_at = $5 WHERE tenant_id = $1 AND slot = $2",
    )
    .bind(tenant_id)
    .bind(slot)
    .bind(status)
    .bind(error)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// A file of an archive.org item.
#[derive(Debug, Clone, Deserialize)]
struct ItemFile {
    name: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(default)]
    metadata: Value,
    #[serde(default)]
    files: Vec<ItemFile>,
}

/// Whether the item's license is public domain (`licenseurl` of a Public Domain Mark or CC0).
fn public_domain(item: &Item) -> bool {
    item.metadata["licenseurl"]
        .as_str()
        .is_some_and(|url| url.contains("publicdomain"))
}

/// The recordings to load: the ones in [`FORMAT`] by name, all or the first `chapters`.
fn pick_files(item: &Item, full: bool, chapters: usize) -> Vec<ItemFile> {
    let mut files: Vec<ItemFile> = item
        .files
        .iter()
        .filter(|f| f.format == FORMAT)
        .cloned()
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    if !full {
        files.truncate(chapters);
    }
    files
}

fn item_url(source: &Url, parts: &[&str]) -> anyhow::Result<Url> {
    let mut url = source.clone();
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("demo source cannot be a base"))?
        .pop_if_empty()
        .extend(parts);
    Ok(url)
}

fn client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!(
            "Berth/",
            env!("CARGO_PKG_VERSION"),
            " (demo library)"
        ))
        .build()?)
}

/// The `demo_book` job: loads one book of the catalog. A stale job (the book was removed or
/// its length changed since) does nothing.
///
/// # Errors
/// Network and storage failures: the job is retried.
pub(crate) async fn run_job(
    state: &AppState,
    tenant_id: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let slot = payload["slot"].as_str().context("slot")?;
    let full = payload["full"].as_bool().unwrap_or(false);
    let row: Option<(bool, String, Option<String>)> = sqlx::query_as(
        "SELECT full, status, book_id FROM demo_books WHERE tenant_id = $1 AND slot = $2",
    )
    .bind(tenant_id)
    .bind(slot)
    .fetch_optional(&state.db)
    .await?;
    let Some((row_full, status, old_book)) = row else {
        return Ok(());
    };
    if row_full != full || status == "ready" {
        return Ok(());
    }
    let book = catalog()
        .into_iter()
        .find(|b| b.slot == slot)
        .context("not in the catalog")?;
    set_status(&state.db, tenant_id, slot, "downloading", None).await?;
    match load(state, tenant_id, &book, full).await {
        Ok(Some(book_id)) => {
            if let Some(old) = old_book.filter(|old| *old != book_id) {
                remove_book(&state.db, tenant_id, &old).await;
            }
            sqlx::query(
                "UPDATE demo_books SET status = 'ready', book_id = $3, error = NULL, updated_at = $4
                 WHERE tenant_id = $1 AND slot = $2",
            )
            .bind(tenant_id)
            .bind(slot)
            .bind(&book_id)
            .bind(now_ms())
            .execute(&state.db)
            .await?;
            tracing::info!(slot, book = %book_id, full, "demo book ready");
            Ok(())
        }
        // Refused for good (the license): no retry.
        Ok(None) => Ok(()),
        Err(err) => {
            // Queued again: the job retries, the settings show the error meanwhile.
            set_status(
                &state.db,
                tenant_id,
                slot,
                "queued",
                Some(&format!("{err:#}")),
            )
            .await?;
            Err(err)
        }
    }
}

/// The job ran out of attempts.
pub(crate) async fn give_up(db: &SqlitePool, tenant_id: &str, payload: &Value, error: &str) {
    if let Some(slot) = payload["slot"].as_str()
        && let Err(err) = set_status(db, tenant_id, slot, "failed", Some(error)).await
    {
        tracing::warn!(error = %err, "could not mark the demo book failed");
    }
    tracing::warn!(error, "demo book failed");
}

/// Downloads and saves a book; `None` when its license is not public domain.
async fn load(
    state: &AppState,
    tenant_id: &str,
    book: &CatalogBook,
    full: bool,
) -> anyhow::Result<Option<String>> {
    let source = state.config.load().demo_source.clone();
    let http = client()?;
    let item: Item = http
        .get(item_url(&source, &["metadata", &book.archive])?)
        .send()
        .await?
        .error_for_status()?
        .json_bytes()
        .await?;
    if !public_domain(&item) {
        let license = item.metadata["licenseurl"].as_str().unwrap_or("none");
        set_status(
            &state.db,
            tenant_id,
            &book.slot,
            "failed",
            Some(&format!("not public domain: {license}")),
        )
        .await?;
        tracing::warn!(slot = %book.slot, license, "demo book refused");
        return Ok(None);
    }
    let picked = pick_files(&item, full, book.chapters);
    anyhow::ensure!(!picked.is_empty(), "no {FORMAT} files in {}", book.archive);
    let cover = cover(&book.slot);
    let mut files: Vec<NewFile> = Vec::with_capacity(picked.len() + 1);
    for (i, file) in picked.iter().enumerate() {
        let size = file
            .size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .with_context(|| format!("no size of {}", file.name))?;
        files.push(NewFile {
            name: format!("{:03}.mp3", i + 1),
            size,
            sha256: None,
        });
    }
    if let Some(cover) = &cover {
        files.push(NewFile {
            name: "cover.jpg".into(),
            size: cover.len() as u64,
            sha256: None,
        });
    }
    let created = ingest::create_from_server(state, tenant_id, &book.title, &files)
        .await
        .map_err(anyhow::Error::from)?;
    let upload = created.id;
    let target = Target {
        state,
        tenant_id,
        upload: &upload,
        http: &http,
        source: &source,
    };
    let loaded = target.fill(book, &picked, cover).await;
    if loaded.is_err() {
        let _ = ingest::discard(&state.db, tenant_id, &upload).await;
    }
    loaded.map(Some)
}

/// An upload the server fills with a demo book.
struct Target<'a> {
    state: &'a AppState,
    tenant_id: &'a str,
    upload: &'a str,
    http: &'a reqwest::Client,
    source: &'a Url,
}

impl Target<'_> {
    /// Streams the recordings and the cover into the upload, then saves the book; its id.
    async fn fill(
        &self,
        book: &CatalogBook,
        picked: &[ItemFile],
        cover: Option<Vec<u8>>,
    ) -> anyhow::Result<String> {
        let (state, tenant_id, upload) = (self.state, self.tenant_id, self.upload);
        for (i, file) in picked.iter().enumerate() {
            let response = self
                .http
                .get(item_url(
                    self.source,
                    &["download", &book.archive, &file.name],
                )?)
                .send()
                .await?
                .error_for_status()?;
            let body = futures_util::stream::try_unfold(response, |mut r| async move {
                Ok::<_, reqwest::Error>(r.chunk().await?.map(|chunk| (chunk, r)))
            });
            ingest::put_file(state, tenant_id, upload, i64::try_from(i)?, Box::pin(body))
                .await
                .map_err(anyhow::Error::from)?;
        }
        if let Some(cover) = cover {
            let body = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(cover))]);
            ingest::put_file(state, tenant_id, upload, i64::try_from(picked.len())?, body)
                .await
                .map_err(anyhow::Error::from)?;
        }
        ingest::complete(state, tenant_id, upload)
            .await
            .map_err(anyhow::Error::from)?;
        ingest::analyze(state, tenant_id, upload).await?;
        let draft = ingest::get(&state.db, tenant_id, upload)
            .await
            .map_err(anyhow::Error::from)?
            .draft
            .context("no proposal")?;
        let known = languages::list(&state.db, tenant_id).await?;
        let category = setting_id(&state.db, tenant_id, CATEGORY_KEY).await?;
        let edits = BookEdits {
            title: book.title.clone(),
            authors: book.authors.clone(),
            narrators: draft.narrators,
            series: book.series.clone().unwrap_or_default(),
            series_no: book.series_no,
            rating: Some(0),
            category_ids: category.into_iter().collect(),
            language: languages::resolve(&known, &book.locale),
        };
        ingest::save(state, tenant_id, upload, &edits)
            .await
            .map_err(anyhow::Error::from)
    }
}

/// `Response::json` without reqwest's `json` feature.
trait JsonBytes {
    async fn json_bytes<T: serde::de::DeserializeOwned>(self) -> anyhow::Result<T>;
}

impl JsonBytes for reqwest::Response {
    async fn json_bytes<T: serde::de::DeserializeOwned>(self) -> anyhow::Result<T> {
        Ok(serde_json::from_slice(&self.bytes().await?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_has_covers_and_both_sets() {
        let books = catalog();
        assert_eq!(books.len(), 18);
        for book in &books {
            assert!(cover(&book.slot).is_some(), "{}", book.slot);
            assert!(LOCALES.contains(&book.locale.as_str()));
            assert!(book.chapters > 0 && book.chapters <= book.files);
        }
    }

    #[test]
    fn files_and_license() {
        let item: Item = serde_json::from_value(json!({
            "metadata": { "licenseurl": "http://creativecommons.org/publicdomain/mark/1.0/" },
            "files": [
                { "name": "b_02.mp3", "format": "64Kbps MP3", "size": "20" },
                { "name": "b_01.mp3", "format": "64Kbps MP3", "size": "10" },
                { "name": "b_01.ogg", "format": "Ogg Vorbis" },
                { "name": "b_03.mp3", "format": "64Kbps MP3", "size": "30" },
            ],
        }))
        .unwrap();
        assert!(public_domain(&item));
        let short: Vec<String> = pick_files(&item, false, 2)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(short, ["b_01.mp3", "b_02.mp3"]);
        assert_eq!(pick_files(&item, true, 2).len(), 3);
        let licensed: Item = serde_json::from_value(json!({
            "metadata": { "licenseurl": "https://creativecommons.org/licenses/by/4.0/" },
        }))
        .unwrap();
        assert!(!public_domain(&licensed));
        let url = item_url(
            &Url::parse("https://archive.org").unwrap(),
            &["download", "x", "a b.mp3"],
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://archive.org/download/x/a%20b.mp3");
    }
}
