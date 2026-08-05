//! Unit tests for the UC-25 DeleteWatchlistHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path (with and
//! without linked videos), not-found (AF-01), and the unauthorized branch
//! (AF-02).

use alexandria_core::catalog::model::FileType;
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::commands::create::CreateWatchlistHandler;
use alexandria_core::watchlists::commands::delete::DeleteWatchlistHandler;
use alexandria_core::watchlists::repos::WatchlistRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeWatchlistRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeWatchlistRepository,
) -> DeleteWatchlistHandler<FakeAuth, FakeWatchlistRepository> {
    DeleteWatchlistHandler::new(auth, repo)
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_empty_watchlist_when_deleted_then_predelete_record_returned_and_row_removed() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let create_handler = CreateWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies", TOKEN)
        .await
        .expect("create watchlist");
    let h = handler(FakeAuth::Allowing, watchlist_repo.clone());

    let result = h.delete(watchlist.uuid, TOKEN).await.expect("delete");

    assert_eq!(result, watchlist);
    assert!(watchlist_repo
        .find_by_uuid(watchlist.uuid)
        .await
        .expect("find")
        .is_none());
}

#[tokio::test]
async fn given_watchlist_with_linked_video_when_deleted_then_progress_gone_and_video_preserved() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let create_handler = CreateWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies", TOKEN)
        .await
        .expect("create watchlist");

    let catalog_repo = FakeCatalogRepository::new();
    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video.clone());
    let add_handler = AddVideoToWatchlistHandler::new(
        FakeAuth::Allowing,
        watchlist_repo.clone(),
        catalog_repo.clone(),
    );
    add_handler
        .add(watchlist.uuid, video_uuid, TOKEN)
        .await
        .expect("link video");

    let h = handler(FakeAuth::Allowing, watchlist_repo.clone());

    h.delete(watchlist.uuid, TOKEN).await.expect("delete");

    assert!(watchlist_repo
        .find_progress(watchlist.uuid, video_uuid)
        .await
        .expect("find progress")
        .is_none());
    assert!(
        catalog_repo
            .find_by_uuid(video_uuid)
            .await
            .expect("find video")
            .is_some(),
        "the VideoFile itself is preserved"
    );
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_uuid_when_deleted_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h.delete(uuid::Uuid::new_v4(), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_deleted_then_unauthorized() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h.delete(uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_deleted_then_unauthorized_not_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h.delete(uuid::Uuid::new_v4(), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
