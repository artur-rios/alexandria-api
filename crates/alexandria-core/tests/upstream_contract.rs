//! What the three services actually answer today.
//!
//! Every other test of enrichment stubs them, which is right: a suite that
//! called MusicBrainz on every push would be slow, would fail whenever
//! somebody else had an outage, and would spend a shared rate limit to
//! re-learn something it already knows. What stubs cannot tell us is whether
//! the real services still answer the shape this code parses. A field
//! renamed upstream, an endpoint moved, a response newly paginated — none of
//! that is visible until an owner's lookup quietly returns nothing.
//!
//! So these exist, and they are **`#[ignore]`d**: `cargo test` never runs
//! them. A scheduled, non-gating job does, weekly, and a red mark there is a
//! prompt to read the log rather than a broken build — the same contract
//! `performance (non-gating)` already has in this workspace.
//!
//! They refuse to run without `ALEXANDRIA_METADATA_CONTACT`. MusicBrainz
//! requires a contact in the `User-Agent` and is entitled to block clients
//! that send none; a test that ran anonymously would risk that block landing
//! on everyone using this software, which is exactly what the production
//! path refuses to do.

use alexandria_core::enrichment::providers::commons::CommonsImageClient;
use alexandria_core::enrichment::providers::lrclib::LrclibClient;
use alexandria_core::enrichment::providers::musicbrainz::MusicBrainzClient;
use alexandria_core::enrichment::providers::{
    ArtistIdentityProvider, ArtistImageProvider, LyricsProvider, LyricsQuery, ProviderError,
    RecordingIdentityProvider, MIN_ARTIST_SCORE,
};

/// The contact these tests identify themselves with, or `None` when the
/// environment did not supply one.
fn contact() -> Option<String> {
    std::env::var("ALEXANDRIA_METADATA_CONTACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Skips with a reason rather than passing silently, so an unset contact
/// reads as "not checked" in the log rather than as "checked and fine".
macro_rules! contact_or_skip {
    () => {
        match contact() {
            Some(contact) => contact,
            None => {
                eprintln!(
                    "SKIPPED: set ALEXANDRIA_METADATA_CONTACT to run the \
                     upstream contract checks; they are never run anonymously"
                );
                return;
            }
        }
    };
}

/// Awaits a provider call, or skips when the service simply did not answer.
///
/// The distinction this file turns on. `Unusable` means the response arrived
/// and did not fit what this code reads — that is drift, and noticing it is
/// the entire purpose here, so it still fails. `Unreachable` and
/// `RateLimited` mean no answer came: the service was busy, the runner's
/// shared address was throttled, the network was out. None of those are a
/// contract change, and none of them are caused by a commit.
///
/// Failing on them trains the reader to ignore a red mark on this job, which
/// costs exactly the signal the job exists to give. So they skip with a
/// reason, the same way an absent contact does — "not checked" is a state
/// this file already knows how to say.
///
/// Observed: MusicBrainz answered `503 Service Unavailable` to the first
/// request of a run while answering two others in the same run seconds
/// later.
macro_rules! reached_or_skip {
    ($call:expr, $service:literal) => {
        match $call.await {
            Ok(value) => value,
            Err(err @ ProviderError::Unusable(_)) => {
                panic!("{} answered something this cannot read: {err}", $service)
            }
            Err(err) => {
                eprintln!(
                    "SKIPPED: {} did not answer ({err}); that is not a \
                     contract change, so nothing was checked",
                    $service
                );
                return;
            }
        }
    };
}

/// A subject for the identity and image checks: unambiguous, and unlikely
/// to leave either catalogue.
const ARTIST: &str = "Miles Davis";
const TRACK: &str = "So What";
const ALBUM: &str = "Kind of Blue";

/// A different subject for the lyrics check, and the reason is not
/// incidental: "So What" is an instrumental. Asking a lyrics service for the
/// words to a jazz instrumental gets a legitimate "no", which would make
/// this check permanently green while proving nothing — it would answer the
/// same way if LRCLIB had been switched off entirely.
const SUNG_TRACK: &str = "Bohemian Rhapsody";
const SUNG_ARTIST: &str = "Queen";

#[tokio::test]
#[ignore = "reaches MusicBrainz; run by the scheduled contract job"]
async fn given_a_known_artist_when_searched_then_musicbrainz_still_answers_a_scored_match() {
    let contact = contact_or_skip!();
    let client = MusicBrainzClient::new(&contact).expect("client");

    let found = reached_or_skip!(client.find_artist(ARTIST), "MusicBrainz")
        .expect("no match at all for an artist they certainly hold");

    // The threshold this code actually gates on. If a well-known exact name
    // stops clearing it, the scoring or the query has moved and every
    // artist in a real library is about to stop resolving.
    assert!(
        found.score >= MIN_ARTIST_SCORE,
        "an exact name scored {} , below the {MIN_ARTIST_SCORE} this accepts",
        found.score
    );
    assert!(!found.mbid.is_empty(), "a match carried no id");
    eprintln!("musicbrainz artist: {} ({})", found.name, found.mbid);
}

#[tokio::test]
#[ignore = "reaches MusicBrainz; run by the scheduled contract job"]
async fn given_a_known_recording_when_searched_then_musicbrainz_still_answers_one() {
    let contact = contact_or_skip!();
    let client = MusicBrainzClient::new(&contact).expect("client");

    let query = LyricsQuery {
        title: TRACK.to_string(),
        artist: ARTIST.to_string(),
        album: Some(ALBUM.to_string()),
        duration_seconds: None,
    };

    let found = reached_or_skip!(client.find_recording(&query), "MusicBrainz")
        .expect("no recording at all for one they certainly hold");

    assert!(!found.mbid.is_empty());
    eprintln!("musicbrainz recording: {} ({})", found.mbid, found.score);
}

#[tokio::test]
#[ignore = "reaches Wikidata and Wikimedia Commons; run by the scheduled contract job"]
async fn given_a_known_artist_when_an_image_is_sought_then_commons_still_serves_one() {
    let contact = contact_or_skip!();

    // Resolved rather than hardcoded: the id is what the production path
    // hands this client, and an id that stopped resolving is the failure
    // this whole file exists to notice.
    let mbid = reached_or_skip!(
        MusicBrainzClient::new(&contact)
            .expect("client")
            .find_artist(ARTIST),
        "MusicBrainz"
    )
    .expect("no match")
    .mbid;

    let asset = reached_or_skip!(
        CommonsImageClient::new(&contact)
            .expect("client")
            .image_for(&mbid),
        "Wikidata or Commons"
    )
    .expect("no photograph for an artist Commons certainly has one of");

    assert!(!asset.bytes.is_empty(), "an image arrived with no bytes");
    assert!(
        asset.source_url.contains("Special:FilePath"),
        "the attribution url changed shape: {}",
        asset.source_url
    );
    eprintln!(
        "commons image: {} bytes, .{} from {}",
        asset.bytes.len(),
        asset.extension,
        asset.source_url
    );
}

#[tokio::test]
#[ignore = "reaches LRCLIB; run by the scheduled contract job"]
async fn given_a_known_recording_when_looked_up_then_lrclib_still_answers_lyrics() {
    let contact = contact_or_skip!();
    let client = LrclibClient::new(&contact).expect("client");

    let query = LyricsQuery {
        title: SUNG_TRACK.to_string(),
        artist: SUNG_ARTIST.to_string(),
        album: None,
        // Deliberately omitted. Their exact-match endpoint is stricter with
        // one, and this checks that the *shape* still parses — pinning a
        // duration would turn an upstream edit-length correction into a
        // failure that says nothing about the contract.
        duration_seconds: None,
    };

    let lyrics = reached_or_skip!(client.lyrics_for(&query), "LRCLIB")
        .expect("no lyrics for a song they certainly hold");

    // Asserted rather than tolerated. A `None` here would mean either that
    // their coverage lost one of the most-catalogued songs there is, or that
    // the field names this parses have moved — and the second is exactly
    // what this file exists to notice.
    assert!(!lyrics.is_empty());
    eprintln!(
        "lrclib: answered from {} (synced: {})",
        lyrics.source,
        lyrics.synced.is_some()
    );
}

/// The macro's own two paths, checked here rather than left to a service to
/// demonstrate — these run in the ordinary `cargo test`, unlike everything
/// above, because the distinction they pin is the whole basis of this file
/// being trustworthy: a red mark means drift, and nothing else does.
mod reaching {
    use super::*;

    #[tokio::test]
    #[should_panic(expected = "answered something this cannot read")]
    async fn given_an_unusable_response_when_reached_then_it_still_fails() {
        // A response that arrived and did not fit is drift. If this ever
        // starts skipping, the job goes quiet exactly when it matters.
        let _: () = reached_or_skip!(
            async { Err::<(), _>(ProviderError::Unusable("no such field".into())) },
            "a service"
        );
    }

    #[tokio::test]
    async fn given_no_answer_when_reached_then_the_check_returns_without_failing() {
        // Reaching the end of this test is the assertion: the macro returns
        // from the enclosing function, so the line below is never run.
        let _: () = reached_or_skip!(
            async { Err::<(), _>(ProviderError::Unreachable("status 503".into())) },
            "a service"
        );

        panic!("the macro did not skip; a busy service would fail the job");
    }

    #[tokio::test]
    async fn given_rate_limiting_when_reached_then_the_check_returns_without_failing() {
        let _: () = reached_or_skip!(
            async { Err::<(), _>(ProviderError::RateLimited) },
            "a service"
        );

        panic!("the macro did not skip; a throttled service would fail the job");
    }
}
