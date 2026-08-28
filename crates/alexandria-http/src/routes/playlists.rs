use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use alexandria_core::playlists::model::{Playlist, PlaylistEntry, PlaylistView};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/playlists` and `PATCH /v1/playlists/{uuid}`
/// (Tasks 1 / 2): the playlist's `name`. Required — an absent or blank
/// value is rejected as invalid input by `validate_playlist_name`, matching
/// the FFI surface's handling of the same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct PlaylistNameRequest {
    pub name: String,
}

/// `POST /v1/playlists` — create a named, empty playlist (Task 1). The body
/// carries `name`; the handler validates it, mints a UUID, and persists the
/// record. Returns `201` with the created `Playlist`, or `400` (invalid
/// name), or `401` (unauthenticated). Both the HTTP and FFI surfaces call
/// the same core handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<PlaylistNameRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Playlist>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create playlist body: {err}")))?;

    let result = state
        .services
        .create_playlist_handler
        .create(&request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// `PATCH /v1/playlists/{uuid}` — rename a playlist (Task 2), leaving its
/// entries and their order untouched. The body carries the new `name`.
/// Returns `200` with the updated `Playlist`, or `400` (invalid name), `404`
/// (uuid), or `401` (unauthenticated). Both the HTTP and FFI surfaces call
/// the same core handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn rename(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<PlaylistNameRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Playlist>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid rename playlist body: {err}")))?;

    let result = state
        .services
        .rename_playlist_handler
        .rename(uuid, &request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// `DELETE /v1/playlists/{uuid}` — delete a playlist (Task 3). Removes the
/// playlist and every entry it holds; the referenced audio files are
/// preserved. Returns `200` with the pre-delete `Playlist` as confirmation,
/// or `404` (uuid), or `401` (unauthenticated). Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `400`.
pub async fn delete(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Playlist>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let result = state
        .services
        .delete_playlist_handler
        .delete(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// `GET /v1/playlists` — every persisted playlist, without their tracks
/// (Task 6). Returns `200` with a JSON array of `Playlist` records, or
/// `401` (unauthenticated).
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Playlist>>, ApiError> {
    let token = bearer_token(&headers);

    let result = state
        .services
        .browse_playlists_handler
        .list(&token)
        .await
        .map_err(ApiError)?;

    Ok(Json(result))
}

/// `GET /v1/playlists/{uuid}` — read a playlist back with its tracks, in
/// position order (Task 6). Returns `200` with a `PlaylistView`, or `404`
/// (uuid), or `401` (unauthenticated).
///
/// The path is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `400`.
pub async fn read(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<PlaylistView>, ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let result = state
        .services
        .browse_playlists_handler
        .read(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok(Json(result))
}

/// Request body for `POST /v1/playlists/{uuid}/entries` (Task 4): the
/// `fileUuids` to append, in order. Required — an absent or malformed value
/// is rejected as invalid input, matching the FFI surface's handling of the
/// same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddEntriesRequest {
    pub file_uuids: Vec<Uuid>,
}

/// `POST /v1/playlists/{uuid}/entries` — append tracks to a playlist (Task
/// 4). The body carries `fileUuids`, appended in order at consecutive
/// positions after whatever the playlist already holds; the whole slice
/// succeeds or none of it does. Returns `200` with the new
/// `Vec<PlaylistEntry>`, or `400` (a resolved file is not audio), `404`
/// (the playlist or a file uuid), or `401` (unauthenticated). Both the HTTP
/// and FFI surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn add_entries(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<AddEntriesRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Vec<PlaylistEntry>>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid add entries body: {err}")))?;

    let result = state
        .services
        .add_entries_handler
        .add(uuid, &request.file_uuids, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// `DELETE /v1/playlists/{uuid}/entries/{entryUuid}` — remove one entry from
/// a playlist (Task 4), addressed by its own `entryUuid` rather than a file
/// uuid, since a playlist may hold the same track more than once. Returns
/// `200` with an empty JSON object as confirmation — the core handler
/// answers nothing beyond success (`Result<(), DomainError>`), so nothing
/// beyond the identifiers already in the URL is available to echo back — or
/// `404` (the playlist or entry), or `401` (unauthenticated). Both the HTTP
/// and FFI surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `400`.
pub async fn remove_entry(
    State(state): State<AppState>,
    path: Result<Path<(Uuid, Uuid)>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let token = bearer_token(&headers);

    let Path((playlist_uuid, entry_uuid)) =
        path.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    state
        .services
        .remove_entry_handler
        .remove(playlist_uuid, entry_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(json!({}))))
}

/// Request body for `POST /v1/playlists/{uuid}/entries/{entryUuid}/move`
/// (Task 5): the `toIndex` to move the entry to. Required — an absent or
/// malformed value is rejected as invalid input, matching the FFI surface's
/// handling of the same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveEntryRequest {
    pub to_index: i64,
}

/// `POST /v1/playlists/{uuid}/entries/{entryUuid}/move` — move one playlist
/// entry to a new index (Task 5), addressed by its own `entryUuid` rather
/// than a file uuid, since a playlist may hold the same track more than
/// once. The body carries `toIndex`; the handler computes the new order and
/// renumbers every entry in one transaction. Returns `200` with the
/// playlist's full new order (`Vec<PlaylistEntry>`), or `400` (`toIndex` is
/// negative or `>=` the playlist's entry count), `404` (the playlist or
/// entry), or `401` (unauthenticated). Both the HTTP and FFI surfaces call
/// the same core handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn move_entry(
    State(state): State<AppState>,
    path: Result<Path<(Uuid, Uuid)>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<MoveEntryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Vec<PlaylistEntry>>), ApiError> {
    let token = bearer_token(&headers);

    let Path((playlist_uuid, entry_uuid)) =
        path.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid move entry body: {err}")))?;

    let result = state
        .services
        .reorder_playlist_handler
        .move_entry(playlist_uuid, entry_uuid, request.to_index, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

// The `DomainError` -> status mapping used by every route above lives
// centrally in `crate::middleware::error::ApiError` (`InvalidInput` -> 400,
// `NotFound` -> 404, `Unauthorized` -> 401), shared with every other feature
// area so this surface answers the same status for the same failure
// everywhere.
