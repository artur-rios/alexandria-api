use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::File;
use crate::catalog::repos::CatalogRepository;
use crate::catalog::runs::{CatalogRunRepository, RunCounts, RunKind};
use crate::errors::DomainError;
use crate::retry::{retry_on_busy, BUSY_ATTEMPTS};

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
/// Like `IndexHandler`, `execute` processes up to `concurrency` cataloged
/// paths at a time (`indexing.concurrency`, the same setting — a re-index is
/// the same hash-every-file workload as an index, so splitting the two knobs
/// would only invite them to disagree).
///
/// Generic over collaborators so the decision logic is unit-tested against
/// trait fakes with no real DB / filesystem / auth service (Testing Spec §6.2).
pub struct RefreshHandler<A, R, F, C, RR> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    concurrency: usize,
    runs: RR,
}

impl<A, R, F, C, RR> RefreshHandler<A, R, F, C, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    RR: CatalogRunRepository,
{
    /// `concurrency` is how many cataloged paths `execute` refreshes at a
    /// time; zero is clamped to 1, as in `IndexHandler::new`.
    pub fn new(auth: A, repo: R, fs: F, clock: C, concurrency: u32, runs: RR) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            concurrency: concurrency.max(1) as usize,
            runs,
        }
    }

    /// Authenticate and return a run id. No input to validate (re-index
    /// touches every cataloged path), so the only failure is AF-02.
    ///
    /// FR-FC-27: a started run is always a recorded run — the record opens
    /// here, where the id is minted, so no caller can start one without it.
    /// Unlike `finish`/`fail`, this write's failure is not swallowed after
    /// retrying: if the record cannot be opened at all, the caller must not
    /// receive a run id it can never query.
    pub async fn start(&self, token: &str) -> Result<RefreshStarted, DomainError> {
        self.auth.authenticate(token).await?;
        let run_id = Uuid::new_v4();
        let started_at = self.clock.now();
        retry_on_busy(BUSY_ATTEMPTS, || {
            self.runs.start(run_id, RunKind::Refresh, None, started_at)
        })
        .await?;
        Ok(RefreshStarted { run_id })
    }

    /// Walk every cataloged path and refresh / mark missing.
    ///
    /// Up to `concurrency` paths are in flight at once, so the order they are
    /// visited in is unspecified. Each path's outcome depends only on that
    /// path's own row and its own bytes, so the tallies do not depend on the
    /// order — every row contributes exactly one outcome.
    ///
    /// A failure that concerns one specific file — its bytes cannot be read, or
    /// a repository write for it fails — is counted in `failed`, logged at
    /// `warn`, and the walk continues. One locked file must not abandon the
    /// rest of the catalog. Only a failure to list the catalog at all aborts.
    pub async fn execute(&self, run_id: Uuid) -> Result<RefreshOutcome, DomainError> {
        let now = self.clock.now();
        let files = match self.repo.list_all().await {
            Ok(files) => files,
            Err(err) => {
                // FR-FC-27: the walk could not proceed at all — that, and
                // only that, is a `failed` run.
                let fail_error = err.to_string();
                let failed_at = self.clock.now();
                if let Err(record_err) = retry_on_busy(BUSY_ATTEMPTS, || {
                    self.runs.fail(run_id, &fail_error, failed_at)
                })
                .await
                {
                    // The walk's own error is the one that matters to the
                    // caller — a bookkeeping failure on top of it must not
                    // replace it. The record stays `running` until startup
                    // reconciliation (FR-FC-29) closes it.
                    tracing::warn!(%run_id, error = %record_err, "could not record run failure");
                }
                return Err(err);
            }
        };

        let (refreshed, marked_missing, unchanged, failed) = stream::iter(files)
            .map(|file| async move {
                match self.refresh_one(&file, now).await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        tracing::warn!(
                            %run_id,
                            path = %file.path,
                            error = %err,
                            "skipping cataloged path that could not be refreshed"
                        );
                        PathOutcome::Failed
                    }
                }
            })
            .buffer_unordered(self.concurrency)
            .fold(
                (0usize, 0usize, 0usize, 0usize),
                |counts, outcome| async move {
                    let (refreshed, marked_missing, unchanged, failed) = counts;
                    match outcome {
                        PathOutcome::Refreshed => {
                            (refreshed + 1, marked_missing, unchanged, failed)
                        }
                        PathOutcome::MarkedMissing => {
                            (refreshed, marked_missing + 1, unchanged, failed)
                        }
                        PathOutcome::Unchanged => {
                            (refreshed, marked_missing, unchanged + 1, failed)
                        }
                        PathOutcome::Failed => (refreshed, marked_missing, unchanged, failed + 1),
                    }
                },
            )
            .await;

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
        // FR-FC-27: the walk finished. Per-file failures are inside the
        // tally and do not make the run failed.
        let finished_at = self.clock.now();
        let counts = RunCounts::Refresh {
            refreshed,
            marked_missing,
            unchanged,
            failed,
        };
        if let Err(err) = retry_on_busy(BUSY_ATTEMPTS, || {
            self.runs.finish(run_id, counts, finished_at)
        })
        .await
        {
            // FR-FC-27: the walk succeeded — only the bookkeeping write
            // failed. Reporting the run as failed would be a lie about work
            // that did happen, and the tally is already in the log line
            // above. The record stays `running` until startup
            // reconciliation (FR-FC-29) closes it.
            tracing::warn!(%run_id, error = %err, "could not record run completion");
        }
        Ok(outcome)
    }

    /// Refresh one cataloged path. `Err` means this one file failed — the
    /// caller counts it and moves on to the rest of the catalog.
    ///
    /// Both writes are wrapped in [`retry_on_busy`], for exactly the reason
    /// UC-01's `insert_file` is: this walk runs `concurrency` writers against
    /// SQLite's single writer while a client reads throughout, and a writer
    /// that waits out its whole `busy_timeout` is answered `SQLITE_BUSY`. Left
    /// unretried, that transient contention becomes a `failed` count — a
    /// re-index silently leaving a stale hash or an unmarked missing file
    /// behind, which is worse here than at first index, since nothing else
    /// will revisit that row until the next run.
    async fn refresh_one(
        &self,
        file: &File,
        now: DateTime<Utc>,
    ) -> Result<PathOutcome, DomainError> {
        if self.fs.path_exists(&file.path).await {
            let new_hash = self.fs.content_hash(&file.path).await?;
            // Task 3 made `content_hash` nullable and stopped indexing from
            // computing it, so `file.content_hash` is `Some` only for a file
            // UC-33 has edited. `None` means "unknown", which must count as
            // changed — treating it as equal-to-anything would let a freshly
            // indexed file's real hash go unrecorded forever, since refresh
            // is the only other path that still hashes (until Task 4 stops
            // that too).
            if file.content_hash.as_deref() != Some(new_hash.as_str()) || file.missing_at.is_some()
            {
                retry_on_busy(BUSY_ATTEMPTS, || {
                    self.repo.refresh_hash(&file.path, &new_hash, now)
                })
                .await?;
                Ok(PathOutcome::Refreshed)
            } else {
                Ok(PathOutcome::Unchanged)
            }
        } else if file.missing_at.is_none() {
            retry_on_busy(BUSY_ATTEMPTS, || self.repo.mark_missing(&file.path, now)).await?;
            Ok(PathOutcome::MarkedMissing)
        } else {
            // Already marked missing and still gone — leave as-is.
            Ok(PathOutcome::Unchanged)
        }
    }
}

/// What a single cataloged path resolved to during a refresh pass.
/// `Failed` is produced by `execute` after it logs the path's error, so the
/// concurrent walk can tally outcomes without sharing a counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOutcome {
    Refreshed,
    MarkedMissing,
    Unchanged,
    Failed,
}
