use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::model::File;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Query-string parameters for `DELETE /v1/files/{uuid}`. `purge=true`
/// dispatches to the UC-08 hard-purge handler; anything else (absent,
/// `purge=false`) is the UC-06 soft-delete. `purge-on-disk` (UC-09) is not
/// modeled here and is left ignored by serde.
#[derive(Debug, Default, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub purge: Option<bool>,
}

/// `DELETE /v1/files/{uuid}` — soft-delete a file (UC-06 / FR-FC-20), or,
/// with `?purge=true`, hard-purge a soft-deleted file's catalog row once its
/// retention window has elapsed (UC-08 / FR-FC-22).
///
/// Soft-delete marks the catalog row `deleted` and stamps `deleted_at` so
/// the record is hidden from active views but remains restorable via UC-07;
/// the on-disk file is untouched. Returns `200` with the updated `File`, or
/// `400` (bad uuid), `404` (uuid not found), `409` (already deleted —
/// restore via UC-07), or `401` (handled by the auth gate before this
/// handler runs).
///
/// `?purge=true` permanently removes the file's catalog row (and its
/// subtype row) instead; the on-disk file is still untouched (NFR-07).
/// Returns `200` with the pre-delete `File` as confirmation, or `400` (bad
/// uuid or non-boolean `purge`), `404` (uuid not found), `409` (not
/// `deleted`, or still within the retention window — AF-01), or `401`. The
/// `?purge-on-disk=true` query form belongs to UC-09 and is not handled
/// here — an unrecognized query key is ignored by serde.
///
/// The path and query are each taken as `Result` so a rejection becomes
/// this surface's `400` + `{"error": …}` envelope rather than axum's
/// bare-text `422`. Authentication has already happened in `require_auth`,
/// so reaching this point means the caller is the owner. Neither operation
/// takes a body — both are parameterless state transitions.
pub async fn soft_delete(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<File>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Query(query) = query.map_err(|_| invalid_input("purge must be true or false"))?;

    let result = if query.purge == Some(true) {
        state
            .services
            .purge_file_handler
            .purge(uuid, &token)
            .await
            .map_err(ApiError)?
    } else {
        state
            .services
            .soft_delete_file_handler
            .soft_delete(uuid, &token)
            .await
            .map_err(ApiError)?
    };

    Ok((StatusCode::OK, Json(result)))
}