-- UC-20: watchlists are named groupings for tracking video consumption
-- (SRD §4.5). WatchProgress rows (UC-22+) are added in a later migration,
-- once a use case actually needs them — following the same incremental
-- approach the collections/bookmarks migrations took.
CREATE TABLE IF NOT EXISTS watchlists (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL
);
