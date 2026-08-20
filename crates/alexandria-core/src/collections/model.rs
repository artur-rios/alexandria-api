use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bookmarks::model::Bookmark;
use crate::catalog::model::File;

/// What a collection may hold (SRD §4.3). A collection is either a grouping of
/// files or a grouping of bookmarks; the discriminator is fixed at creation
/// (UC-10 / FR-CO-01, FR-CO-02) and decides which items UC-13 will accept.
///
/// Serialized lowercase so the HTTP JSON body (`{"kind":"file"}`) and the FFI
/// JSON body carry the same value (FR-FC-24 / NFR-09). Deserialization is what
/// rejects an unrecognised `kind` as invalid input on both surfaces (AF-01) —
/// there is no variant for it to land in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    File,
    Bookmark,
}

impl CollectionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CollectionKind::File => "file",
            CollectionKind::Bookmark => "bookmark",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(CollectionKind::File),
            "bookmark" => Some(CollectionKind::Bookmark),
            _ => None,
        }
    }
}

impl std::fmt::Display for CollectionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A collection about to be persisted (UC-10). The `uuid` is minted by the
/// handler, not the repository, so the value is decided by the same code on
/// both transports and a unit test can assert it against a fake.
#[derive(Debug, Clone)]
pub struct NewCollection {
    pub uuid: Uuid,
    pub name: String,
    pub kind: CollectionKind,
}

/// A persisted collection (SRD §4.3). The internal `id` stays inside the
/// repository — callers address a collection by its public `uuid`, which is
/// what UC-10 returns and what UC-11..14 will take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub uuid: Uuid,
    pub name: String,
    pub kind: CollectionKind,
}

/// A collection as UC-46's listing returns it (FR-CO-08): the record plus the
/// number of items it currently holds.
///
/// A summary type rather than a field on [`Collection`], because the two answer
/// different questions. `Collection` is what UC-10 and UC-11 echo — "what did I
/// just write" — where a count nobody asked for would cost a second query on
/// every write. This one answers "what do I have", where the count is the
/// reason to ask.
///
/// The count is derived by counting the rows that point at the collection, so
/// it cannot drift from the membership, and it counts the same members UC-14
/// lists: soft-deleted items are excluded from both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSummary {
    pub uuid: Uuid,
    pub name: String,
    pub kind: CollectionKind,
    pub item_count: i64,
}

/// Why one item was not added (UC-13 AF-01, AF-02).
///
/// The two are deliberately distinguishable: a bookmark submitted to a file
/// collection is a different mistake from a uuid that names nothing, and a
/// caller that has to explain the outcome needs to know which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemRejection {
    /// The item exists, but belongs to the other kind (AF-01).
    WrongKind,

    /// No item of either kind carries that uuid (AF-02).
    NotFound,
}

/// What became of one submitted item (UC-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemOutcome {
    pub item_uuid: Uuid,
    pub added: bool,
    /// Why it was not added. Absent when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ItemRejection>,
}

/// Result of UC-13 (add items): the collection, and what became of every item
/// the request submitted.
///
/// Per item rather than all-or-nothing. The handler links what it can and
/// reports the rest, because a caller that has to tell its owner what happened
/// cannot do so from a single request-level error: "none of them, because one
/// was wrong" is not an answer anybody can act on. AF-01 and AF-02 are
/// therefore reasons here rather than errors.
///
/// It still echoes the request rather than the collection's full membership —
/// listing that is UC-14's read path (FR-CO-07), not this write's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemsResult {
    pub collection_uuid: Uuid,
    pub items: Vec<CollectionItemOutcome>,
}

/// Result of UC-14's remove (FR-CO-06): the collection and the single item
/// uuid the request just unlinked. The interface is per-item
/// (`DELETE /v1/collections/{uuid}/items/{itemUuid}`, SRD §5.4), unlike
/// UC-13's batch add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemResult {
    pub collection_uuid: Uuid,
    pub item_uuid: Uuid,
}

/// The members of a collection (UC-14 list / FR-CO-07): either files or
/// bookmarks, depending on the collection's `kind`. Untagged so the wire
/// shape is a bare array under `items` — the `kind` field alongside it is
/// what a client uses to know which shape to expect.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CollectionItems {
    Files(Vec<File>),
    Bookmarks(Vec<Bookmark>),
}

/// Result of UC-14's list (FR-CO-07): the collection's uuid, its `kind`, and
/// its current members.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionMembersResult {
    pub collection_uuid: Uuid,
    pub kind: CollectionKind,
    pub items: CollectionItems,
}
