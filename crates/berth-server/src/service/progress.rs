//! Listening progress (design doc §9, §22.B11–B12): devices send batches of events from their
//! offline queue; every event lands once in the `progress_events` journal (idempotent by
//! `(device_id, event_id)`), and the current position of a book is the event with the latest
//! client time, corrected for the device clock: `client_ts + (server_now − client_now)`.
//! A late offline batch therefore never overwrites a later action on another device.

use anyhow::Context;
use berth_core::{BerthError, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{
    db::now_ms,
    service::{books::PolicyFilter, tokens::DeviceAuth},
};

/// Most events in one batch; a longer queue goes in several.
pub const MAX_BATCH: usize = 500;

/// One event of the device's queue.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct ProgressEvent {
    /// `UUIDv7` made by the device; resending the same id is a no-op.
    pub event_id: String,
    /// Book id.
    pub book_id: String,
    /// Position on the book timeline.
    pub position_ms: i64,
    /// Playback speed.
    #[serde(default = "one")]
    pub speed: f64,
    /// `position` (listening, a seek, a pause) or `finished` (the book was listened to the end).
    #[serde(rename = "type")]
    pub event_type: String,
    /// Device clock when it happened, epoch milliseconds.
    pub client_ts: i64,
}

const fn one() -> f64 {
    1.0
}

/// `POST /v1/progress/batch` body.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct ProgressBatch {
    /// Device clock at sending, epoch milliseconds: the server corrects `client_ts` by it.
    pub client_now: i64,
    /// Events, oldest first.
    pub events: Vec<ProgressEvent>,
}

/// The current position in a book.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Position {
    /// Book id.
    pub book_id: String,
    /// Position on the book timeline.
    pub position_ms: i64,
    /// Listened to the end.
    pub finished: bool,
    /// When it was set, server clock (corrected client time), epoch milliseconds.
    pub updated_at: i64,
}

/// Current positions of the user.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Positions {
    /// Server clock, epoch milliseconds: `updated_at - server_now + device_now` is device time.
    pub server_now: i64,
    /// Every book the user has a position in.
    pub positions: Vec<Position>,
}

/// Records a batch; the whole batch is accepted or rejected, so the device can drop it after 200.
///
/// # Errors
/// `Validation` for an empty or too long batch or a malformed event.
pub async fn record(db: &SqlitePool, device: &DeviceAuth, batch: &ProgressBatch) -> Result<()> {
    if batch.events.len() > MAX_BATCH {
        return Err(BerthError::Validation("progress_batch_too_long".into()));
    }
    for event in &batch.events {
        let valid = uuid::Uuid::parse_str(&event.event_id).is_ok()
            && event.position_ms >= 0
            && (0.25..=4.0).contains(&event.speed)
            && matches!(event.event_type.as_str(), "position" | "finished");
        if !valid {
            return Err(BerthError::Validation("progress_event_invalid".into()));
        }
    }
    let now = now_ms();
    let skew = now - batch.client_now;
    let mut tx = db.begin().await.context("begin")?;
    for event in &batch.events {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM books WHERE tenant_id = $1 AND id = $2)",
        )
        .bind(&device.tenant_id)
        .bind(&event.book_id)
        .fetch_one(&mut *tx)
        .await
        .context("check book")?;
        if !known {
            // A book that never existed here: nothing to keep, but no reason to fail the batch.
            continue;
        }
        let inserted = sqlx::query(
            "INSERT INTO progress_events (id, tenant_id, user_id, device_id, event_id, book_id,
                position_ms, speed, event_type, client_ts, server_ts)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (device_id, event_id) DO NOTHING",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&device.tenant_id)
        .bind(&device.user_id)
        .bind(&device.device_id)
        .bind(&event.event_id)
        .bind(&event.book_id)
        .bind(event.position_ms)
        .bind(event.speed)
        .bind(&event.event_type)
        .bind(event.client_ts)
        .bind(now)
        .execute(&mut *tx)
        .await
        .context("journal event")?
        .rows_affected();
        if inserted == 0 {
            continue;
        }
        // A clock running ahead cannot claim the future.
        let at = (event.client_ts + skew).min(now);
        sqlx::query(
            "INSERT INTO progress_current (tenant_id, user_id, book_id, position_ms, finished, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (tenant_id, user_id, book_id) DO UPDATE SET
                position_ms = excluded.position_ms, finished = excluded.finished,
                updated_at = excluded.updated_at
             WHERE excluded.updated_at > progress_current.updated_at",
        )
        .bind(&device.tenant_id)
        .bind(&device.user_id)
        .bind(&event.book_id)
        .bind(event.position_ms)
        .bind(event.event_type == "finished")
        .bind(at)
        .execute(&mut *tx)
        .await
        .context("update position")?;
    }
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Current positions of the user in books that still exist and that the user may see.
///
/// # Errors
/// Database failures.
pub async fn list(
    db: &SqlitePool,
    tenant_id: &str,
    user_id: &str,
    policy: &PolicyFilter,
) -> anyhow::Result<Positions> {
    let positions: Vec<Position> = sqlx::query_as(
        "SELECT p.book_id, p.position_ms, p.finished, p.updated_at FROM progress_current p
         JOIN books b ON b.id = p.book_id
         WHERE p.tenant_id = $1 AND p.user_id = $2 AND b.deleted_at IS NULL
         ORDER BY p.updated_at DESC",
    )
    .bind(tenant_id)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(Positions {
        server_now: now_ms(),
        positions: positions
            .into_iter()
            .filter(|p| policy.allows(&p.book_id))
            .collect(),
    })
}
