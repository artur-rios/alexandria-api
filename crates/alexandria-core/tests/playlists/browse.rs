//! Integration test for `BrowsePlaylistsHandler` against a real migrated
//! database (Testing Specification §6.4).

use alexandria_core::catalog::model::SubtypeMetadata;
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::errors::DomainError;
use alexandria_core::playlists::queries::browse::BrowsePlaylistsHandler;
use alexandria_core::playlists::repos::PlaylistRepository;
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::playlists_fixtures::{
    create_playlist, insert_audio_file, insert_four_audio_files, mark_file_missing, repo_with_pool,
};

#[tokio::test]
async fn given_a_playlist_when_read_then_its_tracks_come_back_in_position_order() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let [a, b, c, d] = insert_four_audio_files(&catalog_repo).await;
    let added = repo
        .add_entries(playlist.uuid, &[a, b, c, d])
        .await
        .expect("added");
    repo.move_entry(playlist.uuid, added[3].id, 0)
        .await
        .expect("moved");

    let view = BrowsePlaylistsHandler::new(FakeAuth::Allowing, repo)
        .read(playlist.uuid, "token")
        .await
        .expect("read");

    assert_eq!(
        view.entries
            .iter()
            .map(|t| t.file.file.uuid)
            .collect::<Vec<_>>(),
        vec![d, a, b, c],
        "a playlist must read back in the order it was arranged in"
    );
}

#[tokio::test]
async fn given_an_entry_whose_file_is_missing_when_read_then_it_is_present_and_flagged() {
    // Design section 5: a missing file stays in the list and is passed over.
    // Dropping it here would delete curation work invisibly, and would make
    // an unplugged drive look like an empty playlist.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&catalog_repo, "a.flac").await;
    repo.add_entries(playlist.uuid, &[song])
        .await
        .expect("added");
    mark_file_missing(&pool, song).await;

    let view = BrowsePlaylistsHandler::new(FakeAuth::Allowing, repo)
        .read(playlist.uuid, "token")
        .await
        .expect("read");

    assert_eq!(
        view.entries.len(),
        1,
        "the entry was dropped rather than flagged"
    );
    assert!(view.entries[0].missing);
}

#[tokio::test]
async fn given_an_unknown_uuid_when_read_then_not_found() {
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = BrowsePlaylistsHandler::new(FakeAuth::Allowing, repo)
        .read(Uuid::new_v4(), "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_a_track_appearing_twice_when_read_then_both_entries_carry_its_metadata() {
    // Pins the batching's dedup: `list_view` resolves each distinct file id
    // once, so a track appearing twice (playlist_entries carries no unique
    // constraint on (playlist_id, file_id)) must still attach to both
    // entries rather than only the first (or only the last, or neither).
    // Metadata is seeded so the assertion below can only pass if the fetched
    // row genuinely reached both entries -- an implementation that resolves
    // the repeat only once and forgets to reuse the result for the second
    // entry (e.g. `HashMap::remove` instead of a lookup that leaves the
    // entry in place) would leave one of the two `None`.
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Repeats").await;
    let song = insert_audio_file(&catalog_repo, "again.flac").await;
    catalog_repo
        .update_metadata(
            song,
            &SubtypeMetadata::Audio {
                title: Some("Again".into()),
                artist: Some("Repeat Offender".into()),
                album: None,
                year: None,
                genre: None,
                track: None,
                album_artist: None,
            },
        )
        .await
        .expect("write metadata");
    repo.add_entries(playlist.uuid, &[song, song])
        .await
        .expect("added");

    let view = BrowsePlaylistsHandler::new(FakeAuth::Allowing, repo)
        .read(playlist.uuid, "token")
        .await
        .expect("read");

    assert_eq!(view.entries.len(), 2);
    assert!(view.entries.iter().all(|t| t.file.file.uuid == song));
    for track in &view.entries {
        match &track.file.metadata {
            Some(SubtypeMetadata::Audio { title, artist, .. }) => {
                assert_eq!(title.as_deref(), Some("Again"));
                assert_eq!(artist.as_deref(), Some("Repeat Offender"));
            }
            other => panic!(
                "expected both entries of the repeated track to carry the batched \
                 audio metadata, got {other:?}"
            ),
        }
    }
}

#[tokio::test]
async fn given_playlists_when_listed_then_every_playlist_comes_back() {
    let (repo, _pool, _dir) = repo_with_pool().await;
    create_playlist(&repo, "Road trip").await;
    create_playlist(&repo, "Focus").await;

    let playlists = BrowsePlaylistsHandler::new(FakeAuth::Allowing, repo)
        .list("token")
        .await
        .expect("listed");

    assert_eq!(playlists.len(), 2);
}

// ---------------- FR-AU-07: auth is checked before the payload ----------------

#[tokio::test]
async fn given_unauthenticated_when_listed_then_unauthorized() {
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = BrowsePlaylistsHandler::new(FakeAuth::Denying, repo)
        .list("")
        .await;

    assert!(matches!(outcome, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_when_read_then_unauthorized() {
    let (repo, pool, _dir) = repo_with_pool().await;
    let catalog_repo = SqliteCatalogRepository::new(pool.clone());
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&catalog_repo, "a.flac").await;
    repo.add_entries(playlist.uuid, &[song])
        .await
        .expect("added");

    let outcome = BrowsePlaylistsHandler::new(FakeAuth::Denying, repo)
        .read(playlist.uuid, "")
        .await;

    assert!(matches!(outcome, Err(DomainError::Unauthorized)));
}

#[tokio::test]
async fn given_unauthenticated_and_unknown_uuid_when_read_then_unauthorized_not_not_found() {
    // Pins FR-AU-07's ordering: auth runs before the playlist lookup, so an
    // unauthenticated caller learns nothing about whether the uuid exists.
    // Swapping `read`'s first two statements (looking the playlist up
    // before authenticating) would pass every other test in this file but
    // leak playlist existence here -- this is the one that catches it.
    let (repo, _pool, _dir) = repo_with_pool().await;

    let outcome = BrowsePlaylistsHandler::new(FakeAuth::Denying, repo)
        .read(Uuid::new_v4(), "")
        .await;

    assert!(matches!(outcome, Err(DomainError::Unauthorized)));
}
