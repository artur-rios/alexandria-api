//! What a purge does to the history of the track it removes.

use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::plays::queries::stats::MusicStatsHandler;

use crate::common::FakeAuth;
use crate::plays_fixtures::{insert_track, record_plays, repos_with_pool, Tags};

#[tokio::test]
async fn given_a_played_track_when_it_is_purged_then_its_plays_go_with_it() {
    let (plays, catalog, pool, _dir) = repos_with_pool().await;
    let leaving = insert_track(
        &catalog,
        "leaving.flac",
        Tags {
            artist: Some("Ada"),
            album: Some("First"),
            genre: Some("Jazz"),
            ..Tags::default()
        },
    )
    .await;
    let staying = insert_track(
        &catalog,
        "staying.flac",
        Tags {
            artist: Some("Bruno"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, leaving, 3).await;
    record_plays(&plays, staying, 1).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    catalog.purge(leaving).await.expect("purge");

    let read = handler.read(None, "token").await.expect("stats");
    // The purged track's plays are gone from the totals as well as from the
    // rankings. A row left behind would be a play nothing could name:
    // invisible in every list, still swelling the number beside them.
    assert_eq!(read.total_plays, 1);
    assert_eq!(read.distinct_tracks, 1);
    assert_eq!(read.top_tracks.len(), 1);
    assert_eq!(read.top_tracks[0].title, "staying.flac");
    assert_eq!(read.top_artists.len(), 1);
    assert_eq!(read.top_artists[0].artist, "Bruno");
    assert!(read.top_albums.is_empty());
    assert!(read.top_genres.is_empty());

    // And nothing is left in the table itself — the assertion above reads
    // through a JOIN, which an orphaned row would slip past.
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM play_events")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}
