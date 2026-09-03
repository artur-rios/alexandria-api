//! Recording a play: the one write in the play history module.

use alexandria_core::catalog::clock::FixedClock;
use alexandria_core::errors::DomainError;
use alexandria_core::plays::commands::record::RecordPlayHandler;
use alexandria_core::plays::queries::stats::MusicStatsHandler;
use chrono::{TimeZone, Utc};
use uuid::Uuid;

use crate::common::FakeAuth;
use crate::plays_fixtures::{insert_text_file, insert_track, repos_with_pool, Tags};

#[tokio::test]
async fn given_an_audio_file_when_a_play_is_recorded_then_it_is_counted() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let track = insert_track(&catalog, "one.flac", Tags::default()).await;
    let handler = RecordPlayHandler::new(FakeAuth::Allowing, FixedClock(Utc::now()), plays.clone());
    let stats = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let recorded = handler.record(track, "token").await.expect("recorded");

    assert_eq!(recorded.file_uuid, track);
    let read = stats.read(None, "token").await.expect("stats");
    assert_eq!(read.total_plays, 1);
    assert_eq!(read.distinct_tracks, 1);
}

#[tokio::test]
async fn given_the_same_track_twice_when_recorded_then_both_plays_count() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let track = insert_track(&catalog, "one.flac", Tags::default()).await;
    let handler = RecordPlayHandler::new(FakeAuth::Allowing, FixedClock(Utc::now()), plays.clone());
    let stats = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    handler.record(track, "token").await.expect("first play");
    handler.record(track, "token").await.expect("second play");

    // Two plays of one track, not one deduplicated play: counting repeats is
    // the entire point.
    let read = stats.read(None, "token").await.expect("stats");
    assert_eq!(read.total_plays, 2);
    assert_eq!(read.distinct_tracks, 1);
    assert_eq!(read.top_tracks[0].plays, 2);
}

#[tokio::test]
async fn given_the_core_clock_when_a_play_is_recorded_then_the_core_stamps_it() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let track = insert_track(&catalog, "one.flac", Tags::default()).await;
    let moment = Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap();
    let handler = RecordPlayHandler::new(FakeAuth::Allowing, FixedClock(moment), plays.clone());
    let stats = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let recorded = handler.record(track, "token").await.expect("recorded");

    // The caller never says when — the handler asks the clock, and that is
    // the value every ranking aggregates over.
    assert_eq!(recorded.played_at, moment);
    let read = stats.read(None, "token").await.expect("stats");
    assert_eq!(read.first_played_at, Some(moment));
    assert_eq!(read.last_played_at, Some(moment));
}

#[tokio::test]
async fn given_a_denying_auth_when_a_play_is_recorded_then_unauthorized_and_nothing_is_written() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    let track = insert_track(&catalog, "one.flac", Tags::default()).await;
    let handler = RecordPlayHandler::new(FakeAuth::Denying, FixedClock(Utc::now()), plays.clone());
    let stats = MusicStatsHandler::new(FakeAuth::Allowing, plays);

    let outcome = handler.record(track, "token").await;

    assert!(matches!(outcome, Err(DomainError::Unauthorized)));
    let read = stats.read(None, "token").await.expect("stats");
    assert_eq!(read.total_plays, 0, "a denied call must not write a play");
}

#[tokio::test]
async fn given_an_unknown_uuid_when_a_play_is_recorded_then_not_found() {
    let (plays, _catalog, _pool, _dir) = repos_with_pool().await;
    let handler = RecordPlayHandler::new(FakeAuth::Allowing, FixedClock(Utc::now()), plays);

    let outcome = handler.record(Uuid::new_v4(), "token").await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_a_file_that_is_not_audio_when_a_play_is_recorded_then_invalid_input() {
    let (plays, catalog, _pool, _dir) = repos_with_pool().await;
    // The statistics are of music; a video's viewing is the watchlists'
    // business, with its own progress model.
    let notes = insert_text_file(&catalog, "notes.txt").await;
    let handler = RecordPlayHandler::new(FakeAuth::Allowing, FixedClock(Utc::now()), plays);

    let outcome = handler.record(notes, "token").await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
}
