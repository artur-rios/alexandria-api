-- no-transaction
--
-- Music moved out of Alexandria (to Orpheus). This removes what an install
-- that held music has stored for it, so the catalog reads back cleanly under
-- a core that no longer knows the `audio` type. Only database rows go: the
-- audio files on disk, and the artist images cached beside the database, are
-- left exactly where they are.
--
-- The first line opts out of sqlx's per-migration transaction, because
-- `PRAGMA foreign_keys` is a no-op inside one. Foreign keys have to be off
-- here: `files` is rebuilt to drop 'audio' from its type CHECK (SQLite cannot
-- alter a CHECK in place), and with them on, `DROP TABLE files` would run an
-- implicit DELETE that cascades into every subtype table and wipes the
-- metadata of every video, document and image in the catalog. The work is
-- still atomic: it runs in its own BEGIN ... COMMIT, and a failure part way
-- leaves that transaction uncommitted, so the database is rolled back when
-- the connection closes.
--
-- sqlx records the migration only after this script has committed, outside
-- any transaction, so a process killed in between runs it again on the next
-- start. Everything below is safe to repeat: the drops say IF EXISTS, and
-- every other statement finds nothing left to do.
PRAGMA foreign_keys = OFF;
BEGIN;

-- Index runs. `scope` holds the comma-joined wire names `IndexScope::to_wire`
-- wrote — no spaces, no duplicates, no empty entries — or NULL for every type,
-- and reading back a name the core no longer knows fails the whole read as a
-- corrupt row. A run scoped to audio alone has nothing left to cover, so it
-- goes, with its failures (foreign keys are off, so ON DELETE CASCADE will not
-- remove them). A mixed scope loses `audio` and keeps the rest, in order. It
-- never becomes NULL, which would widen a run the owner narrowed to every type.
DELETE FROM catalog_run_failures WHERE run_id IN (SELECT id FROM catalog_runs WHERE scope = 'audio');
DELETE FROM catalog_runs WHERE scope = 'audio';
UPDATE catalog_runs
   SET scope = TRIM(REPLACE(',' || scope || ',', ',audio,', ','), ',')
 WHERE ',' || scope || ',' LIKE '%,audio,%';

-- Tables holding a file id without a FOREIGN KEY, which nothing would clean
-- up. The music tables among them are dropped below instead.
DELETE FROM reading_progress WHERE item_file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM watch_progress   WHERE video_file_id IN (SELECT id FROM files WHERE type = 'audio');

-- The surviving subtype tables. An audio file should own no row here, but
-- their ON DELETE CASCADE is what would have removed one, and it is off, so
-- this does its job: nothing is left for `PRAGMA foreign_key_check` to find.
DELETE FROM video_files WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM html_pages  WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM text_files  WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM documents   WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM comic_books WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM images      WHERE file_id IN (SELECT id FROM files WHERE type = 'audio');

DROP TABLE IF EXISTS playlist_entries;
DROP TABLE IF EXISTS playlists;
DROP TABLE IF EXISTS track_lyrics;
DROP TABLE IF EXISTS track_energy;
DROP TABLE IF EXISTS play_events;
DROP TABLE IF EXISTS artist_images;
DROP TABLE IF EXISTS audio_files;

DELETE FROM files WHERE type = 'audio';

-- `files` rebuilt without 'audio' in its type CHECK, following SQLite's
-- documented procedure for a schema change ALTER TABLE cannot make: create
-- the new table, copy, drop the old, rename the new into its place. The
-- subtype tables' FOREIGN KEYs name `files`, so they bind to the renamed table.
-- Columns are those of `00000000000001_catalog.sql` plus `missing_at` (02),
-- `library_id` (20) and `metadata_version` (22), in that order, each declared
-- exactly as it was. Ids are copied, never renumbered: `watch_progress` and
-- `reading_progress` reference them with no foreign key.
CREATE TABLE files_new (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid             TEXT    NOT NULL UNIQUE,
    path             TEXT    NOT NULL UNIQUE,
    name             TEXT    NOT NULL,
    type             TEXT    NOT NULL CHECK (type IN ('video', 'html', 'text', 'document', 'comic', 'image')),
    content_hash     TEXT,
    size_bytes       INTEGER,
    mtime            TEXT,
    state            TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleted')),
    deleted_at       TEXT,
    indexed_at       TEXT    NOT NULL,
    collection_id    INTEGER,
    missing_at       TEXT,
    library_id       INTEGER,
    metadata_version INTEGER NOT NULL DEFAULT 0
);

INSERT INTO files_new (id, uuid, path, name, type, content_hash, size_bytes, mtime, state,
                       deleted_at, indexed_at, collection_id, missing_at, library_id,
                       metadata_version)
SELECT id, uuid, path, name, type, content_hash, size_bytes, mtime, state,
       deleted_at, indexed_at, collection_id, missing_at, library_id,
       metadata_version
  FROM files;

-- Carry the AUTOINCREMENT high-water mark across. Left alone, the new table's
-- sequence would restart at the highest id it was copied, and the next file
-- indexed would take the id of a removed audio file (or of any file purged
-- before it) — and with it any row that still names that id without a
-- foreign key. The old mark is never below the copied ids, but the larger of
-- the two is taken regardless. A fresh install has neither and gets no row.
DELETE FROM sqlite_sequence WHERE name = 'files_new';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'files_new', high_water
  FROM (SELECT MAX(seq) AS high_water
          FROM (SELECT seq FROM sqlite_sequence WHERE name = 'files'
                UNION ALL
                SELECT MAX(id) FROM files_new))
 WHERE high_water IS NOT NULL;

-- Dropping `files` removes its sequence row; the rename moves the new one
-- under its name.
DROP TABLE files;
ALTER TABLE files_new RENAME TO files;

CREATE INDEX idx_files_type    ON files (type);
CREATE INDEX idx_files_state   ON files (state);
CREATE INDEX idx_files_library ON files (library_id);

COMMIT;
PRAGMA foreign_keys = ON;
