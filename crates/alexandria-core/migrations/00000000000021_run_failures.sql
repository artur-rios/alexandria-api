-- Which files a run could not record, not just how many (FR-FC-42).
--
-- The tally already says a run failed on two files. It cannot say which,
-- so an owner told "2 files could not be read" has nowhere to look: those
-- files are on disk, absent from every listing, and named nowhere. The
-- walker knew each path at the moment it gave up and logged it at `warn`,
-- which helps whoever reads a log file and nobody else.
--
-- Rows are bounded per run by the writer (see `record_failure`): a folder
-- whose every file fails — a permissions change on a mount, a disk going
-- read-only — would otherwise write a row per file, turning one bad scan
-- into a table larger than the catalog it failed to build. The tally stays
-- the authority on how many; this says which, for as many as are useful to
-- name.
CREATE TABLE IF NOT EXISTS catalog_run_failures (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id    TEXT    NOT NULL REFERENCES catalog_runs (id) ON DELETE CASCADE,
    path      TEXT    NOT NULL,
    reason    TEXT    NOT NULL,
    failed_at TEXT    NOT NULL
);

-- Every read of this table is "the failures of one run", in the order they
-- happened.
CREATE INDEX IF NOT EXISTS idx_catalog_run_failures_run
    ON catalog_run_failures (run_id, id);
