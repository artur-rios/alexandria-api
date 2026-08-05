use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

/// Result of UC-13 (add items) / UC-14 (remove items): the collection and
/// the item uuids the request just linked or unlinked, echoing the request
/// rather than the collection's full membership — listing full membership is
/// UC-14's own read path (FR-CO-07), not this write's job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionItemsResult {
    pub collection_uuid: Uuid,
    pub item_uuids: Vec<Uuid>,
}
