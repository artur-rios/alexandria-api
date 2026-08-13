# Catalog Run Status (UC-42) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist every index and re-index run and expose its status and outcome to an authenticated caller over both HTTP and FFI, so a client can tell a finished run from a half-finished one.

**Architecture:** Alexandria is a Rust workspace of three crates. `alexandria-core` holds the domain: Command handlers generic over trait "ports" (repositories, filesystem, clock, auth service), so their decision logic is unit-tested against in-memory fakes with no database. `alexandria-http` (axum) and `alexandria-ffi` (C ABI, JSON in / JSON out) are thin transports over the same handler instances, wired once in `alexandria-core/src/services.rs`. Every use case must be reachable and behave identically from both transports (FR-FC-24, "parity"). This feature adds one table, one module, one query handler, one route, one FFI export — and threads a run-repository collaborator through the two existing indexing handlers.

**Tech Stack:** Rust 2021, tokio, axum, sqlx + SQLite, chrono, uuid, serde/serde_json, thiserror, futures-util, cbindgen (generates the C header at build time).

**Spec:** [`docs/superpowers/specs/2026-08-13-catalog-run-status-design.md`](../specs/2026-08-13-catalog-run-status-design.md). Read it before starting — it carries the reasoning behind every decision below. Tracked by issue #99.

## Global Constraints

- **Branch:** all work goes on `feat/uc-42-run-status`, already cut from `main`. Never commit to `main`; never merge the PR yourself (Development Workflow §5 Step 7).
- **Runs stay asynchronous.** FR-FC-08 is unchanged. Starting a run still answers immediately with `202` / a run id. This makes a run *observable*, never synchronous. Do not add an await-until-done option.
- **A run whose walk completes is `complete`, even with per-file failures.** Those are counted in the run's `failed` tally. `failed` is reserved for a run that could not proceed at all — the catalog was unlistable, or the root could not be walked. Implementing this backwards is the single most likely defect in this plan.
- **Status values, verbatim:** `running`, `complete`, `failed`, `interrupted`. **Kind values, verbatim:** `index`, `refresh`.
- **Dual-surface parity** (FR-FC-24): the FFI export must return the same JSON body as the HTTP route.
- **`crates/alexandria-ffi/src/header.h` is generated and git-ignored.** `build.rs` regenerates it via cbindgen and `.gitignore` excludes it — it is not tracked. Never hand-edit it, and never `git add` it. Regenerate it to *verify* the new export is exposed; that verification is the deliverable, not a commit.
- **Test naming:** `given_<condition>_when_<action>_then_<outcome>`, as every existing test in this repo does.
- **House style:** substantial doc comments explaining *why*, citing requirement ids (FR-FC-xx) and use case ids (UC-xx) and alternative-flow ids (AF-xx).
- **Full suite green before the PR:** `cargo test` from the workspace root, plus `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`.
- **Running the tests:** a workspace-wide `cargo test` can exceed ten minutes on a cold build. Run commands in the foreground with the longest timeout available; if `cargo test` times out, run the three crates separately (`cargo test -p alexandria-core`, `-p alexandria-http`, `-p alexandria-ffi`) and say so in the report. That fallback is accepted.

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql` | *(create)* The `catalog_runs` table. |
| `crates/alexandria-core/src/catalog/runs.rs` | *(create)* Run model, `CatalogRunRepository` port, and the SQLite adapter — the shape `auth/local.rs` uses. Deliberately not in `repos.rs`, which is already 1,200 lines. |
| `crates/alexandria-core/src/catalog/mod.rs` | *(modify)* Export `runs`. |
| `crates/alexandria-core/src/catalog/commands/index.rs` | *(modify)* Takes the run repository; `start` opens the record, `execute` closes it. |
| `crates/alexandria-core/src/catalog/commands/refresh.rs` | *(modify)* Same. |
| `crates/alexandria-core/src/catalog/queries/run_status.rs` | *(create)* `GetRunStatusHandler` (UC-42). |
| `crates/alexandria-core/src/services.rs` | *(modify)* Wire the run repository, the new handler, and startup reconciliation. |
| `crates/alexandria-core/tests/common/mod.rs` | *(modify)* `FakeCatalogRunRepository`. |
| `crates/alexandria-core/tests/catalog/runs.rs` | *(create)* Repository and lifecycle tests. |
| `crates/alexandria-core/tests/catalog/run_status.rs` | *(create)* Query handler tests. |
| `crates/alexandria-http/src/routes/runs.rs` | *(create)* `GET /v1/index/runs/{runId}`. |
| `crates/alexandria-http/src/lib.rs` | *(modify)* Route registration, inside the auth gate. |
| `crates/alexandria-http/tests/run_status_api.rs` | *(create)* Integration tests. |
| `crates/alexandria-ffi/src/lib.rs` | *(modify)* `RunJsonResult`, `alexandria_index_run_status_json`. |
| `crates/alexandria-ffi/tests/parity.rs` | *(modify)* UC-42 parity test. |

---

### Task 1: The runs table, model, and repository

The data layer. Everything else builds on the port defined here.

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql`
- Create: `crates/alexandria-core/src/catalog/runs.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`
- Modify: `crates/alexandria-core/tests/common/mod.rs`
- Create: `crates/alexandria-core/tests/catalog/runs.rs`
- Modify: `crates/alexandria-core/tests/catalog.rs`

**Interfaces:**
- Consumes: `DomainError`, `Clock` (both already in the crate).
- Produces, all in `alexandria_core::catalog::runs`:
  - `RunKind` — `Index` | `Refresh`, serialized `"index"` / `"refresh"`.
  - `RunStatus` — `Running` | `Complete` | `Failed` | `Interrupted`, serialized lowercase.
  - `RunCounts` — `Index { scanned, indexed, skipped, failed }` | `Refresh { refreshed, marked_missing, unchanged, failed }`, all `usize`.
  - `CatalogRun { id: Uuid, kind: RunKind, status: RunStatus, root: Option<String>, started_at: DateTime<Utc>, finished_at: Option<DateTime<Utc>>, counts: Option<RunCounts>, error: Option<String> }`.
  - `trait CatalogRunRepository` with `start`, `finish`, `fail`, `get`, `interrupt_running` (exact signatures in Step 4).
  - `SqliteCatalogRunRepository::new(pool: SqlitePool)`.
  - `crate::common::FakeCatalogRunRepository` in the core test harness.

- [ ] **Step 1: Write the migration**

Create `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql`:

```sql
-- UC-42: the lifecycle and outcome of each UC-01 index and UC-02 re-index run
-- (FR-FC-27). `start()` mints a run id and the caller is handed it; before
-- this table nothing recorded what became of that run, so a client could not
-- tell a finished run from one still walking.
--
-- Counts are per-kind and nullable rather than a shared generic set: `scanned`
-- is meaningless for a refresh and `marked_missing` for an index, and they are
-- unknown for either until the walk finishes.
CREATE TABLE IF NOT EXISTS catalog_runs (
    id              TEXT PRIMARY KEY,
    kind            TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    root            TEXT,
    started_at      TEXT    NOT NULL,
    finished_at     TEXT,
    scanned         INTEGER,
    indexed         INTEGER,
    skipped         INTEGER,
    refreshed       INTEGER,
    marked_missing  INTEGER,
    unchanged       INTEGER,
    failed          INTEGER,
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_catalog_runs_started_at ON catalog_runs (started_at);
```

- [ ] **Step 2: Register the test module**

Add to the end of `crates/alexandria-core/tests/catalog.rs`:

```rust
#[path = "catalog/runs.rs"]
mod runs;
```

- [ ] **Step 3: Write the failing tests**

Create `crates/alexandria-core/tests/catalog/runs.rs`:

```rust
//! Tests for the UC-42 run record: the SQLite repository against a real
//! migrated database (Testing Specification §6.4), covering each lifecycle
//! transition and the startup reconciliation FR-FC-29 requires.

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunCounts, RunKind, RunStatus, SqliteCatalogRunRepository,
};
use alexandria_core::db::migrate_database;

async fn repo() -> (SqliteCatalogRunRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");
    (SqliteCatalogRunRepository::new(pool), dir)
}

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

#[tokio::test]
async fn given_a_started_run_when_read_then_it_is_running_with_no_counts() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();

    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.id, id);
    assert_eq!(run.kind, RunKind::Index);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.root.as_deref(), Some("/library"));
    assert_eq!(run.started_at, t(1));
    assert!(run.finished_at.is_none(), "a running run has not finished");
    assert!(run.counts.is_none(), "no tally exists until the walk ends");
    assert!(run.error.is_none());
}

#[tokio::test]
async fn given_a_running_index_run_when_finished_then_it_is_complete_with_its_counts() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start");

    repo.finish(
        id,
        RunCounts::Index {
            scanned: 10,
            indexed: 7,
            skipped: 2,
            failed: 1,
        },
        t(2),
    )
    .await
    .expect("finish");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(run.finished_at, Some(t(2)));
    assert_eq!(
        run.counts,
        Some(RunCounts::Index {
            scanned: 10,
            indexed: 7,
            skipped: 2,
            failed: 1
        })
    );
    assert!(run.error.is_none());
}

#[tokio::test]
async fn given_a_run_with_per_file_failures_when_finished_then_it_is_complete_not_failed() {
    // FR-FC-27: one unreadable file must not make the whole run a failure.
    // `failed` counts them; the run still completed its walk.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    repo.finish(
        id,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 3,
            failed: 5,
        },
        t(2),
    )
    .await
    .expect("finish");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(
        run.status,
        RunStatus::Complete,
        "per-file failures do not make the run failed"
    );
}

#[tokio::test]
async fn given_a_running_refresh_run_when_failed_then_it_carries_the_error() {
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    repo.fail(id, "catalog unreadable", t(2))
        .await
        .expect("fail");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("catalog unreadable"));
    assert_eq!(run.finished_at, Some(t(2)));
    assert!(run.counts.is_none(), "a run that could not proceed has no tally");
}

#[tokio::test]
async fn given_a_refresh_run_when_started_then_it_has_no_root() {
    // A refresh touches every cataloged path and takes no root.
    let (repo, _dir) = repo().await;
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.kind, RunKind::Refresh);
    assert!(run.root.is_none());
}

#[tokio::test]
async fn given_an_unknown_id_when_read_then_none() {
    // UC-42 AF-01.
    let (repo, _dir) = repo().await;
    assert!(repo.get(Uuid::new_v4()).await.expect("get").is_none());
}

#[tokio::test]
async fn given_running_and_terminal_runs_when_reconciled_then_only_running_becomes_interrupted() {
    // FR-FC-29: runs execute in-process and are never resumed, so a row still
    // `running` at startup provably has no task behind it. Terminal rows must
    // be left exactly as they are.
    let (repo, _dir) = repo().await;
    let running = Uuid::new_v4();
    let completed = Uuid::new_v4();
    let failed = Uuid::new_v4();

    repo.start(running, RunKind::Index, Some("/library"), t(1))
        .await
        .expect("start running");
    repo.start(completed, RunKind::Refresh, None, t(1))
        .await
        .expect("start completed");
    repo.finish(
        completed,
        RunCounts::Refresh {
            refreshed: 1,
            marked_missing: 0,
            unchanged: 0,
            failed: 0,
        },
        t(2),
    )
    .await
    .expect("finish");
    repo.start(failed, RunKind::Refresh, None, t(1))
        .await
        .expect("start failed");
    repo.fail(failed, "catalog unreadable", t(2))
        .await
        .expect("fail");

    let reconciled = repo.interrupt_running(t(3)).await.expect("interrupt");

    assert_eq!(reconciled, 1, "only the running row is reconciled");
    let run = repo.get(running).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.finished_at, Some(t(3)));
    assert_eq!(
        repo.get(completed).await.unwrap().unwrap().status,
        RunStatus::Complete
    );
    assert_eq!(
        repo.get(failed).await.unwrap().unwrap().status,
        RunStatus::Failed
    );
}

#[tokio::test]
async fn given_no_running_runs_when_reconciled_then_nothing_changes() {
    let (repo, _dir) = repo().await;
    assert_eq!(repo.interrupt_running(t(3)).await.expect("interrupt"), 0);
}
```

If `alexandria_core::db::migrate_database` is not the correct path for the migration helper, find it with `grep -rn "pub async fn migrate_database" crates/alexandria-core/src` and use the real one — the other core integration tests import it, so copy their import line.

- [ ] **Step 4: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test catalog runs
```

Expected: FAIL to compile — `unresolved import ... catalog::runs`.

- [ ] **Step 5: Write the module**

Create `crates/alexandria-core/src/catalog/runs.rs`:

```rust
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
            other => Err(DomainError::internal(format!("unknown run status: {other}"))),
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
```

- [ ] **Step 6: Export the module**

In `crates/alexandria-core/src/catalog/mod.rs`, add `pub mod runs;` beside the other module declarations, keeping the list's existing order convention.

- [ ] **Step 7: Add the in-memory fake**

Append to `crates/alexandria-core/tests/common/mod.rs`. Add whatever imports it needs to the file's existing `use` block:

```rust
/// In-memory `CatalogRunRepository` (UC-42). Lets the index/refresh handler
/// tests assert the run lifecycle without a database.
#[derive(Debug, Default, Clone)]
pub struct FakeCatalogRunRepository {
    runs: Arc<Mutex<HashMap<Uuid, CatalogRun>>>,
}

impl FakeCatalogRunRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded run for `id`, for assertions.
    pub fn get_recorded(&self, id: Uuid) -> Option<CatalogRun> {
        self.runs.lock().unwrap().get(&id).cloned()
    }

    pub fn count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
}

impl CatalogRunRepository for FakeCatalogRunRepository {
    async fn start(
        &self,
        id: Uuid,
        kind: RunKind,
        root: Option<&str>,
        started_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        self.runs.lock().unwrap().insert(
            id,
            CatalogRun {
                id,
                kind,
                status: RunStatus::Running,
                root: root.map(str::to_string),
                started_at,
                finished_at: None,
                counts: None,
                error: None,
            },
        );
        Ok(())
    }

    async fn finish(
        &self,
        id: Uuid,
        counts: RunCounts,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.get_mut(&id) {
            run.status = RunStatus::Complete;
            run.counts = Some(counts);
            run.finished_at = Some(finished_at);
        }
        Ok(())
    }

    async fn fail(
        &self,
        id: Uuid,
        error: &str,
        finished_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.get_mut(&id) {
            run.status = RunStatus::Failed;
            run.error = Some(error.to_string());
            run.finished_at = Some(finished_at);
        }
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<CatalogRun>, DomainError> {
        Ok(self.runs.lock().unwrap().get(&id).cloned())
    }

    async fn interrupt_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError> {
        let mut runs = self.runs.lock().unwrap();
        let mut reconciled = 0;
        for run in runs.values_mut() {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Interrupted;
                run.finished_at = Some(now);
                reconciled += 1;
            }
        }
        Ok(reconciled)
    }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

```bash
cargo test -p alexandria-core --test catalog runs
```

Expected: PASS, 8 tests.

- [ ] **Step 9: Format, lint, and run the core suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p alexandria-core
```

Expected: clean, green.

- [ ] **Step 10: Commit**

```bash
git add crates/alexandria-core/migrations/00000000000011_catalog_runs.sql crates/alexandria-core/src/catalog/runs.rs crates/alexandria-core/src/catalog/mod.rs crates/alexandria-core/tests/common/mod.rs crates/alexandria-core/tests/catalog/runs.rs crates/alexandria-core/tests/catalog.rs
git commit -m "feat: record index and refresh runs (FR-FC-27)"
```

---

### Task 2: The indexing handlers record their runs

Threads the run repository through `IndexHandler` and `RefreshHandler`. This is the task with mechanical churn: both constructors gain a parameter, and ten call sites must be updated.

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/src/catalog/commands/refresh.rs`
- Modify: `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/refresh.rs`
- Modify: `crates/alexandria-core/tests/throughput.rs`

**Interfaces:**
- Consumes: `CatalogRunRepository`, `RunKind`, `RunCounts`, `FakeCatalogRunRepository` (Task 1).
- Produces:
  - `IndexHandler<A, R, F, C, RR>::new(auth, repo, fs, clock, concurrency, runs)` — the run repository is the **last** parameter, so existing argument order is untouched.
  - `RefreshHandler<A, R, F, C, RR>::new(auth, repo, fs, clock, concurrency, runs)` — same.
  - Both `execute` signatures are unchanged.
  - `DefaultIndexHandler` / `DefaultRefreshHandler` aliases in `services.rs` gain `SqliteCatalogRunRepository`.

- [ ] **Step 1: Extend the test helpers**

`crates/alexandria-core/tests/catalog/refresh.rs` builds handlers through a helper at the top of the file:

```rust
fn refresh_handler<A, R, F, C>(auth: A, repo: R, fs: F, clock: C) -> RefreshHandler<A, R, F, C>
```

Give it the run repository as a fifth parameter and a fifth type parameter, so every existing call site passes one explicitly:

```rust
fn refresh_handler<A, R, F, C, RR>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    runs: RR,
) -> RefreshHandler<A, R, F, C, RR>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    RR: CatalogRunRepository,
{
    RefreshHandler::new(auth, repo, fs, clock, TEST_CONCURRENCY, runs)
}
```

Every existing caller in that file gains `FakeCatalogRunRepository::new()` as its last argument. `crates/alexandria-core/tests/catalog/index.rs` has the equivalent helpers `handler(...)` and `handler_with_library_root(...)` — extend both the same way. The file's token and root constants are `TOKEN` and `ROOT`.

- [ ] **Step 2: Write the failing tests**

Append to `crates/alexandria-core/tests/catalog/refresh.rs`, reusing the fixtures the file's existing happy-path tests build:

```rust
#[tokio::test]
async fn given_a_started_refresh_when_started_then_the_run_is_recorded_running() {
    let runs = FakeCatalogRunRepository::new();
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        FakeFilesystem::builder().build(),
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler.start(TOKEN).await.expect("start");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.kind, RunKind::Refresh);
    assert_eq!(recorded.status, RunStatus::Running);
    assert!(recorded.root.is_none(), "a refresh takes no root");
}

#[tokio::test]
async fn given_a_refresh_that_walks_when_executed_then_the_run_is_recorded_complete() {
    let runs = FakeCatalogRunRepository::new();
    // Seed the catalog and filesystem the way this file's existing
    // "refreshes a changed file" test does — reuse its fixture builders
    // rather than inventing new ones.
    let handler = refresh_handler(
        FakeAuth::Allowing,
        seeded_catalog_repo(),
        seeded_filesystem(),
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler.start(TOKEN).await.expect("start");
    let outcome = handler.execute(started.run_id).await.expect("execute");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Complete);
    assert_eq!(
        recorded.counts,
        Some(RunCounts::Refresh {
            refreshed: outcome.refreshed,
            marked_missing: outcome.marked_missing,
            unchanged: outcome.unchanged,
            failed: outcome.failed,
        }),
        "the recorded tally is the outcome the walk computed"
    );
}

#[tokio::test]
async fn given_a_catalog_that_cannot_be_listed_when_executed_then_the_run_is_recorded_failed() {
    // FR-FC-27: this is the only case that makes a run `failed` — the walk
    // could not proceed at all.
    let runs = FakeCatalogRunRepository::new();
    // A catalog repository whose `list_all` errors. If `tests/common/mod.rs`
    // has no such fake, add a minimal `FailingCatalogRepository` beside
    // `FailingSessionRepository`, returning `DomainError::Disk` from
    // `list_all` and `unimplemented!()` from the methods this path never
    // reaches.
    let handler = refresh_handler(
        FakeAuth::Allowing,
        FailingCatalogRepository,
        FakeFilesystem::builder().build(),
        fixed_clock(now()),
        runs.clone(),
    );

    let started = handler.start(TOKEN).await.expect("start");
    let err = handler.execute(started.run_id).await.expect_err("must fail");

    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Failed);
    assert!(
        recorded.error.is_some(),
        "a failed run carries the underlying error"
    );
    assert!(recorded.counts.is_none());
    let _ = err;
}
```

Then add the equivalent three to `crates/alexandria-core/tests/catalog/index.rs`, asserting `RunKind::Index`, `recorded.root == Some(root)`, and `RunCounts::Index { scanned, indexed, skipped, failed }` taken from the outcome. For the failure case, drive it with a filesystem fake whose `list_files` returns `Err`.

Also add, to the index test file, the case the Global Constraints call out as the likeliest defect:

```rust
#[tokio::test]
async fn given_files_that_individually_fail_when_executed_then_the_run_is_complete_not_failed() {
    // FR-FC-27: per-file failures are counted, not escalated. One unreadable
    // file must not report the whole run as failed.
    let runs = FakeCatalogRunRepository::new();
    // Reuse the fixture from this file's existing test that asserts a
    // non-zero `failed` count — find it by searching the file for `failed`
    // in an assertion. It must produce at least one entry that fails and one
    // that succeeds.
    let handler = handler(/* that test's collaborators */, runs.clone());

    let started = handler
        .start(IndexRequest { root: ROOT.to_string() }, TOKEN)
        .await
        .expect("start");
    let outcome = handler.execute(ROOT, started.run_id).await.expect("execute");

    assert!(outcome.failed > 0, "the fixture must produce a per-file failure");
    let recorded = runs.get_recorded(started.run_id).expect("run recorded");
    assert_eq!(recorded.status, RunStatus::Complete);
}
```

Where a comment above says to reuse an existing fixture, do exactly that — these test files build their collaborators through per-test builders whose shapes vary, and duplicating one here would go stale. Everything else in this plan is given literally.

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test catalog refresh
```

Expected: FAIL to compile — the constructor takes no run repository yet.

- [ ] **Step 4: Thread the run repository through `RefreshHandler`**

In `crates/alexandria-core/src/catalog/commands/refresh.rs`:

Add to the imports: `use crate::catalog::runs::{CatalogRunRepository, RunCounts, RunKind};`

Add `RR` to the struct and its bound:

```rust
pub struct RefreshHandler<A, R, F, C, RR> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    concurrency: usize,
    runs: RR,
}
```

with `RR: CatalogRunRepository` added to the `impl` block's where-clause, and `runs: RR` as the **last** `new` parameter.

In `start`, after authenticating, open the record before returning:

```rust
        self.auth.authenticate(token).await?;
        let run_id = Uuid::new_v4();
        // FR-FC-27: a started run is always a recorded run — the record opens
        // here, where the id is minted, so no caller can start one without it.
        self.runs
            .start(run_id, RunKind::Refresh, None, self.clock.now())
            .await?;
        Ok(RefreshStarted { run_id })
```

In `execute`, close the record. The `list_all` call is the only thing that can abort the run, so record the failure there, and record completion after the tally:

```rust
        let files = match self.repo.list_all().await {
            Ok(files) => files,
            Err(err) => {
                // FR-FC-27: the walk could not proceed at all — that, and only
                // that, is a `failed` run.
                self.runs
                    .fail(run_id, &err.to_string(), self.clock.now())
                    .await?;
                return Err(err);
            }
        };
```

and, immediately before `Ok(outcome)` at the end:

```rust
        // FR-FC-27: the walk finished. Per-file failures are inside the tally
        // and do not make the run failed.
        self.runs
            .finish(
                run_id,
                RunCounts::Refresh {
                    refreshed,
                    marked_missing,
                    unchanged,
                    failed,
                },
                self.clock.now(),
            )
            .await?;
```

- [ ] **Step 5: Thread it through `IndexHandler` the same way**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make the identical changes: `RR` generic, `runs` as the last `new` parameter, `RunKind::Index` with `Some(&request.root)` in `start` (after the existing root validation, so an invalid root never records a run), and in `execute` a `fail` on the `list_files` error path plus a `finish` with `RunCounts::Index { scanned, indexed, skipped, failed }` before `Ok(outcome)`.

- [ ] **Step 6: Update the ten construction sites**

```bash
grep -rn "IndexHandler::new\|RefreshHandler::new" crates --include=*.rs
```

Each gains one argument. In `crates/alexandria-core/src/services.rs` that is a `SqliteCatalogRunRepository::new(pool.clone())` — construct it once near `let repo = SqliteCatalogRunRepository::new(...)` at the top of `build_services`, bind it to `run_repo`, and pass `run_repo.clone()` to both handlers. Also add `SqliteCatalogRunRepository` as the fifth type parameter of the `DefaultIndexHandler` and `DefaultRefreshHandler` aliases. In the test files it is a `FakeCatalogRunRepository::new()`.

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo test -p alexandria-core
```

Expected: PASS, including the seven new tests and every pre-existing index/refresh test unchanged in behavior.

- [ ] **Step 8: Format, lint, and run the full suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: clean, green.

- [ ] **Step 9: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/src/catalog/commands/refresh.rs crates/alexandria-core/src/services.rs crates/alexandria-core/tests/catalog/index.rs crates/alexandria-core/tests/catalog/refresh.rs crates/alexandria-core/tests/throughput.rs
git commit -m "feat: open and close a run record around each walk (FR-FC-27)"
```

---

### Task 3: Startup reconciliation

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`
- Modify: `crates/alexandria-core/tests/catalog/runs.rs`

**Interfaces:**
- Consumes: `SqliteCatalogRunRepository::interrupt_running` (Task 1).
- Produces: no new API. `build_services` reconciles before returning.

- [ ] **Step 1: Write the failing test**

Append to `crates/alexandria-core/tests/catalog/runs.rs`:

```rust
#[tokio::test]
async fn given_a_run_left_running_when_services_are_built_then_it_is_interrupted() {
    // FR-FC-29 end to end: a run recorded as running by a previous process is
    // reconciled at startup, so a client polling it gets a terminal answer
    // instead of waiting on a run that cannot finish.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("alexandria.sqlite");
    let pool = migrate_database(path.to_str().expect("path"))
        .await
        .expect("migrate");

    let repo = SqliteCatalogRunRepository::new(pool.clone());
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Refresh, None, t(1))
        .await
        .expect("start");

    let _services =
        alexandria_core::services::build_services(&Default::default(), pool.clone()).await;

    let run = repo.get(id).await.expect("get").expect("run exists");
    assert_eq!(run.status, RunStatus::Interrupted);
}
```

If `Settings` does not implement `Default`, build it the way the other core integration tests do — `grep -rn "build_services(" crates/alexandria-core/tests` for the established call.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p alexandria-core --test catalog given_a_run_left_running
```

Expected: FAIL — the run is still `Running`.

- [ ] **Step 3: Reconcile in `build_services`**

In `crates/alexandria-core/src/services.rs`, after `run_repo` is constructed and before the handlers are built:

```rust
    // FR-FC-29: any run still recorded as `running` belongs to a process that
    // is gone — runs execute in-process and are never resumed. Reconcile them
    // now, so a client polling one gets a terminal answer instead of waiting
    // forever. A failure here must not stop startup: the catalog is still
    // fully usable, and the stale rows are reconciled on the next boot.
    match run_repo.interrupt_running(clock.now()).await {
        Ok(0) => {}
        Ok(reconciled) => {
            tracing::info!(reconciled, "marked interrupted runs left by a previous process")
        }
        Err(err) => tracing::warn!(error = %err, "could not reconcile interrupted runs"),
    }
```

This requires `clock` to be in scope at that point; it is declared near the top of `build_services` as `let clock = SystemClock;`. If `run_repo` is constructed after `clock`, no reordering is needed.

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p alexandria-core --test catalog given_a_run_left_running
```

Expected: PASS.

- [ ] **Step 5: Format, lint, and run the core suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p alexandria-core
```

Expected: clean, green.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/services.rs crates/alexandria-core/tests/catalog/runs.rs
git commit -m "feat: reconcile interrupted runs at startup (FR-FC-29)"
```

---

### Task 4: The run status query handler

**Files:**
- Create: `crates/alexandria-core/src/catalog/queries/run_status.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/mod.rs`
- Modify: `crates/alexandria-core/src/services.rs`
- Create: `crates/alexandria-core/tests/catalog/run_status.rs`
- Modify: `crates/alexandria-core/tests/catalog.rs`

**Interfaces:**
- Consumes: `CatalogRunRepository`, `CatalogRun` (Task 1); `AuthService`.
- Produces:
  - `alexandria_core::catalog::queries::run_status::GetRunStatusHandler<A, RR>` with `new(auth: A, runs: RR)` and `async fn get(&self, run_id: Uuid, token: &str) -> Result<CatalogRun, DomainError>`.
  - `Services.get_run_status_handler: Arc<DefaultGetRunStatusHandler>` — Tasks 5 and 6 call it.

- [ ] **Step 1: Register the test module**

Add to the end of `crates/alexandria-core/tests/catalog.rs`:

```rust
#[path = "catalog/run_status.rs"]
mod run_status;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/alexandria-core/tests/catalog/run_status.rs`:

```rust
//! Unit tests for the UC-42 GetRunStatusHandler against trait fakes — no
//! database. Coverage: the main flow plus AF-01 (unknown id) and AF-02
//! (unauthenticated), and AF-03's "running runs carry no counts".

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::queries::run_status::GetRunStatusHandler;
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCatalogRunRepository};

const TOKEN: &str = "owner-token";

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

#[tokio::test]
async fn given_a_completed_run_when_read_then_it_is_returned_with_its_counts() {
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1)).await.unwrap();
    runs.finish(
        id,
        RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0,
        },
        t(2),
    )
    .await
    .unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.id, id);
    assert_eq!(run.kind, RunKind::Refresh);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(
        run.counts,
        Some(RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0
        })
    );
}

#[tokio::test]
async fn given_a_running_run_when_read_then_it_has_no_counts_yet() {
    // AF-03: no tally exists until the walk finishes.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.status, RunStatus::Running);
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_an_unknown_run_id_when_read_then_not_found() {
    // AF-01.
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, FakeCatalogRunRepository::new());

    let err = handler
        .get(Uuid::new_v4(), TOKEN)
        .await
        .expect_err("must reject an unknown id");

    assert!(matches!(err, DomainError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_read_then_unauthorized() {
    // AF-02.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1)).await.unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Denying, runs);

    let err = handler
        .get(id, "")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p alexandria-core --test catalog run_status
```

Expected: FAIL to compile — `unresolved import ... queries::run_status`.

- [ ] **Step 4: Write the handler**

Create `crates/alexandria-core/src/catalog/queries/run_status.rs`:

```rust
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::runs::{CatalogRun, CatalogRunRepository};
use crate::errors::DomainError;

/// UC-42 — Query an index or refresh run (FR-FC-28).
///
/// Starting a run answers immediately with a run id (FR-FC-08 keeps runs
/// asynchronous); this is how the caller finds out what became of it. Without
/// it the only observable signals are the catalog counts, which say nothing
/// about whether a walk has finished — a client watching them can read a
/// half-finished run and not know.
///
/// Generic over the auth service and the run repository so the decision logic
/// is unit-tested against trait fakes, then wired with the concrete
/// Runtime/Sqlite collaborators at runtime (services.rs).
pub struct GetRunStatusHandler<A, RR> {
    auth: A,
    runs: RR,
}

impl<A, RR> GetRunStatusHandler<A, RR>
where
    A: AuthService,
    RR: CatalogRunRepository,
{
    pub fn new(auth: A, runs: RR) -> Self {
        Self { auth, runs }
    }

    /// The recorded run for `run_id`.
    pub async fn get(&self, run_id: Uuid, token: &str) -> Result<CatalogRun, DomainError> {
        // AF-02: every catalog operation authenticates the owner.
        self.auth.authenticate(token).await?;
        // AF-01: an id naming no run.
        self.runs.get(run_id).await?.ok_or(DomainError::NotFound)
    }
}
```

- [ ] **Step 5: Export the module**

In `crates/alexandria-core/src/catalog/queries/mod.rs`, add `pub mod run_status;` following the file's existing ordering.

- [ ] **Step 6: Wire it into `Services`**

In `crates/alexandria-core/src/services.rs`: import `GetRunStatusHandler`, add the alias

```rust
pub type DefaultGetRunStatusHandler =
    GetRunStatusHandler<RuntimeAuthService, SqliteCatalogRunRepository>;
```

add `pub get_run_status_handler: Arc<DefaultGetRunStatusHandler>,` to the `Services` struct, construct it with `auth.clone()` and `run_repo.clone()`, and add it to the returned struct literal.

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo test -p alexandria-core --test catalog run_status
```

Expected: PASS, 4 tests.

- [ ] **Step 8: Format, lint, and run the core suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p alexandria-core
```

Expected: clean, green.

- [ ] **Step 9: Commit**

```bash
git add crates/alexandria-core/src/catalog/queries/run_status.rs crates/alexandria-core/src/catalog/queries/mod.rs crates/alexandria-core/src/services.rs crates/alexandria-core/tests/catalog/run_status.rs crates/alexandria-core/tests/catalog.rs
git commit -m "feat: add the UC-42 run status query handler (FR-FC-28)"
```

---

### Task 5: HTTP surface

**Files:**
- Create: `crates/alexandria-http/src/routes/runs.rs`
- Modify: `crates/alexandria-http/src/routes/mod.rs`
- Modify: `crates/alexandria-http/src/lib.rs`
- Create: `crates/alexandria-http/tests/run_status_api.rs`

**Interfaces:**
- Consumes: `Services.get_run_status_handler`, `CatalogRun` (Task 4).
- Produces: `GET /v1/index/runs/{runId}` → `200` with the run body, `404` unknown id, `400` malformed uuid, `401` unauthenticated.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/alexandria-http/tests/run_status_api.rs`:

```rust
//! UC-42 integration tests for `GET /v1/index/runs/{runId}` (Testing
//! Specification §7): the real axum router over a real temp SQLite database.

mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::common::{test_app, TEST_TOKEN};

fn run_request(run_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("/v1/index/runs/{run_id}"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Start a refresh and return its run id.
async fn start_refresh(router: &axum::Router) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    body_json(response).await["runId"]
        .as_str()
        .expect("runId")
        .to_string()
}

#[tokio::test]
async fn given_a_started_run_when_polled_to_completion_then_it_reports_complete_with_counts() {
    // The assertion this whole use case exists to make possible: a client can
    // wait for a run to finish instead of guessing from the catalog counts.
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let run_id = start_refresh(&router).await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let run = loop {
        let response = router
            .clone()
            .oneshot(run_request(&run_id, Some(TEST_TOKEN)))
            .await
            .expect("status");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        if body["status"] != "running" {
            break body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run never left the running state"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };

    assert_eq!(run["runId"], run_id);
    assert_eq!(run["kind"], "refresh");
    assert_eq!(run["status"], "complete");
    assert!(run["finishedAt"].is_string());
    // A completed refresh carries its four counts and no index counts.
    for field in ["refreshed", "markedMissing", "unchanged", "failed"] {
        assert!(run[field].is_number(), "missing {field}: {run}");
    }
    assert!(run["scanned"].is_null(), "index counts must not appear");
    assert!(run["root"].is_null(), "a refresh carries no root");
}

#[tokio::test]
async fn given_an_unknown_run_id_when_read_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(run_request(
            "00000000-0000-4000-8000-000000000000",
            Some(TEST_TOKEN),
        ))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_a_malformed_run_id_when_read_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(run_request("not-a-uuid", Some(TEST_TOKEN)))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_no_token_when_read_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let run_id = start_refresh(&router).await;

    let response = router
        .oneshot(run_request(&run_id, None))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p alexandria-http --test run_status_api
```

Expected: FAIL — `404` for every request, since the route does not exist.

- [ ] **Step 3: Write the route handler**

Create `crates/alexandria-http/src/routes/runs.rs`:

```rust
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use uuid::Uuid;

use alexandria_core::catalog::runs::CatalogRun;

use crate::middleware::auth::invalid_input;
use crate::middleware::error::ApiError;
use crate::routes::bearer_token;
use crate::AppState;

/// `GET /v1/index/runs/{runId}` — report an index or re-index run's status and
/// outcome (UC-42 / FR-FC-28). Starting a run answers `202` with its id and
/// nothing else observed it until now; this is how a caller learns whether the
/// walk finished, and with what tally.
///
/// Returns `200` with the run, `400` (the path segment is not a uuid), `401`
/// (unauthenticated, AF-02 — enforced by the blanket `require_auth` gate this
/// route sits inside), or `404` (no run with that id, AF-01).
pub async fn run_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Result<(StatusCode, Json<CatalogRun>), ApiError> {
    let token = bearer_token(&headers);

    let run_id = Uuid::parse_str(&run_id)
        .map_err(|err| invalid_input(format!("invalid run id: {err}")))?;

    let run = state
        .services
        .get_run_status_handler
        .get(run_id, &token)
        .await
        .map_err(ApiError)?;

    Ok((StatusCode::OK, Json(run)))
}
```

The `run_id` is extracted as a `String` and parsed here rather than as `Path<Uuid>` so a malformed id is a `400` with a message, matching how the other routes report bad input. Confirm against a neighbouring route that takes a uuid (for example `crates/alexandria-http/src/routes/rename.rs`) and follow whichever pattern that file uses.

- [ ] **Step 4: Export and register the route**

In `crates/alexandria-http/src/routes/mod.rs`, add `pub mod runs;` in the file's existing alphabetical position.

In `crates/alexandria-http/src/lib.rs`, add the route to the `v1` router — the one carrying `.route_layer(require_auth)`, **not** the ungated router that holds `/health` and the auth endpoints — beside the other `/v1/index` routes:

```rust
        .route("/v1/index/runs/{run_id}", get(routes::runs::run_status))
```

Match the path-parameter syntax the file's existing routes use (for example `/v1/reading-lists/{uuid}`).

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test -p alexandria-http --test run_status_api
```

Expected: PASS, 4 tests.

- [ ] **Step 6: Format, lint, and run the full suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: clean, green.

- [ ] **Step 7: Commit**

```bash
git add crates/alexandria-http/src/routes/runs.rs crates/alexandria-http/src/routes/mod.rs crates/alexandria-http/src/lib.rs crates/alexandria-http/tests/run_status_api.rs
git commit -m "feat: expose UC-42 run status over HTTP (FR-FC-28)"
```

---

### Task 6: FFI surface

**Files:**
- Modify: `crates/alexandria-ffi/src/lib.rs`
- Verify only (generated + git-ignored, never hand-edited, never committed): `crates/alexandria-ffi/src/header.h`
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: `Services.get_run_status_handler` (Task 4).
- Produces: `alexandria_index_run_status_json(run_id: *const c_char, token: *const c_char) -> RunJsonResult`, plus `RUN_OK`, `RUN_ERR_INVALID_INPUT`, `RUN_ERR_UNAUTHORIZED`, `RUN_ERR_NOT_INITIALIZED`, `RUN_ERR_NOT_FOUND`, `RUN_ERR_OTHER`.

- [ ] **Step 1: Write the failing parity test**

Append to `crates/alexandria-ffi/tests/parity.rs`, adding `alexandria_index_run_status_json` to the `use alexandria_ffi::{…}` list at the top:

```rust
/// UC-42 parity — a run's recorded status must read identically over both
/// transports. The run ids differ by construction (independent databases), so
/// parity asserts every field except the id, which is asserted to be the id
/// each surface was given.
#[tokio::test]
async fn given_a_completed_refresh_when_status_read_via_http_and_ffi_then_bodies_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let refresh_req = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(refresh_req).await.expect("http refresh");
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let http_run_id = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let http_body = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let req = Request::builder()
                .method("GET")
                .uri(format!("/v1/index/runs/{http_run_id}"))
                .header("authorization", format!("Bearer {TEST_TOKEN}"))
                .body(Body::empty())
                .unwrap();
            let resp = router.clone().oneshot(req).await.expect("http status");
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(
                &to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
            )
            .unwrap();
            if body["status"] != "running" {
                break body;
            }
            assert!(std::time::Instant::now() < deadline, "http run never finished");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    };

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_json, ffi_run_id): (String, String) =
        tokio::task::spawn_blocking(move || -> (String, String) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_refresh_start(token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            let run_id = unsafe { CStr::from_ptr(started.run_id) }
                .to_str()
                .unwrap()
                .to_string();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let id = CString::new(run_id.clone()).unwrap();
                let result = alexandria_index_run_status_json(id.as_ptr());
                assert_eq!(result.status, alexandria_ffi::RUN_OK);
                assert!(!result.json.is_null());
                let json = unsafe { CStr::from_ptr(result.json) }
                    .to_str()
                    .unwrap()
                    .to_string();
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(result.json);
                }
                let body: serde_json::Value = serde_json::from_str(&json).unwrap();
                if body["status"] != "running" {
                    break (json, run_id);
                }
                assert!(std::time::Instant::now() < deadline, "ffi run never finished");
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        })
        .await
        .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();

    // ---- compare ----
    assert_eq!(http_body["runId"], http_run_id);
    assert_eq!(ffi_body["runId"], ffi_run_id);
    assert_eq!(http_body["kind"], ffi_body["kind"]);
    assert_eq!(http_body["kind"], serde_json::json!("refresh"));
    assert_eq!(http_body["status"], ffi_body["status"]);
    assert_eq!(http_body["status"], serde_json::json!("complete"));
    for field in ["refreshed", "markedMissing", "unchanged", "failed"] {
        assert_eq!(http_body[field], ffi_body[field], "{field} differs");
    }
}
```

If `IndexStartResult`'s run-id field is not named `run_id`, read the struct at the top of `crates/alexandria-ffi/src/lib.rs` and use its real field name.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p alexandria-ffi --test parity uc42
```

Expected: FAIL to compile — no `alexandria_index_run_status_json`, no `RUN_OK`.

- [ ] **Step 3: Add the result type and status codes**

In `crates/alexandria-ffi/src/lib.rs`, mirroring the existing `AuthJsonResult` block (find it with `grep -n "pub struct AuthJsonResult" -A 40 crates/alexandria-ffi/src/lib.rs` and copy its shape exactly — the `#[repr(C)]`, the `ok`/`err` constructors, the `CString::into_raw`):

```rust
pub const RUN_OK: c_int = 0;
pub const RUN_ERR_INVALID_INPUT: c_int = 1;
pub const RUN_ERR_UNAUTHORIZED: c_int = 2;
pub const RUN_ERR_NOT_INITIALIZED: c_int = 3;
pub const RUN_ERR_NOT_FOUND: c_int = 4;
pub const RUN_ERR_OTHER: c_int = 9;
```

and a `RunJsonResult` with the same two fields and constructors `AuthJsonResult` has, plus:

```rust
fn map_run_err(err: DomainError) -> RunJsonResult {
    match err {
        DomainError::NotFound => RunJsonResult::err(RUN_ERR_NOT_FOUND),
        DomainError::Unauthorized => RunJsonResult::err(RUN_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => RunJsonResult::err(RUN_ERR_INVALID_INPUT),
        _ => RunJsonResult::err(RUN_ERR_OTHER),
    }
}
```

The index surface has no JSON-result type to reuse: `IndexStartResult` carries a run id rather than a body, and `alexandria_index_files_json` returns a bare `*mut c_char` with no status channel, which cannot express AF-01's not-found.

- [ ] **Step 4: Add the export**

```rust
/// Report an index or re-index run's status and outcome (UC-42 / FR-FC-28).
/// `run_id` is the id `alexandria_index_start` or
/// `alexandria_index_refresh_start` returned. On success `json` carries the
/// same body the HTTP `GET /v1/index/runs/{runId}` route returns (FR-FC-24).
///
/// Returns `RUN_ERR_NOT_FOUND` for an id naming no run (AF-01),
/// `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02), and
/// `RUN_ERR_INVALID_INPUT` when `run_id` is not a uuid.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_run_status_json(
    run_id: *const c_char,
    token: *const c_char,
) -> RunJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return RunJsonResult::err(RUN_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its run id would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return RunJsonResult::err(RUN_ERR_UNAUTHORIZED);
    }

    let raw = match cstr_lossy(run_id) {
        Some(s) => s,
        None => return RunJsonResult::err(RUN_ERR_INVALID_INPUT),
    };
    let Ok(run_id) = uuid::Uuid::parse_str(raw.trim()) else {
        return RunJsonResult::err(RUN_ERR_INVALID_INPUT);
    };

    let result = runtime().block_on(async {
        services.get_run_status_handler.get(run_id, &token).await
    });

    match result {
        Ok(run) => {
            let json = serde_json::to_string(&run).unwrap_or_default();
            RunJsonResult::ok(json)
        }
        Err(err) => map_run_err(err),
    }
}
```

This mirrors `alexandria_file_get_by_uuid` exactly, including the ordering rule it documents: authenticate **before** parsing the id, so an unauthenticated caller cannot learn whether its id was well-formed. `authenticated(&services, &token)` is the crate's existing helper; the handler authenticates again internally, which is the established belt-and-braces pattern in this file, not redundancy to remove.

Note that `alexandria_index_files_json` is **not** the pattern to copy — it takes no token and queries the pool directly, bypassing the handler layer entirely. It is a test accessor.

- [ ] **Step 5: Regenerate the C header and verify it**

The header is git-ignored, so this is verification, not a commit. It proves cbindgen exports the new symbol to C callers.

```bash
cargo build -p alexandria-ffi
grep -c "RUN_ERR_NOT_FOUND\|alexandria_index_run_status_json" crates/alexandria-ffi/src/header.h
```

Expected: a count of at least 2. If it is 0, run `cargo clean -p alexandria-ffi && cargo build -p alexandria-ffi` and check again. Do not edit the header by hand and do not `git add` it.

- [ ] **Step 6: Run the parity test to verify it passes**

```bash
cargo test -p alexandria-ffi --test parity uc42
```

Expected: PASS.

- [ ] **Step 7: Format, lint, and run the full suite**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: clean, green.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-ffi/src/lib.rs crates/alexandria-ffi/tests/parity.rs
git commit -m "feat: expose UC-42 run status over FFI (FR-FC-24, FR-FC-28)"
```

---

### Task 7: Documentation

This repo's specs are normative — a use case that is not in the Use Case Specification does not exist.

**Files:**
- Modify: `docs/requirements/Use Case Specification Document.md`
- Modify: `docs/requirements/System Requirements Document.md`
- Modify: `README.md`

- [ ] **Step 1: Add UC-42 to the Use Case Specification**

Add a `### UC-42: Query an index or refresh run` section after UC-41, copying the use-case table, Main Flow, and Alternative Flows **verbatim** from the "UC-42 — Query an index or refresh run" section of [`docs/superpowers/specs/2026-08-13-catalog-run-status-design.md`](../specs/2026-08-13-catalog-run-status-design.md). Match the surrounding sections' formatting exactly: the `| Field | Value |` table, the numbered Main Flow, the `| ID | Condition | Outcome |` table, and the `---` separator.

Then add a traceability row after UC-41's:

```markdown
| UC-42: Query an index or refresh run | FR-FC-24, FR-FC-27, FR-FC-28, FR-FC-29 |
```

- [ ] **Step 2: Add the requirements to the System Requirements Document**

Add three rows after FR-FC-26 in the functional requirements table, copying the wording verbatim from the spec's "New functional requirements" table:

```markdown
| FR-FC-27 | The system shall record every index and re-index run: its id, kind, start time, terminal status, finish time, and the outcome counts for its kind. A run whose walk completes shall be recorded `complete` even when individual files failed — those are counted in the run's `failed` tally, and one file's failure shall not abandon the rest of the walk. A run that could not proceed at all shall be recorded `failed` with the underlying error. |
| FR-FC-28 | The system shall expose a run's recorded status and outcome to an authenticated caller, given the run id returned when the run was started, over both the HTTP and FFI surfaces. |
| FR-FC-29 | The system shall, at startup, mark every run still recorded as running as interrupted; runs execute in-process and are never resumed. |
```

Add an endpoint row after the `/v1/index/refresh` row:

```markdown
| GET | /v1/index/runs/{runId} | Report an index or re-index run's status and outcome. | FR-FC-27, FR-FC-28 |
```

Add a `catalog_runs` entry to the data-model section describing the columns and the four status values, matching how the other tables are described there. Update the F-01/F-02 coverage rows so the FR-FC range includes the three new ids — read the current wording and extend it rather than guessing the format.

- [ ] **Step 3: Update the README**

Add a backlog row for UC-42 under the milestone that owns indexing, matching the table's existing column format and using the `&#9744;` unchecked entity (the work is not merged yet), with issue [#99](https://github.com/artur-rios/alexandria-api/issues/99) as the tracking issue. Update that milestone's count and the Total row arithmetic to match.

If the README documents the HTTP endpoints anywhere outside the backlog, add `GET /v1/index/runs/{runId}` there too.

- [ ] **Step 4: Verify**

```bash
grep -rn "UC-42\|FR-FC-27\|FR-FC-28\|FR-FC-29" docs/requirements README.md
```

Expected: the new use case section, its traceability row, the three requirement rows, the endpoint row, the coverage rows, and the README backlog row.

- [ ] **Step 5: Commit**

```bash
git add docs/requirements README.md
git commit -m "docs: specify UC-42, query an index or refresh run"
```

---

### Task 8: Open the pull request

- [ ] **Step 1: Run the full suite from a clean state**

```bash
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test
```

Expected: green, no formatting diff. Do not proceed on a failure.

- [ ] **Step 2: Confirm no build artifact was staged**

```bash
git ls-files crates/alexandria-ffi/src/header.h
```

Expected: empty output. Any output means the generated header was force-added; remove it with `git rm --cached crates/alexandria-ffi/src/header.h`.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/uc-42-run-status
```

Open a pull request into `main` whose description references the use case and `Closes #99`, summarizes the new endpoint and export, and notes that both indexing handlers gained a collaborator.

- [ ] **Step 4: Stop**

Hand off to a human for review. Do not self-approve and do not merge (Development Workflow §5 Step 7).

---

## Notes for the implementer

- **The `complete` vs `failed` distinction is the heart of this feature.** A run that walked 500 files and failed on 3 is `complete` with `failed: 3`. Only a run that could not proceed at all — the catalog was unlistable, the root unwalkable — is `failed`. Tasks 1 and 2 each have a test pinning this; if you find yourself making either pass by escalating per-file failures, stop and re-read FR-FC-27.
- **Do not make runs synchronous.** FR-FC-08 requires them to be asynchronous, and `202` stays. If a test seems to want a "wait until done" call, it wants polling.
- **Task 2's churn is mechanical but wide.** Ten call sites gain one argument. Run the `grep` in Step 5 rather than hunting by memory, and put the new parameter last so no existing argument order changes.
- **The FFI token question in Task 6 Step 4 is genuinely open** — this plan does not know whether authenticated FFI reads take a token parameter. Read a neighbouring authenticated accessor and copy it. If they do take one, the export signature gains `token: *const c_char` and the parity test must pass `TEST_TOKEN`.
