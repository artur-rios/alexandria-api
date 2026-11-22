//! Unit tests for the UC-24 RemoveVideoFromWatchlistHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB. Coverage follows §6.3: happy path and not-found
//! (AF-01).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::commands::create::CreateWatchlistHandler;
use alexandria_core::watchlists::commands::remove_video::RemoveVideoFromWatchlistHandler;
use alexandria_core::watchlists::repos::WatchlistRepository;

use crate::common::{existing_file, FakeCatalogRepository, FakeWatchlistRepository};

fn handler(
    repo: FakeWatchlistRepository,
) -> RemoveVideoFromWatchlistHandler<FakeWatchlistRepository> {
    RemoveVideoFromWatchlistHandler::new(repo)
}

async fn seeded_linked(watchlist_repo: &FakeWatchlistRepository) -> (uuid::Uuid, uuid::Uuid) {
    let create_handler = CreateWatchlistHandler::new(watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies")
        .await
        .expect("create watchlist");

    let catalog_repo = FakeCatalogRepository::new();
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let add_handler = AddVideoToWatchlistHandler::new(watchlist_repo.clone(), catalog_repo);
    add_handler
        .add(watchlist.uuid, video_uuid)
        .await
        .expect("link video");

    (watchlist.uuid, video_uuid)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_linked_video_when_removed_then_confirmation_and_progress_gone() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_linked(&watchlist_repo).await;
    let h = handler(watchlist_repo.clone());

    let result = h.remove(watchlist_uuid, video_uuid).await.expect("remove");

    assert_eq!(result.watchlist_uuid, watchlist_uuid);
    assert_eq!(result.video_uuid, video_uuid);
    assert!(watchlist_repo
        .find_progress(watchlist_uuid, video_uuid)
        .await
        .expect("find")
        .is_none());
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_video_not_on_watchlist_when_removed_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let create_handler = CreateWatchlistHandler::new(watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies")
        .await
        .expect("create watchlist");
    let h = handler(watchlist_repo);

    let result = h.remove(watchlist.uuid, uuid::Uuid::new_v4()).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_already_removed_video_when_removed_again_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_linked(&watchlist_repo).await;
    let h = handler(watchlist_repo);
    h.remove(watchlist_uuid, video_uuid)
        .await
        .expect("first remove");

    let result = h.remove(watchlist_uuid, video_uuid).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}
