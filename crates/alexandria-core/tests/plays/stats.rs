//! The rankings: what "played most" comes out as, and what an untagged or
//! disputed track does to them.

use alexandria_core::errors::DomainError;
use alexandria_core::plays::queries::stats::{MusicStatsHandler, DEFAULT_LIMIT, MAX_LIMIT};
use chrono::{TimeZone, Utc};

use crate::common::FakeAuth;
use crate::plays_fixtures::{insert_track, record_plays, repos_with_pool, Tags};

#[tokio::test]
async fn given_nothing_played_when_stats_are_read_then_the_summary_is_empty() {
    let (plays, _catalog, _pool, _dir) = repos_with_pool().await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    assert_eq!(read.total_plays, 0);
    assert_eq!(read.distinct_tracks, 0);
    // Not the epoch, and not "now": nothing has been played, so there is no
    // period for the numbers to cover.
    assert_eq!(read.first_played_at, None);
    assert_eq!(read.last_played_at, None);
    assert!(read.top_tracks.is_empty());
    assert!(read.top_artists.is_empty());
    assert!(read.top_albums.is_empty());
    assert!(read.top_genres.is_empty());
}

#[tokio::test]
async fn given_plays_across_tracks_when_stats_are_read_then_tracks_rank_by_count() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let often = insert_track(
        &catalog,
        "often.flac",
        Tags {
            title: Some("Often"),
            artist: Some("Ada"),
            album: Some("First"),
            ..Tags::default()
        },
    )
    .await;
    let seldom = insert_track(
        &catalog,
        "seldom.flac",
        Tags {
            title: Some("Seldom"),
            artist: Some("Ada"),
            album: Some("First"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, often, 3).await;
    record_plays(&plays, seldom, 1).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    assert_eq!(read.total_plays, 4);
    assert_eq!(read.distinct_tracks, 2);
    assert_eq!(
        read.top_tracks
            .iter()
            .map(|t| (t.title.as_str(), t.plays))
            .collect::<Vec<_>>(),
        vec![("Often", 3), ("Seldom", 1)]
    );
    // The tags are read live off the catalog, not copied onto the play rows.
    assert_eq!(read.top_tracks[0].artist.as_deref(), Some("Ada"));
    assert_eq!(read.top_tracks[0].album.as_deref(), Some("First"));
    // The newest of that track's plays, not the oldest and not just any.
    assert_eq!(
        read.top_tracks[0].last_played_at,
        Utc.with_ymd_and_hms(2026, 9, 3, 12, 2, 0).unwrap()
    );
    // One artist, two of their tracks played, four plays between them —
    // `tracks` is what tells a deep catalogue apart from one song on repeat.
    assert_eq!(read.top_artists.len(), 1);
    assert_eq!(read.top_artists[0].artist, "Ada");
    assert_eq!(read.top_artists[0].plays, 4);
    assert_eq!(read.top_artists[0].tracks, 2);
}

#[tokio::test]
async fn given_a_track_with_an_album_artist_when_stats_are_read_then_that_is_the_credit() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let guest = insert_track(
        &catalog,
        "guest.flac",
        Tags {
            artist: Some("Guest Performer"),
            album_artist: Some("Ada"),
            album: Some("First"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, guest, 2).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    // Credited to the album artist, so a record's plays land on whose record
    // it is rather than on each guest in turn.
    assert_eq!(read.top_artists.len(), 1);
    assert_eq!(read.top_artists[0].artist, "Ada");
    assert_eq!(read.top_albums[0].artist.as_deref(), Some("Ada"));
    // The track's own row still shows the performer it names.
    assert_eq!(
        read.top_tracks[0].artist.as_deref(),
        Some("Guest Performer")
    );
}

#[tokio::test]
async fn given_an_album_whose_tracks_name_different_artists_when_read_then_it_has_no_single_one() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let one = insert_track(
        &catalog,
        "one.flac",
        Tags {
            artist: Some("Ada"),
            album: Some("Compilation"),
            ..Tags::default()
        },
    )
    .await;
    let two = insert_track(
        &catalog,
        "two.flac",
        Tags {
            artist: Some("Bruno"),
            album: Some("Compilation"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, one, 1).await;
    record_plays(&plays, two, 1).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    // One album holding both, rather than the same album split in two by
    // who happened to perform each track.
    assert_eq!(read.top_albums.len(), 1);
    assert_eq!(read.top_albums[0].album, "Compilation");
    assert_eq!(read.top_albums[0].plays, 2);
    // And no artist named for it: there is no single answer, and picking
    // one of the two would be picking a winner arbitrarily.
    assert_eq!(read.top_albums[0].artist, None);
    // The artists themselves still rank separately.
    assert_eq!(read.top_artists.len(), 2);
}

#[tokio::test]
async fn given_an_album_where_only_some_tracks_name_an_artist_when_read_then_that_one_is_named() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let credited = insert_track(
        &catalog,
        "credited.flac",
        Tags {
            artist: Some("Ada"),
            album: Some("First"),
            ..Tags::default()
        },
    )
    .await;
    let uncredited = insert_track(
        &catalog,
        "uncredited.flac",
        Tags {
            album: Some("First"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, credited, 1).await;
    record_plays(&plays, uncredited, 1).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    // Silence is not disagreement: every track that says anything says Ada.
    assert_eq!(read.top_albums[0].artist.as_deref(), Some("Ada"));
}

#[tokio::test]
async fn given_an_untagged_track_when_stats_are_read_then_it_ranks_by_filename_and_nowhere_else() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let untagged = insert_track(&catalog, "untitled.flac", Tags::default()).await;
    record_plays(&plays, untagged, 2).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    // It was played, so it counts and it is named — by the only name it has.
    assert_eq!(read.total_plays, 2);
    assert_eq!(read.top_tracks.len(), 1);
    assert_eq!(read.top_tracks[0].title, "untitled.flac");
    assert_eq!(read.top_tracks[0].artist, None);
    // And it invents nobody: an "unknown artist" at the top of the chart is
    // a bug, not a fact about what the owner listens to.
    assert!(read.top_artists.is_empty());
    assert!(read.top_albums.is_empty());
    assert!(read.top_genres.is_empty());
}

#[tokio::test]
async fn given_genres_when_stats_are_read_then_they_rank_by_count() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let jazz = insert_track(
        &catalog,
        "jazz.flac",
        Tags {
            genre: Some("Jazz"),
            ..Tags::default()
        },
    )
    .await;
    let folk = insert_track(
        &catalog,
        "folk.flac",
        Tags {
            genre: Some("Folk"),
            ..Tags::default()
        },
    )
    .await;
    record_plays(&plays, jazz, 3).await;
    record_plays(&plays, folk, 1).await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let read = handler.read(None, "token").await.expect("stats");

    assert_eq!(
        read.top_genres
            .iter()
            .map(|g| (g.genre.as_str(), g.plays))
            .collect::<Vec<_>>(),
        vec![("Jazz", 3), ("Folk", 1)]
    );
}

#[tokio::test]
async fn given_more_tracks_than_the_limit_when_stats_are_read_then_each_ranking_is_cut_to_it() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    for n in 0..(DEFAULT_LIMIT + 2) {
        let track = insert_track(
            &catalog,
            &format!("track-{n}.flac"),
            Tags {
                artist: Some("Ada"),
                genre: Some("Jazz"),
                ..Tags::default()
            },
        )
        .await;
        record_plays(&plays, track, 1).await;
    }
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let defaulted = handler.read(None, "token").await.expect("default");
    let asked = handler.read(Some(3), "token").await.expect("limit 3");

    assert_eq!(defaulted.top_tracks.len(), DEFAULT_LIMIT as usize);
    assert_eq!(asked.top_tracks.len(), 3);
    // The summary counts everything, not just the rows the ranking shows —
    // a chart cut to three is not the owner having played three tracks.
    assert_eq!(asked.total_plays, DEFAULT_LIMIT + 2);
    assert_eq!(asked.distinct_tracks, DEFAULT_LIMIT + 2);
}

#[tokio::test]
async fn given_a_limit_outside_the_range_when_stats_are_read_then_invalid_input() {
    let (plays, _catalog, _pool, _dir) = repos_with_pool().await;
    let handler = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    // Refused rather than clamped: a caller that asked for a thousand and
    // silently got a hundred would report the top hundred as the whole
    // answer.
    assert!(matches!(
        handler.read(Some(0), "token").await,
        Err(DomainError::InvalidInput(_))
    ));
    assert!(matches!(
        handler.read(Some(MAX_LIMIT + 1), "token").await,
        Err(DomainError::InvalidInput(_))
    ));
    assert!(handler.read(Some(MAX_LIMIT), "token").await.is_ok());
}

#[tokio::test]
async fn given_a_denying_auth_when_stats_are_read_then_unauthorized() {
    let (plays, _catalog, _pool, _dir) = repos_with_pool().await;
    let handler = MusicStatsHandler::new(FakeAuth::Denying, plays);

    // And before the limit is looked at: an unauthenticated caller must not
    // learn whether its query would have validated.
    assert!(matches!(
        handler.read(Some(0), "token").await,
        Err(DomainError::Unauthorized)
    ));
}
