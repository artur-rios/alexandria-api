use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;

use alexandria_core::catalog::commands::refresh::RefreshStarted;

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `POST /v1/index/refresh` — re-index and refresh the catalog (UC-02).
/// Takes no body (refresh touches every cataloged path). Returns `202` with a
/// run id immediately; the refresh runs on a spawned task (FR-FC-08).
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<RefreshStarted>), ApiError> {
    let token = bearer_token(&headers);

    let started = state
        .services
        .refresh_handler
        .start(&token)
        .await
        .map_err(ApiError)?;

    let run_id = started.run_id;
    let handler = state.services.refresh_handler.clone();
    tokio::spawn(async move {
        let _ = handler.execute(run_id).await;
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}