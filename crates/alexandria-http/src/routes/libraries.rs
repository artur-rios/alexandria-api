//! Libraries over HTTP (libraries design).

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::libraries::model::{Library, LibraryListing};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Body for `POST /v1/libraries`: the folder, and what to call it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterLibraryRequest {
    pub name: String,
    pub root_path: String,
}

/// `POST /v1/libraries` — treat a folder as a library.
///
/// Returns `201` with the library, `400` (blank name or path), `401`, or
/// `409` when the folder overlaps one that already exists — which names the
/// existing library, because "that folder is already inside Photography" is
/// something the owner can act on where a bare refusal is a puzzle.
///
/// Whatever is already indexed beneath the folder is claimed by the same
/// call: a folder is usually marked after it has been indexed, and a library
/// that showed nothing until the owner re-walked their disk would read as
/// broken.
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RegisterLibraryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Library>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid register library body: {err}")))?;

    let library = state
        .services
        .register_library_handler
        .register(&request.name, &request.root_path, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::CREATED, Json(library)))
}

/// `GET /v1/libraries` — every registered library.
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Vec<Library>>), ApiError> {
    let token = bearer_token(&headers);

    let libraries = state
        .services
        .list_libraries_handler
        .list(&token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(libraries)))
}

/// Query for `GET /v1/libraries/{uuid}`.
#[derive(Debug, Default, Deserialize)]
pub struct BrowseQuery {
    /// The folder to list, relative to the library root. Absent is the top.
    pub path: Option<String>,
}

/// `GET /v1/libraries/{uuid}` — one level of a library's tree.
///
/// One level, not the whole tree: a course with two hundred classes is a
/// large document to build, send and parse so the owner can look at the six
/// things in one folder.
pub async fn browse(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<BrowseQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<LibraryListing>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Query(query) = query.map_err(|err| invalid_input(format!("invalid query: {err}")))?;

    let listing = state
        .services
        .browse_library_handler
        .browse(uuid, query.path.as_deref().unwrap_or(""), &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(listing)))
}

/// `DELETE /v1/libraries/{uuid}` — stop treating the folder as a library.
///
/// The files are kept and return to the type panels. Marking a folder
/// empties part of a panel, which is not visible until after it is done, so
/// the way back restores rather than deletes.
pub async fn remove(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    state
        .services
        .remove_library_handler
        .remove(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok(StatusCode::NO_CONTENT)
}
