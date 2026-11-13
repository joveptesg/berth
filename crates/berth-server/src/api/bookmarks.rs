//! Bookmarks of the user, synced between their devices.

use axum::{Json, extract::State};

use super::auth::{Authenticated, Body as JsonBody};
use crate::{
    AppState,
    error::{ApiError, Problem},
    service::{
        bookmarks::{self, BookmarkBatch, Bookmarks},
        books::PolicyFilter,
    },
};

/// Sends the device's bookmark changes (set, note edited, deleted). The batch is accepted as a
/// whole: drop it from the queue after 200. The latest change of a bookmark wins by the device
/// time corrected for its clock; a deleted bookmark stays deleted. The reply has all bookmarks.
#[utoipa::path(
    post,
    path = "/v1/bookmarks/batch",
    tag = "bookmarks",
    request_body = BookmarkBatch,
    responses(
        (status = OK, description = "Accepted; all bookmarks", body = Bookmarks),
        (status = BAD_REQUEST, description = "`invalid_body`, `bookmark_batch_too_long` or `bookmark_invalid`", body = Problem, content_type = "application/problem+json"),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn bookmarks_batch(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
    JsonBody(batch): JsonBody<BookmarkBatch>,
) -> Result<Json<Bookmarks>, ApiError> {
    bookmarks::record(&state.db, &device, &batch).await?;
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    Ok(Json(
        bookmarks::list(&state.db, &device.tenant_id, &device.user_id, &policy).await?,
    ))
}

/// The user's bookmarks, deleted ones included so that a device drops its copies. A device
/// keeps its own change when it is newer (compare in one clock with `server_now`).
#[utoipa::path(
    get,
    path = "/v1/bookmarks",
    tag = "bookmarks",
    responses(
        (status = OK, description = "All bookmarks", body = Bookmarks),
        (status = UNAUTHORIZED, description = "`unauthenticated` or `device_revoked`", body = Problem, content_type = "application/problem+json"),
    )
)]
pub(crate) async fn bookmarks(
    State(state): State<AppState>,
    Authenticated(device): Authenticated,
) -> Result<Json<Bookmarks>, ApiError> {
    let policy = PolicyFilter::for_user(&state.db, &device.tenant_id, &device.user_id).await?;
    Ok(Json(
        bookmarks::list(&state.db, &device.tenant_id, &device.user_id, &policy).await?,
    ))
}
