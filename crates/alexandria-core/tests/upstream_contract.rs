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
    ArtistIdentityProvider, ArtistImageProvider, LyricsProvider, LyricsQuery,
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

    let found = client
        .find_artist(ARTIST)
        .await
        .expect("MusicBrainz did not answer")
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

    let found = client
        .find_recording(&query)
        .await
        .expect("MusicBrainz did not answer")
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
    let mbid = MusicBrainzClient::new(&contact)
        .expect("client")
        .find_artist(ARTIST)
        .await
        .expect("MusicBrainz did not answer")
        .expect("no match")
        .mbid;

    let asset = CommonsImageClient::new(&contact)
        .expect("client")
        .image_for(&mbid)
        .await
        .expect("Wikidata or Commons did not answer")
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

    let lyrics = client
        .lyrics_for(&query)
        .await
        .expect("LRCLIB did not answer")
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
