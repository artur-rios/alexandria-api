use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::File;
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
    /// Cataloged paths that could not be processed because an operation against
    /// that one file failed (unreadable bytes, or a repository write error).
    /// The run continues past them; each is logged at `warn`.
    pub failed: usize,
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
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the catalog. Only a failure to list the catalog at all aborts.
    pub async fn execute(&self, run_id: Uuid) -> Result<RefreshOutcome, DomainError> {
        let now = self.clock.now();
        let files = self.repo.list_all().await?;

        let mut refreshed = 0usize;
        let mut marked_missing = 0usize;
        let mut unchanged = 0usize;
        let mut failed = 0usize;

        for file in files {
            match self.refresh_one(&file, now).await {
                Ok(PathOutcome::Refreshed) => refreshed += 1,
                Ok(PathOutcome::MarkedMissing) => marked_missing += 1,
                Ok(PathOutcome::Unchanged) => unchanged += 1,
                Err(err) => {
                    failed += 1;
                    tracing::warn!(
                        %run_id,
                        path = %file.path,
                        error = %err,
                        "skipping cataloged path that could not be refreshed"
                    );
                }
            }
        }

        let outcome = RefreshOutcome {
            run_id,
            refreshed,
            marked_missing,
            unchanged,
            failed,
        };
        tracing::info!(
            %run_id,
            refreshed = outcome.refreshed,
            marked_missing = outcome.marked_missing,
            unchanged = outcome.unchanged,
            failed = outcome.failed,
            "re-index complete"
        );
        Ok(outcome)
    }

    /// Refresh one cataloged path. `Err` means this one file failed — the
    /// caller counts it and moves on to the rest of the catalog.
    async fn refresh_one(
        &self,
        file: &File,
        now: DateTime<Utc>,
    ) -> Result<PathOutcome, DomainError> {
        if self.fs.path_exists(&file.path).await {
            let new_hash = self.fs.content_hash(&file.path).await?;
            if new_hash != file.content_hash || file.missing_at.is_some() {
                self.repo.refresh_hash(&file.path, &new_hash, now).await?;
                Ok(PathOutcome::Refreshed)
            } else {
                Ok(PathOutcome::Unchanged)
            }
        } else if file.missing_at.is_none() {
            self.repo.mark_missing(&file.path, now).await?;
            Ok(PathOutcome::MarkedMissing)
        } else {
            // Already marked missing and still gone — leave as-is.
            Ok(PathOutcome::Unchanged)
        }
    }
}

/// What a single cataloged path resolved to during a refresh pass.
enum PathOutcome {
    Refreshed,
    MarkedMissing,
    Unchanged,
}