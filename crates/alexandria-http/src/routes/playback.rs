use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::fs::ServeFile;
use uuid::Uuid;

use alexandria_core::errors::DomainError;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/files/{uuid}/stream` — stream a File's bytes from disk
/// (UC-38 / FR-MP-01, FR-MP-02). Returns `200` with the whole file, or
/// `206` for a `Range` request, or `416` for an unsatisfiable range. Errors
/// are `400` (malformed uuid), `401` (unauthenticated), `404` (unknown
/// uuid), `409` (soft-deleted — restore via UC-07 first), `500` (the file
/// is marked missing or cannot be stat'd).
///
/// Not `/content`: that path is UC-32's text-content read and UC-33's
/// editor. `/stream` also describes the response better — a seekable byte
/// stream, not a JSON content document.
///
/// The core handler resolves and guards *before* any byte is written, so a
/// failure is always a clean JSON error envelope and never a truncated
/// `200`. `ServeFile` then does the Range work, which is why this surface
/// does not parse `Range` itself.
///
/// `mime` is not a direct dependency of this crate (only transitive, via
/// axum/tower-http), so `ServeFile::new` is used with its own extension-based
/// guess, and the response's `content-type` is then overwritten from
/// `source.mime_type` — the catalog's MIME table (Task 1) stays the single
/// source of truth rather than `ServeFile`'s guess.
pub async fn stream(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let source = state
        .services
        .playback_source_handler
        .resolve(uuid, &token)
        .await
        .map_err(ApiError)?;

    let service = ServeFile::new(&source.path);

    let mut response = service
        .oneshot(request)
        .await
        .map_err(|e| ApiError(DomainError::disk(e.to_string())))?
        .into_response();

    let content_type = HeaderValue::from_str(&source.mime_type)
        .map_err(|e| ApiError(DomainError::internal(format!("invalid mime type: {e}"))))?;
    response.headers_mut().insert("content-type", content_type);

    // Advertise seekability even on a full-file response, so a player knows
    // it may issue ranges (FR-MP-02).
    response
        .headers_mut()
        .insert("accept-ranges", HeaderValue::from_static("bytes"));

    Ok(response)
}
