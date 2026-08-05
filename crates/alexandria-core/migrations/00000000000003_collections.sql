-- UC-10: collections are flat groupings of files or bookmarks (SRD §4.3). The
-- `kind` discriminator fixes what a collection may hold; UC-13 enforces that an
-- item's kind matches when it is added.
--
-- On `files.collection_id`: the initial catalog migration left a note promising
-- this migration would add its FOREIGN KEY. It deliberately does not. SQLite
-- cannot add a constraint via ALTER TABLE, so honoring that note would mean a
-- full 12-step rebuild of `files` — the table every other use case reads — for a
-- constraint that is inert here: nothing in this workspace sets
-- `PRAGMA foreign_keys = ON`, which is why `repos.rs` deletes subtype rows
-- explicitly instead of relying on ON DELETE CASCADE. No row links a file to a
-- collection until UC-13, so the constraint has no work to do yet either.
CREATE TABLE IF NOT EXISTS collections (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL,
    kind TEXT    NOT NULL CHECK (kind IN ('file', 'bookmark'))
);

CREATE INDEX IF NOT EXISTS idx_collections_kind ON collections (kind);
