-- UC-22: WatchProgress links a VideoFile to a watchlist, tracking watch
-- state per video (SRD §4.6). The UNIQUE constraint on
-- (watchlist_id, video_file_id) makes adding an already-tracked video
-- idempotent at the storage layer; the handler reads the existing row back
-- rather than resetting progress that UC-23 may have already advanced.
--
-- No FOREIGN KEY, for the same reason the collections/bookmarks migrations
-- have none: SQLite cannot add one via ALTER TABLE, and nothing in this
-- workspace sets `PRAGMA foreign_keys = ON`.
CREATE TABLE IF NOT EXISTS watch_progress (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    watchlist_id    INTEGER NOT NULL,
    video_file_id   INTEGER NOT NULL,
    state           TEXT    NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'watching', 'watched')),
    current_episode INTEGER,
    total_episodes  INTEGER,
    UNIQUE (watchlist_id, video_file_id)
);

CREATE INDEX IF NOT EXISTS idx_watch_progress_watchlist ON watch_progress (watchlist_id);
