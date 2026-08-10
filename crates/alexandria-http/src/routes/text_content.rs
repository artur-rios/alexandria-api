use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::model::{File, FileContent};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/files/{uuid}/content` — read a TextFile's content from disk
/// (UC-32 / FR-TX-01). Returns `200` with the `FileContent`, or `400` (the
/// file is not a TextFile, AF-01), `404` (uuid, AF-03), `409` (the file is
/// soft-deleted — restore via UC-07 first), `500` (the on-disk file cannot
/// be read, AF-02), or `401` (unauthenticated, AF-04). Both the
/// HTTP and FFI surfaces call the same core handler so the two stay at
/// parity (FR-FC-24 / NFR-09).
///
/// The path is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text rejection.
pub async fn get_content(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<FileContent>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let content = state
        .services
        .read_text_file_content_handler
        .read(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(content)))
}

/// Request body for `PUT /v1/files/{uuid}/content` (UC-33 / FR-TX-02,
/// FR-TX-03): the new `content`. Required.
#[derive(Debug, Deserialize)]
pub struct EditContentRequest {
    pub content: String,
}

/// `PUT /v1/files/{uuid}/content` — write edited content back to a
/// TextFile on disk (UC-33 / FR-TX-02, FR-TX-03). The body carries
/// `content`; the handler writes it to the file's on-disk path, verifies
/// the post-write hash, and refreshes the catalog's `contentHash`. Returns
/// `200` with the updated `File`, or `400` (the file is not a TextFile,
/// AF-01, or a malformed body), `404` (uuid, AF-04), `409` (the file is
/// soft-deleted — restore via UC-07 first), `500` (the on-disk write fails,
/// AF-02, or the post-write hash still does not match after one retry,
/// AF-03), or `401` (unauthenticated, AF-05). Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn edit_content(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<EditContentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<File>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid edit content body: {err}")))?;

    let result = state
        .services
        .edit_text_file_content_handler
        .edit(uuid, request.content, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
