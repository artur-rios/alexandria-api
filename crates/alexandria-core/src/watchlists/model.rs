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
