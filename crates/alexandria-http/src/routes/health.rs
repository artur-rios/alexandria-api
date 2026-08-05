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

/// UC-37 — Health check (IR-03, IR-04, IR-05). Probes the SQLite database
/// and the configured filesystem root on every call — this is a liveness +
/// readiness check, not a cached status, so it reflects the current state
/// even if reachability changed since the server started.
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let auth_mode = match state.settings.auth.mode {
        AuthMode::External => "external",
        AuthMode::Local => "local",
    };

    // AF-01: the database is unreachable.
    let database_reachable = sqlx::query("SELECT 1")
        .fetch_one(&state.services.pool)
        .await
        .is_ok();

    // AF-02: the configured filesystem root is unreachable.
    let filesystem_reachable = tokio::fs::metadata(&state.settings.filesystem.root)
        .await
        .is_ok_and(|metadata| metadata.is_dir());

    let status = if database_reachable && filesystem_reachable {
        "ok"
    } else {
        "degraded"
    };

    let body = HealthResponse {
        status,
        database: if database_reachable {
            "reachable"
        } else {
            "unreachable"
        },
        filesystem: if filesystem_reachable {
            "reachable"
        } else {
            "unreachable"
        },
        auth_mode,
    };

    Json(body)
}
