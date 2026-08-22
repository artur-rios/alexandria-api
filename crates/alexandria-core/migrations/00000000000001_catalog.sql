CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid          TEXT    NOT NULL UNIQUE,
    path          TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    type          TEXT    NOT NULL CHECK (type IN ('audio', 'video', 'html', 'text', 'document', 'comic', 'image')),
    content_hash  TEXT,
    size_bytes    INTEGER,
    mtime         TEXT,
    state         TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleted')),
    deleted_at    TEXT,
    indexed_at    TEXT    NOT NULL,
    collection_id INTEGER
    -- FOREIGN KEY (collection_id) REFERENCES collections (id) ON DELETE SET NULL
    -- added by the collections migration (UC-10)
);

CREATE INDEX IF NOT EXISTS idx_files_type  ON files (type);
CREATE INDEX IF NOT EXISTS idx_files_state ON files (state);

CREATE TABLE IF NOT EXISTS audio_files (
    file_id INTEGER PRIMARY KEY,
    title   TEXT,
    artist  TEXT,
    album   TEXT,
    year    INTEGER,
    genre   TEXT,
    track   INTEGER,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS video_files (
    file_id      INTEGER PRIMARY KEY,
    title        TEXT,
    year         INTEGER,
    resolution   TEXT,
    media_kind   TEXT CHECK (media_kind IN ('movie', 'series')),
    episode_count INTEGER,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS html_pages (
    file_id    INTEGER PRIMARY KEY,
    title      TEXT,
    source_url TEXT,
    saved_at   TEXT,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS text_files (
    file_id INTEGER PRIMARY KEY,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS documents (
    file_id     INTEGER PRIMARY KEY,
    title       TEXT,
    author      TEXT,
    year        INTEGER,
    format_kind TEXT CHECK (format_kind IN ('book', 'ebook')),
    page_count  INTEGER,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS comic_books (
    file_id      INTEGER PRIMARY KEY,
    title        TEXT,
    series       TEXT,
    issue_number INTEGER,
    page_count   INTEGER,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS images (
    file_id INTEGER PRIMARY KEY,
    title   TEXT,
    caption TEXT,
    width   INTEGER,
    height  INTEGER,
    FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
);