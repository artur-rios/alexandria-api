use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A bookmark's lifecycle state (SRD §4.4), mirroring `FileState`'s two-phase
/// soft/hard deletion model (UC-18/UC-19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BookmarkState {
    Active,
    Deleted,
}

impl BookmarkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BookmarkState::Active => "active",
            BookmarkState::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(BookmarkState::Active),
            "deleted" => Some(BookmarkState::Deleted),
            _ => None,
        }
    }
}

/// A bookmark about to be persisted (UC-15). The `uuid` is minted by the
/// handler, not the repository, so the value is decided by the same code on
/// both transports and a unit test can assert it against a fake.
#[derive(Debug, Clone)]
pub struct NewBookmark {
    pub uuid: Uuid,
    pub url: String,
    pub title: String,
    /// The containing bookmark collection, if any. The caller has already
    /// confirmed (when `Some`) that the collection exists and is
    /// `kind = bookmark` (UC-15 AF-02).
    pub collection_uuid: Option<Uuid>,
}

/// A persisted bookmark (SRD §4.4). The internal `id` stays inside the
/// repository — callers address a bookmark by its public `uuid`, and its
/// containing collection by the collection's public `uuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    pub uuid: Uuid,
    pub url: String,
    pub title: String,
    pub state: BookmarkState,
    pub deleted_at: Option<DateTime<Utc>>,
    pub collection_uuid: Option<Uuid>,
}
