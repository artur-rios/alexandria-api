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

/// `POST /v1/files/{uuid}/restore` — restore a soft-deleted file (UC-07 /
/// FR-FC-21). Returns the catalog row to `active` and clears `deleted_at`;
/// the on-disk file is untouched. Returns `200` with the restored `File`,
/// or `400` (bad uuid), `404` (uuid not found — also when past the retention
/// window, AF-01), `409` (file is not in `deleted` state, AF-02), or `401`
/// (handled by the auth gate before this handler runs).
///
/// The path is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`.
/// Authentication has already happened in `require_auth`, so reaching this
/// point means the caller is the owner. Restore takes no body — a
/// parameterless state transition — so there is no body extractor.
pub async fn restore(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<File>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let result = state
        .services
        .restore_file_handler
        .restore(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
