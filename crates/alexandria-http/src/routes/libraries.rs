//! Libraries over HTTP (libraries design).

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::commands::index::IndexStarted;
use alexandria_core::catalog::runs::RunPriority;
use alexandria_core::libraries::model::{Library, LibraryListing, RegisteredLibrary};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::{bearer_token, deserialize_priority};
use crate::AppState;

/// Walk `library`'s folder for the run `run_id`, on a task of its own — the
/// walk takes minutes and the request that opened the run has answered.
fn spawn_walk(state: &AppState, library: &Library, run_id: uuid::Uuid) {
    let handler = state.services.index_handler.clone();
    let root = library.root_path.clone();
    let library_uuid = library.uuid;
    tokio::spawn(async move {
        // `execute` writes the run's `failed` record on its own error path
        // (UC-42), so a failure here is recorded, not lost; this line is for
        // the operator.
        if let Err(err) = handler.execute(&root, run_id, library_uuid).await {
            tracing::error!(%run_id, error = %err, "index run aborted");
        }
    });
}

/// Body for `POST /v1/libraries`: the folder, and what to call it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterLibraryRequest {
    pub name: String,
    pub root_path: String,
    /// How hard the first run should push: `"low"` or `"normal"`, lenient as
    /// every start body is — absent or unrecognised is `Normal`.
    #[serde(default, deserialize_with = "deserialize_priority")]
    pub priority: RunPriority,
}

/// `POST /v1/libraries` — add a folder as a library and start indexing it.
///
/// Returns `201` with the library and the `runId` of its first index run,
/// which a client follows with `GET /v1/index/runs/{runId}`; `400` (blank
/// name or path, a folder that does not exist, or one outside the configured
/// `filesystem.root`), `401`, or `409` when the folder overlaps a library
/// that already exists — which names it, because "that folder is already
/// inside Photography" is something the owner can act on where a bare
/// refusal is a puzzle. A refusal stores nothing.
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RegisterLibraryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RegisteredLibrary>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid register library body: {err}")))?;

    let registered = state
        .services
        .register_library_handler
        .register(&request.name, &request.root_path, request.priority, &token)
        .await
        .map_err(ApiError)?;

    spawn_walk(&state, &registered.library, registered.run_id);

    Ok((StatusCode::CREATED, Json(registered)))
}

/// Body for `POST /v1/libraries/{uuid}/index`. Optional as a whole: no body
/// is a normal-priority run.
#[derive(Debug, Default, Deserialize)]
pub struct IndexLibraryRequest {
    #[serde(default, deserialize_with = "deserialize_priority")]
    pub priority: RunPriority,
}

/// `POST /v1/libraries/{uuid}/index` — re-index one library (UC-01).
///
/// Returns `202` with the run id, `401`, `404` for an unknown library, or
/// `409` while the library already has an unfinished run, naming it
/// (FR-FC-42).
pub async fn index(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<IndexLibraryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<IndexStarted>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    // A missing or unreadable body is a normal run, the way a bodiless
    // resume keeps its width: the priority is the only thing it could say.
    let request = body.map(|Json(request)| request).unwrap_or_default();

    let (library, started) = state
        .services
        .index_library_handler
        .index(uuid, request.priority, &token)
        .await
        .map_err(ApiError)?;

    spawn_walk(&state, &library, started.run_id);

    Ok((StatusCode::ACCEPTED, Json(started)))
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

/// Body for `PATCH /v1/libraries/{uuid}`: where the folder is now.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveLibraryRequest {
    pub root_path: String,
}

/// `PATCH /v1/libraries/{uuid}` — the folder moved; correct the record.
///
/// Returns `200` with the moved library, `400` (blank path, or a path outside
/// the configured `filesystem.root` — FR-FC-26), `401`, `404`, or `409` — the
/// destination overlaps another library, or the catalog already holds files
/// there.
///
/// A correction rather than a re-index, which is the point: the files move
/// with the root and keep their uuids, so every watchlist place, reading
/// position and collection membership that pointed at them still does.
/// Re-walking the new location would mint new records and leave these
/// missing.
pub async fn move_root(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<MoveLibraryRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Library>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;
    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid move library body: {err}")))?;

    let library = state
        .services
        .move_library_handler
        .move_to(uuid, &request.root_path, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(library)))
}

/// `DELETE /v1/libraries/{uuid}` — remove a library and its catalog records.
///
/// Every file record in the library goes, with its progress and collection
/// membership; nothing on disk is touched, and adding the folder again
/// re-indexes it (FR-FC-40). Returns `204`, `401`, `404`, or `409` while one
/// of the library's runs is running, naming it (FR-FC-42).
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
