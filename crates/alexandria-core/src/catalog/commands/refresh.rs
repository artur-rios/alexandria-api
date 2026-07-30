use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::fs::Filesystem;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

#[derive(Debug, Clone, Serialize)]
pub struct RefreshStarted {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefreshOutcome {
    #[serde(rename = "runId")]
    pub run_id: Uuid,
    /// Records whose hash changed (or that returned to disk while marked
    /// missing) and were refreshed.
    pub refreshed: usize,
    /// Cataloged paths whose on-disk file is gone (UC-02 AF-01 / FR-FC-11).
    pub marked_missing: usize,
    /// Present and unchanged since the last index — no write performed.
    pub unchanged: usize,
}

/// Re-index and refresh the catalog (UC-02).
///
/// `start` authenticates the caller and returns a fresh run id immediately;
/// `execute` iterates every cataloged path (no tree walk — discovery of *new*
/// files is UC-01's job), recomputes each present file's SHA-256, and:
///   * refreshes hash + `indexed_at` (clearing `missing_at`) when the hash
///     changed or the file returned to disk after being marked missing
///     (FR-FC-10), and
///   * marks `missing_at` (leaving `state` untouched — soft-delete is UC-06)
///     when the on-disk file is gone (FR-FC-11 / AF-01).
///
/// Generic over collaborators so the decision logic is unit-tested against
/// trait fakes with no real DB / filesystem / auth service (Testing Spec §6.2).
pub struct RefreshHandler<A, R, F, C> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
}

impl<A, R, F, C> RefreshHandler<A, R, F, C>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
        }
    }

    /// Authenticate and return a run id. No input to validate (re-index
    /// touches every cataloged path), so the only failure is AF-02.
    pub async fn start(&self, token: &str) -> Result<RefreshStarted, DomainError> {
        self.auth.authenticate(token).await?;
        Ok(RefreshStarted {
            run_id: Uuid::new_v4(),
        })
    }

    /// Walk every cataloged path and refresh / mark missing.
    pub async fn execute(&self, run_id: Uuid) -> Result<RefreshOutcome, DomainError> {
        let now = self.clock.now();
        let files = self.repo.list_all().await?;

        let mut refreshed = 0usize;
        let mut marked_missing = 0usize;
        let mut unchanged = 0usize;

        for file in files {
            if self.fs.path_exists(&file.path).await {
                let new_hash = self.fs.content_hash(&file.path).await?;
                if new_hash != file.content_hash || file.missing_at.is_some() {
                    self.repo
                        .refresh_hash(&file.path, &new_hash, now)
                        .await?;
                    refreshed += 1;
                } else {
                    unchanged += 1;
                }
            } else if file.missing_at.is_none() {
                self.repo.mark_missing(&file.path, now).await?;
                marked_missing += 1;
            } else {
                // Already marked missing and still gone — leave as-is.
                unchanged += 1;
            }
        }

        let outcome = RefreshOutcome {
            run_id,
            refreshed,
            marked_missing,
            unchanged,
        };
        tracing::info!(
            %run_id,
            refreshed = outcome.refreshed,
            marked_missing = outcome.marked_missing,
            unchanged = outcome.unchanged,
            "re-index complete"
        );
        Ok(outcome)
    }
}