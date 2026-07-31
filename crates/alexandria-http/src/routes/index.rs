use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::catalog::commands::index::{IndexRequest, IndexStarted};

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct IndexBody {
    pub root: String,
}

pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IndexBody>,
) -> Result<(StatusCode, Json<IndexStarted>), ApiError> {
    let token = bearer_token(&headers);
    let request = IndexRequest {
        root: body.root.clone(),
    };

    let started = state
        .services
        .index_handler
        .start(request, &token)
        .await
        .map_err(ApiError)?;

    let handler = state.services.index_handler.clone();
    let root = body.root;
    let run_id = started.run_id;
    tokio::spawn(async move {
        // Per-file failures are counted inside `execute`; an `Err` here means
        // the run could not start at all (e.g. the root became unlistable).
        // Log it — nothing else observes this task's result.
        if let Err(err) = handler.execute(&root, run_id).await {
            tracing::error!(%run_id, error = %err, "index run aborted");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}