use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::watchlists::model::{WatchProgress, Watchlist};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/watchlists` (UC-20 / FR-WL-01): the
/// watchlist's `name`. Required — an absent or empty value is rejected as
/// invalid input (AF-01), matching the FFI surface's handling of the same
/// payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct CreateWatchlistRequest {
    pub name: String,
}

/// `POST /v1/watchlists` — create a named watchlist for tracking video
/// consumption (UC-20 / FR-WL-01). The body carries `name`; the handler
/// validates it, mints a UUID, and persists the record. Returns `201` with
/// the created `Watchlist`, or `400` (invalid name). Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateWatchlistRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Watchlist>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create watchlist body: {err}")))?;

    let result = state
        .services
        .create_watchlist_handler
        .create(&request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// Request body for `POST /v1/watchlists/{uuid}/items` (UC-22 / FR-WL-02,
/// FR-WL-03): the `videoUuid` to link. Required — an absent or malformed
/// value is rejected as invalid input, matching the FFI surface's handling
/// of the same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddVideoRequest {
    pub video_uuid: Uuid,
}

/// `POST /v1/watchlists/{uuid}/items` — add a video to a watchlist (UC-22 /
/// FR-WL-02, FR-WL-03). The body carries `videoUuid`; the handler verifies
/// the watchlist and video exist and that the target is a video file before
/// linking it. Adding an already-linked video is idempotent and returns its
/// existing progress unchanged. Returns `200` with the `WatchProgress`, or
/// `400` (target is not a video), `404` (watchlist or video uuid), or `401`
/// (unauthenticated). Both the HTTP and FFI surfaces call the same core
/// handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn add_video(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<AddVideoRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<WatchProgress>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid add video body: {err}")))?;

    let result = state
        .services
        .add_video_to_watchlist_handler
        .add(uuid, request.video_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
