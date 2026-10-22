//! Book uploads: one upload (a dropped folder or file) becomes one book.
//!
//! 1. [`create`] lists the files; each gets a target: through the server (local storage),
//!    a presigned PUT signed with its sha256 and size (S3, design doc §22.A5), or nothing
//!    when the tenant already stores the bytes (deduplication within the tenant).
//! 2. [`put_file`] streams a file through the server; [`complete`] checks presigned objects
//!    and queues the `ingest_pipeline` job.
//! 3. [`analyze`] (the job) reads tags, durations, chapters and covers and proposes a book.
//! 4. [`save`] creates the book from the proposal and the admin's edits.

use std::{cmp::Ordering, path::PathBuf, time::Duration};

use anyhow::Context;
use berth_core::{BerthError, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::AsyncWriteExt;

use super::{
    blobs,
    books::{self, BookEdits},
    storage as profiles,
};
use crate::{
    AppState,
    db::now_ms,
    jobs, media,
    storage::{BlobStore, ContentHash, ObjectBlobStore, put_headers},
};

const MAX_FILES: usize = 2000;
const PRESIGN_TTL: Duration = Duration::from_hours(6);
const AUDIO: [&str; 8] = ["mp3", "m4b", "m4a", "aac", "ogg", "opus", "flac", "wav"];
const IMAGES: [&str; 4] = ["jpg", "jpeg", "png", "webp"];
const MAX_COVER_BYTES: u64 = 10 << 20;

/// A file the client is about to send.
#[derive(Debug, Clone)]
pub struct NewFile {
    /// Path inside the dropped folder.
    pub name: String,
    /// Bytes.
    pub size: u64,
    /// Hex sha256: required for presigned uploads, optional through the server.
    pub sha256: Option<String>,
}

/// How to send one file.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum FileTarget {
    /// `PUT` the bytes to the upload's file endpoint on this server.
    Server,
    /// `PUT` the bytes to `url` with exactly these headers.
    Presigned {
        /// Presigned URL.
        url: String,
        /// Headers that are part of the signature.
        headers: Vec<(String, String)>,
    },
    /// Already stored: nothing to send.
    Stored,
}

/// A created upload.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Created {
    /// Upload id.
    pub id: String,
    /// Per file, in the given order.
    pub targets: Vec<FileTarget>,
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default()
}

fn kind_of(name: &str) -> Option<&'static str> {
    let ext = extension(name);
    if AUDIO.contains(&ext.as_str()) {
        Some("audio")
    } else if IMAGES.contains(&ext.as_str()) {
        Some("image")
    } else {
        None
    }
}

/// Starts an upload into the default library and storage. Files of other types are skipped
/// by the client; here they are an error.
///
/// # Errors
/// `Validation` with `upload_empty`, `upload_too_many`, `upload_file_unsupported`,
/// `upload_no_audio` or `upload_hash_required` (S3 needs each file's sha256).
pub async fn create(
    state: &AppState,
    tenant_id: &str,
    name: &str,
    files: &[NewFile],
) -> Result<Created> {
    let invalid = |code: &str| BerthError::Validation(code.into());
    if files.is_empty() {
        return Err(invalid("upload_empty"));
    }
    if files.len() > MAX_FILES {
        return Err(invalid("upload_too_many"));
    }
    let kinds: Vec<&str> = files
        .iter()
        .map(|f| kind_of(&f.name).ok_or_else(|| invalid("upload_file_unsupported")))
        .collect::<Result<_>>()?;
    if !kinds.contains(&"audio") {
        return Err(invalid("upload_no_audio"));
    }
    let hashes: Vec<Option<String>> = files
        .iter()
        .map(|f| match &f.sha256 {
            None => Ok(None),
            Some(h) => ContentHash::parse(&h.to_lowercase())
                .map(|c| Some(c.as_str().to_owned()))
                .ok_or_else(|| invalid("upload_hash_invalid")),
        })
        .collect::<Result<_>>()?;

    let storage_id = profiles::default_id(&state.db, tenant_id)
        .await?
        .context("no default storage")?;
    let store = profiles::open(state, tenant_id, &storage_id).await?;
    let presigned = store.supports_presigned();
    let library_id = books::default_library_id(&state.db, tenant_id).await?;

    let id = uuid::Uuid::now_v7().to_string();
    let mut targets = Vec::with_capacity(files.len());
    let mut tx = state.db.begin().await.context("begin")?;
    let now = now_ms();
    let name = name.trim();
    sqlx::query(
        "INSERT INTO uploads (id, tenant_id, library_id, storage_id, name, status, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, 'uploading', $6, $6)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(&library_id)
    .bind(&storage_id)
    .bind(if name.is_empty() { &files[0].name } else { name })
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("insert upload")?;
    for (idx, ((file, kind), hash)) in files.iter().zip(&kinds).zip(&hashes).enumerate() {
        let stored = match hash {
            Some(hash) => blobs::is_active(&mut *tx, tenant_id, hash, &storage_id)
                .await
                .context("dedup")?,
            None => false,
        };
        let target = match (stored, hash) {
            (true, _) => FileTarget::Stored,
            (false, Some(hash)) if presigned => {
                let content = ContentHash::parse(hash).context("validated hash")?;
                let url = store
                    .presign_put(tenant_id, &content, file.size, PRESIGN_TTL)
                    .await
                    .context("presign")?
                    .context("presigned store without a URL")?;
                let digest: [u8; 32] = hex::decode(hash)
                    .ok()
                    .and_then(|d| d.try_into().ok())
                    .context("digest")?;
                FileTarget::Presigned {
                    url: url.into(),
                    headers: put_headers(&digest, file.size)
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect(),
                }
            }
            (false, None) if presigned => return Err(invalid("upload_hash_required")),
            (false, _) => FileTarget::Server,
        };
        sqlx::query(
            "INSERT INTO upload_files (tenant_id, upload_id, idx, name, size, kind, blob_hash, stored)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(tenant_id)
        .bind(&id)
        .bind(i64::try_from(idx).unwrap_or(i64::MAX))
        .bind(&file.name)
        .bind(i64::try_from(file.size).unwrap_or(i64::MAX))
        .bind(kind)
        .bind(hash)
        .bind(stored)
        .execute(&mut *tx)
        .await
        .context("insert upload file")?;
        targets.push(target);
    }
    tx.commit().await.context("commit upload")?;
    tracing::info!(upload = %id, files = files.len(), "upload started");
    Ok(Created { id, targets })
}

#[derive(sqlx::FromRow)]
struct FileRow {
    idx: i64,
    name: String,
    size: i64,
    kind: String,
    blob_hash: Option<String>,
    stored: bool,
}

async fn upload_row(
    db: &SqlitePool,
    tenant_id: &str,
    id: &str,
) -> Result<(String, String, String)> {
    sqlx::query_as(
        "SELECT status, storage_id, library_id FROM uploads WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .context("load upload")?
    .ok_or(BerthError::NotFound)
}

async fn files(db: &SqlitePool, tenant_id: &str, id: &str) -> sqlx::Result<Vec<FileRow>> {
    sqlx::query_as(
        "SELECT idx, name, size, kind, blob_hash, stored FROM upload_files
         WHERE tenant_id = $1 AND upload_id = $2 ORDER BY idx",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await
}

fn temp_path(state: &AppState) -> anyhow::Result<PathBuf> {
    let dir = state.config.load().data_dir.join("tmp");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir.join(format!("upload-{}", uuid::Uuid::now_v7())))
}

/// Receives one file through the server: hashes it on the way, then stores it.
///
/// # Errors
/// `NotFound`, `Conflict("upload_closed")` once the upload is complete,
/// `Validation` with `upload_size_mismatch` or `upload_hash_mismatch`.
pub async fn put_file<S, E>(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    idx: i64,
    mut body: S,
) -> Result<()>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let (status, storage_id, _) = upload_row(&state.db, tenant_id, id).await?;
    if status != "uploading" {
        return Err(BerthError::Conflict("upload_closed".into()));
    }
    let file = files(&state.db, tenant_id, id)
        .await
        .context("files")?
        .into_iter()
        .find(|f| f.idx == idx)
        .ok_or(BerthError::NotFound)?;
    if file.stored {
        return Ok(());
    }

    let path = temp_path(state)?;
    let result = async {
        let mut out = tokio::fs::File::create(&path).await.context("temp file")?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("read upload body")?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            out.write_all(&chunk).await.context("write temp file")?;
        }
        out.flush().await.context("write temp file")?;
        drop(out);
        if i64::try_from(size).ok() != Some(file.size) {
            return Err(BerthError::Validation("upload_size_mismatch".into()));
        }
        let hash = hex::encode(hasher.finalize());
        if file.blob_hash.as_ref().is_some_and(|declared| *declared != hash) {
            return Err(BerthError::Validation("upload_hash_mismatch".into()));
        }
        if !blobs::is_active(&state.db, tenant_id, &hash, &storage_id).await.context("dedup")? {
            let store = profiles::open(state, tenant_id, &storage_id).await?;
            let content = ContentHash::parse(&hash).context("hash")?;
            let reader = tokio::fs::File::open(&path).await.context("reopen temp file")?;
            store.put(tenant_id, &content, Box::new(reader)).await.context("store file")?;
        }
        let mut tx = state.db.begin().await.context("begin")?;
        blobs::register(&mut tx, tenant_id, &hash, size, Some(&extension(&file.name)), &storage_id, "active")
            .await
            .context("register blob")?;
        sqlx::query(
            "UPDATE upload_files SET blob_hash = $3, stored = TRUE WHERE upload_id = $1 AND idx = $2 AND tenant_id = $4",
        )
        .bind(id)
        .bind(idx)
        .bind(&hash)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .context("mark stored")?;
        tx.commit().await.context("commit file")?;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    result
}

/// All files are sent: checks presigned uploads and queues the analysis.
///
/// # Errors
/// `NotFound`, `Conflict("upload_closed")`, `Validation("upload_file_missing")` when a file
/// is not stored (or a presigned object is missing or has another size).
pub async fn complete(state: &AppState, tenant_id: &str, id: &str) -> Result<()> {
    let (status, storage_id, _) = upload_row(&state.db, tenant_id, id).await?;
    if status != "uploading" {
        return Err(BerthError::Conflict("upload_closed".into()));
    }
    let list = files(&state.db, tenant_id, id).await.context("files")?;
    let pending: Vec<&FileRow> = list.iter().filter(|f| !f.stored).collect();
    if !pending.is_empty() {
        let store = profiles::open(state, tenant_id, &storage_id).await?;
        for file in pending {
            let missing = || BerthError::Validation("upload_file_missing".into());
            let hash = file.blob_hash.as_deref().ok_or_else(missing)?;
            let content = ContentHash::parse(hash).ok_or_else(missing)?;
            // design doc §22.A5: the object must exist with the declared size.
            let size = store
                .size(tenant_id, &content)
                .await
                .context("check object")?;
            if size.and_then(|s| i64::try_from(s).ok()) != Some(file.size) {
                return Err(missing());
            }
            let mut tx = state.db.begin().await.context("begin")?;
            blobs::register(
                &mut tx,
                tenant_id,
                hash,
                size.unwrap_or(0),
                Some(&extension(&file.name)),
                &storage_id,
                "active",
            )
            .await
            .context("register blob")?;
            sqlx::query("UPDATE upload_files SET stored = TRUE WHERE upload_id = $1 AND idx = $2")
                .bind(id)
                .bind(file.idx)
                .execute(&mut *tx)
                .await
                .context("mark stored")?;
            tx.commit().await.context("commit")?;
        }
    }
    let mut tx = state.db.begin().await.context("begin")?;
    let moved = sqlx::query(
        "UPDATE uploads SET status = 'analyzing', updated_at = $3 WHERE tenant_id = $1 AND id = $2 AND status = 'uploading'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(now_ms())
    .execute(&mut *tx)
    .await
    .context("close upload")?;
    if moved.rows_affected() == 0 {
        return Err(BerthError::Conflict("upload_closed".into()));
    }
    jobs::enqueue(
        &mut tx,
        tenant_id,
        jobs::INGEST,
        &json!({ "upload_id": id }),
    )
    .await
    .context("queue analysis")?;
    tx.commit().await.context("commit")?;
    state.wake_jobs();
    Ok(())
}

/// A file of the proposed book.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct DraftFile {
    /// Name in the upload.
    pub name: String,
    /// Content hash.
    pub blob_hash: String,
    /// Bytes.
    pub size: u64,
    /// Start on the book timeline.
    pub offset_ms: u64,
    /// Playing time.
    pub duration_ms: u64,
}

/// A chapter of the proposed book, on the book timeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct DraftChapter {
    /// Title.
    pub title: String,
    /// Start.
    pub start_ms: u64,
    /// End.
    pub end_ms: u64,
}

/// The book proposed from the files; the admin edits it before saving.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Draft {
    /// Title.
    pub title: String,
    /// Authors.
    pub authors: Vec<String>,
    /// Narrators.
    pub narrators: Vec<String>,
    /// Series.
    pub series: String,
    /// Number in the series.
    pub series_no: Option<f64>,
    /// Playing time.
    pub duration_ms: u64,
    /// Files in play order.
    pub files: Vec<DraftFile>,
    /// Chapters.
    pub chapters: Vec<DraftChapter>,
    /// Cover blob.
    pub cover_hash: Option<String>,
}

/// Natural order: "2.mp3" before "10.mp3".
fn natural(a: &str, b: &str) -> Ordering {
    let key = |s: &str| {
        let mut parts: Vec<(u8, u64, String)> = Vec::new();
        let mut chars = s.chars().peekable();
        while let Some(&c) = chars.peek() {
            let digit = c.is_ascii_digit();
            let mut part = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() != digit {
                    break;
                }
                part.push(c);
                chars.next();
            }
            if digit {
                parts.push((0, part.parse().unwrap_or(u64::MAX), String::new()));
            } else {
                parts.push((1, 0, part.to_lowercase()));
            }
        }
        parts
    };
    key(a).cmp(&key(b))
}

/// "Author - Title [Narrator]" as many folders are named; tracker tags in brackets and
/// years in parentheses are dropped.
fn parse_folder(name: &str) -> (Option<String>, String, Option<String>) {
    let mut rest = name.trim().replace('_', " ");
    while rest.starts_with('[') {
        match rest.find(']') {
            Some(end) => rest = rest[end + 1..].trim().to_owned(),
            None => break,
        }
    }
    let mut narrator = None;
    if rest.ends_with(']') || rest.ends_with(')') {
        let open = if rest.ends_with(']') { '[' } else { '(' };
        if let Some(start) = rest.rfind(open) {
            let inner = rest[start + 1..rest.len() - 1].trim().to_owned();
            if !inner.chars().all(|c| c.is_ascii_digit()) && !inner.is_empty() {
                narrator = Some(inner);
            }
            rest = rest[..start].trim().to_owned();
        }
    }
    // A trailing year "(2020)" after the narrator was cut.
    if let Some(start) = rest.rfind('(')
        && rest.ends_with(')')
        && rest[start + 1..rest.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        rest = rest[..start].trim().to_owned();
    }
    for separator in [" - ", " — ", " – "] {
        if let Some((author, title)) = rest.split_once(separator) {
            return (
                Some(author.trim().to_owned()),
                title.trim().to_owned(),
                narrator,
            );
        }
    }
    (None, rest, narrator)
}

fn stem(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    file.rsplit_once('.').map_or(file, |(s, _)| s).to_owned()
}

/// The book the files describe.
fn propose(upload_name: &str, mut audio: Vec<(FileRow, media::AudioInfo)>) -> Draft {
    // Track numbers when they order every file, else names.
    let mut tracks: Vec<u64> = audio.iter().filter_map(|(_, i)| i.track).collect();
    tracks.sort_unstable();
    tracks.dedup();
    if tracks.len() == audio.len() {
        audio.sort_by_key(|(_, i)| i.track);
    } else {
        audio.sort_by(|(a, _), (b, _)| natural(&a.name, &b.name));
    }

    let (folder_author, folder_title, folder_narrator) = parse_folder(&stem(upload_name));
    let first = audio.first().map(|(_, i)| i.clone()).unwrap_or_default();
    let single = audio.len() == 1;
    let title = first
        .album
        .clone()
        .or_else(|| single.then(|| first.title.clone()).flatten())
        .unwrap_or(folder_title);
    let author = first
        .artist
        .clone()
        .or_else(|| first.album_artist.clone())
        .or(folder_author);
    // The folder name "[Narrator]" beats guesses from other tags; Composer comes last, as
    // Russian releases often name the translator there.
    let narrator = first
        .narrator
        .clone()
        .or(folder_narrator)
        .or_else(|| {
            first
                .album_artist
                .clone()
                .filter(|aa| Some(aa) != first.artist.as_ref())
        })
        .or_else(|| first.composer.clone());

    let mut draft = Draft {
        title,
        authors: author.map(|a| books::split_names(&a)).unwrap_or_default(),
        narrators: narrator.map(|n| books::split_names(&n)).unwrap_or_default(),
        ..Draft::default()
    };
    let mut offset = 0u64;
    for (file, info) in &audio {
        let end = offset + info.duration_ms;
        if info.chapters.is_empty() {
            let title = info.title.clone().filter(|_| !single).unwrap_or_else(|| {
                if single {
                    draft.title.clone()
                } else {
                    stem(&file.name)
                }
            });
            draft.chapters.push(DraftChapter {
                title,
                start_ms: offset,
                end_ms: end,
            });
        } else {
            for chapter in &info.chapters {
                let start = offset + chapter.start_ms.min(info.duration_ms);
                let stop = chapter
                    .end_ms
                    .map_or(end, |e| offset + e.min(info.duration_ms));
                if stop > start {
                    let title = if chapter.title.is_empty() {
                        format!("{}", draft.chapters.len() + 1)
                    } else {
                        chapter.title.clone()
                    };
                    draft.chapters.push(DraftChapter {
                        title,
                        start_ms: start,
                        end_ms: stop,
                    });
                }
            }
        }
        draft.files.push(DraftFile {
            name: file.name.clone(),
            blob_hash: file.blob_hash.clone().unwrap_or_default(),
            size: u64::try_from(file.size).unwrap_or(0),
            offset_ms: offset,
            duration_ms: info.duration_ms,
        });
        offset = end;
    }
    draft.duration_ms = offset;
    draft
}

/// Where a stored file can be read: in place for local storage, else a downloaded copy.
async fn readable(
    state: &AppState,
    store: &ObjectBlobStore,
    tenant_id: &str,
    hash: &str,
) -> anyhow::Result<(PathBuf, bool)> {
    let content = ContentHash::parse(hash).context("hash")?;
    if let Some(path) = store.local_path(tenant_id, &content) {
        return Ok((path, false));
    }
    let path = temp_path(state)?;
    let mut out = tokio::fs::File::create(&path).await?;
    let mut stream = store.get(tenant_id, &content).await?;
    while let Some(chunk) = stream.next().await {
        out.write_all(&chunk?).await?;
    }
    out.flush().await?;
    Ok((path, true))
}

/// The `ingest_pipeline` job: inspects the files and stores the proposed book.
pub(crate) async fn analyze(state: &AppState, tenant_id: &str, id: &str) -> anyhow::Result<()> {
    let (status, storage_id, _) = upload_row(&state.db, tenant_id, id).await?;
    if status != "analyzing" {
        return Ok(()); // already done by an earlier attempt
    }
    let name: String = sqlx::query_scalar("SELECT name FROM uploads WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    let store = profiles::open(state, tenant_id, &storage_id).await?;
    let list = files(&state.db, tenant_id, id).await?;
    let mut audio = Vec::new();
    let mut images = Vec::new();
    for file in list {
        let hash = file.blob_hash.clone().context("file without blob")?;
        if file.kind == "image" {
            images.push((file, hash));
            continue;
        }
        let (path, temporary) = readable(state, &store, tenant_id, &hash).await?;
        let ext = extension(&file.name);
        let info = tokio::task::spawn_blocking(move || {
            let info = media::inspect(&path, &ext);
            if temporary {
                let _ = std::fs::remove_file(&path);
            }
            info
        })
        .await?
        .with_context(|| format!("cannot read {}", file.name))?;
        audio.push((file, info));
    }
    let embedded = audio.iter().find_map(|(_, i)| i.cover.clone());
    let mut draft = propose(&name, audio);

    // A picture next to the audio wins over the embedded one: it is usually larger.
    images.sort_by_key(|(f, _)| {
        let lower = f.name.to_lowercase();
        !["cover", "folder", "front", "обложк"]
            .iter()
            .any(|w| lower.contains(w))
    });
    if let Some((file, hash)) = images
        .into_iter()
        .find(|(f, _)| u64::try_from(f.size).unwrap_or(u64::MAX) <= MAX_COVER_BYTES)
    {
        let mut conn = state.db.acquire().await?;
        let mime = match extension(&file.name).as_str() {
            "png" => "image/png",
            "webp" => "image/webp",
            _ => "image/jpeg",
        };
        sqlx::query("UPDATE blobs SET codec = $3 WHERE tenant_id = $1 AND hash = $2")
            .bind(tenant_id)
            .bind(&hash)
            .bind(mime)
            .execute(&mut *conn)
            .await?;
        draft.cover_hash = Some(hash);
    } else if let Some(cover) = embedded {
        draft.cover_hash =
            Some(blobs::store_bytes(state, tenant_id, &storage_id, cover.data, &cover.mime).await?);
    }

    sqlx::query(
        "UPDATE uploads SET status = 'ready', draft = $3, error = NULL, updated_at = $4
         WHERE tenant_id = $1 AND id = $2 AND status = 'analyzing'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(serde_json::to_string(&draft)?)
    .bind(now_ms())
    .execute(&state.db)
    .await?;
    tracing::info!(upload = %id, files = draft.files.len(), chapters = draft.chapters.len(), "upload analysed");
    Ok(())
}

/// Marks an upload failed after the analysis gave up.
pub(crate) async fn fail(
    db: &SqlitePool,
    tenant_id: &str,
    id: &str,
    error: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE uploads SET status = 'failed', error = $3, updated_at = $4 WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(error)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// Creates the book from the proposal and the admin's edits.
///
/// # Errors
/// `NotFound`, `Conflict("upload_not_ready")`, the `Validation` codes of
/// [`BookEdits::validated`].
pub async fn save(
    state: &AppState,
    tenant_id: &str,
    id: &str,
    edits: &BookEdits,
) -> Result<String> {
    let edits = edits.validated()?;
    let upload = get(&state.db, tenant_id, id).await?;
    let Some(draft) = upload.draft.filter(|_| upload.status == "ready") else {
        return Err(BerthError::Conflict("upload_not_ready".into()));
    };
    let (_, _, library_id) = upload_row(&state.db, tenant_id, id).await?;
    let book_id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    let ms = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);

    let mut tx = state.db.begin().await.context("begin")?;
    let claimed = sqlx::query(
        "UPDATE uploads SET status = 'saved', updated_at = $3
         WHERE tenant_id = $1 AND id = $2 AND status = 'ready'",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("claim upload")?;
    if claimed.rows_affected() == 0 {
        return Err(BerthError::Conflict("upload_not_ready".into()));
    }
    let revision = books::next_revision(&mut tx, tenant_id)
        .await
        .context("revision")?;
    sqlx::query(
        "INSERT INTO books (id, tenant_id, library_id, title, duration_ms, cover_hash, revision, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
    )
    .bind(&book_id)
    .bind(tenant_id)
    .bind(&library_id)
    .bind(&edits.title)
    .bind(ms(draft.duration_ms))
    .bind(&draft.cover_hash)
    .bind(revision)
    .bind(now)
    .execute(&mut *tx)
    .await
    .context("insert book")?;
    books::write_edits(&mut tx, tenant_id, &book_id, &edits, revision)
        .await
        .context("book metadata")?;
    for (idx, file) in draft.files.iter().enumerate() {
        sqlx::query(
            "INSERT INTO book_files (tenant_id, book_id, idx, blob_hash, offset_ms, duration_ms)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant_id)
        .bind(&book_id)
        .bind(i64::try_from(idx).unwrap_or(i64::MAX))
        .bind(&file.blob_hash)
        .bind(ms(file.offset_ms))
        .bind(ms(file.duration_ms))
        .execute(&mut *tx)
        .await
        .context("insert file")?;
        sqlx::query("UPDATE blobs SET refcount = refcount + 1 WHERE tenant_id = $1 AND hash = $2")
            .bind(tenant_id)
            .bind(&file.blob_hash)
            .execute(&mut *tx)
            .await
            .context("refcount")?;
    }
    for (idx, chapter) in draft.chapters.iter().enumerate() {
        sqlx::query(
            "INSERT INTO chapters (tenant_id, book_id, idx, title, start_ms, end_ms) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant_id)
        .bind(&book_id)
        .bind(i64::try_from(idx).unwrap_or(i64::MAX))
        .bind(&chapter.title)
        .bind(ms(chapter.start_ms))
        .bind(ms(chapter.end_ms))
        .execute(&mut *tx)
        .await
        .context("insert chapter")?;
    }
    sqlx::query("UPDATE uploads SET book_id = $2 WHERE id = $1")
        .bind(id)
        .bind(&book_id)
        .execute(&mut *tx)
        .await
        .context("link book")?;
    tx.commit().await.context("commit book")?;
    tracing::info!(upload = %id, book = %book_id, "book saved");
    Ok(book_id)
}

/// An upload as the admin sees it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Upload {
    /// Id.
    pub id: String,
    /// Dropped folder or file.
    pub name: String,
    /// `uploading`, `analyzing`, `ready`, `saved` or `failed`.
    pub status: String,
    /// The proposed book once analysed.
    pub draft: Option<Draft>,
    /// Why the analysis failed.
    pub error: Option<String>,
    /// The saved book.
    pub book_id: Option<String>,
    /// Files in the upload.
    pub files: i64,
    /// Files stored so far.
    pub stored: i64,
    /// Bytes in the upload.
    pub bytes: i64,
    /// When started.
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct UploadRow {
    id: String,
    name: String,
    status: String,
    draft: Option<String>,
    error: Option<String>,
    book_id: Option<String>,
    files: i64,
    stored: i64,
    bytes: i64,
    created_at: i64,
}

impl From<UploadRow> for Upload {
    fn from(r: UploadRow) -> Self {
        Self {
            draft: r.draft.and_then(|d| serde_json::from_str(&d).ok()),
            id: r.id,
            name: r.name,
            status: r.status,
            error: r.error,
            book_id: r.book_id,
            files: r.files,
            stored: r.stored,
            bytes: r.bytes,
            created_at: r.created_at,
        }
    }
}

/// One upload.
///
/// # Errors
/// `NotFound`.
pub async fn get(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<Upload> {
    let row: Option<UploadRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.status, u.draft, u.error, u.book_id, u.created_at,
            (SELECT COUNT(*) FROM upload_files f WHERE f.upload_id = u.id) AS files,
            (SELECT COUNT(*) FROM upload_files f WHERE f.upload_id = u.id AND f.stored) AS stored,
            (SELECT COALESCE(SUM(size), 0) FROM upload_files f WHERE f.upload_id = u.id) AS bytes
         FROM uploads u WHERE u.tenant_id = $1 AND u.id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await
    .context("load upload")?;
    Ok(row.ok_or(BerthError::NotFound)?.into())
}

/// Latest uploads, newest first.
///
/// # Errors
/// Database failures.
pub async fn recent(db: &SqlitePool, tenant_id: &str, limit: i64) -> anyhow::Result<Vec<Upload>> {
    let rows: Vec<UploadRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.status, u.draft, u.error, u.book_id, u.created_at,
            (SELECT COUNT(*) FROM upload_files f WHERE f.upload_id = u.id) AS files,
            (SELECT COUNT(*) FROM upload_files f WHERE f.upload_id = u.id AND f.stored) AS stored,
            (SELECT COALESCE(SUM(size), 0) FROM upload_files f WHERE f.upload_id = u.id) AS bytes
         FROM uploads u WHERE u.tenant_id = $1 ORDER BY u.created_at DESC LIMIT $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Drops an unsaved upload from the queue; stored bytes stay for deduplication.
///
/// # Errors
/// `NotFound`, `Conflict("upload_saved")`.
pub async fn discard(db: &SqlitePool, tenant_id: &str, id: &str) -> Result<()> {
    let (status, _, _) = upload_row(db, tenant_id, id).await?;
    if status == "saved" {
        return Err(BerthError::Conflict("upload_saved".into()));
    }
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query("DELETE FROM upload_files WHERE tenant_id = $1 AND upload_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete files")?;
    sqlx::query("DELETE FROM uploads WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("delete upload")?;
    tx.commit().await.context("commit")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_and_folder_names() {
        let mut names = vec!["10.mp3", "2.mp3", "1.mp3", "Part 1/03.mp3"];
        names.sort_by(|a, b| natural(a, b));
        assert_eq!(names, ["1.mp3", "2.mp3", "10.mp3", "Part 1/03.mp3"]);
        assert_eq!(
            parse_folder("Азимов Айзек - Сами боги [Юрий Красиков]"),
            (
                Some("Азимов Айзек".into()),
                "Сами боги".into(),
                Some("Юрий Красиков".into())
            )
        );
        assert_eq!(
            parse_folder("[NNM-Club]_Рэй Далио_Принципы успеха (2020)"),
            (None, "Рэй Далио Принципы успеха".into(), None)
        );
        assert_eq!(
            parse_folder("Джеймс Дэшнер — Код лихорадки"),
            (Some("Джеймс Дэшнер".into()), "Код лихорадки".into(), None)
        );
    }

    fn file(idx: i64, name: &str) -> FileRow {
        FileRow {
            idx,
            name: name.into(),
            size: 10,
            kind: "audio".into(),
            blob_hash: Some(format!("h{idx}")),
            stored: true,
        }
    }

    #[test]
    fn proposes_files_and_chapters_on_one_timeline() {
        let info = |track: Option<u64>, title: &str, ms: u64, chapters: Vec<media::Chapter>| {
            media::AudioInfo {
                duration_ms: ms,
                title: Some(title.into()),
                album: Some("Книга".into()),
                artist: Some("Автор".into()),
                album_artist: Some("Чтец".into()),
                track,
                chapters,
                ..media::AudioInfo::default()
            }
        };
        let draft = propose(
            "Автор - Книга",
            vec![
                (file(0, "b.mp3"), info(Some(2), "Вторая", 2000, vec![])),
                (
                    file(1, "a.mp3"),
                    info(
                        Some(1),
                        "Первая",
                        1000,
                        vec![
                            media::Chapter {
                                title: "Пролог".into(),
                                start_ms: 0,
                                end_ms: Some(400),
                            },
                            media::Chapter {
                                title: String::new(),
                                start_ms: 400,
                                end_ms: None,
                            },
                        ],
                    ),
                ),
            ],
        );
        assert_eq!(draft.title, "Книга");
        assert_eq!(draft.authors, ["Автор"]);
        assert_eq!(
            draft.narrators,
            ["Чтец"],
            "album artist other than the artist"
        );
        assert_eq!(draft.duration_ms, 3000);
        assert_eq!(
            draft
                .files
                .iter()
                .map(|f| (f.name.as_str(), f.offset_ms))
                .collect::<Vec<_>>(),
            [("a.mp3", 0), ("b.mp3", 1000)]
        );
        assert_eq!(
            draft
                .chapters
                .iter()
                .map(|c| (c.title.as_str(), c.start_ms, c.end_ms))
                .collect::<Vec<_>>(),
            [("Пролог", 0, 400), ("2", 400, 1000), ("Вторая", 1000, 3000)]
        );
    }
}
