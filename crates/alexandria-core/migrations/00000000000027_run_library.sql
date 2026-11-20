-- The library an index run walks (libraries-only design, section 2).
--
-- A library is now the only thing an index run walks, so the run records
-- which one: run status names it to a client, and one library may not have
-- two unfinished runs at once (FR-FC-42), which needs the run to say whose it
-- is. NULL for a refresh, which re-checks the whole catalog and belongs to no
-- library, and for every run written before this column existed.
--
-- No FOREIGN KEY, for the reason `files.library_id` has none: SQLite cannot
-- add one through ALTER TABLE. Removing a library deletes its runs explicitly.
ALTER TABLE catalog_runs ADD COLUMN library_id INTEGER;

CREATE INDEX IF NOT EXISTS idx_catalog_runs_library ON catalog_runs (library_id);
