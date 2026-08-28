//! Integration test for `ReorderPlaylistHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::reorder::ReorderPlaylistHandler;
use alexandria_core::playlists::repos::{PlaylistRepository, SqlitePlaylistRepository};
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{create_playlist, insert_four_audio_files, repo_with_pool};

async fn ordered_uuids(repo: &SqlitePlaylistRepository, playlist: Uuid) -> Vec<Uuid> {
    repo.list_entries(playlist)
        .await
        .expect("listed")
        .into_iter()
        .map(|e| e.file_uuid)
        .collect()
}

#[tokio::test]
async fn given_four_tracks_when_the_last_moves_to_the_front_then_the_rest_shift_down() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    let after = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[3].id, 0, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![d, a, b, c]);
    assert_eq!(
        after.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

#[tokio::test]
async fn given_four_tracks_when_the_first_moves_to_the_end_then_the_rest_shift_up() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[0].id, 3, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![b, c, d, a]);
}

#[tokio::test]
async fn given_an_entry_when_moved_to_where_it_already_is_then_nothing_changes() {
    // A drag that lands on the row it started from. Cheap to get wrong: an
    // implementation that removes then re-inserts can land it off by one.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[2].id, 2, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_an_index_past_the_end_when_moved_then_invalid_input() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    let outcome = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[0].id, 4, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_a_negative_index_when_moved_then_invalid_input() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    let outcome = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[0].id, -1, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![a, b, c, d]);
}
