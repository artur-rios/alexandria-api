use chrono::Duration;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::model::{Bookmark, BookmarkState};
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::clock::Clock;
use crate::errors::DomainError;

/// UC-19 — Hard-purge a bookmark (FR-BM-04). Permanently removes a
/// soft-deleted bookmark's record once the retention window has elapsed.
///
/// Same shape as [`PurgeFileHandler`](crate::catalog::commands::purge::PurgeFileHandler):
/// construction wires the collaborators (`AuthService`, `BookmarkRepository`,
/// `Clock`) plus the configured soft-delete retention window in days
/// (`retention_days`, NFR-10 — the same setting UC-08 uses). The retention
/// boundary matches UC-08's: `elapsed > retention_days` is purgeable;
/// `elapsed == retention_days` is still restorable and not yet purgeable.
pub struct PurgeBookmarkHandler<A, BR, C> {
    auth: A,
    repo: BR,
    clock: C,
    retention_days: u32,
}

impl<A, BR, C> PurgeBookmarkHandler<A, BR, C>
where
    A: AuthService,
    BR: BookmarkRepository,
    C: Clock,
{
    pub fn new(auth: A, repo: BR, clock: C, retention_days: u32) -> Self {
        Self {
            auth,
            repo,
            clock,
            retention_days,
        }
    }

    /// Hard-purge `uuid`'s record and return the `Bookmark` as it was
    /// immediately before deletion (a confirmation snapshot; the row itself
    /// no longer exists once this returns `Ok`).
    pub async fn purge(&self, uuid: Uuid, token: &str) -> Result<Bookmark, DomainError> {
        // AF-03: the caller must be authenticated, evaluated before any
        // payload is consulted (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: the bookmark must exist.
        let bookmark = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: an `active` bookmark never started a retention window, so
        // it can never be purgeable.
        if bookmark.state != BookmarkState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // A `deleted` row should always carry a `deleted_at` (UC-18 stamped
        // it). A `deleted` row without one is corrupt data; surface it as
        // `InvalidState` rather than silently treating the row as purgeable.
        let deleted_at = bookmark.deleted_at.ok_or(DomainError::InvalidState)?;

        // AF-01: only past-retention records are purgeable.
        let elapsed = self.clock.now() - deleted_at;
        if elapsed <= Duration::days(i64::from(self.retention_days)) {
            return Err(DomainError::InvalidState);
        }

        self.repo.purge(uuid).await?;
        Ok(bookmark)
    }
}
