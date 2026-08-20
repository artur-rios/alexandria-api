use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;

use alexandria_core::settings::SettingsView;

use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/settings` — report the client-relevant configuration (UC-47 /
/// FR-FC-30).
///
/// Today that is the soft-delete retention window, which the core enforces on
/// every restore and purge and published nowhere. A client that shows how long
/// a deleted record remains restorable had to assume the default, and an
/// assumption is wrong the moment an operator configures something else.
///
/// Returns `200` with the settings, or `401` (unauthenticated).
pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<SettingsView>), ApiError> {
    let token = bearer_token(&headers);

    let settings = state
        .services
        .get_settings_handler
        .get(&token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(settings)))
}
