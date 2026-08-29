-- Music enrichment: artist photography and lyrics fetched from public
-- services (music enrichment design). Two tables, because the two facts are
-- about different things.

-- An artist image is about an ARTIST, not a file: one lookup serves every
-- track they appear on, and storing it per file would re-ask the same
-- question once per track and hold the same bytes many times over.
--
-- Keyed by `artist_name` because that is the only artist identity this
-- catalog has -- there is no `artists` table, an artist is a tag value on a
-- file (see `audio_files.album_artist`). The name is stored exactly as the
-- catalog holds it; matching is the enrichment command's job, not the
-- schema's.
--
-- `mbid` is the MusicBrainz artist id the name resolved to, kept so a wrong
-- match can be explained and cleared rather than only observed. NULL when
-- nothing matched.
--
-- `image_path` points into the image cache directory; the bytes are never
-- stored here, the same division `playback.thumbnail_cache_dir` already
-- makes. NULL when no image was found.
--
-- `outcome` is what makes this resumable. It records that a lookup HAPPENED
-- and what it concluded -- including concluding nothing. Without it "this
-- artist has no photo" and "this artist was never looked up" are the same
-- row, and every run re-asks a question already answered no.
CREATE TABLE IF NOT EXISTS artist_images (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    artist_name TEXT    NOT NULL UNIQUE,
    mbid        TEXT,
    source_url  TEXT,
    image_path  TEXT,
    outcome     TEXT    NOT NULL,
    fetched_at  TEXT    NOT NULL
);

-- Lyrics are about a RECORDING, and the closest thing this catalog has to a
-- recording is a file -- so this is keyed by `file_id`. Two files of the same
-- song get two rows, which is the honest answer: they may differ in edit,
-- length, or language, and collapsing them would show one file's lyrics under
-- another file's timing.
--
-- No FOREIGN KEY, for the reason `reading_progress` and `playlist_entries`
-- have none: SQLite cannot add one through ALTER TABLE. Foreign keys are
-- enforced in this workspace, so the absence is exactly why purging a file
-- must delete this table's rows explicitly -- nothing cascades to them.
--
-- `plain` is the unsynchronized text; `synced` is LRC-format text with
-- timestamps, present only when the provider had it. Both NULL is a valid,
-- meaningful row: it is a lookup that found nothing, recorded so it is not
-- repeated.
CREATE TABLE IF NOT EXISTS track_lyrics (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id    INTEGER NOT NULL UNIQUE,
    mbid       TEXT,
    plain      TEXT,
    synced     TEXT,
    source     TEXT,
    outcome    TEXT    NOT NULL,
    fetched_at TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_track_lyrics_file ON track_lyrics (file_id);
