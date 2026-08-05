-- UC-26: ReadingLists group Documents/ComicBooks for tracking reading
-- consumption (SRD §4.7), mirroring `watchlists`.
CREATE TABLE IF NOT EXISTS reading_lists (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL
);
