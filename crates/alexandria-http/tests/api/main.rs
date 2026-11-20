//! alexandria-http's integration tests, as one test binary.
//!
//! Each file here used to be its own top-level `tests/*.rs` target, so
//! `cargo test` compiled `common/mod.rs` and the whole router stack into 28
//! separate crates and linked 28 executables, every one carrying the full
//! dependency graph (sqlx, axum, ffmpeg). As modules of a single crate they
//! are compiled and linked once, and their tests share one harness that runs
//! them in parallel instead of one binary after another.
//!
//! Nothing here touches process-wide state (environment variables, the
//! working directory, a global tracing subscriber), which is what makes the
//! shared process safe. A test that needs such state belongs in its own
//! top-level file under `tests/`, as `alexandria-core`'s `config.rs` and
//! `browse_batching.rs` are.
//!
//! Run one area with a filter: `cargo test -p alexandria-http --test api catalog_api::`.

mod common;

mod audio_api;
mod auth_api;
mod auth_register_api;
mod bookmarks_api;
mod catalog_api;
mod collections_api;
mod collections_browse_api;
mod health;
mod libraries_api;
mod playback_comic_page_api;
mod playback_stream_api;
mod playback_thumbnail_api;
mod reading_lists_add_item_api;
mod reading_lists_api;
mod reading_lists_browse_api;
mod reading_lists_delete_api;
mod reading_lists_remove_item_api;
mod reading_lists_update_progress_api;
mod run_control_api;
mod run_status_api;
mod settings_api;
mod text_content_edit_api;
mod text_content_read_api;
mod watchlists_add_video_api;
mod watchlists_api;
mod watchlists_browse_api;
mod watchlists_delete_api;
mod watchlists_remove_video_api;
mod watchlists_update_progress_api;
