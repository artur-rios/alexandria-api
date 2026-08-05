use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use alexandria_core::auth::local::{LocalCredentialsResult, LocalLoginResult};

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// Request body shared by both local-auth endpoints (UC-34 / UC-35): the
/// owner's `email` and `password`. Both required.
#[derive(Debug, Deserialize)]
pub struct LocalCredentialsRequest {
    pub email: String,
    pub password: String,
}

/// `POST /v1/auth/local/login` — verify email + password against the
/// encrypted local credential row and create a session (UC-34 / FR-AU-04).
/// Deliberately outside the blanket `require_auth` gate: this is how a
/// caller obtains credentials in the first place. Returns `200` with the
/// `LocalLoginResult` (including the session id the caller presents on
/// subsequent requests), or `401` (wrong email/password, AF-02, or the
/// active auth mode is not local, AF-01), or `500` (local credentials have
/// not been set — configuration error, AF-03, run UC-35 first).
pub async fn login(
    State(state): State<AppState>,
    body: Result<Json<LocalCredentialsRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<LocalLoginResult>), ApiError> {
    let Json(request) = body.map_err(|err| invalid_input(format!("invalid login body: {err}")))?;

    let result = state
        .services
        .local_login_handler
        .login(&request.email, &request.password)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}

/// `POST /v1/auth/local/credentials` — set or change the local-login email
/// and password (UC-35 / FR-AU-05, FR-AU-06). Deliberately outside the
/// blanket `require_auth` gate: first-time setup has no credentials yet to
/// authenticate with. The handler itself enforces the conditional
/// authorization the use case calls for — unauthenticated is only
/// accepted when no credentials exist yet (AF-03). Returns `200` with the
/// `LocalCredentialsResult`, or `400` (invalid email or empty password,
/// AF-02, or a malformed body), `401` (credentials already exist and the
/// caller did not authenticate, AF-03), or `409` (the active auth mode is
/// not local login, AF-01).
pub async fn set_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<LocalCredentialsRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<LocalCredentialsResult>), ApiError> {
    let token = bearer_token(&headers);

    let Json(request) =
        body.map_err(|err| invalid_input(format!("invalid credentials body: {err}")))?;

    let result = state
        .services
        .set_local_credentials_handler
        .set(request.email, request.password, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(result)))
}
