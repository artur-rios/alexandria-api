use serde::Serialize;
use uuid::Uuid;

/// A watchlist about to be persisted (UC-20). The `uuid` is minted by the
/// handler, not the repository, so the value is decided by the same code on
/// both transports and a unit test can assert it against a fake.
#[derive(Debug, Clone)]
pub struct NewWatchlist {
    pub uuid: Uuid,
    pub name: String,
}

/// A persisted watchlist (SRD §4.5). The internal `id` stays inside the
/// repository — callers address a watchlist by its public `uuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Watchlist {
    pub uuid: Uuid,
    pub name: String,
}

/// A video's watch state (SRD §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchState {
    Pending,
    Watching,
    Watched,
}

impl WatchState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WatchState::Pending => "pending",
            WatchState::Watching => "watching",
            WatchState::Watched => "watched",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(WatchState::Pending),
            "watching" => Some(WatchState::Watching),
            "watched" => Some(WatchState::Watched),
            _ => None,
        }
    }
}

/// A persisted WatchProgress (SRD §4.6): links a video to a watchlist and
/// tracks its watch state, with the current/total episode for series
/// (UC-23 / FR-WL-05). Both the watchlist and the video are addressed by
/// their public uuid — the internal FK ids stay inside the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchProgress {
    pub watchlist_uuid: Uuid,
    pub video_uuid: Uuid,
    pub state: WatchState,
    pub current_episode: Option<i64>,
    pub total_episodes: Option<i64>,
}

/// Confirmation that a video was removed from a watchlist (UC-24 /
/// FR-WL-06): the WatchProgress is deleted, the VideoFile is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistItemResult {
    pub watchlist_uuid: Uuid,
    pub video_uuid: Uuid,
}

/// A watchlist with the WatchProgress of every video it tracks (UC-21 /
/// FR-WL-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchlistWithProgress {
    pub uuid: Uuid,
    pub name: String,
    pub items: Vec<WatchProgress>,
}
