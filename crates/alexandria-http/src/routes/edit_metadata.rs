use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::model::{FileMetadata, SubtypeMetadata};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `PATCH /v1/files/{uuid}/metadata` — edit a file's type-specific metadata
/// (UC-04 / FR-FC-14..18). The body is the `SubtypeMetadata` enum (internally
/// tagged by `type`) carrying the editable subtype fields. A PATCH is a full
/// replace of the editable columns; absent fields write `NULL`. Returns the
/// updated file plus its written metadata. Both the HTTP and FFI surfaces
/// call the same core handler, so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`. Authentication has already happened in `require_auth`, so
/// reaching this point means the caller is the owner and a parse failure is
/// genuinely about the payload (AF-01), not about credentials.
pub async fn edit_metadata(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    metadata: Result<Json<SubtypeMetadata>, JsonRejection>,
) -> Result<(StatusCode, Json<FileMetadata>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(metadata) =
        metadata.map_err(|err| invalid_input(format!("invalid metadata body: {err}")))?;

    let result = state
        .services
        .edit_metadata_handler
        .edit(uuid, metadata, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
