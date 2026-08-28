//! Integration test for `DeletePlaylistHandler` against a real migrated
//! database (Testing Specification §6.4).
//!
//! `given_a_playlist_with_entries_when_deleted_then_its_entries_go_too` is
//! deliberately NOT here yet: it needs `PlaylistRepository::add_entries`,
//! which Task 3 adds. Task 3 appends that test to this file rather than it
//! being weakened into one that deletes an empty playlist -- a test named
//! for entries going too cannot fail for the reason it names if there were
//! never any entries to lose.

use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::delete::DeletePlaylistHandler;
use alexandria_core::playlists::repos::PlaylistRepository;
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{create_playlist, repo_with_pool};

#[tokio::test]
async fn given_a_playlist_when_deleted_then_it_is_gone() {
    let (repo, _pool, _dir) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;

    let deleted = DeletePlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .delete(playlist.uuid, "token")
        .await
        .expect("deleted");

    assert_eq!(deleted.uuid, playlist.uuid);
    assert!(repo
        .find_by_uuid(playlist.uuid)
        .await
        .expect("find")
        .is_none());
}

#[tokio::test]
async fn given_an_unknown_uuid_when_deleted_then_not_found() {
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = DeletePlaylistHandler::new(FakeAuth::Allowing, repo)
        .delete(Uuid::new_v4(), "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
