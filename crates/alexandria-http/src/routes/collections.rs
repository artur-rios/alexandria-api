use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::collections::model::{Collection, CollectionKind};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/collections` (UC-10 / FR-CO-01, FR-CO-02): the
/// collection's `name` and its `kind` discriminator. Both are required — an
/// absent, empty, or unrecognised value is rejected as invalid input (AF-01),
/// matching the FFI surface's handling of the same payload (FR-FC-24 /
/// NFR-09). `kind` is typed as the domain enum, so an unrecognised value fails
/// here while deserializing rather than reaching the handler.
#[derive(Debug, Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
    pub kind: CollectionKind,
}

/// `POST /v1/collections` — create a flat file or bookmark collection (UC-10 /
/// FR-CO-01, FR-CO-02). The body carries `name` and `kind`; the handler
/// validates the name, mints a UUID, and persists the record. Returns `201`
/// with the created `Collection`, or `400` (invalid name or kind). Both the
/// HTTP and FFI surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
/// Authentication has already happened in `require_auth`, so reaching this
/// point means the caller is the owner and a parse failure is genuinely about
/// the payload, not about credentials.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateCollectionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Collection>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create collection body: {err}")))?;

    let result = state
        .services
        .create_collection_handler
        .create(&request.name, request.kind, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}

/// Request body for `PATCH /v1/collections/{uuid}` (UC-11 / FR-CO-03): the
/// collection's new `name`. Required — an absent or empty field is rejected
/// as invalid input, matching the FFI surface's handling of the same payload
/// (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct RenameCollectionRequest {
    pub name: String,
}

/// `PATCH /v1/collections/{uuid}` — rename a collection (UC-11 / FR-CO-03).
/// The body carries the new `name`; the handler validates it and updates the
/// record. Returns `200` with the updated `Collection`, or `400` (invalid
/// name), `404` (uuid), or `401` (unauthenticated). Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path and body are taken as `Result` so their rejections become this
/// surface's `400` + `{"error": …}` envelope rather than axum's bare-text
/// `422`/`400`.
pub async fn rename(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<RenameCollectionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Collection>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid rename collection body: {err}")))?;

    let result = state
        .services
        .rename_collection_handler
        .rename(uuid, &request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// `DELETE /v1/collections/{uuid}` — delete a collection (UC-12 / FR-CO-04).
/// Unlinks every item the collection holds without deleting them, then
/// removes the collection. Returns `200` with the pre-delete `Collection` as
/// confirmation, or `404` (uuid), or `401` (unauthenticated). Both the HTTP
/// and FFI surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The path is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `400`.
pub async fn delete(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Collection>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let result = state
        .services
        .delete_collection_handler
        .delete(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
