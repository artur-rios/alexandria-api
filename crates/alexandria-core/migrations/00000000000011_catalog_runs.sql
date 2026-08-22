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
    already_cataloged INTEGER,
    refreshed       INTEGER,
    marked_missing  INTEGER,
    unchanged       INTEGER,
    failed          INTEGER,
    error           TEXT
);

CREATE INDEX IF NOT EXISTS idx_catalog_runs_started_at ON catalog_runs (started_at);
