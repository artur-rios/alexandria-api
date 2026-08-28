use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::catalog::commands::index::{IndexRequest, IndexStarted};
use alexandria_core::catalog::index_scope::IndexScope;
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
    /// The file types this run records — the wire names `FileType` reads back
    /// (`"audio"`, `"video"`, …) — the same words the FFI surface's
    /// comma-separated `types` list carries (FR-FC-24). Absent and `[]` both
    /// mean every type; an unrecognised name is a `400`.
    ///
    /// Not lenient the way `priority` is, and deliberately: an unreadable
    /// priority falls back to the *safe* value, while a scope's only fallback
    /// would be "every type" — the opposite of what a caller asking for a
    /// narrower scope wants. `Vec<String>` rather than a `serde_json::Value`
    /// so a non-string element is rejected by the same route as a misspelt
    /// one.
    #[serde(default)]
    pub types: Vec<String>,
}

/// `POST /v1/index` — start an asynchronous indexing scan of a root path
/// (UC-01 / FR-FC-01..09). Returns `202` with the run id; the scan runs on a
/// spawned task (FR-FC-08).
///
/// The body is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`,
/// matching what the FFI surface reports for the same payload
/// (FR-FC-24 / NFR-09).
///
/// An unrecognised `types` entry is a `400` as well, refused before `start`
/// so that a request which never runs opens no run record (FR-FC-27). Unlike
/// `priority` it is not defaulted — `IndexScope` says why.
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IndexBody>, JsonRejection>,
) -> Result<(StatusCode, Json<IndexStarted>), ApiError> {
    let token = bearer_token(&headers);
    let Json(body) = body.map_err(|err| invalid_input(format!("invalid index body: {err}")))?;
    // Parsed before `start`, so a misspelt type is refused without a run
    // record being opened — the same order the root check keeps (FR-FC-27).
    let scope = IndexScope::parse(&body.types).map_err(ApiError)?;
    let request = IndexRequest {
        root: body.root.clone(),
        priority: body.priority,
        scope: scope.clone(),
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
        if let Err(err) = handler.execute(&root, run_id, &scope).await {
            tracing::error!(%run_id, error = %err, "index run aborted");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}
