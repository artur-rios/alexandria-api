//! Play history over HTTP (play history design).

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::plays::model::{MusicStats, PlayEvent};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Body for `POST /v1/plays`: which track was played.
///
/// No timestamp field, deliberately. The core stamps the play from its own
/// clock, so a client cannot record a play in the middle of last year, and
/// the two surfaces cannot disagree about when "now" was.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordPlayRequest {
    pub file_uuid: Uuid,
}

/// Query string for `GET /v1/plays/stats`: how many rows each ranking
/// answers with. Absent means the handler's default of ten.
#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    pub limit: Option<i64>,
}

/// `POST /v1/plays` — record that a track was played.
///
/// Returns `201` with the recorded play, `400` when the file is not audio,
/// `404` when the uuid does not resolve, or `401`. Both the HTTP and FFI
/// surfaces call the same core handler so the two stay at parity
/// (FR-FC-24 / NFR-09).
///
/// A `POST` with no idempotency of any kind: playing the same track twice
/// is two plays, which is the entire point of counting them.
///
/// The body is taken as a `Result` so its rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text `422`/`400`.
pub async fn record(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RecordPlayRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<PlayEvent>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid record play body: {err}")))?;

    let play = state
        .services
        .record_play_handler
        .record(request.file_uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(play)))
}

/// `GET /v1/plays/stats?limit=10` — what was played most.
///
/// Returns `200` with the summary and the four rankings, `400` when
/// `limit` is outside 1..=100, or `401`. One response rather than a route
/// per ranking: they are read together, and four round trips could each see
/// a different instant and disagree with each other.
pub async fn stats(
    State(state): State<AppState>,
    query: Result<Query<StatsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<MusicStats>), ApiError> {
    let token = bearer_token(&headers);

    let Query(query) = query.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let stats = state
        .services
        .music_stats_handler
        .read(query.limit, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(stats)))
}
