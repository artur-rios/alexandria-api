use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use alexandria_core::auth::AuthService;
use alexandria_core::errors::DomainError;

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Reject unauthenticated callers before any route extractor runs.
///
/// Ordering matters: axum runs a handler's extractors (`Path`, `Json`) before
/// the handler body, so authenticating inside the handler let a malformed body
/// answer `422` and a non-UUID path answer `400` to a caller who had presented
/// no credentials at all. SRD §7 and FR-AU-07 say an unauthenticated call is
/// denied, full stop — it must not learn whether its payload parsed.
///
/// Applied as a `route_layer` over the `/v1` routes only, so `/health` stays
/// open. Handlers still call `AuthService` themselves: this is the transport
/// gate, and the domain check remains unit-testable against trait fakes.
pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let token = bearer_token(request.headers());
    match state.services.auth.authenticate(&token).await {
        Ok(_) => next.run(request).await,
        Err(err) => ApiError(err).into_response(),
    }
}

/// Map an extractor rejection to the project's error envelope.
///
/// axum's own rejections answer `422` with a bare-text body; every other
/// failure on this surface answers `{"error": …}`. A body or path the domain
/// cannot read is invalid input (`400`), which is also what the FFI surface
/// reports for the same payload (FR-FC-24 / NFR-09).
pub fn invalid_input(message: impl Into<String>) -> ApiError {
    ApiError(DomainError::InvalidInput(message.into()))
}
