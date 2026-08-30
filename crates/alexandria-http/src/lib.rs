#![deny(unsafe_code)]

pub mod middleware;
pub mod routes;

use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, patch, post, put};
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
        .route("/v1/settings", get(routes::settings::get))
        .route("/v1/index", post(routes::index::index))
        .route("/v1/index/refresh", post(routes::refresh::refresh))
        .route("/v1/index/runs", get(routes::runs::active_runs))
        .route("/v1/index/runs/{run_id}", get(routes::runs::run_status))
        .route(
            "/v1/index/runs/{run_id}/pause",
            post(routes::runs::pause_run),
        )
        .route(
            "/v1/index/runs/{run_id}/resume",
            post(routes::runs::resume_run),
        )
        .route(
            "/v1/index/runs/{run_id}/cancel",
            post(routes::runs::cancel_run),
        )
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
        .route(
            "/v1/files/{uuid}/content",
            put(routes::text_content::edit_content),
        )
        .route("/v1/files/{uuid}/stream", get(routes::playback::stream))
        .route(
            "/v1/files/{uuid}/pages/{page}",
            get(routes::playback::comic_page),
        )
        .route(
            "/v1/files/{uuid}/thumbnail",
            get(routes::playback::thumbnail),
        )
        .route("/v1/files/{uuid}", get(routes::browse::get_file))
        .route("/v1/files/{uuid}", delete(routes::delete_file::delete_file))
        .route("/v1/files/{uuid}/restore", post(routes::restore::restore))
        .route(
            "/v1/collections",
            get(routes::collections::list).post(routes::collections::create),
        )
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
        // Music enrichment (music enrichment design). A run is a POST
        // because it changes what the catalog holds; reading one track's
        // stored result is a plain GET and stays available even when
        // enrichment itself is switched off.
        // Libraries (libraries design). A folder browsed as a tree, whose
        // files are shown only there.
        .route(
            "/v1/libraries",
            post(routes::libraries::register).get(routes::libraries::list),
        )
        .route(
            "/v1/libraries/{uuid}",
            get(routes::libraries::browse)
                .patch(routes::libraries::move_root)
                .delete(routes::libraries::remove),
        )
        .route("/v1/enrichment/runs", post(routes::enrichment::run))
        .route(
            "/v1/enrichment/tracks/{uuid}",
            get(routes::enrichment::read_track),
        )
        .route(
            "/v1/playlists",
            post(routes::playlists::create).get(routes::playlists::list),
        )
        .route(
            "/v1/playlists/{uuid}",
            patch(routes::playlists::rename).get(routes::playlists::read),
        )
        .route("/v1/playlists/{uuid}", delete(routes::playlists::delete))
        .route(
            "/v1/playlists/{uuid}/entries",
            post(routes::playlists::add_entries),
        )
        .route(
            "/v1/playlists/{uuid}/entries/{entry_uuid}",
            delete(routes::playlists::remove_entry),
        )
        .route(
            "/v1/playlists/{uuid}/entries/{entry_uuid}/move",
            post(routes::playlists::move_entry),
        )
        .route_layer(from_fn_with_state(
            state.clone(),
            middleware::auth::require_auth,
        ));

    // `/health`, the local register, login, and credentials endpoints are
    // deliberately outside the gate: `/health` reports reachability to a
    // caller with no catalog credentials, registration is how the account
    // comes to exist at all (UC-41), and login is how a caller obtains
    // credentials in the first place (UC-34). Registration is safe ungated
    // because it succeeds only once (UC-41 AF-02); `/credentials` enforces
    // authentication in its own handler (UC-35).
    //
    // `/recovery/redeem` (UC-43) is the same kind of case: the code
    // presented *is* the credential, so it must be reachable by a caller who
    // cannot authenticate. `/account` and `/recovery/regenerate` (UC-44) are
    // routed here too but authenticate in their own handlers.
    //
    // `/auth/windows/login` (UC-45) is ungated for the same reason as
    // `/auth/local/login`: a caller has no session yet, which is the entire
    // point of the call.
    Router::new()
        .route("/health", get(routes::health::health))
        .route("/v1/auth/local/register", post(routes::auth::register))
        .route("/v1/auth/local/login", post(routes::auth::login))
        .route("/v1/auth/windows/login", post(routes::auth::windows_login))
        .route(
            "/v1/auth/local/credentials",
            post(routes::auth::set_credentials),
        )
        .route("/v1/auth/local/account", get(routes::auth::account))
        .route(
            "/v1/auth/local/recovery/redeem",
            post(routes::auth::redeem_recovery_code),
        )
        .route(
            "/v1/auth/local/recovery/regenerate",
            post(routes::auth::regenerate_recovery_codes),
        )
        .merge(v1)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
