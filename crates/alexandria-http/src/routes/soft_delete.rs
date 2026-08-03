use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::model::File;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `DELETE /v1/files/{uuid}` — soft-delete a file (UC-06 / FR-FC-20). Marks
/// the catalog row `deleted` and stamps `deleted_at` so the record is hidden
/// from active views but remains restorable via UC-07; the on-disk file is
/// untouched. Returns `200` with the updated `File`, or `400` (bad uuid),
/// `404` (uuid not found), `409` (already deleted — restore via UC-07), or
/// `401` (handled by the auth gate before this handler runs). The
/// `?purge=true` / `?purge-on-disk=true` query forms belong to UC-08 / UC-09
/// and are not handled here — a `DELETE` without those flags is always a
/// soft-delete.
///
/// The path is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`.
/// Authentication has already happened in `require_auth`, so reaching this
/// point means the caller is the owner. Soft-delete takes no body — a
/// parameterless state transition — so there is no body extractor.
pub async fn soft_delete(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<File>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let result = state
        .services
        .soft_delete_file_handler
        .soft_delete(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}