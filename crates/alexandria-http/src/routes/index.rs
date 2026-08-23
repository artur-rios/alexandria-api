use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::catalog::commands::index::{IndexRequest, IndexStarted};
use alexandria_core::catalog::runs::RunPriority;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::{bearer_token, deserialize_priority};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct IndexBody {
    pub root: String,
    /// How hard this run should push (FR-FC-08). `"low"` or `"normal"`,
    /// matching `RunPriority`'s wire spelling exactly (FR-FC-24). Absent or
    /// unrecognised both mean `Normal` — see `deserialize_priority`.
    #[serde(default, deserialize_with = "deserialize_priority")]
    pub priority: RunPriority,
}

/// `POST /v1/index` — start an asynchronous indexing scan of a root path
/// (UC-01 / FR-FC-01..09). Returns `202` with the run id; the scan runs on a
/// spawned task (FR-FC-08).
///
/// The body is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`,
/// matching what the FFI surface reports for the same payload
/// (FR-FC-24 / NFR-09).
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IndexBody>, JsonRejection>,
) -> Result<(StatusCode, Json<IndexStarted>), ApiError> {
    let token = bearer_token(&headers);
    let Json(body) = body.map_err(|err| invalid_input(format!("invalid index body: {err}")))?;
    let request = IndexRequest {
        root: body.root.clone(),
        priority: body.priority,
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
        // `execute` has already written the `failed` run record on its own
        // error path (UC-42), so the failure is recorded, not lost. This log
        // line is for the operator.
        if let Err(err) = handler.execute(&root, run_id).await {
            tracing::error!(%run_id, error = %err, "index run aborted");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}
