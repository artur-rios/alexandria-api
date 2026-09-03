use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// A play that has been recorded: which track, and when it was played.
///
/// The timestamp is the core's, taken from the `Clock` when the play is
/// recorded, never a value the caller supplies — a client that could name
/// the moment could also name one in the middle of last year, and every
/// ranking below is an aggregate over exactly this column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayEvent {
    pub file_uuid: Uuid,
    pub played_at: DateTime<Utc>,
}

/// What was played most (play history design).
///
/// One shape holding every ranking rather than a query per ranking: the
/// four lists are read together, on one screen, and answering them
/// separately would mean four round trips whose totals could disagree with
/// each other because each saw a different instant.
///
/// A track with no tags still counts toward `total_plays`, and still
/// appears in `top_tracks` under its filename — the file is a thing that
/// was played. It appears in none of the other three: an untagged track has
/// no artist, album, or genre, and ranking it under "unknown" would invent
/// an artist who does not exist and, given enough untagged files, put them
/// at the top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicStats {
    /// Every play ever recorded, tagged or not.
    pub total_plays: i64,
    /// How many distinct tracks those plays are spread across.
    pub distinct_tracks: i64,
    /// The oldest and newest play, so a client can say what period the
    /// numbers cover without reading the events themselves. `None` on both
    /// when nothing has been played.
    pub first_played_at: Option<DateTime<Utc>>,
    pub last_played_at: Option<DateTime<Utc>>,
    pub top_tracks: Vec<TrackPlays>,
    pub top_artists: Vec<ArtistPlays>,
    pub top_albums: Vec<AlbumPlays>,
    pub top_genres: Vec<GenrePlays>,
}

/// One track in the ranking, with the tags it was carrying when the
/// statistics were read.
///
/// Deliberately reads the tags live rather than snapshotting them onto the
/// play row: correcting a misspelled artist is meant to correct the history
/// too, and a snapshot would leave the old spelling ranking as a second
/// artist forever. The trade is the other direction — retagging a track
/// moves its past plays to the new artist — which is the same "the catalog
/// is the single source of truth" rule every other listing follows.
///
/// A soft-deleted or missing track stays in the ranking: the play happened,
/// and the file is still in the catalog. A purge is what takes it out, and
/// takes its plays with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackPlays {
    pub file_uuid: Uuid,
    /// The track's title, or its filename when the tag is absent — the
    /// ranking is of files, and every file has a name even when nothing
    /// tagged it.
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub plays: i64,
    pub last_played_at: DateTime<Utc>,
}

/// One artist in the ranking, counted across every track credited to them.
///
/// The credit is `album_artist` where a track carries one, falling back to
/// `artist` — the same precedence the catalog's own album grouping uses
/// (album artist design), so a compilation's plays land on the album's
/// artist rather than on each guest performer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistPlays {
    pub artist: String,
    pub plays: i64,
    /// How many distinct tracks of theirs were played, which is what tells
    /// a deep catalogue apart from one song on repeat.
    pub tracks: i64,
}

/// One album in the ranking.
///
/// Grouped by album name alone, not by album-and-artist: a compilation
/// whose tracks each name a different performer is one album, and grouping
/// by the pair would scatter it across as many rows as it has guests.
/// `artist` is filled in only when every played track on the album agrees
/// on one — otherwise there is no single answer to who the album is by, and
/// naming one of them would be picking a winner arbitrarily.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPlays {
    pub album: String,
    pub artist: Option<String>,
    pub plays: i64,
}

/// One genre in the ranking, as tagged on the tracks themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenrePlays {
    pub genre: String,
    pub plays: i64,
}
