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

/// Every playback response carries `X-Content-Type-Options: nosniff`.
///
/// These three routes are this API's only byte-serving surface, and what
/// they serve includes `text/html`, `multipart/related` and `image/svg+xml`
/// straight from the library. A library HTML or SVG file containing a script,
/// opened in a webview at its stream URL, would otherwise execute in the
/// API's origin. Impact is limited — auth is a Bearer header, so there is no
/// cookie for such a script to steal — but the header is one line and the
/// surface is new. `nosniff` also holds browsers to the catalog MIME table's
/// answer rather than letting them re-sniff the bytes.
const NOSNIFF: (axum::http::HeaderName, HeaderValue) = (
    axum::http::header::X_CONTENT_TYPE_OPTIONS,
    HeaderValue::from_static("nosniff"),
);

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

    response.headers_mut().insert(NOSNIFF.0, NOSNIFF.1);

    Ok(response)
}

/// `GET /v1/files/{uuid}/pages/{page}` — one page of a CBZ ComicBook
/// (UC-39 / FR-MP-04), 1-based. Returns `200` with the archive entry's own
/// bytes, undecoded. Errors are `400` (not a comic, not a CBZ, page out of
/// range, malformed path), `401`, `404`, `409`, `500`.
pub async fn comic_page(
    State(state): State<AppState>,
    path: Result<Path<(Uuid, u32)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = bearer_token(&headers);

    let Path((uuid, page)) =
        path.map_err(|_| invalid_input("path segments must be a UUID and a page number"))?;

    let page = state
        .services
        .comic_page_handler
        .read_page(uuid, page, &token)
        .await
        .map_err(ApiError)?;

    let content_type = HeaderValue::from_str(&page.mime_type)
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));

    Ok((
        [(axum::http::header::CONTENT_TYPE, content_type), NOSNIFF],
        page.bytes,
    )
        .into_response())
}

/// `GET /v1/files/{uuid}/thumbnail` — a downscaled JPEG for a video, image,
/// or comic (UC-40 / FR-MP-05). Errors are `400` (a type with no
/// thumbnail), `401`, `404`, `409`, `500`.
pub async fn thumbnail(
    State(state): State<AppState>,
    uuid: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = bearer_token(&headers);

    let Path(uuid) = uuid.map_err(|_| invalid_input("path segment is not a valid UUID"))?;

    let thumb = state
        .services
        .thumbnail_handler
        .thumbnail(uuid, &token)
        .await
        .map_err(ApiError)?;

    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("image/jpeg"),
            ),
            NOSNIFF,
        ],
        thumb.bytes,
    )
        .into_response())
}
