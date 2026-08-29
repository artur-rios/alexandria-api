//! The ports enrichment reaches the outside world through, and the one gate
//! every MusicBrainz request passes.
//!
//! Ports rather than concrete clients for the reason every handler here is
//! generic over its repository: the command layer's decisions — what counts
//! as a good enough match, what is retried, what is recorded — are the part
//! worth testing, and none of them should need a network, a key, or a
//! service that might be down while the suite runs.

pub mod commons;
pub mod lrclib;
pub mod musicbrainz;

use std::time::Duration;

use crate::errors::DomainError;

/// Why a lookup produced no answer.
///
/// Every variant means the same thing to the command layer — record
/// [`super::model::EnrichmentOutcome::Failed`] and carry on — but they are
/// kept apart because they mean very different things in a log, and because
/// a rate limit is the one that says the gate below is set wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The service could not be reached at all.
    Unreachable(String),
    /// The service refused because requests came too fast.
    RateLimited,
    /// The service answered with something this code cannot read.
    Unusable(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Unreachable(why) => write!(f, "unreachable: {why}"),
            ProviderError::RateLimited => write!(f, "rate limited"),
            ProviderError::Unusable(why) => write!(f, "unusable response: {why}"),
        }
    }
}

impl From<ProviderError> for DomainError {
    /// Only for the paths that genuinely fail a command. The enrichment run
    /// itself never converts — it records an outcome and continues, which is
    /// design section 5.
    fn from(error: ProviderError) -> Self {
        DomainError::ServiceUnavailable(error.to_string())
    }
}

/// An artist MusicBrainz believes the searched name refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistMatch {
    pub mbid: String,
    /// The name as MusicBrainz spells it, which is not necessarily the name
    /// that was searched — kept so a match can be explained.
    pub name: String,
    /// MusicBrainz's own confidence, 0-100.
    pub score: u8,
}

/// The smallest MusicBrainz score this code will accept.
///
/// A search always returns *something*: ask for a misspelled artist and the
/// top hit comes back with a low score rather than an empty list. Taking the
/// first result unconditionally is how a library ends up showing the wrong
/// person's photograph on an artist page, which is worse than showing none —
/// a blank is obviously missing, a confident wrong face is not.
///
/// 90 rather than 100: exact matches score 100, but so should "Beyoncé"
/// searched as "Beyonce", and a library tagged without accents is the normal
/// case rather than the exception.
pub const MIN_ARTIST_SCORE: u8 = 90;

/// Resolves a tag's artist name to a MusicBrainz artist.
#[allow(async_fn_in_trait)]
pub trait ArtistIdentityProvider: Send + Sync {
    /// The best match for `name`, or `None` when the service had nothing at
    /// all. A match below [`MIN_ARTIST_SCORE`] is still returned — scoring it
    /// is the caller's decision to make and to record, not this port's to
    /// silently swallow.
    async fn find_artist(&self, name: &str) -> Result<Option<ArtistMatch>, ProviderError>;
}

/// A recording MusicBrainz believes a track is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingMatch {
    pub mbid: String,
    /// MusicBrainz's own confidence, 0-100, scored against
    /// [`MIN_RECORDING_SCORE`] by the caller.
    pub score: u8,
}

/// The smallest MusicBrainz score accepted for a recording.
///
/// Higher than [`MIN_ARTIST_SCORE`], and deliberately. An artist search has
/// one field to be wrong about; a recording search matches title, artist and
/// album together, so a genuine hit scores very high and anything middling is
/// usually a different take, a live version, or another track from the same
/// record. The consequence of a wrong one is only a wrong provenance id
/// rather than wrong words on screen — LRCLIB is what supplies the lyrics —
/// but an id that names the wrong recording is worse than no id, because it
/// looks like an answer.
pub const MIN_RECORDING_SCORE: u8 = 95;

/// Resolves a track to a MusicBrainz recording.
///
/// Separate from [`ArtistIdentityProvider`] rather than a second method on
/// it: they are asked at different times, for different reasons, and one of
/// them costs a second of the rate budget per *track* where the other costs
/// one per *artist*. Keeping them apart makes that difference visible at the
/// call site instead of hidden behind a shared trait.
#[allow(async_fn_in_trait)]
pub trait RecordingIdentityProvider: Send + Sync {
    async fn find_recording(
        &self,
        query: &LyricsQuery,
    ) -> Result<Option<RecordingMatch>, ProviderError>;
}

/// An artist image and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistImageAsset {
    /// The page or file the bytes came from, kept for attribution —
    /// Wikimedia Commons licences require it.
    pub source_url: String,
    pub bytes: Vec<u8>,
    /// The file extension to store the bytes under, without a dot.
    pub extension: String,
}

/// Finds a photograph for an artist already resolved to a MusicBrainz id.
#[allow(async_fn_in_trait)]
pub trait ArtistImageProvider: Send + Sync {
    async fn image_for(&self, mbid: &str) -> Result<Option<ArtistImageAsset>, ProviderError>;
}

/// What a lyrics lookup searches on.
///
/// Duration is included because it is the field that tells two recordings of
/// one song apart — a radio edit from an album cut — and a lyrics provider
/// matching on title and artist alone will happily return the wrong one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsQuery {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_seconds: Option<u32>,
}

/// Lyrics as a provider answered them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsMatch {
    pub plain: Option<String>,
    /// LRC-format text with timestamps, when the provider had it.
    pub synced: Option<String>,
    /// Which service answered.
    pub source: String,
}

impl LyricsMatch {
    /// Whether this actually carries anything.
    ///
    /// A provider answering `200` with both fields empty is a "no", not a
    /// find — recording it as found would store a blank and never ask again.
    pub fn is_empty(&self) -> bool {
        let empty = |value: &Option<String>| {
            value
                .as_ref()
                .map(|text| text.trim().is_empty())
                .unwrap_or(true)
        };
        empty(&self.plain) && empty(&self.synced)
    }
}

/// Finds lyrics for one recording.
#[allow(async_fn_in_trait)]
pub trait LyricsProvider: Send + Sync {
    async fn lyrics_for(&self, query: &LyricsQuery) -> Result<Option<LyricsMatch>, ProviderError>;
}

/// MusicBrainz's published limit: one request per second per source.
pub const MUSICBRAINZ_INTERVAL: Duration = Duration::from_millis(1000);

/// Serializes requests so no two leave closer together than `interval`.
///
/// This is a term of use, not a performance tuning knob. MusicBrainz rate-
/// limits to one request per second and is entitled to block a client that
/// ignores it — and the block would land on everyone running this software,
/// not on the one instance that misbehaved.
///
/// A single mutex held across the wait, deliberately. It makes concurrent
/// callers queue rather than all sleep the same interval and then fire
/// together, which is what a per-caller delay would do and is exactly the
/// burst the limit exists to prevent.
pub struct RateGate {
    interval: Duration,
    last: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl RateGate {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: tokio::sync::Mutex::new(None),
        }
    }

    /// MusicBrainz's own rate.
    pub fn musicbrainz() -> Self {
        Self::new(MUSICBRAINZ_INTERVAL)
    }

    /// The one MusicBrainz gate for this process.
    ///
    /// A gate per client enforces the limit per client, which is not what
    /// MusicBrainz's terms say and not what this module's own documentation
    /// claimed. Two clients — one per built service graph, or one per
    /// request — would silently double the outbound rate against a term the
    /// code otherwise refuses to start without honouring. A `OnceLock` makes
    /// "one per process" a property of the type rather than an instruction
    /// to whoever wires it up next.
    pub fn shared_musicbrainz() -> &'static RateGate {
        static GATE: std::sync::OnceLock<RateGate> = std::sync::OnceLock::new();
        GATE.get_or_init(RateGate::musicbrainz)
    }

    /// A self-imposed pace for services that publish no limit.
    ///
    /// Not a documented rate anyone gave us — LRCLIB publishes none — which
    /// is exactly why it is here: a `Pending` run over a ten-thousand-track
    /// library would otherwise fire ten thousand requests as fast as the
    /// network allows, earn a `429`, record every one of them as retryable,
    /// and repeat the burst on the next run.
    pub fn courteous() -> Self {
        Self::new(Duration::from_millis(250))
    }

    /// Wait until another request may be sent, then record that it was.
    pub async fn admit(&self) {
        let mut last = self.last.lock().await;
        let now = tokio::time::Instant::now();

        if let Some(previous) = *last {
            let elapsed = now.duration_since(previous);
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
        }

        // Read the clock again rather than reusing `now`: the sleep above
        // may have been long, and stamping the pre-sleep instant would let
        // the *next* caller through early by exactly that much.
        *last = Some(tokio::time::Instant::now());
    }
}

impl Default for RateGate {
    fn default() -> Self {
        Self::musicbrainz()
    }
}

/// The `User-Agent` every outbound request carries.
///
/// MusicBrainz requires an agent naming the application and a contact, and
/// says so in terms they enforce. Built in one place so the three clients
/// cannot drift into sending different — or anonymous — strings.
pub fn user_agent(contact: &str) -> String {
    format!(
        "{}/{} ( {} )",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        contact.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_contact_when_the_agent_is_built_then_it_names_app_and_contact() {
        let agent = user_agent("owner@example.com");

        assert!(agent.contains("alexandria-core"), "{agent}");
        assert!(agent.contains("owner@example.com"), "{agent}");
    }

    #[test]
    fn given_empty_lyrics_when_checked_then_they_count_as_nothing() {
        // A provider answering 200 with blank fields is a "no". Storing it as
        // a find would cache an empty string and never ask again.
        let answered_blank = LyricsMatch {
            plain: Some("   ".to_string()),
            synced: None,
            source: "test".to_string(),
        };

        assert!(answered_blank.is_empty());
    }

    #[test]
    fn given_synced_only_when_checked_then_they_count_as_something() {
        let synced_only = LyricsMatch {
            plain: None,
            synced: Some("[00:01.00] a line".to_string()),
            source: "test".to_string(),
        };

        assert!(!synced_only.is_empty());
    }

    #[test]
    fn given_a_failed_outcome_when_asked_then_it_is_not_settled() {
        use crate::enrichment::model::EnrichmentOutcome;

        // The whole resumability rule: only a failure is re-asked.
        assert!(!EnrichmentOutcome::Failed.is_settled());
        assert!(EnrichmentOutcome::Found.is_settled());
        assert!(EnrichmentOutcome::NotFound.is_settled());
        assert!(EnrichmentOutcome::Rejected.is_settled());
    }

    #[test]
    fn given_an_unknown_stored_outcome_when_parsed_then_it_is_retried() {
        // A row written by a newer version, or corrupted, should cost a
        // re-ask rather than a permanently wrong "already settled".
        assert_eq!(
            crate::enrichment::model::EnrichmentOutcome::from_stored("something-else"),
            crate::enrichment::model::EnrichmentOutcome::Failed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn given_the_gate_when_two_requests_arrive_then_the_second_waits_the_interval() {
        // The terms MusicBrainz enforces, asserted on the clock rather than
        // trusted. Paused time, so this costs no wall-clock second.
        let gate = RateGate::new(Duration::from_millis(1000));
        let started = tokio::time::Instant::now();

        gate.admit().await;
        gate.admit().await;

        assert!(
            tokio::time::Instant::now().duration_since(started) >= Duration::from_millis(1000),
            "the second request was admitted early"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn given_the_gate_when_the_caller_was_slow_then_it_does_not_wait_again() {
        // A caller that already spent longer than the interval doing its own
        // work must not be charged the interval a second time.
        let gate = RateGate::new(Duration::from_millis(1000));

        gate.admit().await;
        tokio::time::sleep(Duration::from_millis(1500)).await;

        let before = tokio::time::Instant::now();
        gate.admit().await;

        assert!(
            tokio::time::Instant::now().duration_since(before) < Duration::from_millis(100),
            "an already-elapsed interval was charged again"
        );
    }
}
