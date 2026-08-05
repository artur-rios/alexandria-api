#![deny(unsafe_code)]

pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, patch, post};
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

    // Every `/v1` route requires the owner's credentials. `route_layer` runs
    // the gate before the matched route's extractors, so an unauthenticated
    // caller is denied without its body or path ever being parsed (FR-AU-07).
    let v1 = Router::new()
        .route("/v1/index", post(routes::index::index))
        .route("/v1/index/refresh", post(routes::refresh::refresh))
        .route(
            "/v1/files/{uuid}/metadata",
            patch(routes::edit_metadata::edit_metadata),
        )
        .route("/v1/files/{uuid}/rename", post(routes::rename::rename))
        .route("/v1/files", get(routes::browse::list_files))
        .route(
            "/v1/files/{uuid}/content",
            get(routes::text_content::get_content),
        )
        .route("/v1/files/{uuid}", get(routes::browse::get_file))
        .route("/v1/files/{uuid}", delete(routes::delete_file::delete_file))
        .route("/v1/files/{uuid}/restore", post(routes::restore::restore))
        .route("/v1/collections", post(routes::collections::create))
        .route("/v1/collections/{uuid}", patch(routes::collections::rename))
        .route(
            "/v1/collections/{uuid}",
            delete(routes::collections::delete),
        )
        .route(
            "/v1/collections/{uuid}/items",
            post(routes::collections::add_items),
        )
        .route(
            "/v1/collections/{uuid}/items",
            get(routes::collections::list_items),
        )
        .route(
            "/v1/collections/{uuid}/items/{item_uuid}",
            delete(routes::collections::remove_item),
        )
        .route("/v1/bookmarks", post(routes::bookmarks::create))
        .route("/v1/bookmarks", get(routes::bookmarks::list))
        .route("/v1/bookmarks/{uuid}", patch(routes::bookmarks::update))
        .route("/v1/bookmarks/{uuid}", delete(routes::bookmarks::delete))
        .route(
            "/v1/bookmarks/{uuid}/restore",
            post(routes::bookmarks::restore),
        )
        .route("/v1/watchlists", post(routes::watchlists::create))
        .route("/v1/watchlists", get(routes::watchlists::list))
        .route(
            "/v1/watchlists/{uuid}/items",
            post(routes::watchlists::add_video),
        )
        .route(
            "/v1/watchlists/{uuid}/items/{video_uuid}",
            patch(routes::watchlists::update_progress),
        )
        .route(
            "/v1/watchlists/{uuid}/items/{video_uuid}",
            delete(routes::watchlists::remove_video),
        )
        .route("/v1/watchlists/{uuid}", delete(routes::watchlists::delete))
        .route("/v1/reading-lists", post(routes::reading_lists::create))
        .route(
            "/v1/reading-lists/{uuid}/items",
            post(routes::reading_lists::add_item),
        )
        .route("/v1/reading-lists", get(routes::reading_lists::list))
        .route(
            "/v1/reading-lists/{uuid}/items/{item_uuid}",
            patch(routes::reading_lists::update_progress),
        )
        .route(
            "/v1/reading-lists/{uuid}/items/{item_uuid}",
            delete(routes::reading_lists::remove_item),
        )
        .route(
            "/v1/reading-lists/{uuid}",
            delete(routes::reading_lists::delete),
        )
        .route_layer(from_fn_with_state(
            state.clone(),
            middleware::auth::require_auth,
        ));

    // `/health` is deliberately outside the gate — it reports reachability to
    // an operator or orchestrator that holds no catalog credentials.
    Router::new()
        .route("/health", get(routes::health::health))
        .merge(v1)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
