use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::watchlists::model::Watchlist;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body for `POST /v1/watchlists` (UC-20 / FR-WL-01): the
/// watchlist's `name`. Required — an absent or empty value is rejected as
/// invalid input (AF-01), matching the FFI surface's handling of the same
/// payload (FR-FC-24 / NFR-09).
#[derive(Debug, Deserialize)]
pub struct CreateWatchlistRequest {
    pub name: String,
}

/// `POST /v1/watchlists` — create a named watchlist for tracking video
/// consumption (UC-20 / FR-WL-01). The body carries `name`; the handler
/// validates it, mints a UUID, and persists the record. Returns `201` with
/// the created `Watchlist`, or `400` (invalid name). Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateWatchlistRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Watchlist>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid create watchlist body: {err}")))?;

    let result = state
        .services
        .create_watchlist_handler
        .create(&request.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(result)))
}
