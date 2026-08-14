use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use alexandria_core::errors::{error_body, DomainError, ErrorClass};

/// A `DomainError` rendered as this surface's error response: the mapped status
/// plus the shared `{"error": …}` envelope. Every failure path on `/v1` goes
/// through it, so clients see one envelope regardless of which layer rejected
/// the request.
///
/// The body itself is built by the core's `error_body`, which the FFI surface
/// also uses — parity on the failure path (FR-FC-24, FR-AU-08, NFR-09) is then
/// a property of the code rather than of two `match` arms staying in step.
/// This layer's only job is turning the class into a status code.
#[derive(Debug)]
pub struct ApiError(pub DomainError);

/// The HTTP status for a transport-independent error class.
fn status_for(class: ErrorClass) -> StatusCode {
    match class {
        ErrorClass::NotFound => StatusCode::NOT_FOUND,
        ErrorClass::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorClass::BadRequest => StatusCode::BAD_REQUEST,
        ErrorClass::Conflict => StatusCode::CONFLICT,
        ErrorClass::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
        ErrorClass::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorClass::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (class, body) = error_body(&self.0);
        // Serialized through the core's own renderer rather than `Json(body)`
        // so the bytes are the same ones the FFI surface hands back, which is
        // what the parity test compares.
        let json = body.to_json();
        (
            status_for(class),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()
    }
}
