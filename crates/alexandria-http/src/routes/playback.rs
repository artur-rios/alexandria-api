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

    let response = service
        .oneshot(request)
        .await
        .map_err(|e| ApiError(DomainError::disk(e.to_string())))?
        .into_response();

    finish_stream(response, uuid, &source.mime_type)
}

/// Turn what `ServeFile` produced into what this API promises.
///
/// The core handler stats the file first, so a file that was already gone is
/// a `Disk` error and a clean JSON envelope. That leaves a residual window:
/// the file can still vanish between the stat and `ServeFile`'s own open.
/// `ServeFile` answers that with its own bare `404` — no body, and nothing in
/// this API's `{"error": …}` shape — and stamping `content-type:
/// video/mp4` onto it would hand the client a response that is neither a
/// valid error nor valid video. It is converted into the same `Disk` error
/// the stat guard would have produced, so it flows through the existing
/// `ApiError` mapping and comes back as a proper JSON `500`. (`ServeFile`
/// also answers `404` for `PermissionDenied`, which belongs in the same
/// bucket: the bytes are on disk but unreadable.)
///
/// Playback headers are stamped only on the two statuses that actually carry
/// the file's bytes, `200` and `206`. A `416` or a `304` is `ServeFile`'s own
/// answer about the *request*, carries no body from the file, and has no
/// business claiming the file's content type.
fn finish_stream(
    mut response: Response,
    uuid: Uuid,
    mime_type: &str,
) -> Result<Response, ApiError> {
    use axum::http::StatusCode;

    if response.status() == StatusCode::NOT_FOUND {
        return Err(ApiError(DomainError::disk(format!(
            "file {uuid} could not be opened after it was stat'd"
        ))));
    }

    if !matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        return Ok(response);
    }

    let content_type = HeaderValue::from_str(mime_type)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    /// A response shaped like one `ServeFile` would have returned.
    fn serve_file_response(status: StatusCode) -> Response {
        Response::builder()
            .status(status)
            .header("content-type", "application/octet-stream")
            .body(Body::empty())
            .expect("build response")
    }

    #[test]
    fn given_file_vanished_after_stat_when_finished_then_disk_error() {
        // Arrange — the file was there when the core handler stat'd it and
        // gone by the time `ServeFile` opened it, so `ServeFile` produced its
        // own empty 404. The real race cannot be driven deterministically
        // from a test, but the decision it lands on is exactly this one.
        let response = serve_file_response(StatusCode::NOT_FOUND);

        // Act
        let result = finish_stream(response, Uuid::nil(), "video/mp4");

        // Assert — the same `Disk` error the stat guard raises, which the
        // `ApiError` mapping renders as a JSON 500. Not a bare 404 stamped
        // `video/mp4`.
        assert!(matches!(result, Err(ApiError(DomainError::Disk(_)))));
    }

    #[test]
    fn given_partial_content_when_finished_then_playback_headers_stamped() {
        // Arrange — a 206 does carry the file's bytes.
        let response = serve_file_response(StatusCode::PARTIAL_CONTENT);

        // Act
        let response = finish_stream(response, Uuid::nil(), "video/mp4").expect("206 passes");

        // Assert
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers().get("content-type").unwrap(), "video/mp4");
        assert_eq!(response.headers().get("accept-ranges").unwrap(), "bytes");
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
    }

    #[test]
    fn given_range_not_satisfiable_when_finished_then_passed_through_unstamped() {
        // Arrange — a 416 is `ServeFile`'s answer about the *request*; it
        // carries no bytes from the file and must not claim its MIME type.
        let response = serve_file_response(StatusCode::RANGE_NOT_SATISFIABLE);

        // Act
        let response = finish_stream(response, Uuid::nil(), "video/mp4").expect("416 passes");

        // Assert
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/octet-stream"
        );
    }
}
