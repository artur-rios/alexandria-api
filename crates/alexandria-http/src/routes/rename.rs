use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::model::File;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/files/{uuid}/rename` (UC-05 / FR-FC-19): the new
/// file name. `name` is required — an absent or empty field is rejected as
/// invalid input, matching the FFI surface's handling of the same payload
/// (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// `POST /v1/files/{uuid}/rename` — rename a file (and its on-disk file)
/// (UC-05 / FR-FC-19). The body carries the new `name`; the handler validates
/// it as a host-OS file name, renames the on-disk file, and updates the
/// catalog's `name` and `path`. Returns `200` with the updated `File`, or
/// `400` (invalid name), `404` (uuid), `409` (deleted state), or `500`
/// (disk error). Both the HTTP and FFI surfaces call the same core handler
/// so the two stay at parity (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`. Authentication has already happened in `require_auth`, so
/// reaching this point means the caller is the owner and a parse failure is
/// genuinely about the payload, not about credentials.
pub async fn rename(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<RenameRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<File>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) = body.map_err(|err| invalid_input(format!("invalid rename body: {err}")))?;

    let result = state
        .services
        .rename_file_handler
        .rename(uuid, request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
