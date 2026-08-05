use uuid::Uuid;

use crate::auth::AuthService;
use crate::errors::DomainError;
use crate::reading_lists::model::{ReadingProgress, ReadingState};
use crate::reading_lists::repos::ReadingListRepository;

/// Whether advancing a ReadingProgress from `from` to `to` is a valid
/// transition (UC-29 / FR-RL-04). The ReadingProgress lifecycle (Use Case
/// Specification Document §4.3) only defines two forward edges: `Pending` →
/// `Reading` and `Reading` → `Read`. Anything else — going backward,
/// skipping a state, or resubmitting the current state — is rejected
/// (AF-01), so the state machine can only ever move forward one step.
/// Mirrors `watchlists::commands::update_progress::is_valid_transition`.
pub fn is_valid_transition(from: ReadingState, to: ReadingState) -> bool {
    matches!(
        (from, to),
        (ReadingState::Pending, ReadingState::Reading)
            | (ReadingState::Reading, ReadingState::Read)
    )
}

/// UC-29 — Update reading progress (FR-RL-04, FR-RL-05). Advances an item's
/// read state on a reading list, recording the current issue for a comic
/// series.
///
/// Generic over the auth service and the reading list repository, so the
/// decision logic is unit-tested against a trait fake, then wired with the
/// concrete Bearer/Sqlite collaborators at runtime (services.rs). No `Clock`
/// or `Filesystem` collaborator — updating progress touches no timestamps
/// and nothing on disk.
pub struct UpdateReadingProgressHandler<A, R> {
    auth: A,
    repo: R,
}

impl<A, R> UpdateReadingProgressHandler<A, R>
where
    A: AuthService,
    R: ReadingListRepository,
{
    pub fn new(auth: A, repo: R) -> Self {
        Self { auth, repo }
    }

    /// Update the ReadingProgress linking `item_uuid` to
    /// `reading_list_uuid` to `state`, replacing
    /// `current_issue`/`total_issues` with the given values (full replace,
    /// not a merge — `None` clears the field).
    pub async fn update(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        state: ReadingState,
        current_issue: Option<i64>,
        total_issues: Option<i64>,
        token: &str,
    ) -> Result<ReadingProgress, DomainError> {
        // AF-03: the caller must be authenticated. Evaluated before the
        // ReadingProgress is looked up (FR-AU-07 / SRD §7).
        self.auth.authenticate(token).await?;

        // AF-02: a ReadingProgress must exist for the item on that reading
        // list.
        let current = self
            .repo
            .find_progress(reading_list_uuid, item_uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the requested transition must be valid.
        if !is_valid_transition(current.state, state) {
            return Err(DomainError::InvalidState);
        }

        self.repo
            .update_progress(
                reading_list_uuid,
                item_uuid,
                state,
                current_issue,
                total_issues,
            )
            .await
    }
}
