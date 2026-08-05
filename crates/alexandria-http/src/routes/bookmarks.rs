use axum::extract::rejection::JsonRejection;
use axum::extract::State;
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
