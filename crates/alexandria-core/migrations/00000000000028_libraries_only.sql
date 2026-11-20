-- no-transaction
-- Libraries are the only way into the catalog (libraries-only design,
-- section 6). Every file now belongs to exactly one library, so this removes
-- what an install catalogued outside one — its files, everything attached to
-- them, and the index runs that walked roots no library holds — and rebuilds
-- `files` so a file without a library cannot be written again. The same
-- rebuild lets `audio` back into the type CHECK, and `audio_files` returns:
-- audio is a catalog type again (files you can play, not a music manager).
--
-- Only database rows go. Every file on disk is left exactly where it is; the
-- owner adds the folders they want as libraries and re-indexes.
--
-- The first line opts out of sqlx's per-migration transaction, for the
-- reason migration 25 gives: `PRAGMA foreign_keys` is a no-op inside one, and
-- with foreign keys on, `DROP TABLE files` below would cascade into every
-- subtype table and wipe the metadata of the files that stay. The work is
-- still atomic in its own BEGIN ... COMMIT, and safe to repeat if the process
-- dies between that commit and sqlx recording the migration: every statement
-- finds nothing left to do the second time.
--
-- Paths are compared the way `insert_file` and `LibraryRepository` compare
-- them: forward slashes on both sides, a root with exactly one trailing
-- separator, and a `substr` prefix test rather than `LIKE`, whose `_` and `%`
-- are wildcards and whose ASCII case-folding would join `/media/Photos` to a
-- library at `/media/photos`.

PRAGMA foreign_keys = OFF;

BEGIN;

-- 1. Files outside every library, and what hangs off them. The progress
-- tables hold a file id with no FOREIGN KEY; the subtype tables have one, but
-- it is off. A collection membership is a column of the file row and goes
-- with it.
DELETE FROM reading_progress WHERE item_file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM watch_progress   WHERE video_file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM video_files WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM html_pages  WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM text_files  WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM documents   WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM comic_books WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM images      WHERE file_id IN (SELECT id FROM files WHERE library_id IS NULL);
DELETE FROM files WHERE library_id IS NULL;

-- 2. Index runs (a root) whose root is neither a library's folder nor inside
-- one. A paused or interrupted one would otherwise be resumable against a
-- folder no library holds, writing files that belong to none. A run over a
-- folder *holding* a library (`/` above `/lib`) goes too: it walked far more
-- than the library. Refresh runs (no root) re-check the whole catalog and
-- stay. Failures go first, since their cascade is off.
DELETE FROM catalog_run_failures
 WHERE run_id IN (
       SELECT r.id FROM catalog_runs r
        WHERE r.root IS NOT NULL
          AND NOT EXISTS (
              SELECT 1 FROM libraries l
               WHERE substr(rtrim(replace(r.root, '\', '/'), '/') || '/', 1,
                            length(rtrim(replace(l.root_path, '\', '/'), '/') || '/'))
                     = rtrim(replace(l.root_path, '\', '/'), '/') || '/'));
DELETE FROM catalog_runs
 WHERE root IS NOT NULL
   AND NOT EXISTS (
       SELECT 1 FROM libraries l
        WHERE substr(rtrim(replace(catalog_runs.root, '\', '/'), '/') || '/', 1,
                     length(rtrim(replace(l.root_path, '\', '/'), '/') || '/'))
              = rtrim(replace(l.root_path, '\', '/'), '/') || '/');

-- The index runs that stay are their library's, so they report it. Longest
-- root first, for the reason `insert_file` orders its match: overlapping
-- libraries are refused, and if one ever slipped through, the answer is the
-- same every time.
UPDATE catalog_runs
   SET library_id = (
       SELECT l.id FROM libraries l
        WHERE substr(rtrim(replace(catalog_runs.root, '\', '/'), '/') || '/', 1,
                     length(rtrim(replace(l.root_path, '\', '/'), '/') || '/'))
              = rtrim(replace(l.root_path, '\', '/'), '/') || '/'
        ORDER BY length(l.root_path) DESC, l.id
        LIMIT 1)
 WHERE root IS NOT NULL;

-- 3. Failures any run recorded on a path outside every library (a refresh's
-- included): that file is not in the catalog any more, so there is nothing
-- for the failure to be about.
DELETE FROM catalog_run_failures
 WHERE NOT EXISTS (
       SELECT 1 FROM libraries l
        WHERE substr(replace(catalog_run_failures.path, '\', '/'), 1,
                     length(rtrim(replace(l.root_path, '\', '/'), '/') || '/'))
              = rtrim(replace(l.root_path, '\', '/'), '/') || '/');

-- 4. Rebuild `files`: audio back in the type CHECK, and a library required.
-- Ids are copied, so every table that points at a file still points at it.
CREATE TABLE files_new (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid             TEXT    NOT NULL UNIQUE,
    path             TEXT    NOT NULL UNIQUE,
    name             TEXT    NOT NULL,
    type             TEXT    NOT NULL CHECK (type IN ('video', 'html', 'text', 'document', 'comic', 'image', 'audio')),
    content_hash     TEXT,
    size_bytes       INTEGER,
    mtime            TEXT,
    state            TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleted')),
    deleted_at       TEXT,
    indexed_at       TEXT    NOT NULL,
    collection_id    INTEGER,
    missing_at       TEXT,
    library_id       INTEGER NOT NULL,
    metadata_version INTEGER NOT NULL DEFAULT 0
);

INSERT INTO files_new (id, uuid, path, name, type, content_hash, size_bytes, mtime, state,
                       deleted_at, indexed_at, collection_id, missing_at, library_id,
                       metadata_version)
SELECT id, uuid, path, name, type, content_hash, size_bytes, mtime, state,
       deleted_at, indexed_at, collection_id, missing_at, library_id,
       metadata_version
  FROM files;

-- Carry the AUTOINCREMENT high-water mark over, as migration 25 does: the
-- progress tables hold file ids with no FOREIGN KEY, so a new file reusing a
-- deleted file's id would inherit whatever still pointed at it.
DELETE FROM sqlite_sequence WHERE name = 'files_new';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'files_new', high_water
  FROM (SELECT MAX(seq) AS high_water
          FROM (SELECT seq FROM sqlite_sequence WHERE name = 'files'
                UNION ALL
                SELECT MAX(id) FROM files_new))
 WHERE high_water IS NOT NULL;

DROP TABLE files;
ALTER TABLE files_new RENAME TO files;

CREATE INDEX idx_files_type    ON files (type);
CREATE INDEX idx_files_state   ON files (state);
CREATE INDEX idx_files_library ON files (library_id);

-- 5. Audio's subtype table, with the columns it had when migration 25
-- dropped it (migrations 1, 15 and 19).
CREATE TABLE IF NOT EXISTS audio_files (
    file_id          INTEGER PRIMARY KEY,
    title            TEXT,
    artist           TEXT,
    album            TEXT,
    year             INTEGER,
    genre            TEXT,
    track            INTEGER,
    duration_seconds REAL,
    album_artist     TEXT,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

COMMIT;

PRAGMA foreign_keys = ON;
