use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::catalog::commands::index::{IndexRequest, IndexStarted};

use crate::middleware::error::ApiError;
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
        let _ = handler.execute(&root, run_id).await;
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}

fn bearer_token(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")))
        .unwrap_or("")
        .to_string()
}