use axum::extract::rejection::JsonRejection;
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
/// default, so the whole body is optional too — see `refresh`'s doc comment
/// for how an absent or unreadable body reaches that default.
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
/// Taken as `Result<Json<RefreshBody>, JsonRejection>`, with *every*
/// rejection folded into `RefreshBody::default()` — not
/// `Option<Json<RefreshBody>>`. `Option<..>`'s `OptionalFromRequest` impl
/// for `Json` (axum 0.8) only resolves to `None` when the `content-type`
/// header is absent entirely; a JSON content-type with an empty body, a
/// non-JSON content-type, or malformed JSON are all real extraction
/// failures under that impl, which `Option` would have surfaced as this
/// surface's usual `400`/`415` — breaking any caller whose HTTP library
/// attaches a JSON content-type by default, a shape that worked before this
/// task (`refresh` took no body extractor at all). Folding every rejection
/// into the default here, explicitly, is what actually delivers "an absent
/// or garbled body behaves like an absent or garbled `priority` field" — the
/// leniency `deserialize_priority` already gives a *present* body's field.
///
/// Returns `202` with a run id immediately; the refresh runs on a spawned
/// task (FR-FC-08).
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RefreshBody>, JsonRejection>,
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
