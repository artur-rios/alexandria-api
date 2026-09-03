-- Play history: one row per track played through, the collection mechanism
-- the music statistics are aggregated from (play history design).
--
-- Append-only, and deliberately without a `uuid`. Every other table here
-- gives its rows a public identifier because callers address them -- rename
-- this playlist, remove that entry. Nothing addresses a single play: it is
-- never edited, never deleted on its own, and never referred to again once
-- written. A play is a fact about a moment, and the moment is over.
--
-- A real FOREIGN KEY, unlike `playlist_entries` and `watch_progress`, which
-- carry none because SQLite cannot add one to a table that already exists.
-- This table is new, so it can declare one at creation, and the cascade is
-- what keeps a purged track's plays from outliving it: they are counted by
-- joining `files`, so an orphaned row would be a play nothing could name --
-- invisible in every ranking, yet still swelling the totals beside them.
CREATE TABLE IF NOT EXISTS play_events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id   INTEGER NOT NULL,
    played_at TEXT    NOT NULL,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

-- The two shapes every statistic reads in: "how often was this file played"
-- (each ranking groups by file_id before it joins the tags) and "what was
-- played in this window" (the first and last play the summary reports).
CREATE INDEX IF NOT EXISTS idx_play_events_file      ON play_events (file_id);
CREATE INDEX IF NOT EXISTS idx_play_events_played_at ON play_events (played_at);
