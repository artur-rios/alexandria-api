use std::fmt;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// What a completed lookup concluded.
///
/// The reason this is stored rather than inferred from whether the payload
/// columns are null: "this artist has no photograph anywhere" and "this
/// artist was never looked up" would otherwise be the same row, and every
/// run would re-ask a question three services have already answered no to —
/// at one request per second, forever.
///
/// [`Self::Failed`] is deliberately distinct from [`Self::NotFound`]. Not
/// found is an answer and should not be retried; failed is the absence of an
/// answer, and a later run is entitled to ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EnrichmentOutcome {
    /// The service answered, and had something.
    Found,
    /// The service answered, and had nothing. Settled: not re-asked.
    NotFound,
    /// No answer — the service was unreachable, rate-limited, or replied with
    /// something unusable. Re-asked by a later run.
    Failed,
    /// A candidate was returned but scored below the match threshold. Settled
    /// like [`Self::NotFound`], and separate from it so a wrong-looking
    /// library can be told apart from a genuinely obscure one.
    Rejected,
}

/// The stored values that count as settled, as a SQL list literal.
///
/// The resumability rule lives in `EnrichmentOutcome::is_settled`, and the
/// candidate query has to implement the *same* rule in SQL. Writing that
/// query as `outcome <> 'failed'` looked equivalent and was not: an
/// unrecognized value — a row from a newer version, or corruption — reads as
/// `Failed` and therefore retryable in Rust, while `<> 'failed'` makes it
/// settled and drops the file out of every run. Naming the settled values
/// positively, once, is what keeps the two halves from disagreeing.
pub const SETTLED_OUTCOMES_SQL: &str = "('found', 'notFound', 'rejected')";

impl EnrichmentOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            EnrichmentOutcome::Found => "found",
            EnrichmentOutcome::NotFound => "notFound",
            EnrichmentOutcome::Failed => "failed",
            EnrichmentOutcome::Rejected => "rejected",
        }
    }

    /// Whether a later run should ask again.
    ///
    /// Only a failure is worth re-asking: the other three are answers. This
    /// is the whole of the resumability rule, in one place, so the command
    /// and the repository's "what still needs looking up" query cannot
    /// disagree about what "already done" means.
    pub fn is_settled(&self) -> bool {
        !matches!(self, EnrichmentOutcome::Failed)
    }

    /// Parse a stored value back. Anything unrecognized reads as
    /// [`Self::Failed`] — the one variant that is retried — so a row written
    /// by a newer version, or corrupted, costs a re-ask rather than a
    /// permanently wrong "settled".
    pub fn from_stored(value: &str) -> Self {
        match value {
            "found" => EnrichmentOutcome::Found,
            "notFound" => EnrichmentOutcome::NotFound,
            "rejected" => EnrichmentOutcome::Rejected,
            _ => EnrichmentOutcome::Failed,
        }
    }
}

impl fmt::Display for EnrichmentOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored artist image lookup, whatever it concluded.
///
/// Keyed by the artist's name as the catalog holds it, because that is the
/// only artist identity this catalog has — there is no `artists` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistImage {
    pub artist_name: String,
    /// The MusicBrainz artist id the name resolved to, or `None` when
    /// nothing matched. Kept even on a rejected match so a wrong one can be
    /// explained rather than only observed.
    pub mbid: Option<String>,
    /// Where the image came from, for attribution — Wikimedia Commons
    /// requires it, and an image whose provenance is lost cannot be credited.
    pub source_url: Option<String>,
    /// Path to the cached bytes, relative to the image cache directory.
    pub image_path: Option<String>,
    pub outcome: EnrichmentOutcome,
    pub fetched_at: DateTime<Utc>,
}

/// A stored lyrics lookup, whatever it concluded.
///
/// Keyed by file, not by title: the closest thing this catalog has to a
/// recording is a file, and two files of the same song may differ in edit,
/// length, or language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackLyrics {
    pub file_uuid: Uuid,
    pub mbid: Option<String>,
    /// The unsynchronized text.
    pub plain: Option<String>,
    /// LRC-format text with timestamps, when the provider had it.
    pub synced: Option<String>,
    /// Which service answered, for attribution and so a later change of
    /// provider can tell its own rows from another's.
    pub source: Option<String>,
    pub outcome: EnrichmentOutcome,
    pub fetched_at: DateTime<Utc>,
}

/// What one enrichment run did.
///
/// Counted rather than itemized: a run over a whole library touches thousands
/// of rows, and a caller wants to know it finished and roughly what happened,
/// not to receive the list back. The per-item detail is in the tables, which
/// is where a client reads it from anyway.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentReport {
    /// Items considered — artists plus tracks in scope.
    pub considered: u32,
    /// Lookups that found something.
    pub found: u32,
    /// Lookups that answered nothing.
    pub not_found: u32,
    /// Candidates that scored below the match threshold.
    pub rejected: u32,
    /// Lookups that got no answer. These are re-asked by a later run.
    pub failed: u32,
    /// Items skipped because a settled outcome was already stored, or
    /// because the item carries no artist to search on.
    pub skipped: u32,
}

impl EnrichmentReport {
    /// Record one item's outcome.
    pub fn record(&mut self, outcome: EnrichmentOutcome) {
        self.considered += 1;
        match outcome {
            EnrichmentOutcome::Found => self.found += 1,
            EnrichmentOutcome::NotFound => self.not_found += 1,
            EnrichmentOutcome::Rejected => self.rejected += 1,
            EnrichmentOutcome::Failed => self.failed += 1,
        }
    }

    /// Record one item that was not looked up at all.
    pub fn skip(&mut self) {
        self.considered += 1;
        self.skipped += 1;
    }
}

/// What the caller asked to enrich.
///
/// A named scope rather than a free-form filter: these three are what a
/// client actually offers — this track, this artist, or everything not yet
/// looked up — and each has a different, bounded cost the caller should be
/// choosing between deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichmentScope {
    /// One file: its artist's image and its own lyrics.
    File(Uuid),
    /// One artist by name: their image, and the lyrics of every audio file
    /// they are the album artist of.
    Artist(String),
    /// Every audio file with no settled outcome yet. Resumable, and the
    /// expensive one.
    Pending,
}
