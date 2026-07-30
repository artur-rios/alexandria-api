-- UC-02: re-index marks cataloged paths whose on-disk file is gone. The marker
-- is a nullable timestamp; `state` stays `active`/`deleted` (SRD §4.2). The
-- existing CHECK constraint on `state` is untouched.
ALTER TABLE files ADD COLUMN missing_at TEXT;