-- The failures recorded against audio files (FR-FC-42), which migration 25
-- left behind.
--
-- Migration 25 deleted the runs scoped to audio alone, and their failures
-- with them, but a run over every type, or over audio and something else,
-- survives, and so do the rows it recorded for the audio files it could not
-- read: `song.mp3 unreadable` listed among a run's failures, for a file the
-- catalog no longer knows the type of. Since 0.5 the walk skips an audio
-- file without reading it, so it can never fail on one again, and every such
-- row is a leftover.
--
-- An audio file is one with an extension the 0.4 classifier mapped to
-- `audio`. LIKE is case-insensitive for ASCII, so `SONG.MP3` matches too.
-- The run's `failed` tally is left as it is: it counts what that run met at
-- the time, and it already may exceed the rows kept (see `record_failure`).
DELETE FROM catalog_run_failures
 WHERE path LIKE '%.mp3'
    OR path LIKE '%.flac'
    OR path LIKE '%.wav'
    OR path LIKE '%.ogg'
    OR path LIKE '%.oga'
    OR path LIKE '%.m4a'
    OR path LIKE '%.aac'
    OR path LIKE '%.opus'
    OR path LIKE '%.wma';
