use serde::Serialize;
use uuid::Uuid;

/// A reading list about to be persisted (UC-26). The `uuid` is minted by
/// the handler, not the repository, so the value is decided by the same
/// code on both transports and a unit test can assert it against a fake.
#[derive(Debug, Clone)]
pub struct NewReadingList {
    pub uuid: Uuid,
    pub name: String,
}

/// A persisted reading list (SRD §4.7). The internal `id` stays inside the
/// repository — callers address a reading list by its public `uuid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingList {
    pub uuid: Uuid,
    pub name: String,
}

/// An item's read state (SRD §4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadingState {
    Pending,
    Reading,
    Read,
}

impl ReadingState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadingState::Pending => "pending",
            ReadingState::Reading => "reading",
            ReadingState::Read => "read",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ReadingState::Pending),
            "reading" => Some(ReadingState::Reading),
            "read" => Some(ReadingState::Read),
            _ => None,
        }
    }
}

/// Which kind of read-eligible file a ReadingProgress tracks (UC-28 /
/// FR-RL-02, FR-RL-03): a reading list may hold either a `Document` (book)
/// or a `Comic`. Recorded at link time so a caller can tell them apart
/// without a second lookup against the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadingTargetKind {
    Document,
    Comic,
}

impl ReadingTargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadingTargetKind::Document => "document",
            ReadingTargetKind::Comic => "comic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "document" => Some(ReadingTargetKind::Document),
            "comic" => Some(ReadingTargetKind::Comic),
            _ => None,
        }
    }
}

/// A persisted ReadingProgress (SRD §4.7): links an item to a reading list
/// and tracks its read state, with the current/total issue for a comic
/// series (UC-29 / FR-RL-05). Both the reading list and the item are
/// addressed by their public uuid — the internal FK ids stay inside the
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingProgress {
    pub reading_list_uuid: Uuid,
    pub item_uuid: Uuid,
    pub target_kind: ReadingTargetKind,
    pub state: ReadingState,
    pub current_issue: Option<i64>,
    pub total_issues: Option<i64>,
}

/// A reading list with the ReadingProgress of every item it tracks (UC-27 /
/// FR-RL-08).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadingListWithProgress {
    pub uuid: Uuid,
    pub name: String,
    pub items: Vec<ReadingProgress>,
}
