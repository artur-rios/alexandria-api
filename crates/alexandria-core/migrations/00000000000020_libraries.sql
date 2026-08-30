-- Libraries: a registered folder whose files are browsed as a tree, and
-- shown only there (libraries design).
--
-- `root_path` is what a file's tree position is relative to, so a library
-- that moves on disk is one row to correct rather than a re-index.
--
-- `name` is the owner's, defaulting to the folder's own name. Not derived
-- from `root_path` at read time: a folder called `2024-final-v2` is a path,
-- not a title, and the owner renaming the library must not mean renaming
-- their directory.
CREATE TABLE IF NOT EXISTS libraries (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid      TEXT    NOT NULL UNIQUE,
    name      TEXT    NOT NULL,
    root_path TEXT    NOT NULL UNIQUE
);

-- Which library a file belongs to, or NULL for the great majority that
-- belong to none.
--
-- Recorded rather than derived from the path at read time. Deriving it would
-- mean every listing carrying the set of library roots and testing each path
-- against all of them -- the exclusion rule would live in whatever happened
-- to be querying, which is several places, and one of them will forget. A
-- column means the type listing excludes with `library_id IS NULL` and
-- cannot accidentally not.
--
-- A single column, so a file is in one library or none. Two libraries owning
-- one file would mean two answers to "where does this appear", and every
-- screen would need a rule for choosing between them.
--
-- No FOREIGN KEY, for the reason the rest of this schema has none: SQLite
-- cannot add one through ALTER TABLE. Deleting a library must therefore
-- clear this column explicitly -- which is also what returns its files to
-- the type panels rather than stranding them nowhere.
ALTER TABLE files ADD COLUMN library_id INTEGER;

CREATE INDEX IF NOT EXISTS idx_files_library ON files (library_id);
