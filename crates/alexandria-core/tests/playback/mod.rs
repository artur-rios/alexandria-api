//! F-10 integration — the playback handlers wired with real collaborators.
//!
//! `build_services` is exercised the way an embedder calls it: a real on-disk
//! SQLite database created and migrated by `migrate_database`, then
//! `Settings::default()`. There is no `alexandria-core`-local pool helper —
//! `tests/common` in this crate only holds hand-written repository fakes for
//! the non-playback feature areas, so this test builds its own pool.
//!
//! The assertion is deliberately not `Arc::strong_count(...) >= 1`, which
//! would pass even for a handler wired to the wrong collaborators (an
//! `Arc::new` always has a strong count of at least 1). Instead each handler
//! is actually called against the real, empty, migrated database with a
//! random UUID that cannot exist in it. That only returns `NotFound` if the
//! handler reached a real `SqliteCatalogRepository::find_by_uuid` backed by
//! this pool — i.e. the handler is genuinely constructed, reachable, and
//! talking to the catalog repository `build_services` gave it.

use alexandria_core::config::Settings;
use alexandria_core::errors::DomainError;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::build_services;
use uuid::Uuid;

#[tokio::test]
async fn given_built_services_when_inspected_then_playback_handlers_present() {
    // Arrange — a real, migrated on-disk database and the default settings.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(db_path.to_str().expect("path"))
        .await
        .expect("migrate");
    let settings = Settings::default();

    // Act
    let services = build_services(&settings, pool).await;
    let random_uuid = Uuid::new_v4();

    // Assert — each handler is reachable and talking to the real (empty)
    // pool: a random UUID resolves to `NotFound`, which only happens once the
    // catalog repository is actually queried.
    let source_result = services.playback_source_handler.resolve(random_uuid).await;
    assert!(matches!(source_result, Err(DomainError::NotFound)));

    let comic_result = services.comic_page_handler.read_page(random_uuid, 1).await;
    assert!(matches!(comic_result, Err(DomainError::NotFound)));

    let thumbnail_result = services.thumbnail_handler.thumbnail(random_uuid).await;
    assert!(matches!(thumbnail_result, Err(DomainError::NotFound)));
}

mod thumbnail_cache;
