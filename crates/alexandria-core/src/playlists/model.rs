use serde::Serialize;
use uuid::Uuid;

use crate::catalog::model::FileView;

/// A playlist about to be persisted. The `uuid` is minted by the handler,
/// not the repository, so the value is decided by the same code on both
/// transports and a unit test can assert it against a fake.
#[derive(Debug, Clone)]
pub struct NewPlaylist {
    pub uuid: Uuid,
    pub name: String,
}

/// A persisted playlist. The internal `id` stays inside the repository —
/// callers address a playlist by its public `uuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub uuid: Uuid,
    pub name: String,
}

/// One track held by a playlist. `id` is the entry's own identity, not the
/// file's — `playlist_entries` deliberately carries no `UNIQUE
/// (playlist_id, file_id)`, so the same track may appear as two distinct
/// entries. `position` is contiguous `0..n-1` within the playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    pub id: i64,
    pub file_uuid: Uuid,
    pub position: i64,
}

/// A playlist read back with its tracks (Task 6). Each track is answered as
/// `FileView` — the same shape every other listing answers (`catalog::
/// queries::browse`) — so a client parses a playlist with what it already
/// has for the catalog, plus `entry_id`/`position` (the playlist-specific
/// facts `FileView` has no room for) and `missing` (design section 5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistView {
    pub playlist: Playlist,
    pub entries: Vec<PlaylistTrack>,
}

/// One track in a `PlaylistView`, in the playlist's position order.
///
/// `missing` mirrors `file.file.missing_at.is_some()` as a plain bool so a
/// client does not have to know `missing_at`'s presence *means* missing —
/// design section 5: a track whose file has gone missing on disk stays in
/// the list, flagged, rather than being dropped. Dropping it would delete
/// curation work invisibly and make an unplugged drive look like an empty
/// playlist rather than a broken one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistTrack {
    pub entry_id: i64,
    pub position: i64,
    pub file: FileView,
    pub missing: bool,
}
