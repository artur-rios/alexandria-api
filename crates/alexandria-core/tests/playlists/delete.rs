//! Integration test for `DeletePlaylistHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::delete::DeletePlaylistHandler;
use alexandria_core::playlists::repos::PlaylistRepository;
use sqlx::Row;
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{create_playlist, insert_audio_file, repo_with_pool};

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

#[tokio::test]
async fn given_a_playlist_with_entries_when_deleted_then_its_entries_go_too() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let track = insert_audio_file(&catalog_repo, "a.flac").await;
    repo.add_entries(playlist.uuid, &[track])
        .await
        .expect("added entry");

    DeletePlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .delete(playlist.uuid, "token")
        .await
        .expect("deleted");

    let count: i64 = sqlx::query("SELECT COUNT(*) AS count FROM playlist_entries")
        .fetch_one(&pool)
        .await
        .expect("count entries")
        .try_get("count")
        .expect("count column");
    assert_eq!(count, 0, "deleting a playlist must delete its entries too");
}
