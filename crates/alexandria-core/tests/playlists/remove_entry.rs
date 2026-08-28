//! Integration test for `RemoveEntryHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::repos::SqliteCatalogRepository;
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::commands::remove_entry::RemoveEntryHandler;
use alexandria_core::playlists::repos::PlaylistRepository;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{create_playlist, insert_audio_file, repo_with_pool};

#[tokio::test]
async fn given_a_track_held_twice_when_one_entry_is_removed_then_the_other_stays() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&catalog_repo, "a.flac").await;
    let added = repo
        .add_entries(playlist.uuid, &[song, song])
        .await
        .expect("added");

    RemoveEntryHandler::new(FakeAuth::Allowing, repo.clone())
        .remove(playlist.uuid, added[0].id, "token")
        .await
        .expect("removed");

    let left = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, added[1].id, "the wrong entry was removed");
}

#[tokio::test]
async fn given_a_middle_entry_when_it_is_removed_then_positions_stay_contiguous() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let a = insert_audio_file(&catalog_repo, "a.flac").await;
    let b = insert_audio_file(&catalog_repo, "b.flac").await;
    let c = insert_audio_file(&catalog_repo, "c.flac").await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c])
        .await
        .expect("added");

    RemoveEntryHandler::new(FakeAuth::Allowing, repo.clone())
        .remove(playlist.uuid, added[1].id, "token")
        .await
        .expect("removed");

    let left = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(
        left.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        left.iter().map(|e| e.file_uuid).collect::<Vec<_>>(),
        vec![a, c]
    );
}

#[tokio::test]
async fn given_an_entry_of_another_playlist_when_removed_then_not_found() {
    // The entry id is global; without the playlist check, one playlist could
    // delete another's row.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let mine = create_playlist(&repo, "Mine").await;
    let theirs = create_playlist(&repo, "Theirs").await;
    let song = insert_audio_file(&catalog_repo, "a.flac").await;
    let added = repo.add_entries(theirs.uuid, &[song]).await.expect("added");

    let outcome = RemoveEntryHandler::new(FakeAuth::Allowing, repo.clone())
        .remove(mine.uuid, added[0].id, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
    assert_eq!(
        repo.list_entries(theirs.uuid).await.expect("listed").len(),
        1
    );
}

#[tokio::test]
async fn given_an_unknown_playlist_when_an_entry_is_removed_then_not_found() {
    use uuid::Uuid;

    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = RemoveEntryHandler::new(FakeAuth::Allowing, repo)
        .remove(Uuid::new_v4(), 1, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
