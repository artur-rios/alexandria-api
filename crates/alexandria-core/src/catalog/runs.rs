use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::catalog::run_registry::{RunPhase, RunProgress};
use crate::errors::DomainError;

/// Which command produced a run (FR-FC-27). The two share a lifecycle but not
/// their tallies, which is why `RunCounts` is per-kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Index,
    Refresh,
}

impl RunKind {
    fn as_str(self) -> &'static str {
        match self {
            RunKind::Index => "index",
            RunKind::Refresh => "refresh",
        }
    }

    fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "index" => Ok(RunKind::Index),
            "refresh" => Ok(RunKind::Refresh),
            other => Err(DomainError::internal(format!("unknown run kind: {other}"))),
        }
    }
}

/// Where a run stands (FR-FC-27, FR-FC-29).
///
/// `Complete` means the walk finished — including when individual files
/// failed, which are counted in the run's own `failed` tally. `Failed` is
/// reserved for a run that could not proceed at all. `Interrupted` is what
/// startup reconciliation leaves behind for a run whose process stopped.
///
/// `Paused` and `Cancelled` are what an owner asked for. `Paused` is the only
/// non-terminal one of the five: the run stopped where it was and can be
/// picked up again, so it carries no `finished_at` and keeps the `phase` that
/// says where it stopped. `Cancelled` is terminal, exactly like `Complete`
/// and `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Complete,
    Failed,
    Interrupted,
    Paused,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Complete => "complete",
            RunStatus::Failed => "failed",
            RunStatus::Interrupted => "interrupted",
            RunStatus::Paused => "paused",
            RunStatus::Cancelled => "cancelled",
        }
    }

    fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "running" => Ok(RunStatus::Running),
            "complete" => Ok(RunStatus::Complete),
            "failed" => Ok(RunStatus::Failed),
            "interrupted" => Ok(RunStatus::Interrupted),
            "paused" => Ok(RunStatus::Paused),
            "cancelled" => Ok(RunStatus::Cancelled),
            other => Err(DomainError::internal(format!(
                "unknown run status: {other}"
            ))),
        }
    }
}

/// A finished run's tally, mirroring `IndexOutcome` / `RefreshOutcome`.
/// Untagged so it flattens into the run body without a discriminator — `kind`
/// already says which shape to expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum RunCounts {
    #[serde(rename_all = "camelCase")]
    Index {
        scanned: usize,
        indexed: usize,
        skipped: usize,
        already_cataloged: usize,
        failed: usize,
    },
    #[serde(rename_all = "camelCase")]
    Refresh {
        refreshed: usize,
        marked_missing: usize,
        unchanged: usize,
        failed: usize,
    },
}

/// One recorded index or re-index run (UC-42 / FR-FC-27).
///
/// Fields that do not apply are omitted from the serialized body rather than
/// sent as `null`: a running run carries no counts and no finish time, a
/// refresh carries no root, and only a failed run carries an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRun {
    #[serde(rename = "runId")]
    pub id: Uuid,
    pub kind: RunKind,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub counts: Option<RunCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Which half of the run is executing (FR-FC-28). `None` for a run that
    /// never published one, and `None` again once the run is terminal:
    /// `status = "complete"` alongside `phase = "processing"` would tell a
    /// client two contradictory things. `total` and `processed` survive that
    /// transition — those are the tally, and they stay true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<RunPhase>,
    /// How many entries the run has to get through, once discovery has
    /// counted them. `None` while discovery is still counting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// How many entries the run has finished with — indexed, skipped, and
    /// failed alike. `None` for a run that never published progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processed: Option<usize>,
    /// How long the run has spent *working*: elapsed wall time (to
    /// `finished_at`, or to now for a run still going) minus the time it
    /// spent paused.
    ///
    /// Computed by `GetRunStatusHandler`, which holds the clock — a
    /// repository has no business asking what time it is, and a running run's
    /// elapsed time is not a stored value. Repository implementations leave
    /// this at 0.
    pub active_millis: i64,
    /// When the run was paused, for a run that is paused right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<DateTime<Utc>>,
    /// Total time the run has spent paused. Not serialized: it is the input
    /// `active_millis` is derived from, and a client holding `activeMillis`
    /// has no use for it.
    #[serde(skip)]
    pub paused_millis: i64,
}

/// Run records repository port (UC-42). Unit-testable against an in-memory
/// fake with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait CatalogRunRepository: Send + Sync {
    /// Open a run's record as `running` (FR-FC-27). Called by `start()`
    /// before it returns the id it minted, so a started run is always a
    /// recorded run.
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Close a run's record as `complete` with its tally. Per-file failures
    /// live inside `counts`; they do not make the run failed.
    ///
    /// Returns `DomainError::internal` if `counts`'s variant does not match
    /// the run's own `RunKind` — writing the wrong variant would leave the
    /// row's real count columns unset, silently masquerading as "no counts
    /// yet" once read back.
    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Close a run's record as `failed` — it could not proceed at all.
    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Stop a run's record short, leaving it resumable.
    ///
    /// The one transition that is *not* terminal, which is why it is the one
    /// that writes neither `finished_at` nor a tally, and the one that leaves
    /// `phase` alone: `finish`, `fail`, and `cancel` clear it because
    /// `status = 'complete'` beside `phase = 'processing'` is a
    /// contradiction, but `status = 'paused'` beside it is exactly the fact a
    /// client needs — the run stopped mid-walk, not mid-discovery.
    ///
    /// `paused_at` is when it stopped: the input a later resume subtracts to
    /// keep `active_millis` honest about time spent working.
    ///
    /// **Conditional on the run still being `running`**, and `Ok(false)` when
    /// it was not. `running` is the only status a pause is ever legal from, so
    /// the condition costs nothing on the legal path — but it is the only
    /// thing standing between a cancel and being silently downgraded. A walk
    /// closes its cell *before* its terminal write, and a `cancel` arriving in
    /// that window finds no live cell, writes `cancelled` directly, and is
    /// then overwritten by the walk's own `pause` — which touches neither
    /// `finished_at` nor `phase`, so the row would end up `paused` with a
    /// finish time already stamped, and a run the owner asked to abandon left
    /// sitting there resumable. The signal's own no-downgrade guard
    /// (`RunCell::raise`) cannot cover this: by then the cell is gone.
    ///
    /// Callers must not ignore the `false`. See `record_halt`, which logs it,
    /// and `RunControlHandler`, which reports it.
    async fn pause(&self, id: Uuid, paused_at: DateTime<Utc>) -> Result<bool, DomainError>;

    /// Close a run's record as `cancelled` — the owner abandoned it.
    ///
    /// Terminal, so it stamps `finished_at` and clears `phase` exactly as
    /// `finish` and `fail` do. The progress columns stay: how far a cancelled
    /// run got is still true, and still worth reporting.
    ///
    /// `counts` is the partial tally the walk had reached, and is kept for the
    /// record — a cancelled run is never resumed, so what it got through is
    /// final, and a client deserves the same four numbers a completed run
    /// gives it rather than only `processed`/`total` from the last flush.
    /// `None` is for the caller that has no tally to offer: the control
    /// handler acting on a run no process is executing, where nobody holds a
    /// partial count. (Pause takes no tally for the opposite reason — a paused
    /// run is resumed and re-walks, so its partial tally is superseded rather
    /// than final.)
    ///
    /// Rejects a `counts` whose variant does not match the run's own kind, for
    /// the reason [`CatalogRunRepository::finish`] does.
    async fn cancel(
        &self,
        id: Uuid,
        counts: Option<RunCounts>,
        cancelled_at: DateTime<Utc>,
    ) -> Result<(), DomainError>;

    /// Flush a run's live progress into its record (FR-FC-28).
    ///
    /// Called periodically while the run executes, not once per entry: the
    /// in-memory cell is authoritative for a live run, and this write only
    /// exists so a run this process is no longer executing can still report
    /// how far it got. A failure is therefore not fatal — see the handlers,
    /// which log it and carry on.
    async fn record_progress(&self, id: Uuid, progress: &RunProgress) -> Result<(), DomainError>;

    /// One run's record, or `None` for an unknown id (UC-42 AF-01).
    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError>;

    /// Mark every run still `running` as `interrupted`, returning how many
    /// were reconciled (FR-FC-29). Runs execute in-process and are never
    /// resumed, so a `running` row seen at startup has no task behind it.
    async fn interrupt_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError>;
}

#[derive(Clone)]
pub struct SqliteCatalogRunRepository {
    pool: SqlitePool,
}

impl SqliteCatalogRunRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// Parse an RFC 3339 column into a `DateTime<Utc>`; a corrupt value is an
/// internal error rather than a silent default.
fn parse_time(raw: &str, column: &str) -> Result<DateTime<Utc>, DomainError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|err| DomainError::internal(format!("corrupt catalog_runs.{column}: {err}")))
}

/// Reject a `finish()` call whose `RunCounts` variant does not match the
/// run's own kind. Writing the wrong variant would silently leave the row's
/// real count columns NULL — this turns that into a loud error instead.
fn check_counts_match_kind(kind: RunKind, counts: &RunCounts) -> Result<(), DomainError> {
    let matches = matches!(
        (kind, counts),
        (RunKind::Index, RunCounts::Index { .. }) | (RunKind::Refresh, RunCounts::Refresh { .. })
    );
    if matches {
        Ok(())
    } else {
        Err(DomainError::internal(format!(
            "counts kind mismatch: run is {:?} but counts are {:?}",
            kind, counts
        )))
    }
}

impl SqliteCatalogRunRepository {
    /// Close a run terminally with its tally.
    ///
    /// Shared by `finish` and `cancel`, which differ only in the status they
    /// write: both are terminal, both stamp `finished_at`, both clear `phase`,
    /// and both keep the four counts. Sharing it is what stops the two from
    /// drifting on any of that.
    async fn close_with_counts(
        &self,
        id: Uuid,
        status: RunStatus,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        // Guard against a caller passing the wrong kind's tally: writing it
        // would leave the row's own kind's columns NULL, and `get` would then
        // report a terminal run with no counts — a corrupted write that looks
        // like "no counts yet" instead of failing loudly.
        let row = sqlx::query("SELECT kind FROM catalog_runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = row {
            let kind = RunKind::parse(&row.try_get::<String, _>("kind")?)?;
            check_counts_match_kind(kind, &counts)?;
        }

        let query = match counts {
            RunCounts::Index {
                scanned,
                indexed,
                skipped,
                already_cataloged,
                failed,
            } => sqlx::query(
                // `phase = NULL` because the run is terminal: a row reading
                // `status = 'complete', phase = 'processing'` tells a client
                // two contradictory things. `total` and `processed` stay —
                // those are the tally, and they remain true.
                "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL, \
                 scanned = ?, indexed = ?, skipped = ?, already_cataloged = ?, failed = ? \
                 WHERE id = ?",
            )
            .bind(status.as_str())
            .bind(finished_at.to_rfc3339())
            // These are file counts from a single walk; a library large
            // enough to overflow `i64` is not reachable, so the narrowing
            // is not checked at runtime.
            .bind(scanned as i64)
            .bind(indexed as i64)
            .bind(skipped as i64)
            .bind(already_cataloged as i64)
            .bind(failed as i64)
            .bind(id.to_string()),
            RunCounts::Refresh {
                refreshed,
                marked_missing,
                unchanged,
                failed,
            } => sqlx::query(
                // `phase = NULL`: terminal, as above.
                "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL, \
                 refreshed = ?, marked_missing = ?, unchanged = ?, failed = ? WHERE id = ?",
            )
            .bind(status.as_str())
            .bind(finished_at.to_rfc3339())
            .bind(refreshed as i64)
            .bind(marked_missing as i64)
            .bind(unchanged as i64)
            .bind(failed as i64)
            .bind(id.to_string()),
        };
        query.execute(&self.pool).await?;
        Ok(())
    }
}

impl CatalogRunRepository for SqliteCatalogRunRepository {
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO catalog_runs (id, kind, status, root, started_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(kind.as_str())
        .bind(RunStatus::Running.as_str())
        .bind(root)
        .bind(started_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        self.close_with_counts(id, RunStatus::Complete, counts, finished_at)
            .await
    }

    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE catalog_runs SET status = ?, finished_at = ?, error = ?, phase = NULL \
             WHERE id = ?",
        )
        .bind(RunStatus::Failed.as_str())
        .bind(finished_at.to_rfc3339())
        .bind(error)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pause(&self, id: Uuid, paused_at: DateTime<Utc>) -> Result<bool, DomainError> {
        // No `phase = NULL` here, unlike every other transition below and
        // above: a paused run is not terminal, so its phase is not a
        // contradiction but the very thing that says where it stopped.
        // No `finished_at` either — it has not finished.
        //
        // `AND status = 'running'` is the guard the trait doc explains: a
        // pause is only ever legal from `running`, and without the condition
        // this write would overwrite a `cancelled` row that landed while the
        // walk was between closing its cell and recording itself — leaving
        // `paused` beside a `finished_at` the pause never wrote, and a run the
        // owner abandoned looking resumable.
        let result = sqlx::query(
            "UPDATE catalog_runs SET status = ?, paused_at = ? WHERE id = ? AND status = ?",
        )
        .bind(RunStatus::Paused.as_str())
        .bind(paused_at.to_rfc3339())
        .bind(id.to_string())
        .bind(RunStatus::Running.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel(
        &self,
        id: Uuid,
        counts: Option<RunCounts>,
        cancelled_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let Some(counts) = counts else {
            // No tally on offer — the control handler acting on a run no
            // process is executing. `phase = NULL`: terminal, exactly as in
            // `finish` and `fail`.
            sqlx::query(
                "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL WHERE id = ?",
            )
            .bind(RunStatus::Cancelled.as_str())
            .bind(cancelled_at.to_rfc3339())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
            return Ok(());
        };
        // A cancelled run is never resumed, so the tally it reached is final —
        // it is kept for exactly the reason a completed run's is.
        self.close_with_counts(id, RunStatus::Cancelled, counts, cancelled_at)
            .await
    }

    async fn record_progress(&self, id: Uuid, progress: &RunProgress) -> Result<(), DomainError> {
        sqlx::query("UPDATE catalog_runs SET phase = ?, total = ?, processed = ? WHERE id = ?")
            .bind(progress.phase.as_str())
            // File counts from a single walk; a library large enough to
            // overflow `i64` is not reachable, so the narrowing is unchecked
            // exactly as it is in `finish`.
            .bind(progress.total.map(|total| total as i64))
            .bind(progress.processed as i64)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        let row = sqlx::query(
            "SELECT kind, status, root, started_at, finished_at, scanned, indexed, \
             skipped, already_cataloged, refreshed, marked_missing, unchanged, failed, error, \
             phase, total, processed, paused_at, paused_millis \
             FROM catalog_runs WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let kind = RunKind::parse(&row.try_get::<String, _>("kind")?)?;
        let status = RunStatus::parse(&row.try_get::<String, _>("status")?)?;
        let started_at = parse_time(&row.try_get::<String, _>("started_at")?, "started_at")?;
        let finished_at = row
            .try_get::<Option<String>, _>("finished_at")?
            .map(|raw| parse_time(&raw, "finished_at"))
            .transpose()?;

        // Counts exist only once a walk has finished. Presence of the first
        // column of the kind's set decides — `finish` writes them together.
        // The reverse narrowing: these columns hold file counts a single walk
        // produced, so a stored value large enough to overflow `usize` on a
        // 32-bit target is not reachable, and the cast is not checked.
        let counts = match kind {
            RunKind::Index => row
                .try_get::<Option<i64>, _>("scanned")?
                .map(|scanned| -> Result<RunCounts, DomainError> {
                    Ok(RunCounts::Index {
                        scanned: scanned as usize,
                        indexed: row.try_get::<i64, _>("indexed")? as usize,
                        skipped: row.try_get::<i64, _>("skipped")? as usize,
                        already_cataloged: row.try_get::<i64, _>("already_cataloged")? as usize,
                        failed: row.try_get::<i64, _>("failed")? as usize,
                    })
                })
                .transpose()?,
            RunKind::Refresh => row
                .try_get::<Option<i64>, _>("refreshed")?
                .map(|refreshed| -> Result<RunCounts, DomainError> {
                    Ok(RunCounts::Refresh {
                        refreshed: refreshed as usize,
                        marked_missing: row.try_get::<i64, _>("marked_missing")? as usize,
                        unchanged: row.try_get::<i64, _>("unchanged")? as usize,
                        failed: row.try_get::<i64, _>("failed")? as usize,
                    })
                })
                .transpose()?,
        };

        // The last flushed progress (FR-FC-28). A stored `phase` that parses
        // to nothing is dropped rather than failing the read: progress is a
        // display field, and refusing to answer at all would be a worse
        // outcome than answering without it.
        let phase = row
            .try_get::<Option<String>, _>("phase")?
            .as_deref()
            .and_then(RunPhase::parse);
        let paused_at = row
            .try_get::<Option<String>, _>("paused_at")?
            .map(|raw| parse_time(&raw, "paused_at"))
            .transpose()?;

        Ok(Some(CatalogRun {
            id,
            kind,
            status,
            root: row.try_get("root")?,
            started_at,
            finished_at,
            counts,
            error: row.try_get("error")?,
            phase,
            total: row
                .try_get::<Option<i64>, _>("total")?
                .map(|total| total as usize),
            processed: row
                .try_get::<Option<i64>, _>("processed")?
                .map(|processed| processed as usize),
            // Derived by `GetRunStatusHandler`, which holds the clock.
            active_millis: 0,
            paused_at,
            paused_millis: row.try_get("paused_millis")?,
        }))
    }

    async fn interrupt_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError> {
        let result = sqlx::query(
            "UPDATE catalog_runs SET status = ?, finished_at = ?, phase = NULL \
                 WHERE status = ?",
        )
        .bind(RunStatus::Interrupted.as_str())
        .bind(now.to_rfc3339())
        .bind(RunStatus::Running.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
