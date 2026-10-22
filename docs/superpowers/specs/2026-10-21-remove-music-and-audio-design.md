# Remove music and audio from Alexandria

## Intent

Alexandria's owner also builds **Orpheus**, a project dedicated to music.
Two apps that both manage, tag and play music is one too many, so music
leaves Alexandria entirely. Alexandria becomes a library for things you
**read and watch**: text and Markdown, HTML pages, PDFs and ebooks, comics,
images, and videos played from the folders the app tracks.

This is a scope cut, not a feature. Success means Alexandria has no
notion of an audio file left: it doesn't index them, store them, play
them, enrich them or describe them in its documentation, and everything
that stays works exactly as before.

## Goals

- Remove `audio` as a catalog file type across the core, HTTP server, FFI
  layer, UI and docs.
- Remove every feature that exists only for music (list below).
- When a scan meets an audio file (mp3, flac, …) in a tracked folder, skip
  it silently, the way any other unrecognised extension is skipped.
- On upgrade, delete music that's already in the database: the
  `files` rows of type audio, their subtype rows, and everything hanging off
  them (playlists, play history, lyrics, energy, artist images).
- Keep video, text/Markdown, HTML, PDF/ebooks, comics and images working
  unchanged.

## Stored index scopes

Tracked folders and index runs both persist a type scope by name, so
`audio` can be found in stored data after the type is gone. Wherever a
scope is stored (the UI's `LibrarySource.scope` and the core's
`libraries` table, if it keeps one; `catalog_runs.scope` in the core):

- **Audio-only tracked folders** are removed from the tracked list on
  upgrade. They are not left unscannable, and their scope is never simply
  emptied, because an empty scope means "every type" and would start
  indexing a former music folder's cover art and booklets.
- **Mixed scopes** (e.g. audio + image) drop `audio` and keep the rest.
- **Audio-only index runs** (`catalog_runs` with scope exactly `audio`)
  are deleted, whatever their status, including paused or interrupted
  ones, since there is nothing left for them to resume.
- **Mixed-scope runs** keep their row with `audio` stripped from the
  stored scope, so run queries and resume never hit the "corrupt scope"
  error path.

The UI applies its folder cleanup once, on the first launch after the
upgrade.

## Non-goals

- No new video features. Video keeps what it has today: ffmpeg metadata
  (title, year, resolution, duration), keyframe thumbnails, Range-capable
  streaming, the media_kit player with subtitle and audio-track selection,
  watchlists and watch progress.
- No new PDF capabilities (e.g. text extraction) and no changes to the
  text, HTML, ebook, comic or image viewers.
- No migration of music data to Orpheus. Existing music records are
  dropped; the files on disk are untouched.

## Anti-goals (failure even if it "works")

- Breaking video. ffmpeg-next, `/v1/files/{uuid}/stream`, the
  `/thumbnail` endpoint and media_kit's video backend are shared with
  video and must stay. Video's audio-track selection is a video feature and
  stays.
- Deleting or touching any file on disk. Only database rows are removed.
- Leaving dead half-features: an orphaned player bar, an empty "Music"
  tab, a stats screen with nothing in it, FFI functions that return
  "not found" forever, config keys that do nothing.
- Breaking existing databases. The upgrade must run cleanly on a database
  that has music, playlists and play history in it.

## What gets removed

### alexandria-api (Rust workspace, v0.4.0)

- **File type:** the `Audio` variant of `FileType` and `SubtypeMetadata`
  (`crates/alexandria-core/src/catalog/model.rs`), the audio extensions in
  `catalog/classify.rs`, audio MIME entries in `playback/mime.rs`, the
  audio arm in `catalog/extraction.rs`, `index_scope.rs` and the thumbnail
  handler.
- **Tag reading:** `catalog/audio_tags.rs` (lofty: tags, duration, cover
  art). The `lofty` dependency goes from the workspace and every crate,
  including the http crate's dev-dependency used to build audio fixtures.
- **Playlists:** `playlists/` module, `/v1/playlists*` routes,
  `alexandria_playlist_*` FFI functions.
- **Play history and stats:** `plays/` module, `/v1/plays`,
  `/v1/plays/stats`, `alexandria_play_record`, `alexandria_music_stats`.
- **Enrichment:** `enrichment/` module (MusicBrainz, LRCLIB, Wikimedia
  Commons), `/v1/enrichment/*`, `alexandria_enrichment_*`,
  `alexandria_artist_image*`, and the `[metadata]` config section. If
  `reqwest` is used by nothing else afterwards, it goes too.
- **Energy/visualizer:** `playback/energy.rs`, the `/energy` route,
  `alexandria_file_track_energy`.
- **Tests** that exist only for the above (playlists, plays, enrichment,
  track energy, audio cases in playback/stream/thumbnail/backfill tests).
  Tests that use an audio file merely as "some non-video file" get a
  different fixture rather than being deleted.
- **Schema:** a new migration (`00000000000025_…`) that deletes audio
  rows and drops `audio_files`, `playlists`, `playlist_entries`,
  `artist_images`, `track_lyrics`, `track_energy`, `play_events`, and
  rebuilds `files` so its type CHECK no longer allows `audio`. Old
  migrations are not edited, since sqlx checksums them.
- **Docs in this repo:** README feature table (F-11 Playlists, F-13 Music
  enrichment, audio in the type list), `docs/requirements/*`,
  `docs/initial/*`, `docs/System Behavior Document.md`,
  `config.toml.example`, and a CHANGELOG entry.

### alexandria-ui (Flutter desktop)

- `FileType.audio` in `lib/features/catalog/domain/file_type.dart` and
  every branch on it (`file_details_view.dart`, `catalog_search_view.dart`,
  `home_dashboard.dart`, `missing_files_screen.dart`, and any others).
- Music playback: `audio_playback_controller.dart`, the playback queue,
  `music_grouping.dart`, `music_browse.dart`, `album_cover_controller.dart`,
  `now_playing_screen.dart`, `sound_bars.dart`, `music_rows.dart`,
  `music_library_view.dart`, `album_visor.dart`, `sleeve_design.dart`.
- The persistent player bar (`shell/presentation/playback_bar.dart`) and
  its slot in the shell.
- `lib/features/playlists/`, `lib/features/stats/`,
  `lib/features/enrichment/`, and the music metadata editor/form.
- `media_kit_libs_audio` from `pubspec.yaml`; `media_kit_libs_video`
  stays.
- FFI bindings regenerated with ffigen from the new
  `alexandria_ffi.h`.
- Matching unit, widget and integration tests and audio fixtures
  (`integration_test/support/{flac,wav}_fixture.dart`).
- Requirements and initial docs in the UI repo.

### alexandria-docs (Hugo/Docsy site)

- Overview, usage and architecture pages: remove audio from the media
  types and the "persistent player" description, describe Alexandria as a
  reading and watching library, and mention Orpheus as the home for music.

### Use cases

The API and UI repos each keep their own Use Case Specification Document,
numbered independently. The music-only use cases are:

- **alexandria-api:** UC-49 (Manage a playlist), UC-50 (Play a playlist).
- **alexandria-ui:** UC-15 (Edit music metadata), UC-20 (Play audio),
  UC-21 (See what is playing), UC-46 (Browse the music library), UC-47
  (Manage a playlist), UC-48 (Play a playlist), UC-50 (See what has been
  played most).

They are marked **Removed (moved out of scope, see Orpheus)** in their
document rather than deleted, so the numbering of every other use case
stays stable. Any use case that lists audio alongside other types is
edited to drop audio. Search each document for audio, music, track,
playlist and lyrics rather than relying on this list alone; the API
README's feature and issue tables (F-11, F-13, F-14) follow the same
rule.

## Constraints

- Three repos change together, and the UI depends on the API's FFI header.
  Order: **api → ui → docs**. The UI regenerates its bindings from the
  API's new header.
- The HTTP API loses routes, which is a breaking change: bump the
  workspace version to **0.5.0**.
- Existing code style, the commit conventions in each repo, and the
  existing test suites apply. Every suite that passes today must pass
  afterwards (minus deleted tests).
- Historical design docs, plans and reports under
  `docs/superpowers/` describing music features stay as they are. They
  record what was built, and the new spec explains why it went.

## Decided by the owner

- Music and audio go from Alexandria entirely. Orpheus owns music.
- Ebooks, comics and images stay.
- Videos can be any format the app already recognises, and they come only
  from folders the app tracks.
- Audio files in tracked folders are skipped silently on scan, and
  existing music records (including playlists and play history) are
  deleted on upgrade.
- Tracked folders set to index music only are removed from the tracked
  list on upgrade.

## Claude's calls (owner can override)

- No new video features; this change is removal only.
- Music use cases are marked removed instead of renumbered.
- Version bump to 0.5.0.
- Historical specs and plans are kept unchanged.
- The cleanup is a forward migration that rebuilds `files`, not an edit of
  migration 001.
- Mixed scopes drop `audio`; audio-only index runs are deleted; mixed
  runs keep their row with `audio` stripped.
- `reqwest` is removed with enrichment (it has no other user).
- App-owned caches on disk (thumbnail and artist-image caches) are left
  alone; only database rows are removed.

## Left to the builder

- Exact migration SQL (SQLite table rebuild for the CHECK change, order
  of drops given foreign keys, `PRAGMA foreign_keys` handling).
- Whether `playback/` keeps its name now that it serves only video and
  thumbnails.
- How to re-lay out the UI shell and home dashboard once the player bar
  and music sections are gone, keeping it consistent with the existing
  design.
- Splitting the work into commits and PRs per repo.
