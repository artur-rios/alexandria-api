use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::catalog::commands::refresh::RefreshStarted;
use alexandria_core::catalog::runs::RunPriority;

use crate::middleware::error::ApiError;
use crate::routes::{bearer_token, deserialize_priority};
use crate::AppState;

/// The optional body `POST /v1/index/refresh` accepts. Every field has a
/// default, so the whole body is optional too — see `refresh`'s
/// `Option<Json<..>>` parameter, which is what makes a request with no body
/// at all (every caller before Task 12) keep working unchanged.
#[derive(Debug, Default, Deserialize)]
pub struct RefreshBody {
    /// How hard this run should push (FR-FC-08). Same wire spelling and the
    /// same lenient parsing as `IndexBody::priority` — see
    /// `deserialize_priority`.
    #[serde(default, deserialize_with = "deserialize_priority")]
    pub priority: RunPriority,
}

/// `POST /v1/index/refresh` — re-index and refresh the catalog (UC-02).
/// Refresh touches every cataloged path, so the only thing a body can carry
/// is `priority` — and that is optional, so the body itself is optional.
///
/// `Option<Json<RefreshBody>>` rather than `Result<Json<RefreshBody>, _>`:
/// axum turns *any* extraction failure (no body, no `content-type`, or
/// malformed JSON) into `None` rather than a rejection. That is deliberate
/// here — this body's only field is a value this route already treats
/// leniently (see `deserialize_priority`), so an absent or garbled body
/// falling back to the default is consistent with an absent or garbled
/// *field inside* a present body doing the same, rather than one being a
/// silent default and the other a `400`.
///
/// Returns `202` with a run id immediately; the refresh runs on a spawned
/// task (FR-FC-08).
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<RefreshBody>>,
) -> Result<(StatusCode, Json<RefreshStarted>), ApiError> {
    let token = bearer_token(&headers);
    let priority = body.map(|Json(b)| b.priority).unwrap_or_default();

    let started = state
        .services
        .refresh_handler
        .start(priority, &token)
        .await
        .map_err(ApiError)?;

    let run_id = started.run_id;
    let handler = state.services.refresh_handler.clone();
    tokio::spawn(async move {
        // Per-file failures are counted inside `execute`; an `Err` here means
        // the run could not start at all (e.g. the catalog was unreadable).
        // `execute` has already written the `failed` run record on its own
        // error path (UC-42), so the failure is recorded, not lost. This log
        // line is for the operator.
        if let Err(err) = handler.execute(run_id).await {
            tracing::error!(%run_id, error = %err, "re-index run aborted");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(started)))
}
