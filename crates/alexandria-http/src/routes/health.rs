use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use alexandria_core::config::AuthMode;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub database: &'static str,
    pub filesystem: &'static str,
    #[serde(rename = "authMode")]
    pub auth_mode: &'static str,
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let auth_mode = match state.settings.auth.mode {
        AuthMode::External => "external",
        AuthMode::Local => "local",
    };

    let body = HealthResponse {
        status: "ok",
        database: "reachable",
        filesystem: "reachable",
        auth_mode,
    };

    Json(body)
}
