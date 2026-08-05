use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

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
