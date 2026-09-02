//! Music enrichment over HTTP (music enrichment design).

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::enrichment::model::{ArtistImage, EnrichmentReport, EnrichmentScope};
use alexandria_core::enrichment::queries::TrackEnrichmentView;
use alexandria_core::errors::DomainError;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Body for `POST /v1/enrichment/runs`: what to enrich.
///
/// Exactly one of the three, and none of them means the sweep. Modelled as
/// two optional fields rather than a tagged union because that is what a
/// form or a client with one text box actually sends, and the ambiguity is
/// resolved here once rather than at each caller.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentRunRequest {
    /// Enrich one file: its artist's image and its own lyrics.
    pub file_uuid: Option<Uuid>,
    /// Enrich one artist by name, and the lyrics of every track of theirs.
    pub artist: Option<String>,
    /// How many of the sweep to do in this call, when neither of the above
    /// is named. What lets a caller show progress and stop between batches.
    pub limit: Option<u32>,
}

impl EnrichmentRunRequest {
    /// The scope this body names.
    ///
    /// Naming both is refused rather than silently preferring one: a caller
    /// that sent both does not know what it asked for, and picking for it
    /// would hide that from them until the results looked wrong.
    fn scope(self) -> Result<EnrichmentScope, ApiError> {
        let limit = self.limit;
        match (self.file_uuid, self.artist) {
            (Some(_), Some(_)) => Err(invalid_input("name either fileUuid or artist, not both")),
            (Some(uuid), None) => Ok(EnrichmentScope::File(uuid)),
            (None, Some(artist)) if !artist.trim().is_empty() => {
                Ok(EnrichmentScope::Artist(artist.trim().to_string()))
            }
            (None, Some(_)) => Err(invalid_input("artist is blank")),
            (None, None) => Ok(EnrichmentScope::Pending {
                limit: limit.filter(|limit| *limit > 0),
            }),
        }
    }
}

/// `POST /v1/enrichment/runs` — look up artist photography and lyrics.
///
/// Returns `200` with a count of what happened, `400` (both scopes named, or
/// enrichment enabled with no contact configured), `401` (unauthenticated),
/// or `409` when enrichment is switched off.
///
/// `409` for "switched off" rather than `404`: the capability exists and the
/// route is real, the installation has simply not turned it on — which is a
/// conflict with current state, and is exactly what `GET /v1/settings`
/// reports so a client never has to discover it by being refused.
///
/// A run that reaches nothing still succeeds. A service being down, rate-
/// limiting, or having no answer is ordinary and is counted in the report,
/// not raised — a run that failed on the first unreachable host would never
/// get through a library (design section 5).
pub async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<EnrichmentRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<EnrichmentReport>), ApiError> {
    let token = bearer_token(&headers);

    // An absent body is the sweep, which is the common case and should not
    // require sending `{}`.
    let request = match body {
        Ok(Json(request)) => request,
        Err(JsonRejection::MissingJsonContentType(_)) => EnrichmentRunRequest::default(),
        Err(err) => return Err(invalid_input(format!("invalid enrichment body: {err}"))),
    };
    let scope = request.scope()?;

    let handler = state
        .services
        .enrich_handler
        .as_ref()
        .ok_or_else(|| ApiError(DomainError::InvalidState))?;

    let report = handler.enrich(scope, &token).await.map_err(ApiError)?;

    Ok((StatusCode::OK, Json(report)))
}

/// Query for `GET /v1/enrichment/tracks/{uuid}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEnrichmentQuery {
    /// Whose image to read, when the caller wants one.
    ///
    /// Passed in rather than resolved from the file's tags because the
    /// caller — a player already showing the track — is holding them, and
    /// re-reading them here would be a second query for a fact it has.
    pub artist: Option<String>,
}

/// `GET /v1/enrichment/tracks/{uuid}` — what enrichment has stored for one
/// track.
///
/// Available whether or not enrichment is switched on: reading what was
/// already cached is not a network operation, so an owner who turned it on,
/// ran it once and turned it off keeps what they fetched.
pub async fn read_track(
    State(state): State<AppState>,
    uuid: Result<axum::extract::Path<Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<TrackEnrichmentQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<TrackEnrichmentView>), ApiError> {
    let token = bearer_token(&headers);

    let axum::extract::Path(uuid) =
        uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Query(query) = query.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let view = state
        .services
        .read_enrichment_handler
        .read(uuid, query.artist.as_deref(), &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(view)))
}

/// The artist a picture is asked for.
#[derive(Debug, serde::Deserialize)]
pub struct ArtistImageQuery {
    pub name: String,
}

/// `GET /v1/enrichment/artist-image?name=…` — the photograph stored for one
/// artist (FR-PL-15).
///
/// A read, never a lookup, and available whether or not enrichment is
/// switched on: an artists list is a screenful of rows, and reading what was
/// already fetched is not a network operation. `404` for an artist nobody has
/// looked up and for one looked up without success — a client has nothing
/// different to draw for the two.
pub async fn read_artist_image(
    State(state): State<AppState>,
    query: Result<Query<ArtistImageQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<ArtistImage>), ApiError> {
    let token = bearer_token(&headers);
    let Query(query) = query.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let image = state
        .services
        .read_enrichment_handler
        .artist_image(&query.name, &token)
        .await
        .map_err(ApiError)?
        .ok_or(ApiError(DomainError::NotFound))?;

    Ok((StatusCode::OK, Json(image)))
}

/// `POST /v1/enrichment/artist-image?name=…` — look one artist's photograph
/// up and keep it (FR-PL-15).
///
/// **Reaches the network**, once, for one artist. A row already settled —
/// found, or looked for and not found — is answered from storage without a
/// request, which is what keeps a library of five hundred artists from being
/// five hundred requests every session. Answers the row whatever it
/// concluded, so a caller can tell "found" from "nothing to be found" and
/// stop asking.
pub async fn fetch_artist_image(
    State(state): State<AppState>,
    query: Result<Query<ArtistImageQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<ArtistImage>), ApiError> {
    let token = bearer_token(&headers);
    let Query(query) = query.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    // Absent for the same reason a run is refused: the capability is real,
    // this installation has not turned it on.
    let handler = state
        .services
        .enrich_handler
        .clone()
        .ok_or(ApiError(DomainError::InvalidState))?;

    let image = handler
        .artist_image(&query.name, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(image)))
}
