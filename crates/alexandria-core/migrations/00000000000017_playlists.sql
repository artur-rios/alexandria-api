-- Playlists: named, ORDERED groupings of audio files (UC-31's shape, one
-- medium over). Mirrors `reading_lists`, with two deliberate differences
-- spelled out below.
CREATE TABLE IF NOT EXISTS playlists (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL
);

-- No FOREIGN KEY, for the same reason `reading_progress` has none: SQLite
-- cannot add one via ALTER TABLE. Foreign keys are enforced in this
-- workspace, so the absence here is exactly why purging a file must delete
-- this table's rows explicitly -- nothing cascades to them.
--
-- And deliberately NO `UNIQUE (playlist_id, file_id)`, which is what
-- `reading_progress` carries. A playlist may hold the same track more than
-- once: a set can legitimately open and close with the same song. An entry's
-- identity is therefore its own `id`, not its file, and removing "that track"
-- means removing that entry.
--
-- `position` is contiguous 0..n-1 within a playlist. An append extends the
-- sequence with new positions after whatever the playlist already holds; a
-- removal or a purge shifts later positions down to close the gap it
-- leaves; a move recomputes and rewrites the playlist's full stored order.
-- Every mutation restores contiguity, but not every mutation renumbers --
-- an append never touches an existing row's position.
--
-- `uuid` is the entry's public identifier (§4.0's identifier strategy):
-- `id` stays internal, the way `playlists.id` does for `playlists.uuid`.
-- An entry's identity is its own row, not its file (see above), so this is
-- the token HTTP and FFI address an entry by -- not `id`, which is an
-- internal rowid no other public identifier in this schema exposes.
CREATE TABLE IF NOT EXISTS playlist_entries (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid        TEXT    NOT NULL UNIQUE,
    playlist_id INTEGER NOT NULL,
    file_id     INTEGER NOT NULL,
    position    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_playlist_entries_list
    ON playlist_entries (playlist_id, position);
CREATE INDEX IF NOT EXISTS idx_playlist_entries_file
    ON playlist_entries (file_id);
