//! Background jobs in the `jobs` table: a worker claims a due job with a conditional
//! `UPDATE` that sets a lease (`locked_by`, `locked_until`); a lease that expires (a crashed
//! worker) is taken over. Failures retry with backoff; after the last attempt the job is done
//! with `last_error`, and its kind's failure handler runs. Handlers must be idempotent.

use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use sqlx::SqliteConnection;

use crate::{
    AppState,
    db::now_ms,
    service::{ai, demo, ingest},
};

/// Lease of a claimed job: longer than any job runs.
const LEASE_MS: i64 = 30 * 60 * 1000;
const MAX_ATTEMPTS: i64 = 5;
const BACKOFF_MS: i64 = 30_000;
/// The worker also polls, so jobs enqueued by another process (the CLI) run too.
const POLL: Duration = Duration::from_secs(5);

/// Analyses an upload: `{"upload_id": ...}`.
pub(crate) const INGEST: &str = "ingest_pipeline";
/// Loads a book of the demo library: `{"slot": ..., "full": ...}`.
pub(crate) const DEMO_BOOK: &str = "demo_book";
/// Asks the AI about the next book without a description: `{"after": ...}`.
pub(crate) const AI_DESCRIBE: &str = "ai_describe";

/// Adds a job, due now; call [`AppState::wake_jobs`] after the transaction commits.
pub(crate) async fn enqueue(
    conn: &mut SqliteConnection,
    tenant_id: &str,
    kind: &str,
    payload: &Value,
) -> sqlx::Result<String> {
    let id = uuid::Uuid::now_v7().to_string();
    let now = now_ms();
    sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, payload, run_at, created_at) VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(kind)
    .bind(payload.to_string())
    .bind(now)
    .execute(conn)
    .await?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct Claimed {
    id: String,
    tenant_id: String,
    kind: String,
    payload: String,
    attempts: i64,
}

/// Runs due jobs until none is left; returns how many ran.
///
/// # Errors
/// Database failures (job failures are recorded, not returned).
pub async fn run_due(state: &AppState) -> anyhow::Result<usize> {
    let worker = uuid::Uuid::now_v7().to_string();
    let mut ran = 0;
    loop {
        let now = now_ms();
        let job: Option<Claimed> = sqlx::query_as(
            "UPDATE jobs SET locked_by = $1, locked_until = $2, attempts = attempts + 1
             WHERE id = (SELECT id FROM jobs
                         WHERE done_at IS NULL AND run_at <= $3
                           AND (locked_until IS NULL OR locked_until < $3)
                         ORDER BY run_at LIMIT 1)
               AND (locked_until IS NULL OR locked_until < $3)
             RETURNING id, tenant_id, kind, payload, attempts",
        )
        .bind(&worker)
        .bind(now + LEASE_MS)
        .bind(now)
        .fetch_optional(&state.db)
        .await
        .context("claim job")?;
        let Some(job) = job else {
            return Ok(ran);
        };
        ran += 1;
        let payload: Value = serde_json::from_str(&job.payload).unwrap_or_default();
        let result = run(state, &job.tenant_id, &job.kind, &payload).await;
        let now = now_ms();
        match result {
            Ok(()) => {
                sqlx::query(
                    "UPDATE jobs SET done_at = $2, last_error = NULL, locked_by = NULL, locked_until = NULL
                     WHERE id = $1",
                )
                .bind(&job.id)
                .bind(now)
                .execute(&state.db)
                .await?;
            }
            Err(err) => {
                let message = format!("{err:#}");
                let last = job.attempts >= MAX_ATTEMPTS;
                tracing::warn!(job = %job.id, kind = %job.kind, attempt = job.attempts, error = %message, "job failed");
                let retry_at = now + BACKOFF_MS * (1 << (job.attempts - 1).clamp(0, 10));
                sqlx::query(
                    "UPDATE jobs SET last_error = $2, locked_by = NULL, locked_until = NULL,
                        run_at = $3, done_at = $4 WHERE id = $1",
                )
                .bind(&job.id)
                .bind(&message)
                .bind(retry_at)
                .bind(last.then_some(now))
                .execute(&state.db)
                .await?;
                if last {
                    gave_up(state, &job.tenant_id, &job.kind, &payload, &message).await;
                }
            }
        }
    }
}

async fn run(state: &AppState, tenant_id: &str, kind: &str, payload: &Value) -> anyhow::Result<()> {
    match kind {
        INGEST => {
            let upload = payload["upload_id"].as_str().context("upload_id")?;
            ingest::analyze(state, tenant_id, upload).await
        }
        DEMO_BOOK => demo::run_job(state, tenant_id, payload).await,
        AI_DESCRIBE => ai::run_job(state, tenant_id, payload).await,
        _ => anyhow::bail!("unknown job kind {kind}"),
    }
}

async fn gave_up(state: &AppState, tenant_id: &str, kind: &str, payload: &Value, error: &str) {
    if kind == DEMO_BOOK {
        demo::give_up(&state.db, tenant_id, payload, error).await;
    }
    if kind == AI_DESCRIBE
        && let Err(err) = ai::stop(&state.db, tenant_id).await
    {
        tracing::warn!(error = %err, "could not stop the AI run");
    }
    if kind == INGEST
        && let Some(upload) = payload["upload_id"].as_str()
        && let Err(err) = ingest::fail(&state.db, tenant_id, upload, error).await
    {
        tracing::warn!(error = %err, "could not mark the upload failed");
    }
}

/// A job as the dashboard shows it.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct JobView {
    /// Kind, such as `ingest_pipeline`.
    pub kind: String,
    /// `queued`, `running`, `retrying`, `done` or `failed`.
    pub status: String,
    /// Attempts so far.
    pub attempts: i64,
    /// Error of the last failed attempt.
    pub last_error: Option<String>,
    /// When queued.
    pub created_at: i64,
}

/// The tenant's latest jobs, newest first.
///
/// # Errors
/// Database failures.
pub async fn recent(
    db: &sqlx::SqlitePool,
    tenant_id: &str,
    limit: i64,
) -> sqlx::Result<Vec<JobView>> {
    sqlx::query_as(
        "SELECT kind, attempts, last_error, created_at,
            CASE WHEN done_at IS NOT NULL AND last_error IS NOT NULL THEN 'failed'
                 WHEN done_at IS NOT NULL THEN 'done'
                 WHEN locked_until > $3 THEN 'running'
                 WHEN attempts > 0 THEN 'retrying'
                 ELSE 'queued' END AS status
         FROM jobs WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(now_ms())
    .fetch_all(db)
    .await
}

/// Starts the worker: it runs when woken and every few seconds.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            if let Err(err) = run_due(&state).await {
                tracing::warn!(error = ?err, "job worker failed");
            }
            let _ = tokio::time::timeout(POLL, state.jobs.notified()).await;
        }
    });
}
