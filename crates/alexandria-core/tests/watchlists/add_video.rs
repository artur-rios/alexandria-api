//! Unit tests for the UC-22 AddVideoToWatchlistHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no real
//! DB. Coverage follows §6.3: happy path, the wrong-file-type failure (AF-01),
//! not-found (AF-02, watchlist and video), and idempotent re-add.

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::model::{NewWatchlist, WatchState};
use alexandria_core::watchlists::repos::WatchlistRepository;

use crate::common::{existing_file, FakeCatalogRepository, FakeWatchlistRepository};

fn handler(
    watchlist_repo: FakeWatchlistRepository,
    catalog_repo: FakeCatalogRepository,
) -> AddVideoToWatchlistHandler<FakeWatchlistRepository, FakeCatalogRepository> {
    AddVideoToWatchlistHandler::new(watchlist_repo, catalog_repo)
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
    let h = handler(watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, video_uuid).await.expect("add");

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
    let h = handler(watchlist_repo, catalog_repo);

    let first = h.add(watchlist_uuid, video_uuid).await.expect("first add");
    let second = h.add(watchlist_uuid, video_uuid).await.expect("second add");

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
    let notes = existing_file("/notes/a.txt", FileType::Text);
    let notes_uuid = notes.uuid;
    catalog_repo.seed(notes);
    let h = handler(watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, notes_uuid).await;

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
    let h = handler(watchlist_repo, catalog_repo);

    let result = h.add(uuid::Uuid::new_v4(), video_uuid).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_unknown_video_when_add_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let watchlist_uuid = seeded_watchlist(&watchlist_repo).await;
    let h = handler(watchlist_repo, catalog_repo);

    let result = h.add(watchlist_uuid, uuid::Uuid::new_v4()).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}
