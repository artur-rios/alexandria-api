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
        // Per-file failures are counted inside `execute`; an `Err` here means
        // the run could not start at all (e.g. the catalog was unreadable).
        // Log it — nothing else observes this task's result.
        if let Err(err) = handler.execute(run_id).await {
            tracing::error!(%run_id, error = %err, "re-index run aborted");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}