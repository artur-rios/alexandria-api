use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use alexandria_core::errors::DomainError;

/// A `DomainError` rendered as this surface's error response: the mapped status
/// plus a `{"error": …}` body. Every failure path on `/v1` goes through it, so
/// clients see one envelope regardless of which layer rejected the request.
#[derive(Debug)]
pub struct ApiError(pub DomainError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            DomainError::NotFound => (StatusCode::NOT_FOUND, "not found"),
            DomainError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            DomainError::InvalidInput(msg) => (StatusCode::BAD_REQUEST, msg.as_str()),
            DomainError::InvalidState => (StatusCode::CONFLICT, "invalid state"),
            DomainError::Disk(_) => (StatusCode::INTERNAL_SERVER_ERROR, "disk error"),
            DomainError::Database(_) | DomainError::Migration(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "database error")
            }
            DomainError::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "configuration error"),
            DomainError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };

        let body = Json(json!({ "error": message }));
        (status, body).into_response()
    }
}
