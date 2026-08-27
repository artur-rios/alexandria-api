use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::commands::index::IndexStarted;
use alexandria_core::catalog::runs::{CatalogRun, RunKind, RunPriority};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::{bearer_token, deserialize_optional_priority};
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

/// `POST /v1/index/runs/{runId}/pause` — stop a running run where it stands,
/// leaving it resumable (UC-48 / FR-FC-32). Calls the same
/// `RunControlHandler::pause` `alexandria_index_pause` (FFI, Task 11) calls.
///
/// Returns `200` on success, `400` (the path segment is not a uuid), `401`
/// (unauthenticated, AF-02), `404` (no run with that id, AF-01), or `409`
/// (the run is not currently `running` — pausing an already-paused or
/// already-finished run is refused rather than silently accepted).
pub async fn pause_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    run_id: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let token = bearer_token(&headers);

    let Path(run_id) = run_id.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    state
        .services
        .run_control_handler
        .pause(run_id, &token)
        .await
        .map_err(ApiError)?;

    Ok(StatusCode::OK)
}

/// `POST /v1/index/runs/{runId}/cancel` — abandon a running or paused run
/// (UC-48 / FR-FC-34). Terminal — a cancelled run is never resumed. Calls the
/// same `RunControlHandler::cancel` `alexandria_index_cancel` (FFI, Task 11)
/// calls.
///
/// Returns `200` on success, `400` (the path segment is not a uuid), `401`
/// (unauthenticated, AF-02), `404` (no run with that id, AF-01), or `409`
/// (the run is already terminal — `complete`, `failed`, or already
/// `cancelled` — there is nothing left to abandon).
pub async fn cancel_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    run_id: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let token = bearer_token(&headers);

    let Path(run_id) = run_id.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    state
        .services
        .run_control_handler
        .cancel(run_id, &token)
        .await
        .map_err(ApiError)?;

    Ok(StatusCode::OK)
}

/// The optional body `POST /v1/index/runs/{runId}/resume` accepts. Its one
/// field is optional too, so the whole body is — see `resume_run` for how an
/// absent or unreadable body reaches the default.
#[derive(Debug, Default, Deserialize)]
pub struct ResumeBody {
    /// Re-pace the run (FR-FC-08 / FR-FC-33). `"low"` or `"normal"`, the same
    /// wire spelling `IndexBody::priority` accepts — but *three*-valued here:
    /// absent, `null`, or unrecognised is `None`, meaning keep the width the
    /// run already has. See `deserialize_optional_priority`.
    #[serde(default, deserialize_with = "deserialize_optional_priority")]
    pub priority: Option<RunPriority>,
}

/// `POST /v1/index/runs/{runId}/resume` — put a paused run back to work
/// (UC-48 / FR-FC-33). Answers `202` with the *same* run id it was given —
/// matching how `POST /v1/index` and `POST /v1/index/refresh` answer a fresh
/// start, because a resume does not mint a new run, it continues the one it
/// was given (the same parity `alexandria_index_resume`, FFI Task 11,
/// observes).
///
/// `RunControlHandler::resume` only records the state transition; it does
/// not walk anything. Spawning the walk is this route's job, exactly as
/// `index()` and `refresh()` spawn their own — the handler stays free of the
/// runtime so whichever transport owns one is the one that spawns. Which
/// handler gets spawned depends on `RunResumed::kind`: an index run resumes
/// into `index_handler.execute(&root, run_id, &scope)` — the scope read
/// back off the run, so a resumed segment covers the file types the run was
/// started with — a refresh into
/// `refresh_handler.execute(run_id)` (a refresh carries no root — it touches
/// everything cataloged). A resumed index run whose stored `root` is somehow
/// absent — it should never be, every row `RunKind::Index` writes carries one
/// — is refused with `500` and logged at `error`, rather than silently doing
/// nothing: a caller told `202` for a run that never actually resumes would
/// have no way to notice. This mirrors `alexandria_index_resume`'s own
/// `RUN_ERR_OTHER` fallback exactly.
///
/// Returns `202` with `{"runId": …}` on success, `400` (the path segment is
/// not a uuid), `401` (unauthenticated, AF-02), `404` (no run with that id,
/// AF-01), or `409` (the run is not currently `paused`).
///
/// The body is taken as `Result<Json<ResumeBody>, JsonRejection>`, with
/// *every* rejection folded into `ResumeBody::default()` — the same shape
/// `refresh.rs` uses, and for the same reason. `Option<Json<ResumeBody>>`
/// would not do: axum 0.8's `OptionalFromRequest` impl for `Json` resolves to
/// `None` only when the `content-type` header is absent entirely, so a JSON
/// content-type with an empty body — exactly what a client that always
/// attaches one sends to a route that never took a body before — would come
/// back as a `400`. This route answered every bodiless resume before Task 15
/// and must go on answering them.
pub async fn resume_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    run_id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<ResumeBody>, JsonRejection>,
) -> Result<(StatusCode, Json<IndexStarted>), ApiError> {
    let token = bearer_token(&headers);

    let Path(run_id) = run_id.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let priority = body.map(|Json(b)| b.priority).unwrap_or_default();

    let resumed = state
        .services
        .run_control_handler
        .resume(run_id, &token, priority)
        .await
        .map_err(ApiError)?;

    match resumed.kind {
        RunKind::Index => {
            let root = match resumed.root {
                Some(root) => root,
                None => {
                    // Every row `RunKind::Index` writes carries a root
                    // (`start` requires one); reaching this means the stored
                    // row and its kind have drifted apart. Fail loudly rather
                    // than resume nothing and answer `202` for a run that
                    // never actually resumes.
                    tracing::error!(
                        run_id = %resumed.run_id,
                        "resumed index run has no stored root; refusing to spawn"
                    );
                    return Err(ApiError(alexandria_core::errors::DomainError::internal(
                        "resumed index run has no stored root",
                    )));
                }
            };
            let handler = state.services.index_handler.clone();
            // The run's own scope, off its row (FR-FC-01): a resumed segment
            // walks the file types the run was started with, not every type.
            let scope = resumed.scope;
            let spawned_run_id = resumed.run_id;
            tokio::spawn(async move {
                // Same shape as `index()`'s own spawn: an `Err` here means
                // the run could not resume at all; `execute` has already
                // written its own terminal row on that path (UC-48), so the
                // failure is recorded, not lost.
                if let Err(err) = handler.execute(&root, spawned_run_id, &scope).await {
                    tracing::error!(run_id = %spawned_run_id, error = %err, "resumed index run aborted");
                }
            });
        }
        RunKind::Refresh => {
            let handler = state.services.refresh_handler.clone();
            let spawned_run_id = resumed.run_id;
            tokio::spawn(async move {
                if let Err(err) = handler.execute(spawned_run_id).await {
                    tracing::error!(run_id = %spawned_run_id, error = %err, "resumed re-index run aborted");
                }
            });
        }
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(IndexStarted {
            run_id: resumed.run_id,
        }),
    ))
}

/// Query-string parameters for `GET /v1/index/runs` (UC-42 / FR-FC-35).
#[derive(Debug, Default, Deserialize)]
pub struct RunListParams {
    #[serde(default)]
    pub status: Option<String>,
}

/// Only one filter exists today — `active`, the two non-terminal statuses
/// (`running`, `paused`) `GetActiveRunsHandler::list` answers with — so this
/// has only one real decision to make: an absent `status` defaults to
/// `active` rather than being rejected, the same way `GET /v1/files`'s
/// `state` filter defaults rather than requires (`browse.rs::parse_state`).
/// Every route on this surface answers *something* runs-shaped when asked
/// for the runs collection with no further steer, and "what is still going"
/// is the only listing this handler can produce.
///
/// An unrecognised value (`?status=complete`, a typo) is rejected as `400`
/// rather than silently treated as `active`: unlike an absent filter, a
/// caller who named a specific status almost certainly wanted *that* status,
/// which this endpoint cannot serve — no query here can list terminal runs —
/// and answering with active runs anyway would look like a match. The FFI
/// surface (`alexandria_index_runs_active_json`, Task 11) offers no filter
/// argument at all, so there is nothing for this to disagree with on the
/// values it does accept; parity here is that `status=active` and no
/// `status` both call `GetActiveRunsHandler::list`, exactly as the FFI
/// accessor unconditionally does.
fn parse_status(status: Option<&str>) -> Result<(), ApiError> {
    match status.filter(|v| !v.is_empty()) {
        None | Some("active") => Ok(()),
        Some(other) => Err(invalid_input(format!("unknown status: {other}"))),
    }
}

/// `GET /v1/index/runs?status=active` — every outstanding (`running` or
/// `paused`) index and re-index run at once, newest first, each with live
/// progress overlaid exactly as `GET /v1/index/runs/{runId}` overlays a
/// single run (UC-42 / FR-FC-35). Calls the same
/// `GetActiveRunsHandler::list` `alexandria_index_runs_active_json` (FFI,
/// Task 11) calls — byte-for-byte the same body shape on both surfaces
/// (FR-FC-24 / NFR-09).
///
/// A caller with nothing outstanding gets `200` and an empty JSON array, not
/// an error — an idle library is the normal case, not a failure.
///
/// Returns `400` for an unrecognised `status` (see `parse_status`), `401`
/// for an unauthenticated caller (AF-02).
pub async fn active_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<RunListParams>, QueryRejection>,
) -> Result<Json<Vec<CatalogRun>>, ApiError> {
    // `Result<Query<..>, QueryRejection>` rather than the bare extractor, so
    // a malformed query string becomes this surface's `400` + `{"error": …}`
    // envelope rather than axum's bare-text rejection — the same reason
    // `run_status` and the control routes above take their path segment as
    // `Result<Path<Uuid>, PathRejection>`.
    let Query(params) = params.map_err(|_| invalid_input("malformed query string"))?;
    parse_status(params.status.as_deref())?;
    let token = bearer_token(&headers);

    let runs = state
        .services
        .get_active_runs_handler
        .list(&token)
        .await
        .map_err(ApiError)?;

    Ok(Json(runs))
}
