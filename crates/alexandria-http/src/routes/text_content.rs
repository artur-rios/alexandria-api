use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::model::FileContent;

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/files/{uuid}/content` — read a TextFile's content from disk
/// (UC-32 / FR-TX-01). Returns `200` with the `FileContent`, or `400` (the
/// file is not a TextFile, AF-01), `404` (uuid, AF-03), `500` (the on-disk
/// file cannot be read, AF-02), or `401` (unauthenticated, AF-04). Both the
/// HTTP and FFI surfaces call the same core handler so the two stay at
/// parity (FR-FC-24 / NFR-09).
pub async fn get_content(
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<FileContent>), ApiError> {
    let token = bearer_token(&headers);

    let content = state
        .services
        .read_text_file_content_handler
        .read(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(content)))
}
