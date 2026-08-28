//! Integration test for `AddEntriesHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::add_entries::AddEntriesHandler;
use alexandria_core::playlists::repos::PlaylistRepository;
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{
    create_playlist, insert_audio_file, insert_text_file, repo_with_pool,
};

#[tokio::test]
async fn given_an_empty_playlist_when_tracks_are_added_then_they_take_positions_from_zero() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let first = insert_audio_file(&catalog_repo, "a.flac").await;
    let second = insert_audio_file(&catalog_repo, "b.flac").await;

    let added = AddEntriesHandler::new(FakeAuth::Allowing, repo)
        .add(playlist.uuid, &[first, second], "token")
        .await
        .expect("added");

    assert_eq!(
        added.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn given_a_playlist_holding_a_track_when_the_same_track_is_added_then_it_is_held_twice() {
    // The whole reason `playlist_entries` has no UNIQUE (playlist_id,
    // file_id): a set can open and close with the same song. This test fails
    // the moment someone copies that constraint over from reading lists.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&catalog_repo, "a.flac").await;
    let handler = AddEntriesHandler::new(FakeAuth::Allowing, repo.clone());

    handler
        .add(playlist.uuid, &[song], "token")
        .await
        .expect("first");
    handler
        .add(playlist.uuid, &[song], "token")
        .await
        .expect("second");

    let entries = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].file_uuid, entries[1].file_uuid);
    assert_ne!(entries[0].id, entries[1].id, "each entry is its own row");
    assert_eq!(
        entries.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn given_a_non_audio_file_when_added_then_invalid_input() {
    // A playlist holds audio (design "What a playlist is here"). Video has
    // watchlists and books have reading lists.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let note = insert_text_file(&catalog_repo, "note.md").await;

    let outcome = AddEntriesHandler::new(FakeAuth::Allowing, repo)
        .add(playlist.uuid, &[note], "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_an_unknown_file_when_added_then_not_found_and_nothing_is_added() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let real = insert_audio_file(&catalog_repo, "a.flac").await;

    let outcome = AddEntriesHandler::new(FakeAuth::Allowing, repo.clone())
        .add(playlist.uuid, &[real, Uuid::new_v4()], "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
    assert!(
        repo.list_entries(playlist.uuid)
            .await
            .expect("listed")
            .is_empty(),
        "a partial add left the real track behind"
    );
}

#[tokio::test]
async fn given_an_unknown_playlist_when_tracks_are_added_then_not_found() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let track = insert_audio_file(&catalog_repo, "a.flac").await;

    let outcome = AddEntriesHandler::new(FakeAuth::Allowing, repo)
        .add(Uuid::new_v4(), &[track], "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
