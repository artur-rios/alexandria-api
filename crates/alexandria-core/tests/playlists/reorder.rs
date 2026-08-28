//! Integration test for `ReorderPlaylistHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::reorder::ReorderPlaylistHandler;
use alexandria_core::playlists::repos::{PlaylistRepository, SqlitePlaylistRepository};
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{
    create_playlist, insert_audio_file, insert_four_audio_files, repo_with_pool,
};

async fn ordered_uuids(repo: &SqlitePlaylistRepository, playlist: Uuid) -> Vec<Uuid> {
    repo.list_entries(playlist)
        .await
        .expect("listed")
        .into_iter()
        .map(|e| e.file_uuid)
        .collect()
}

/// The stored `position` of every entry, in the order `list_entries`
/// returns them (already `ORDER BY position`). `ordered_uuids` above
/// discards `position` once it has used it to sort, which is exactly why it
/// cannot catch a reorder that gets the *values* wrong while still landing
/// the right uuids in the right slots -- e.g. writing `1, 2, 3, 4` instead
/// of `0, 1, 2, 3`. `remove_entry.rs`'s contiguity test asserts stored
/// positions directly for the same reason; a move must too (design's
/// testing bullet: reorder puts the track where it was dropped, AND the
/// positions after it stay contiguous).
async fn ordered_positions(repo: &SqlitePlaylistRepository, playlist: Uuid) -> Vec<i64> {
    repo.list_entries(playlist)
        .await
        .expect("listed")
        .into_iter()
        .map(|e| e.position)
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
        .move_entry(playlist.uuid, added[3].uuid, 0, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![d, a, b, c]);
    assert_eq!(
        after.iter().map(|e| e.file_uuid).collect::<Vec<_>>(),
        vec![d, a, b, c],
        "the returned order should match what was actually persisted"
    );
    assert_eq!(
        ordered_positions(&repo, playlist.uuid).await,
        vec![0, 1, 2, 3],
        "positions must stay contiguous 0..n-1 after the move"
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
        .move_entry(playlist.uuid, added[0].uuid, 3, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![b, c, d, a]);
    assert_eq!(
        ordered_positions(&repo, playlist.uuid).await,
        vec![0, 1, 2, 3],
        "positions must stay contiguous 0..n-1 after the move"
    );
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
        .move_entry(playlist.uuid, added[2].uuid, 2, "token")
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
        .move_entry(playlist.uuid, added[0].uuid, 4, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_a_negative_index_when_moved_then_invalid_input() {
    // Note: this does not discriminate the `to_index < 0` guard on its own
    // -- `-1 as usize` wraps to a huge value that the `>= entries.len()`
    // arm alone would also reject with the same `InvalidInput`. It pins the
    // bounds guard as a whole (negative input is refused, order untouched),
    // not that specific arm; the guard is correct and worth keeping as a
    // statement of intent regardless.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");

    let outcome = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(playlist.uuid, added[0].uuid, -1, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_uuids(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_an_entry_of_another_playlist_when_moved_then_not_found() {
    // The entry uuid is global; without the playlist check, one playlist
    // could reorder using another's row -- mirrors
    // `remove_entry.rs`'s equivalent test for the same hazard.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let mine = create_playlist(&repo, "Mine").await;
    let theirs = create_playlist(&repo, "Theirs").await;
    let a = insert_audio_file(&catalog_repo, "a.flac").await;
    let b = insert_audio_file(&catalog_repo, "b.flac").await;
    let c = insert_audio_file(&catalog_repo, "c.flac").await;
    let added = repo
        .add_entries(theirs.uuid, &[a, b, c])
        .await
        .expect("added");

    let outcome = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo.clone())
        .move_entry(mine.uuid, added[0].uuid, 2, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
    assert_eq!(ordered_uuids(&repo, theirs.uuid).await, vec![a, b, c]);
}

#[tokio::test]
async fn given_an_unknown_playlist_when_an_entry_is_moved_then_not_found() {
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = ReorderPlaylistHandler::new(FakeAuth::Allowing, repo)
        .move_entry(Uuid::new_v4(), Uuid::new_v4(), 0, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
