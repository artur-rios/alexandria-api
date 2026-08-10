-- UC-28: ReadingProgress links a Document or ComicBook to a reading list,
-- tracking read state and, for a comic series, the current issue (SRD
-- §4.7). `target_kind` records whether the linked file is a `document` or
-- `comic` (FR-RL-02/03), since a reading list can hold either. The UNIQUE
-- constraint on (reading_list_id, item_file_id) makes adding an
-- already-tracked item idempotent at the storage layer; the handler reads
-- the existing row back rather than resetting progress that UC-29 may have
-- already advanced.
--
-- No FOREIGN KEY, for the same reason `watch_progress` has none: SQLite
-- cannot add one via ALTER TABLE. Foreign keys are enforced in this workspace
-- (sqlx sets `PRAGMA foreign_keys = ON` per connection), so the absence here is
-- exactly why purging a file must delete this table's rows explicitly —
-- nothing cascades to them.
CREATE TABLE IF NOT EXISTS reading_progress (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    reading_list_id INTEGER NOT NULL,
    item_file_id    INTEGER NOT NULL,
    target_kind     TEXT    NOT NULL CHECK (target_kind IN ('document', 'comic')),
    state           TEXT    NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'reading', 'read')),
    current_issue   INTEGER,
    total_issues    INTEGER,
    UNIQUE (reading_list_id, item_file_id)
);

CREATE INDEX IF NOT EXISTS idx_reading_progress_list ON reading_progress (reading_list_id);
