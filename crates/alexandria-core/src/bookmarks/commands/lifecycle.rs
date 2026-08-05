use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::AuthService;
use crate::bookmarks::model::{Bookmark, BookmarkState};
use crate::bookmarks::repos::BookmarkRepository;
use crate::catalog::clock::Clock;
use crate::errors::DomainError;

/// UC-18 — Soft-delete and restore a bookmark (FR-BM-03, FR-BM-05). One
/// handler for both flows, matching how the use case groups them: soft-delete
/// marks a bookmark `deleted` (restorable); restore reverses it. Unlike
/// UC-07 (restore a file), this use case's restore has no retention-window
/// precondition — its spec text imposes none, where UC-07's explicitly does.
///
/// Like `SoftDeleteFileHandler` the command takes a `Clock` (rather than
/// reading `SystemClock` directly) so unit tests can stamp a deterministic
/// `deleted_at` via `FixedClock`. There is no `Filesystem` collaborator — a
/// bookmark is catalog-only metadata with nothing on disk.
pub struct BookmarkLifecycleHandler<A, BR, C> {
    auth: A,
    repo: BR,
    clock: C,
}

impl<A, BR, C> BookmarkLifecycleHandler<A, BR, C>
where
    A: AuthService,
    BR: BookmarkRepository,
    C: Clock,
{
    pub fn new(auth: A, repo: BR, clock: C) -> Self {
        Self { auth, repo, clock }
    }

    /// Mark `uuid` soft-deleted and return the updated `Bookmark`.
    pub async fn soft_delete(&self, uuid: Uuid, token: &str) -> Result<Bookmark, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-01: the bookmark must exist.
        let bookmark = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // A bookmark already `deleted` is restored via the `restore` method
        // below, not re-soft-deleted — mirrors UC-06's guard against
        // resetting `deleted_at` on an already-deleted record.
        if bookmark.state == BookmarkState::Deleted {
            return Err(DomainError::InvalidState);
        }

        let now: DateTime<Utc> = self.clock.now();
        self.repo.soft_delete(uuid, now).await
    }

    /// Restore `uuid` to `active` and return the updated `Bookmark`.
    pub async fn restore(&self, uuid: Uuid, token: &str) -> Result<Bookmark, DomainError> {
        // AF-02: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-01: the bookmark must exist.
        let bookmark = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Restoring an already-active bookmark is a no-op transition that
        // never happened — reject rather than silently re-confirm "active".
        if bookmark.state == BookmarkState::Active {
            return Err(DomainError::InvalidState);
        }

        self.repo.restore(uuid).await
    }
}
