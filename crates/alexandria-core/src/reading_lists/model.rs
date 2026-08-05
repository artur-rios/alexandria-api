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
