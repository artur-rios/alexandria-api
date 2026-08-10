-- UC-15: bookmarks are browser bookmarks, optionally grouped into a
-- `kind = 'bookmark'` collection (SRD §4.4). Same two-phase soft/hard
-- deletion model as files (UC-18/UC-19), so the columns mirror `files`'
-- lifecycle shape.
--
-- No FOREIGN KEY on `collection_id`, for the same reason `files.collection_id`
-- has none (see the collections migration's note): SQLite cannot add one via
-- ALTER TABLE. Foreign keys are enforced in this workspace (sqlx sets
-- `PRAGMA foreign_keys = ON` per connection), so the absence here is what
-- makes UC-12's unlink of a bookmark collection's members a manual step
-- rather than a cascade.
CREATE TABLE IF NOT EXISTS bookmarks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid          TEXT    NOT NULL UNIQUE,
    url           TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    state         TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleted')),
    deleted_at    TEXT,
    collection_id INTEGER
);

CREATE INDEX IF NOT EXISTS idx_bookmarks_state ON bookmarks (state);
CREATE INDEX IF NOT EXISTS idx_bookmarks_collection_id ON bookmarks (collection_id);
