//! Running an enrichment pass (music enrichment design).

use alexandria_core::config::MetadataSettings;
use alexandria_core::enrichment::commands::EnrichHandler;
use alexandria_core::enrichment::model::{EnrichmentOutcome, EnrichmentScope};
use alexandria_core::enrichment::providers::MIN_ARTIST_SCORE;
use alexandria_core::errors::DomainError;

use crate::common::FakeAuth;
use crate::enrichment_fixtures::*;

/// Assemble the handler over whatever fakes a test wants.
macro_rules! handler {
    ($repo:expr, $identity:expr, $images:expr, $lyrics:expr, $store:expr, $settings:expr) => {
        EnrichHandler::new(
            FakeAuth::Allowing,
            $repo,
            $identity,
            $images,
            $lyrics,
            $store,
            FixedClock,
            $settings,
        )
    };
}

#[tokio::test]
async fn given_many_tracks_by_one_artist_when_enriched_then_the_artist_is_looked_up_once() {
    // A library is a small number of artists across many tracks. Without the
    // per-run guard an album of twelve tracks asks MusicBrainz twelve times
    // for one artist -- twelve seconds at the rate limit, for one answer.
    let repo = FakeEnrichmentRepository::with_candidates(vec![
        candidate("So What", "Miles Davis"),
        candidate("Freddie Freeloader", "Miles Davis"),
        candidate("Blue in Green", "Miles Davis"),
    ]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let handler = handler!(
        repo.clone(),
        identity.clone(),
        FakeImages::with_image(),
        FakeLyrics::with_text(),
        FakeImageStore::default(),
        available_settings()
    );

    let report = handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    assert_eq!(
        identity.ask_count(),
        1,
        "the artist was looked up per track"
    );
    assert_eq!(
        repo.stored_image("Miles Davis").map(|image| image.outcome),
        Some(EnrichmentOutcome::Found)
    );
    // Three tracks' lyrics, one artist image.
    assert_eq!(report.found, 4);
}

#[tokio::test]
async fn given_a_low_scoring_match_when_enriched_then_it_is_rejected_not_used() {
    // MusicBrainz answers a misspelled name with a low-scoring hit rather
    // than an empty list. A confidently wrong face on an artist page is
    // worse than a blank one, so the score decides.
    let repo =
        FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Daviss")]);
    let handler = handler!(
        repo.clone(),
        FakeIdentity::matching("mb-wrong", "Miles Davis", MIN_ARTIST_SCORE - 1),
        FakeImages::with_image(),
        FakeLyrics::with_nothing(),
        FakeImageStore::default(),
        available_settings()
    );

    handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    let stored = repo.stored_image("Miles Daviss").expect("a row");
    assert_eq!(stored.outcome, EnrichmentOutcome::Rejected);
    assert!(
        stored.image_path.is_none(),
        "a rejected match stored an image"
    );
    // The id is kept even so, which is what makes a wrong-looking library
    // explainable rather than only observable.
    assert_eq!(stored.mbid.as_deref(), Some("mb-wrong"));
}

#[tokio::test]
async fn given_a_track_with_no_lyrics_when_enriched_then_the_outcome_is_recorded() {
    // "No lyrics" and "never looked" must be different rows, or every run
    // re-asks a question already answered no.
    let track = candidate("So What", "Miles Davis");
    let file_uuid = track.file_uuid;
    let repo = FakeEnrichmentRepository::with_candidates(vec![track]);
    let handler = handler!(
        repo.clone(),
        FakeIdentity::matching("mb-1", "Miles Davis", 100),
        FakeImages::with_nothing(),
        FakeLyrics::with_nothing(),
        FakeImageStore::default(),
        available_settings()
    );

    handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    let stored = repo.stored_lyrics(file_uuid).expect("a row");
    assert_eq!(stored.outcome, EnrichmentOutcome::NotFound);
    assert!(
        stored.outcome.is_settled(),
        "a no was left open to re-asking"
    );
}

#[tokio::test]
async fn given_a_settled_outcome_when_a_pending_run_repeats_then_it_is_not_re_asked() {
    // Resumability, from the caller's side: a second pass over a library
    // already enriched must not spend the rate limit re-asking.
    let track = candidate("So What", "Miles Davis");
    let repo = FakeEnrichmentRepository::with_candidates(vec![track]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let lyrics = FakeLyrics::with_nothing();

    let first = handler!(
        repo.clone(),
        identity.clone(),
        FakeImages::with_image(),
        lyrics.clone(),
        FakeImageStore::default(),
        available_settings()
    );
    first
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("first run");

    let second = handler!(
        repo.clone(),
        identity.clone(),
        FakeImages::with_image(),
        lyrics.clone(),
        FakeImageStore::default(),
        available_settings()
    );
    let report = second
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("second run");

    assert_eq!(identity.ask_count(), 1, "the artist was asked twice");
    assert_eq!(lyrics.ask_count(), 1, "the lyrics were asked twice");
    assert_eq!(report.skipped, 2, "nothing was skipped on the second pass");
}

#[tokio::test]
async fn given_a_provider_that_is_down_when_enriched_then_the_run_continues() {
    // Design section 5: a service being down is ordinary. A run that aborted
    // on the first unreachable host would never get through a library.
    let repo = FakeEnrichmentRepository::with_candidates(vec![
        candidate("So What", "Miles Davis"),
        candidate("Blue Train", "John Coltrane"),
    ]);
    let handler = handler!(
        repo.clone(),
        FakeIdentity::unreachable(),
        FakeImages::with_image(),
        FakeLyrics::unreachable(),
        FakeImageStore::default(),
        available_settings()
    );

    let report = handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("a failing provider failed the whole run");

    // Two artists and two tracks, all failed, none of it fatal.
    assert_eq!(report.failed, 4);
    assert_eq!(
        repo.stored_image("Miles Davis").map(|image| image.outcome),
        Some(EnrichmentOutcome::Failed)
    );
    // And a failure is the one outcome a later run asks again.
    assert!(!EnrichmentOutcome::Failed.is_settled());
}

#[tokio::test]
async fn given_the_image_cannot_be_written_when_enriched_then_it_is_retried_later() {
    // A full disk is temporary. Recording this as NotFound would permanently
    // deny an artist a photograph that exists.
    let repo = FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Davis")]);
    let handler = handler!(
        repo.clone(),
        FakeIdentity::matching("mb-1", "Miles Davis", 100),
        FakeImages::with_image(),
        FakeLyrics::with_nothing(),
        FakeImageStore::failing(),
        available_settings()
    );

    handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    let stored = repo.stored_image("Miles Davis").expect("a row");
    assert_eq!(stored.outcome, EnrichmentOutcome::Failed);
    assert!(!stored.outcome.is_settled());
}

#[tokio::test]
async fn given_a_track_with_no_artist_when_enriched_then_nothing_is_asked() {
    // Searching on nothing returns whatever is most popular. Skipped rather
    // than asked blindly.
    let mut track = candidate("Untitled", "Miles Davis");
    track.artist = None;
    track.album_artist = None;
    let repo = FakeEnrichmentRepository::with_candidates(vec![track]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let lyrics = FakeLyrics::with_text();
    let handler = handler!(
        repo.clone(),
        identity.clone(),
        FakeImages::with_image(),
        lyrics.clone(),
        FakeImageStore::default(),
        available_settings()
    );

    let report = handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    assert_eq!(identity.ask_count(), 0);
    assert_eq!(lyrics.ask_count(), 0, "lyrics were searched with no artist");
    assert_eq!(report.skipped, 2);
}

#[tokio::test]
async fn given_a_track_when_lyrics_are_searched_then_the_duration_is_sent() {
    // Duration is what tells a radio edit from an album cut. Without it a
    // provider answers with whichever version it happens to hold.
    let repo = FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Davis")]);
    let lyrics = FakeLyrics::with_text();
    let handler = handler!(
        repo,
        FakeIdentity::matching("mb-1", "Miles Davis", 100),
        FakeImages::with_image(),
        lyrics.clone(),
        FakeImageStore::default(),
        available_settings()
    );

    handler
        .enrich(EnrichmentScope::Pending, "token")
        .await
        .expect("run");

    let query = lyrics.last_query().expect("a query");
    assert_eq!(query.duration_seconds, Some(545));
    assert_eq!(query.title, "So What");
}

#[tokio::test]
async fn given_enrichment_is_disabled_when_a_run_starts_then_it_is_refused() {
    // The shipped default. Nothing is read and nothing is asked.
    let repo = FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Davis")]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let handler = handler!(
        repo,
        identity.clone(),
        FakeImages::with_image(),
        FakeLyrics::with_text(),
        FakeImageStore::default(),
        MetadataSettings::default()
    );

    let outcome = handler.enrich(EnrichmentScope::Pending, "token").await;

    assert!(matches!(outcome, Err(DomainError::InvalidState)));
    assert_eq!(
        identity.ask_count(),
        0,
        "a disabled run reached the network"
    );
}

#[tokio::test]
async fn given_no_contact_when_a_run_starts_then_it_is_refused_naming_the_reason() {
    // MusicBrainz's terms require a contact in the User-Agent and they are
    // entitled to block clients that send none -- a block that would land on
    // everyone running this software. Refused here rather than risked there.
    let repo = FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Davis")]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let handler = handler!(
        repo,
        identity.clone(),
        FakeImages::with_image(),
        FakeLyrics::with_text(),
        FakeImageStore::default(),
        MetadataSettings {
            enabled: true,
            contact: "   ".to_string(),
            image_cache_dir: "artist-images".to_string(),
        }
    );

    let outcome = handler.enrich(EnrichmentScope::Pending, "token").await;

    match outcome {
        Err(DomainError::InvalidInput(message)) => {
            assert!(message.contains("contact"), "{message}");
        }
        other => panic!("expected an invalid-input refusal naming the contact, got {other:?}"),
    }
    assert_eq!(identity.ask_count(), 0);
}

#[tokio::test]
async fn given_a_denied_caller_when_a_run_starts_then_nothing_is_asked() {
    let repo = FakeEnrichmentRepository::with_candidates(vec![candidate("So What", "Miles Davis")]);
    let identity = FakeIdentity::matching("mb-1", "Miles Davis", 100);
    let handler = EnrichHandler::new(
        FakeAuth::Denying,
        repo,
        identity.clone(),
        FakeImages::with_image(),
        FakeLyrics::with_text(),
        FakeImageStore::default(),
        FixedClock,
        available_settings(),
    );

    let outcome = handler.enrich(EnrichmentScope::Pending, "token").await;

    assert!(matches!(outcome, Err(DomainError::Unauthorized)));
    assert_eq!(identity.ask_count(), 0);
}
