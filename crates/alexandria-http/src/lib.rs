#![deny(unsafe_code)]

pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axum::middleware::from_fn;
use axum::routing::{get, patch, post};
use axum::Router;
use tower_http::trace::TraceLayer;

use alexandria_core::config::Settings;
use alexandria_core::services::Services;

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub services: Arc<Services>,
}

pub fn app(settings: Settings, services: Arc<Services>) -> Router {
    let state = AppState {
        settings: Arc::new(settings),
        services,
    };

    Router::new()
        .route("/health", get(routes::health::health))
        .route("/v1/index", post(routes::index::index))
        .route("/v1/index/refresh", post(routes::refresh::refresh))
        .route(
            "/v1/files/:uuid/metadata",
            patch(routes::edit_metadata::edit_metadata),
        )
        .route("/v1/files", get(routes::browse::list_files))
        .route("/v1/files/:uuid", get(routes::browse::get_file))
        .layer(from_fn(middleware::auth::auth_stub))
        .layer(from_fn(middleware::error::error_stub))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
