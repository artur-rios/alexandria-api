use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::runs::CatalogRun;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/index/runs/{runId}` — report an index or re-index run's status and
/// outcome (UC-42 / FR-FC-28). Starting a run answers `202` with its id and
/// nothing else observed it until now; this is how a caller learns whether the
/// walk finished, and with what tally.
///
/// Returns `200` with the run, `400` (the path segment is not a uuid), `401`
/// (unauthenticated, AF-02 — enforced by the blanket `require_auth` gate this
/// route sits inside), or `404` (no run with that id, AF-01).
///
/// The path is taken as `Result<Path<Uuid>, PathRejection>` so a malformed
/// segment becomes this surface's `400` + `{"error": …}` envelope rather than
/// axum's bare-text rejection — the same pattern `rename.rs` and
/// `edit_metadata.rs` use for their own uuid path parameters.
pub async fn run_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    run_id: Result<Path<Uuid>, PathRejection>,
) -> Result<(StatusCode, Json<CatalogRun>), ApiError> {
    let token = bearer_token(&headers);

    let Path(run_id) = run_id.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let run = state
        .services
        .get_run_status_handler
        .get(run_id, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(run)))
}
