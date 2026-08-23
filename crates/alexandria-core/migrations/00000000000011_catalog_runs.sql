-- Pre-release baseline: amended in place, never stacked. sqlx checksums a
-- migration's file content, so any edit here — a comment included — makes an
-- existing database fail startup with `DomainError::Migration`. That is the
-- accepted trade while the project is pre-release (Operations & Infrastructure
-- Document §2.5): an existing database is deleted and rebuilt, not migrated.
--
-- UC-42: the lifecycle and outcome of each UC-01 index and UC-02 re-index run
-- (FR-FC-27). `start()` mints a run id and the caller is handed it; before
-- this table nothing recorded what became of that run, so a client could not
-- tell a finished run from one still walking.
--
-- Counts are per-kind and nullable rather than a shared generic set: `scanned`
-- is meaningless for a refresh and `marked_missing` for an index, and they are
-- unknown for either until the walk finishes.
--
-- `phase`, `total`, and `processed` are the progress columns (FR-FC-28). They
-- are not written per file — that would put a SQLite write in front of every
-- entry, which is the cost FR-FC-08 keeps off the indexing path — but flushed
-- periodically from the run's in-memory cell (`catalog::run_registry`), which
-- is authoritative while the run executes. What they are for is the run this
-- process is no longer executing: a client then sees the last flush rather
-- than nothing at all. All three are nullable because a run that stopped
-- inside discovery never flushed one.
--
-- `paused_at` and `paused_millis` are what `active_millis` is derived from:
-- elapsed wall time minus the time the run spent paused. `paused_millis`
-- defaults to 0 so a run that was never paused needs no special case. Only
-- the read side exists so far: `active_millis` divides by them today, and the
-- pause/resume command is what will write them.
--
-- `concurrency` records how many entries the run was processing at a time, so
-- a resumed run continues at the width it was started with rather than at
-- whatever the configuration happens to say later. Written by `start()` from
-- the caller's chosen run priority (`RunPriority`, FR-FC-08); NULL only for a
-- run started before run priority existed, and a resume of one of those falls
-- back to the configured default. It is declared here rather than in a later
-- migration because this baseline file is amended in place, never stacked, so
-- the table's shape is settled in one edit.
--
-- `segment` counts how many times the run has been put back to work: 0 for the
-- segment `start()` opened, and one more for every `resume`. It exists because
-- `status` alone cannot tell "still running" from "running again". A walk drops
-- its in-memory cell before recording how it stopped, and a pause and a resume
-- can both land in that gap — leaving the walk's own late write facing a row
-- that reads `running` because a *different* segment is now walking it. The
-- walk captures this number when it starts and both halt verbs match on it, so
-- the late write is refused instead of pausing, or terminally cancelling, a run
-- that is actively working. Only `resume` advances it, which is what lets a
-- walk's cancel still land behind a control call's to fill in the tally that
-- call had none of.
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
    already_cataloged INTEGER,
    refreshed       INTEGER,
    marked_missing  INTEGER,
    unchanged       INTEGER,
    failed          INTEGER,
    error           TEXT,
    phase           TEXT,
    total           INTEGER,
    processed       INTEGER,
    paused_at       TEXT,
    paused_millis   INTEGER NOT NULL DEFAULT 0,
    concurrency     INTEGER,
    segment         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_catalog_runs_started_at ON catalog_runs (started_at);
