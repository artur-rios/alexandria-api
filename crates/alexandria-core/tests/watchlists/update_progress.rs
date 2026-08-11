//! Unit tests for the UC-23 UpdateWatchProgressHandler (Testing
//! Specification §6). Each test exercises exactly the handler against trait
//! fakes — no real DB or auth service. Coverage follows §6.3: happy path
//! (both valid transitions, episode tracking), every invalid transition
//! (AF-01), not-found (AF-02, video not on the watchlist), and the
//! unauthorized branch (AF-03).

use alexandria_core::catalog::model::FileType;
use alexandria_core::errors::DomainError;
use alexandria_core::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use alexandria_core::watchlists::commands::create::CreateWatchlistHandler;
use alexandria_core::watchlists::commands::update_progress::{
    is_valid_transition, UpdateWatchProgressHandler,
};
use alexandria_core::watchlists::model::WatchState;

use crate::common::{existing_file, FakeAuth, FakeCatalogRepository, FakeWatchlistRepository};

const TOKEN: &str = "bearer-token";

fn handler(
    auth: FakeAuth,
    repo: FakeWatchlistRepository,
) -> UpdateWatchProgressHandler<FakeAuth, FakeWatchlistRepository> {
    UpdateWatchProgressHandler::new(auth, repo)
}

/// A watchlist with one linked (Pending) video, ready for a transition.
async fn seeded_pending(
    watchlist_repo: &FakeWatchlistRepository,
    catalog_repo: FakeCatalogRepository,
) -> (uuid::Uuid, uuid::Uuid) {
    let create_handler = CreateWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies", TOKEN)
        .await
        .expect("create watchlist");

    let video = existing_file("/videos/a.mp4", FileType::Video);
    let video_uuid = video.uuid;
    catalog_repo.seed(video);
    let add_handler =
        AddVideoToWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone(), catalog_repo);
    add_handler
        .add(watchlist.uuid, video_uuid, TOKEN)
        .await
        .expect("link video");

    (watchlist.uuid, video_uuid)
}

// ---------------- is_valid_transition ----------------

#[test]
fn given_forward_transitions_when_checked_then_valid() {
    assert!(is_valid_transition(
        WatchState::Pending,
        WatchState::Watching
    ));
    assert!(is_valid_transition(
        WatchState::Watching,
        WatchState::Watched
    ));
}

/// FR-WL-05: an owner watching a series reports episode after episode while
/// the state stays `Watching`. That is a `Watching` → `Watching` update, so
/// the self-edge has to be legal or per-episode tracking caps out at the two
/// writes that enter and leave the state.
#[test]
fn given_still_watching_when_checked_then_valid() {
    assert!(is_valid_transition(
        WatchState::Watching,
        WatchState::Watching
    ));
}

#[test]
fn given_backward_same_or_skipped_transitions_when_checked_then_invalid() {
    assert!(!is_valid_transition(
        WatchState::Watched,
        WatchState::Pending
    ));
    assert!(!is_valid_transition(
        WatchState::Watching,
        WatchState::Pending
    ));
    assert!(!is_valid_transition(
        WatchState::Watched,
        WatchState::Watching
    ));
    assert!(!is_valid_transition(
        WatchState::Pending,
        WatchState::Watched
    ));
    // The two self-edges that carry no progress. `Watching` → `Watching` is
    // deliberately absent — it is the per-episode edge FR-WL-05 needs, and
    // `given_still_watching_when_checked_then_valid` covers it.
    assert!(!is_valid_transition(
        WatchState::Pending,
        WatchState::Pending
    ));
    assert!(!is_valid_transition(
        WatchState::Watched,
        WatchState::Watched
    ));
}

// ---------------- Main flow ----------------

#[tokio::test]
async fn given_pending_when_updated_to_watching_then_state_updated() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watching,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("update");

    assert_eq!(result.state, WatchState::Watching);
}

#[tokio::test]
async fn given_watching_when_updated_to_watched_then_state_updated() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);
    h.update(
        watchlist_uuid,
        video_uuid,
        WatchState::Watching,
        None,
        None,
        TOKEN,
    )
    .await
    .expect("first update");

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watched,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("second update");

    assert_eq!(result.state, WatchState::Watched);
}

#[tokio::test]
async fn given_series_episode_when_updated_then_episode_recorded() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watching,
            Some(3),
            Some(12),
            TOKEN,
        )
        .await
        .expect("update");

    assert_eq!(result.current_episode, Some(3));
    assert_eq!(result.total_episodes, Some(12));
}

/// FR-WL-05 end to end: a 12-episode series is watched episode by episode.
/// Every step after the first is a `Watching` → `Watching` update — the shape
/// per-episode tracking actually takes — and each one must be accepted and
/// must record its own episode.
#[tokio::test]
async fn given_series_watched_episode_by_episode_then_each_episode_recorded() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    for episode in 1..=11 {
        let result = h
            .update(
                watchlist_uuid,
                video_uuid,
                WatchState::Watching,
                Some(episode),
                Some(12),
                TOKEN,
            )
            .await
            .unwrap_or_else(|e| panic!("episode {episode} rejected: {e}"));
        assert_eq!(result.current_episode, Some(episode));
        assert_eq!(result.state, WatchState::Watching);
    }

    // The last episode finishes the series, which is the forward edge.
    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watched,
            Some(12),
            Some(12),
            TOKEN,
        )
        .await
        .expect("finish the series");

    assert_eq!(result.state, WatchState::Watched);
    assert_eq!(result.current_episode, Some(12));
}

#[tokio::test]
async fn given_episode_fields_omitted_when_updated_then_cleared_not_left_untouched() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);
    h.update(
        watchlist_uuid,
        video_uuid,
        WatchState::Watching,
        Some(3),
        Some(12),
        TOKEN,
    )
    .await
    .expect("first update");

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watched,
            None,
            None,
            TOKEN,
        )
        .await
        .expect("second update");

    assert_eq!(result.current_episode, None);
    assert_eq!(result.total_episodes, None);
}

// ---------------- AF-01: invalid transition ----------------

#[tokio::test]
async fn given_backward_transition_when_updated_then_invalid_state() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Watched,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_resubmitted_pending_when_updated_then_invalid_state() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let catalog_repo = FakeCatalogRepository::new();
    let (watchlist_uuid, video_uuid) = seeded_pending(&watchlist_repo, catalog_repo).await;
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h
        .update(
            watchlist_uuid,
            video_uuid,
            WatchState::Pending,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::InvalidState)));
}

// ---------------- AF-02: not found ----------------

#[tokio::test]
async fn given_video_not_on_watchlist_when_updated_then_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let create_handler = CreateWatchlistHandler::new(FakeAuth::Allowing, watchlist_repo.clone());
    let watchlist = create_handler
        .create("Weekend movies", TOKEN)
        .await
        .expect("create watchlist");
    let h = handler(FakeAuth::Allowing, watchlist_repo);

    let result = h
        .update(
            watchlist.uuid,
            uuid::Uuid::new_v4(),
            WatchState::Watching,
            None,
            None,
            TOKEN,
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotFound)));
}

// ---------------- AF-03: unauthorized ----------------

#[tokio::test]
async fn given_unauthenticated_when_updated_then_unauthorized() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h
        .update(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            WatchState::Watching,
            None,
            None,
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_progress_when_updated_then_unauthorized_not_not_found() {
    let watchlist_repo = FakeWatchlistRepository::new();
    let h = handler(FakeAuth::Denying, watchlist_repo);

    let result = h
        .update(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            WatchState::Watching,
            None,
            None,
            "",
        )
        .await;

    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
