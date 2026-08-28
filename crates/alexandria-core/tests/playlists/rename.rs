//! Integration test for `RenamePlaylistHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::rename::RenamePlaylistHandler;
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{create_playlist, repo_with_pool};

#[tokio::test]
async fn given_a_playlist_when_renamed_then_the_new_name_is_stored() {
    let (repo, _pool, _dir) = repo_with_pool().await;
    let created = create_playlist(&repo, "Raod trip").await;

    let renamed = RenamePlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .rename(created.uuid, "Road trip", "token")
        .await
        .expect("renamed");

    assert_eq!(renamed.name, "Road trip");
    assert_eq!(
        renamed.uuid, created.uuid,
        "renaming must not mint a new uuid"
    );
}

#[tokio::test]
async fn given_a_blank_new_name_when_renamed_then_invalid_input() {
    let (repo, _pool, _dir) = repo_with_pool().await;
    let created = create_playlist(&repo, "Road trip").await;

    let outcome = RenamePlaylistHandler::new(FakeAuth::Allowing, repo)
        .rename(created.uuid, "  ", "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_an_unknown_uuid_when_renamed_then_not_found() {
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = RenamePlaylistHandler::new(FakeAuth::Allowing, repo)
        .rename(Uuid::new_v4(), "Road trip", "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
