use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::model::{
    WatchProgress, WatchState, Watchlist, WatchlistWithProgress,
};

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

/// Query-string parameters for `GET /v1/watchlists` (UC-21 / FR-WL-08). An
/// omitted or empty `watchlistUuid` means every watchlist is returned; a
/// malformed value is rejected as `400` invalid input, matching the FFI
/// surface (FR-FC-24 / NFR-09).
#[derive(Debug, Default, Deserialize)]
pub struct WatchlistListParams {
    #[serde(rename = "watchlistUuid", default)]
    pub watchlist_uuid: Option<String>,
}

/// `GET /v1/watchlists` — browse watchlists and their items' watch progress
/// (UC-21 / FR-WL-08), optionally filtered to a single watchlist. Returns
/// `200` with a JSON array of `WatchlistWithProgress` records, or `400`
/// (malformed `watchlistUuid`), `404` (the referenced watchlist does not
/// exist), or `401` (unauthenticated).
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<WatchlistListParams>, QueryRejection>,
) -> Result<Json<Vec<WatchlistWithProgress>>, ApiError> {
    let token = bearer_token(&headers);

    let Query(params) = params.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let watchlist_uuid = match params.watchlist_uuid.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(v) => Some(Uuid::parse_str(v).map_err(|_| {
            ApiError(DomainError::InvalidInput(format!(
                "invalid watchlistUuid: {v}"
            )))
        })?),
    };

    let result = state
        .services
        .browse_watchlists_handler
        .list(watchlist_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok(Json(result))
}

/// Request body for `PATCH /v1/watchlists/{uuid}/items/{videoUuid}` (UC-23 /
/// FR-WL-04, FR-WL-05): the new `state`, and optionally `currentEpisode` /
/// `totalEpisodes` for a series. Full replace, not a merge — an absent
/// episode field clears it. `state` is required and must be a recognised
/// `WatchState` — an absent or unrecognised value is rejected as invalid
/// input, matching the FFI surface's handling of the same payload
/// (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchProgressRequest {
    pub state: String,
    pub current_episode: Option<i64>,
    pub total_episodes: Option<i64>,
}

/// `PATCH /v1/watchlists/{uuid}/items/{videoUuid}` — update watch progress
/// (UC-23 / FR-WL-04, FR-WL-05). The body carries the new `state` and,
/// optionally, the current/total episode for a series; the handler validates
/// the transition (`Pending` → `Watching` → `Watched`, one step at a time)
/// before applying it. Returns `200` with the updated `WatchProgress`, or
/// `400` (unrecognised `state`), `404` (the video is not on that watchlist,
/// AF-02), `409` (invalid transition, AF-01), or `401` (unauthenticated).
/// Both the HTTP and FFI surfaces call the same core handler so the two stay
/// at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn update_progress(
    State(state): State<AppState>,
    uuids: Result<Path<(Uuid, Uuid)>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<UpdateWatchProgressRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<WatchProgress>), ApiError> {
    let token = bearer_token(&headers);

    let Path((watchlist_uuid, video_uuid)) =
        uuids.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid update progress body: {err}")))?;
    let new_state = WatchState::parse(&request.state)
        .ok_or_else(|| invalid_input(format!("unknown state: {}", request.state)))?;

    let result = state
        .services
        .update_watch_progress_handler
        .update(
            watchlist_uuid,
            video_uuid,
            new_state,
            request.current_episode,
            request.total_episodes,
            &token,
        )
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
