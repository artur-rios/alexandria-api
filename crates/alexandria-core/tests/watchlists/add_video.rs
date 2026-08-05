//! Unit tests for the UC-22 AddVideoToWatchlistHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path,
//! the wrong-file-type failure (AF-01), not-found (AF-02, watchlist and
//! video), the unauthorized branch (AF-03), and idempotent re-add.

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::model::{NewWatchlist, WatchState};
use alexandria_core::watchlists::repos::WatchlistRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeWatchlistRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    watchlist_repo: FakeWatchlistRepository,
    catalog_repo: FakeCatalogRepository,
) -> AddVideoToWatchlistHandler<FakeAuth, FakeWatchlistRepository, FakeCatalogRepository> {
    AddVideoToWatchlistHandler::new(auth, watchlist_repo, catalog_repo)
}

async fn seeded_watchlist(repo: &FakeWatchlistRepository) -> uuid::Uuid {
    let watchlist = repo
        .insert_watchlist(NewWatchlist {
            uuid: uuid::Uuid::new_v4(),
            name: "Weekend movies".to_string(),
        })
        .await
        .expect("seed watchlist");
    watchlist.uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_valid_video_when_add_then_pending_progress_returned() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let watchlist_uuid = seeded_watchlist(&watchlist_repo).await;
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let h = handler(FakeAuth::Allowing, watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, video_uuid, TOKEN).await.expect("add");

    assert_eq!(result.watchlist_uuid, watchlist_uuid);
    assert_eq!(result.video_uuid, video_uuid);
    assert_eq!(result.state, WatchState::Pending);
    assert_eq!(result.current_episode, None);
    assert_eq!(result.total_episodes, None);
}

#[tokio::test]
async fn given_already_linked_video_when_add_again_then_idempotent() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let watchlist_uuid = seeded_watchlist(&watchlist_repo).await;
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let h = handler(FakeAuth::Allowing, watchlist_repo, catalog_repo);

    let first = h
        .add(watchlist_uuid, video_uuid, TOKEN)
        .await
        .expect("first add");
    let second = h
        .add(watchlist_uuid, video_uuid, TOKEN)
        .await
        .expect("second add");

    assert_eq!(
        first, second,
        "re-adding returns the same progress, not reset"
    );
}

// ---------------- AF-01: invalid input (wrong file type) ----------------

#[tokio::test]
async fn given_non_video_file_when_add_then_invalid_input() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let watchlist_uuid = seeded_watchlist(&watchlist_repo).await;
    let audio = existing_file("/audio/a.mp3", FileType::Audio);
    let audio_uuid = audio.uuid;
    catalog_repo.seed(audio);
    let h = handler(FakeAuth::Allowing, watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, audio_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::InvalidInput(_))));
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_unknown_watchlist_when_add_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let h = handler(FakeAuth::Allowing, watchlist_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), video_uuid, TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_unknown_video_when_add_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let watchlist_uuid = seeded_watchlist(&watchlist_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, uuid::Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_add_then_unauthorized() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_watchlist_when_add_then_unauthorized_not_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
