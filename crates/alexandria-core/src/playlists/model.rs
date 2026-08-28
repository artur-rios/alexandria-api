use serde::Serialize;
use uuid::Uuid;

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
