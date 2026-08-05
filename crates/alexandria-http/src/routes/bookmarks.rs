use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::bookmarks::model::Bookmark;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/bookmarks` (UC-15 / FR-BM-01): the bookmark's
/// `url`, `title`, and an optional `collectionUuid`. `url` and `title` are
/// required — an absent, empty, or malformed value is rejected as invalid
/// input (AF-01), matching the FFI surface's handling of the same payload
/// (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBookmarkRequest {
    pub url: String,
    pub title: String,
    pub collection_uuid: Option<Uuid>,
}

/// `POST /v1/bookmarks` — create a browser bookmark, optionally in an
/// existing bookmark collection (UC-15 / FR-BM-01). The body carries `url`,
/// `title`, and an optional `collectionUuid`; the handler validates the
/// fields, confirms a referenced collection exists and is `kind = bookmark`,
/// mints a UUID, and persists the record. Returns `201` with the created
/// `Bookmark`, or `400` (invalid url/title, or the collection is not a
/// bookmark collection), or `404` (referenced collection does not exist).
/// Both the HTTP and FFI surfaces call the same core handler so the two stay
/// at parity (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBookmarkRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Bookmark>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create bookmark body: {err}")))?;

    let result = state
        .services
        .create_bookmark_handler
        .create(
            &request.url,
            &request.title,
            request.collection_uuid,
            &token,
        )
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// Request body for `PATCH /v1/bookmarks/{uuid}` (UC-16 / FR-BM-02): the
/// bookmark's new `url`, `title`, and containing collection. Full replace,
/// not a merge — a missing or `null` `collectionUuid` clears the link rather
/// than leaving it untouched, matching `CreateBookmarkRequest`'s shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBookmarkRequest {
    pub url: String,
    pub title: String,
    pub collection_uuid: Option<Uuid>,
}

/// `PATCH /v1/bookmarks/{uuid}` — update a bookmark's url, title, and
/// containing collection (UC-16 / FR-BM-02). The body carries `url`,
/// `title`, and `collectionUuid` (all replaced); the handler validates the
/// fields and, when a collection is referenced, confirms it exists and is
/// `kind = bookmark`. Returns `200` with the updated `Bookmark`, or `400`
/// (invalid url/title, or the collection is not a bookmark collection),
/// `404` (the bookmark or the referenced collection does not exist), `409`
/// (the bookmark is soft-deleted — restore via UC-18 first), or `401`
/// (unauthenticated). Both the HTTP and FFI surfaces call the same core
/// handler so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn update(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<UpdateBookmarkRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Bookmark>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid update bookmark body: {err}")))?;

    let result = state
        .services
        .update_bookmark_handler
        .update(
            uuid,
            &request.url,
            &request.title,
            request.collection_uuid,
            &token,
        )
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
