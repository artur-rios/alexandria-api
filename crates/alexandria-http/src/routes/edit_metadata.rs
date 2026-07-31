use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::model::{FileMetadata, SubtypeMetadata};

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `PATCH /v1/files/{uuid}/metadata` — edit a file's type-specific metadata
/// (UC-04 / FR-FC-14..18). The body is the `SubtypeMetadata` enum (internally
/// tagged by `type`) carrying the editable subtype fields. A PATCH is a full
/// replace of the editable columns; absent fields write `NULL`. Returns the
/// updated file plus its written metadata. Both the HTTP and FFI surfaces
/// call the same core handler, so the two stay at parity (FR-FC-24 / NFR-09).
pub async fn edit_metadata(
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    headers: HeaderMap,
    Json(metadata): Json<SubtypeMetadata>,
) -> Result<(StatusCode, Json<FileMetadata>), ApiError> {
    let token = bearer_token(&headers);

    let result = state
        .services
        .edit_metadata_handler
        .edit(uuid, metadata, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}