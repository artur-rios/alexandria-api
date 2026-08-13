use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Complete,
    Failed,
    Interrupted,
}

impl RunStatus {
    // Unused within this task (the SQLite adapter writes status literals
    // directly in its queries), kept for symmetry with `RunKind::as_str` and
    // for later tasks (HTTP/FFI surfaces) that need the raw string form.
    #[allow(dead_code)]
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Complete => "complete",
            RunStatus::Failed => "failed",
            RunStatus::Interrupted => "interrupted",
        }
    }

    fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            "running" => Ok(RunStatus::Running),
            "complete" => Ok(RunStatus::Complete),
            "failed" => Ok(RunStatus::Failed),
            "interrupted" => Ok(RunStatus::Interrupted),
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
             VALUES (?, ?, 'running', ?, ?)",
        )
        .bind(id.to_string())
        .bind(kind.as_str())
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
        let query = match counts {
            RunCounts::Index {
                scanned,
                indexed,
                skipped,
                failed,
            } => sqlx::query(
                "UPDATE catalog_runs SET status = 'complete', finished_at = ?, \
                 scanned = ?, indexed = ?, skipped = ?, failed = ? WHERE id = ?",
            )
            .bind(finished_at.to_rfc3339())
            .bind(scanned as i64)
            .bind(indexed as i64)
            .bind(skipped as i64)
            .bind(failed as i64)
            .bind(id.to_string()),
            RunCounts::Refresh {
                refreshed,
                marked_missing,
                unchanged,
                failed,
            } => sqlx::query(
                "UPDATE catalog_runs SET status = 'complete', finished_at = ?, \
                 refreshed = ?, marked_missing = ?, unchanged = ?, failed = ? WHERE id = ?",
            )
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

    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        sqlx::query(
            "UPDATE catalog_runs SET status = 'failed', finished_at = ?, error = ? WHERE id = ?",
        )
        .bind(finished_at.to_rfc3339())
        .bind(error)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        let row = sqlx::query(
            "SELECT kind, status, root, started_at, finished_at, scanned, indexed, \
             skipped, refreshed, marked_missing, unchanged, failed, error \
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
        let counts = match kind {
            RunKind::Index => row
                .try_get::<Option<i64>, _>("scanned")?
                .map(|scanned| -> Result<RunCounts, DomainError> {
                    Ok(RunCounts::Index {
                        scanned: scanned as usize,
                        indexed: row.try_get::<i64, _>("indexed")? as usize,
                        skipped: row.try_get::<i64, _>("skipped")? as usize,
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

        Ok(Some(CatalogRun {
            id,
            kind,
            status,
            root: row.try_get("root")?,
            started_at,
            finished_at,
            counts,
            error: row.try_get("error")?,
        }))
    }

    async fn interrupt_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError> {
        let result = sqlx::query(
            "UPDATE catalog_runs SET status = 'interrupted', finished_at = ? \
             WHERE status = 'running'",
        )
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
