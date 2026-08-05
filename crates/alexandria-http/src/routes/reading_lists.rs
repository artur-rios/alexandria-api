use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::errors::DomainError;
use alexandria_core::reading_lists::model::{
    ReadingList, ReadingListWithProgress, ReadingProgress, ReadingState,
};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/reading-lists` (UC-26 / FR-RL-01): the
/// reading list's `name`. Required — an absent or empty value is rejected
/// as invalid input (AF-01), matching the FFI surface's handling of the
/// same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct CreateReadingListRequest {
    pub name: String,
}

/// `POST /v1/reading-lists` — create a named reading list for tracking
/// book/comic consumption (UC-26 / FR-RL-01). The body carries `name`; the
/// handler validates it, mints a UUID, and persists the record. Returns
/// `201` with the created `ReadingList`, or `400` (invalid name). Both the
/// HTTP and FFI surfaces call the same core handler so the two stay at
/// parity (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateReadingListRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ReadingList>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create reading list body: {err}")))?;

    let result = state
        .services
        .create_reading_list_handler
        .create(&request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// Request body for `POST /v1/reading-lists/{uuid}/items` (UC-28 /
/// FR-RL-02, FR-RL-03): the `itemUuid` to link. Required — an absent or
/// malformed value is rejected as invalid input, matching the FFI surface's
/// handling of the same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddItemRequest {
    pub item_uuid: Uuid,
}

/// `POST /v1/reading-lists/{uuid}/items` — add a book or comic to a reading
/// list (UC-28 / FR-RL-02, FR-RL-03). The body carries `itemUuid`; the
/// handler verifies the reading list and item exist and that the target is
/// a Document or ComicBook before linking it. Adding an already-linked item
/// is idempotent and returns its existing progress unchanged. Returns `200`
/// with the `ReadingProgress`, or `400` (target is ineligible), `404`
/// (reading list or item uuid), or `401` (unauthenticated). Both the HTTP
/// and FFI surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn add_item(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<AddItemRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ReadingProgress>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid add item body: {err}")))?;

    let result = state
        .services
        .add_item_to_reading_list_handler
        .add(uuid, request.item_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// Query-string parameters for `GET /v1/reading-lists` (UC-27 / FR-RL-08).
/// An omitted or empty `readingListUuid` means every reading list is
/// returned; a malformed value is rejected as `400` invalid input, matching
/// the FFI surface (FR-FC-24 / NFR-09).
#[derive(Debug, Default, Deserialize)]
pub struct ReadingListListParams {
    #[serde(rename = "readingListUuid", default)]
    pub reading_list_uuid: Option<String>,
}

/// `GET /v1/reading-lists` — browse reading lists and their items' read
/// progress (UC-27 / FR-RL-08), optionally filtered to a single reading
/// list. Returns `200` with a JSON array of `ReadingListWithProgress`
/// records, or `400` (malformed `readingListUuid`), `404` (the referenced
/// reading list does not exist), or `401` (unauthenticated).
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<ReadingListListParams>, QueryRejection>,
) -> Result<Json<Vec<ReadingListWithProgress>>, ApiError> {
    let token = bearer_token(&headers);

    let Query(params) = params.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let reading_list_uuid = match params
        .reading_list_uuid
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(v) => Some(Uuid::parse_str(v).map_err(|_| {
            ApiError(DomainError::InvalidInput(format!(
                "invalid readingListUuid: {v}"
            )))
        })?),
    };

    let result = state
        .services
        .browse_reading_lists_handler
        .list(reading_list_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok(Json(result))
}

/// Request body for `PATCH /v1/reading-lists/{uuid}/items/{itemUuid}`
/// (UC-29 / FR-RL-04, FR-RL-05): the new `state`, and optionally
/// `currentIssue` / `totalIssues` for a comic series. Full replace, not a
/// merge — an absent issue field clears it. `state` is required and must be
/// a recognised `ReadingState` — an absent or unrecognised value is
/// rejected as invalid input, matching the FFI surface's handling of the
/// same payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReadingProgressRequest {
    pub state: String,
    pub current_issue: Option<i64>,
    pub total_issues: Option<i64>,
}

/// `PATCH /v1/reading-lists/{uuid}/items/{itemUuid}` — update reading
/// progress (UC-29 / FR-RL-04, FR-RL-05). The body carries the new `state`
/// and, optionally, the current/total issue for a comic series; the handler
/// validates the transition (`Pending` → `Reading` → `Read`, one step at a
/// time) before applying it. Returns `200` with the updated
/// `ReadingProgress`, or `400` (unrecognised `state`), `404` (the item is
/// not on that reading list, AF-02), `409` (invalid transition, AF-01), or
/// `401` (unauthenticated). Both the HTTP and FFI surfaces call the same
/// core handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn update_progress(
    State(state): State<AppState>,
    uuids: Result<Path<(Uuid, Uuid)>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<UpdateReadingProgressRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ReadingProgress>), ApiError> {
    let token = bearer_token(&headers);

    let Path((reading_list_uuid, item_uuid)) =
        uuids.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid update progress body: {err}")))?;
    let new_state = ReadingState::parse(&request.state)
        .ok_or_else(|| invalid_input(format!("unknown state: {}", request.state)))?;

    let result = state
        .services
        .update_reading_progress_handler
        .update(
            reading_list_uuid,
            item_uuid,
            new_state,
            request.current_issue,
            request.total_issues,
            &token,
        )
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
