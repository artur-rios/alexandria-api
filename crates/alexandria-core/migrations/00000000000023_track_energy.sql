-- The sound of a track, in numbers a visualiser can draw (UC-21).
--
-- The player draws bars that move with the music. Nothing in the playing
-- path can see the sound to draw them from: the engine reports what is
-- playing, whether it is running and where it has got to, and no samples at
-- all. This is where the sound itself is kept, computed once from the file
-- and read back at the position playing.
--
-- Row-major bytes: `bands` levels for the first frame, then the next, each
-- one 0 (silence) to 255 (the loudest moment of this track). One frame per
-- `frame_ms`. A four-minute track at sixteen bands and ten frames a second
-- is about thirty-eight kilobytes, which is why the whole envelope is one
-- BLOB rather than a row per frame: it is read whole, always, and a table of
-- two and a half thousand rows per track would be a join for nothing.
--
-- `version` is what the analysis was computed by. An envelope from an older
-- analysis is not wrong, it is merely differently scaled; when the shape of
-- it changes, a newer core recomputes rather than drawing somebody else's
-- numbers.
--
-- Computed on demand rather than at index time, and this is the whole reason
-- the table exists: decoding every file in a library the moment it is
-- indexed would cost minutes of CPU for tracks nobody has played. It is
-- computed the first time a track is played and kept from then on.
--
-- No FOREIGN KEY, for the reason `track_lyrics` has none: SQLite cannot add
-- one through ALTER TABLE, so purging a file deletes this table's rows
-- explicitly.
CREATE TABLE IF NOT EXISTS track_energy (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id    INTEGER NOT NULL UNIQUE,
    bands      INTEGER NOT NULL,
    frame_ms   INTEGER NOT NULL,
    version    INTEGER NOT NULL,
    levels     BLOB    NOT NULL,
    computed_at TEXT   NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_track_energy_file ON track_energy (file_id);
