use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::model::{File, PurgeOnDiskOutcome};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Query-string parameters for `DELETE /v1/files/{uuid}`. `purge=true`
/// dispatches to the UC-08 hard-purge handler; `purge-on-disk=true`
/// dispatches to the UC-09 purge-on-disk handler; anything else (absent, or
/// both `false`) is the UC-06 soft-delete. Setting both `purge` and
/// `purge-on-disk` to `true` is rejected as invalid input — the two are
/// distinct operations (BR-11) and the caller must pick one.
#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    pub purge: Option<bool>,
    #[serde(rename = "purge-on-disk")]
    pub purge_on_disk: Option<bool>,
}

/// The success body of [`delete_file`]. Untagged so the wire shape for
/// UC-06/UC-08 (a bare `File`) is unchanged; only the UC-09 branch adds the
/// `diskFilePresent` field via [`PurgeOnDiskOutcome`].
#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum DeleteResult {
    File(File),
    PurgeOnDisk(PurgeOnDiskOutcome),
}

/// `DELETE /v1/files/{uuid}` — soft-delete a file (UC-06 / FR-FC-20), or,
/// with `?purge=true`, hard-purge a soft-deleted file's catalog row once its
/// retention window has elapsed (UC-08 / FR-FC-22), or, with
/// `?purge-on-disk=true`, delete the on-disk file and remove its catalog row
/// regardless of retention (UC-09 / FR-FC-23, FR-FC-24).
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
/// `deleted`, or still within the retention window — AF-01), or `401`.
///
/// `?purge-on-disk=true` deletes the on-disk file first, then the catalog
/// row (and its subtype row); there is no retention gate — an `active`
/// record is purgeable too. Returns `200` with a [`PurgeOnDiskOutcome`]
/// (the pre-delete `File` plus `diskFilePresent`, `false` when no on-disk
/// file was there to delete — AF-01, still a success), or `400` (bad uuid,
/// non-boolean `purge-on-disk`, or both `purge` and `purge-on-disk` set),
/// `404` (uuid not found — AF-03), `500` (disk error — AF-02, the record is
/// left untouched), or `401` (AF-04).
///
/// One `500` on the `?purge-on-disk=true` branch is not what it looks like:
/// when the on-disk delete succeeds but the catalog write then fails, the
/// error renders as the generic `{"error": "database error"}` even though the
/// file is already gone from disk. The response is indistinguishable from
/// "nothing happened", so a client must not read it as such. Retrying the
/// same call is safe and is the intended recovery — the second attempt finds
/// no on-disk file (AF-01), removes the now-orphaned row, and returns `200`
/// with `diskFilePresent: false`.
///
/// The path and query are each taken as `Result` so a rejection becomes
/// this surface's `400` + `{"error": …}` envelope rather than axum's
/// bare-text `422`. Authentication has already happened in `require_auth`,
/// so reaching this point means the caller is the owner. Neither operation
/// takes a body — both are parameterless state transitions.
pub async fn delete_file(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<DeleteResult>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Query(query) =
        query.map_err(|_| invalid_input("purge and purge-on-disk must be true or false"))?;

    if query.purge == Some(true) && query.purge_on_disk == Some(true) {
        return Err(invalid_input(
            "purge and purge-on-disk are distinct operations and cannot both be true",
        ));
    }

    let result = if query.purge_on_disk == Some(true) {
        DeleteResult::PurgeOnDisk(
            state
                .services
                .purge_file_on_disk_handler
                .purge_on_disk(uuid, &token)
                .await
                .map_err(ApiError)?,
        )
    } else if query.purge == Some(true) {
        DeleteResult::File(
            state
                .services
                .purge_file_handler
                .purge(uuid, &token)
                .await
                .map_err(ApiError)?,
        )
    } else {
        DeleteResult::File(
            state
                .services
                .soft_delete_file_handler
                .soft_delete(uuid, &token)
                .await
                .map_err(ApiError)?,
        )
    };

    Ok((StatusCode::OK, Json(result)))
}
