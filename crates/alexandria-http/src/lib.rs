#![deny(unsafe_code)]

pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axum::middleware::from_fn;
use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use alexandria_core::config::Settings;

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
}

pub fn app(settings: Settings) -> Router {
    let state = AppState {
        settings: Arc::new(settings),
    };

    Router::new()
        .route("/health", get(routes::health::health))
        .layer(from_fn(middleware::auth::auth_stub))
        .layer(from_fn(middleware::error::error_stub))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
