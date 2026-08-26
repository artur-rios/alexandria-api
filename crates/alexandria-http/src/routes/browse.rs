use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use alexandria_core::catalog::model::{FileType, FileView, StateFilter};
use alexandria_core::catalog::queries::browse::FileFilter;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Query-string parameters for `GET /v1/files` (UC-03 / FR-FC-12). All
/// optional: an omitted or empty `type` means no type filter; an omitted or
/// empty `state` defaults to `active` (excludes soft-deleted records per the
/// use case's main-flow step 2); an omitted or empty `collectionUuid` means
/// no collection filter. An unrecognised `type` or `state` is rejected as
/// `400` invalid input rather than silently ignored — the FFI surface
/// rejects the same inputs identically (FR-FC-24 / NFR-09). A malformed
/// `collectionUuid` is likewise `400`; a well-formed one that matches no
/// collection returns an empty list rather than an error (UC-14 / FR-CO-05).
#[derive(Debug, Default, Deserialize)]
pub struct FileListParams {
    #[serde(rename = "type", default)]
    pub file_type: Option<String>,
    #[serde(rename = "state", default)]
    pub state: Option<String>,
    #[serde(rename = "collectionUuid", default)]
    pub collection_uuid: Option<String>,
}

/// Map a state query value to the `StateFilter`, defaulting to `Active` per
/// the use case's main-flow step 2 (deleted records excluded by default). An
/// unrecognised value is invalid input, not a silent fallback to the default.
fn parse_state(s: Option<&str>) -> Result<StateFilter, ApiError> {
    match s.filter(|v| !v.is_empty()) {
        None => Ok(StateFilter::Active),
        Some(v) => StateFilter::parse(v).ok_or_else(|| {
            ApiError(alexandria_core::errors::DomainError::InvalidInput(format!(
                "unknown state: {v}"
            )))
        }),
    }
}

/// `GET /v1/files` — list/query files by type and lifecycle state (UC-03 /
/// FR-FC-12). Returns `200` with a JSON array of `FileView` records — the
/// same shape `GET /v1/files/{uuid}` returns for one file (issue #116): the
/// `File`, its `SubtypeMetadata`, and the extracted scalars, assembled by
/// `BrowseFilesHandler::list` via the repository's batched query rather than
/// one detail lookup per row. Soft-deleted records are excluded by default
/// unless `state=deleted` or `state=all` is requested.
pub async fn list_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<FileListParams>,
) -> Result<Json<Vec<FileView>>, ApiError> {
    let token = bearer_token(&headers);

    let mut filter = FileFilter::new().with_state(parse_state(params.state.as_deref())?);
    if let Some(t) = params.file_type.as_deref().filter(|s| !s.is_empty()) {
        let file_type = parse_file_type(t).ok_or_else(|| {
            ApiError(alexandria_core::errors::DomainError::InvalidInput(format!(
                "unknown file type: {t}"
            )))
        })?;
        filter = filter.with_type(file_type);
    }
    if let Some(c) = params.collection_uuid.as_deref().filter(|s| !s.is_empty()) {
        let collection_uuid = Uuid::parse_str(c).map_err(|_| {
            ApiError(alexandria_core::errors::DomainError::InvalidInput(format!(
                "invalid collectionUuid: {c}"
            )))
        })?;
        filter = filter.with_collection(collection_uuid);
    }

    let files = state
        .services
        .browse_files_handler
        .list(filter, &token)
        .await
        .map_err(ApiError)?;

    Ok(Json(files))
}

/// `GET /v1/files/{uuid}` — get a single file's metadata by its public UUID
/// (UC-03 / FR-FC-13). Returns `200` with a `FileView` (the file plus its
/// stored subtype metadata when the subtype has one), or `400` (bad uuid),
/// `404` (AF-01) or `401` (AF-02).
///
/// The path is taken as `Result` so a rejection becomes this surface's
/// `400` + `{"error": …}` envelope rather than axum's bare-text rejection.
pub async fn get_file(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<FileView>), ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let view = state
        .services
        .browse_files_handler
        .get_by_uuid(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(view)))
}

fn parse_file_type(s: &str) -> Option<FileType> {
    match s {
        "audio" => Some(FileType::Audio),
        "video" => Some(FileType::Video),
        "html" => Some(FileType::Html),
        "text" => Some(FileType::Text),
        "document" => Some(FileType::Document),
        "comic" => Some(FileType::Comic),
        "image" => Some(FileType::Image),
        _ => None,
    }
}
