//! Unit tests for the UC-21 BrowseWatchlistsHandler (Testing Specification
//! §6). Each test exercises exactly the handler against trait fakes — no
//! real DB or auth service. Coverage follows §6.3: happy path (all
//! watchlists, single watchlist), not-found (AF-01), and the unauthorized
//! branch (AF-02).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::model::{NewWatchlist, WatchState};
use alexandria_core::watchlists::queries::browse::BrowseWatchlistsHandler;
use alexandria_core::watchlists::repos::WatchlistRepository;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeWatchlistRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeWatchlistRepository,
) -> BrowseWatchlistsHandler<FakeAuth, FakeWatchlistRepository> {
    BrowseWatchlistsHandler::new(auth, repo)
}

async fn seeded_watchlist(repo: &FakeWatchlistRepository, name: &str) -> uuid::Uuid {
    let watchlist = repo
        .insert_watchlist(NewWatchlist {
            uuid: uuid::Uuid::new_v4(),
            name: name.to_string(),
        })
        .await
        .expect("seed watchlist");
    watchlist.uuid
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_no_filter_when_listed_then_every_watchlist_returned_with_progress() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let a_uuid = seeded_watchlist(&watchlist_repo, "A list").await;
    let b_uuid = seeded_watchlist(&watchlist_repo, "B list").await;

    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let add_handler =
        AddVideoToWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone(), catalog_repo);
    add_handler
        .add(a_uuid, video_uuid, TOKEN)
        .await
        .expect("link video");

    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h.list(None, TOKEN).await.expect("list");

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].uuid, a_uuid);
    assert_eq!(result[0].items.len(), 1);
    assert_eq!(result[0].items[0].video_uuid, video_uuid);
    assert_eq!(result[0].items[0].state, WatchState::Pending);
    assert_eq!(result[1].uuid, b_uuid);
    assert!(result[1].items.is_empty());
}

#[tokio::test]
async fn given_watchlist_uuid_when_listed_then_only_that_watchlist_returned() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let a_uuid = seeded_watchlist(&watchlist_repo, "A list").await;
    let _b_uuid = seeded_watchlist(&watchlist_repo, "B list").await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h.list(Some(a_uuid), TOKEN).await.expect("list");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].uuid, a_uuid);
}

#[tokio::test]
async fn given_no_watchlists_when_listed_then_empty_array() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h.list(None, TOKEN).await.expect("list");

    assert!(result.is_empty());
}

// ---------------- AF-01: not found ----------------

#[tokio::test]
async fn given_unknown_watchlist_uuid_when_listed_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h.list(Some(uuid::Uuid::new_v4()), TOKEN).await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-02: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_listed_then_unauthorized() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h.list(None, "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_listed_then_unauthorized_not_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h.list(Some(uuid::Uuid::new_v4()), "").await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
