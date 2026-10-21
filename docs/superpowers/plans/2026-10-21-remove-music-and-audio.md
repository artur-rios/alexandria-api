# Remove Music and Audio Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the `audio` file type and every music feature from Alexandria's core, HTTP server, FFI layer, Flutter UI and docs site, and clean stored music data out of existing installs on upgrade.

**Architecture:** This is pure removal, done one feature at a time so every task ends with green builds:
- **API (alexandria-api):** enrichment, then play history, then playlists, then track energy, then the audio type itself. A final forward migration then deletes the data and tightens the schema.
- **UI (alexandria-ui):** the same order, with one difference. Every UI task up to Task 13 still runs against the 0.4 core. Task 13 switches the UI to the 0.5 core and its new header in one step.
- **Docs site (alexandria-docs):** edited last.

**Tech Stack:**
- API: Rust workspace (sqlx/SQLite, axum, cbindgen, ffmpeg-next)
- UI: Flutter desktop (Riverpod, freezed, ffigen, media_kit, shared_preferences)
- Docs: Hugo/Docsy

**Spec:** `docs/superpowers/specs/2026-10-21-remove-music-and-audio-design.md` (in alexandria-api)

## Global Constraints

- Remove `audio` everywhere; keep `video`, `html`, `text`, `document`, `comic`, `image` working unchanged.
- No new video features. ffmpeg-next, `/v1/files/{uuid}/stream`, `/thumbnail`, `media_kit` and `media_kit_libs_video` stay. Video audio-track selection stays (`tracks.audio`, `_TrackKind.audio`, `videoAudioTracks` l10n).
- Never delete or touch files on disk; only database rows and stored settings. App-owned caches (thumbnails, artist images) on disk are left alone.
- Audio files in tracked folders are skipped silently on scan (same as any unrecognised extension), not reported as failures.
- Old migrations are never edited (sqlx checksums them); the cleanup is a new migration `00000000000025_remove_audio.sql`.
- Workspace version 0.4.0 → **0.5.0**; UI accepts core `>=0.5.0 <0.6.0`.
- Music use cases are marked **Removed (moved out of scope, see Orpheus)**, never renumbered or deleted:
  - API: UC-49 and UC-50.
  - UI: UC-15, UC-20, UC-21, UC-46, UC-47, UC-48 and UC-50.
- Historical files under `docs/superpowers/` (specs, plans, reports) are not edited.
- Test names:
  - Rust: `given_<state>_when_<action>_then_<outcome>`.
  - Dart: `GivenX_WhenY_ThenZ`.
- Commits: lowercase Conventional Commits, 50/72, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Branches: `feature/remove-music` in alexandria-api (from `develop`) and alexandria-ui (from `develop`); `feature/remove-music` in alexandria-docs (from `main`). Nothing is pushed or opened as a PR without the owner's go-ahead.

**API verification (run at the end of every API task):**
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: all pass.

**UI verification (run at the end of every UI task):**
```bash
flutter pub get && dart run build_runner build --delete-conflicting-outputs && flutter gen-l10n && flutter analyze --fatal-infos --fatal-warnings && dart run custom_lint && flutter test
```
Expected: all pass. Integration tests run one file at a time (`flutter test <file> -d windows`). Only run the ones that the task touched.

## Review Focus

1. **Upgrading a real install.** Take a database holding music, playlists, play events, lyrics, energy, a paused audio-only run, and videos with watch progress. The migration must succeed. Videos, their `video_files` rows, ids and watch progress must survive. `PRAGMA foreign_key_check` must be empty afterwards. Covered by Task 6.
2. **A folder mixing mp3/flac with videos and text.** The scan catalogues only the non-audio files. The run finishes successfully, with no failure rows for the audio files. Covered by Task 5.
3. **Run history after upgrade.** Listing runs and resuming a paused mixed-scope run must not hit `corrupt catalog_runs.scope`. Covered by Task 6.
4. **Old configuration.** A `config.toml` that still has a `[metadata]` section must start cleanly. A UI settings store that still holds `musicLookup*` and `opensPlayerOnPlay` keys must load cleanly. Old HTTP clients calling removed routes get 404. Covered by Tasks 1 and 9.
5. **Video still has sound after `media_kit_libs_audio` is dropped.** Covered by Task 11's manual check.

---

## Part A — alexandria-api

Before Task 1:
```bash
git switch develop && git switch -c feature/remove-music
```

### Task 1: Remove music enrichment

**Files:**
- Delete:
  - `crates/alexandria-core/src/enrichment/`
  - `crates/alexandria-http/src/routes/enrichment.rs`
  - `crates/alexandria-core/tests/enrichment.rs`, `tests/enrichment/`, `tests/upstream_contract.rs`
- Modify:
  - `crates/alexandria-core/src/lib.rs:8`
  - `services.rs` (enrichment aliases ~`:291-342`, fields and construction)
  - `config.rs`: remove `MetadataSettings`, `MetadataUnavailable` (`:410-500`), `Settings.metadata` (`:519`), and the env overrides `ALEXANDRIA_METADATA_*` (`:621-634`)
  - `settings/mod.rs`: remove `MetadataSettingsView` and `SettingsView.metadata`
  - `errors.rs`: delete `unavailable()`, `too_many_requests()` and `ServiceUnavailable` if they have no other callers (clippy will tell)
  - `alexandria-http/src/routes/mod.rs:7`, `src/lib.rs:161-173`
  - `alexandria-ffi/src/lib.rs`:
    - remove the `EnrichmentScope` import (`:16`) and the `ENRICHMENT_*` consts
    - remove `EnrichmentJsonResult` and `enrichment_scope_from_json`
    - remove `alexandria_enrichment_run`, `alexandria_enrichment_read_track`, `alexandria_artist_image`, `alexandria_artist_image_fetch`
  - `config.toml.example`: remove the `[metadata]` section (`:118-144`)
  - `Cargo.toml` and `crates/alexandria-core/Cargo.toml`: remove `reqwest`. Also remove the core dev-dep `axum` and tokio `test-util` if nothing else uses them.
  - `.github/workflows/ci.yml`: delete the `upstream-contract` job
  - FFI tests: `tests/smoke.rs:3028-3097`; `tests/parity.rs`, the enrichment and artist-image cases
- Test: `crates/alexandria-core/tests/config.rs`, `crates/alexandria-http/tests/` (new test in the existing routing/catalog API test file)

**Interfaces:** Produces: `SettingsView` without `metadata`; `Settings` without `metadata`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/alexandria-core/tests/config.rs
#[test]
fn given_a_config_with_a_legacy_metadata_section_when_parsed_then_it_loads_and_ignores_it() {
    let raw = "[metadata]\nenabled = true\ncontact = \"me@example.com\"\nimage_cache_dir = \"artist-images\"\n";
    let settings = Settings::from_toml_str(raw).expect("legacy section is ignored"); // use the existing parse entry point
    assert_eq!(settings.playback.thumbnail_cache_dir, Settings::default().playback.thumbnail_cache_dir);
}
```
```rust
// crates/alexandria-http/tests/<routing test file>
#[tokio::test]
async fn given_the_enrichment_routes_are_gone_when_called_then_not_found() {
    // POST /v1/enrichment/runs, GET /v1/enrichment/tracks/{uuid}, GET /v1/enrichment/artist-image → 404
}
```

- [ ] **Step 2: Run them.** The route test fails because the routes still exist. The config test passes already, and pins the behaviour for the removal that follows.

- [ ] **Step 3: Delete and edit the files above.** Follow compiler and clippy errors until nothing references `enrichment`, `reqwest` or `MetadataSettings`.

- [ ] **Step 4: Check.** Run the API verification, plus `rg -n "enrichment|musicbrainz|lrclib|reqwest" crates .github config.toml.example`. Expected: no hits outside `docs/superpowers`.

- [ ] **Step 5: Commit** `refactor: remove music enrichment`

### Task 2: Remove play history and music stats

**Files:**
- Delete:
  - `crates/alexandria-core/src/plays/`
  - `crates/alexandria-http/src/routes/plays.rs`
  - `crates/alexandria-core/tests/plays.rs`, `tests/plays/`
  - `crates/alexandria-http/tests/plays_api.rs`
- Modify:
  - `alexandria-core/src/lib.rs:14`, `services.rs` (play aliases, fields, construction)
  - `routes/mod.rs:13`, `src/lib.rs:195-199`
  - `alexandria-ffi/src/lib.rs`: remove the `PLAY_*` consts, `PlayJsonResult`, `RecordPlayBody`, `MusicStatsQuery`, `alexandria_play_record` and `alexandria_music_stats`
  - The play cases in `ffi/tests/smoke.rs` and `parity.rs`

- [ ] **Step 1: Write the failing test.** Add `given_the_plays_routes_are_gone_when_called_then_not_found` (POST `/v1/plays`, GET `/v1/plays/stats` → 404) next to Task 1's route test.
- [ ] **Step 2: Run it.** Expected: FAIL.
- [ ] **Step 3: Delete and edit the files above.** The `play_events` table stays until Task 6.
- [ ] **Step 4: Run the API verification.** Expected: PASS.
- [ ] **Step 5: Commit** `refactor: remove play history and music stats`

### Task 3: Remove playlists

**Files:**
- Delete:
  - `crates/alexandria-core/src/playlists/`
  - `crates/alexandria-http/src/routes/playlists.rs`
  - `crates/alexandria-core/tests/playlists.rs`, `tests/playlists/`, `tests/playlist_batching.rs`
  - `crates/alexandria-http/tests/playlists_api.rs`
- Modify:
  - `alexandria-core/src/lib.rs:13`, `services.rs`
  - `catalog/repos.rs`: inside `purge` (`:2103-2180`), remove the `playlist_entries` cleanup and renumbering
  - `routes/mod.rs:12`, `src/lib.rs:174-194`
  - `alexandria-ffi/src/lib.rs`: remove the `PLAYLIST_*` consts, `PlaylistJsonResult`, and every `alexandria_playlist*` / `alexandria_playlists_list` export
  - `tests/repos_integrity.rs:257` (playlist purge case: delete it)
  - FFI `smoke.rs:2814-2938`, plus the playlist cases in `parity.rs`

- [ ] **Step 1: Write the failing test.** Add `given_the_playlist_routes_are_gone_when_called_then_not_found` (GET/POST `/v1/playlists` → 404).
- [ ] **Step 2: Run it.** Expected: FAIL.
- [ ] **Step 3: Delete and edit the files above.** The `playlists` and `playlist_entries` tables stay until Task 6.
- [ ] **Step 4: Run the API verification.** Expected: PASS.
- [ ] **Step 5: Commit** `refactor: remove playlists`

### Task 4: Remove track energy and derived lyrics/energy content

**Files:**
- Delete: `crates/alexandria-core/src/playback/energy.rs`, `crates/alexandria-core/tests/track_energy_decode.rs`
- Modify:
  - `playback/mod.rs:8`, `services.rs` (`DefaultEnergyHandler` `:179`)
  - `catalog/repos.rs`:
    - delete the trait method `forget_derived_content` (`:325`) and its impl (`:2194-2222`)
    - in `purge`, remove the `track_lyrics` / `track_energy` cleanup
  - `catalog/commands/refresh.rs:555` (caller)
  - `playback/test_support.rs:249`
  - `crates/alexandria-core/tests/common/mod.rs`: remove the `forget_derived_content` tracking at `:129, 253, 782, 2919`
  - `tests/catalog/refresh.rs:311, 351`: delete these derived-content tests
  - `alexandria-http/src/lib.rs:83`, `routes/playback.rs:207-230`
  - `alexandria-http/Cargo.toml`: remove `base64` (energy is its only user there)
  - `alexandria-ffi/src/lib.rs`: remove `alexandria_track_energy` (`:1161`) and its tests

- [ ] **Step 1: Write the failing test.** Add `given_the_energy_route_is_gone_when_called_then_not_found` (GET `/v1/files/{uuid}/energy` → 404).
- [ ] **Step 2: Run it.** Expected: FAIL.
- [ ] **Step 3: Delete and edit the files above.**
- [ ] **Step 4: Run the API verification.** Expected: PASS.
- [ ] **Step 5: Commit** `refactor: remove track energy`

### Task 5: Remove the audio file type and lofty

**Files:**
- Delete: `crates/alexandria-core/src/catalog/audio_tags.rs` (`catalog/mod.rs:1`); `crates/alexandria-core/tests/metadata_backfill.rs`, except the video case `given_a_media_kind_the_owner_set_when_filled_then_it_survives` (`:121`), which moves into `tests/catalog/refresh.rs`
- Modify (core):
  - `catalog/model.rs`: remove these
    - `FileType::Audio`
    - the `as_str`, `from_wire` and `subtype_table` arms
    - `SubtypeMetadata::Audio`
    - the `file_type()` arm

    Keep `METADATA_VERSION` and its value unchanged; the fill mechanism stays generic.
  - `catalog/classify.rs:25`: drop the arm (`mp3 flac wav ogg oga m4a aac opus wma`) so those return "unrecognised"
  - `catalog/extraction.rs`: drop the `audio: M` type parameter and field, the `:102` arm and `audio_into`
  - `catalog/commands/index.rs` and `refresh.rs`: drop the `M: AudioMetadataReader` generic
  - `catalog/repos.rs`: remove these
    - the audio insert and delete SQL
    - `batch_audio`, `AudioRow`, `AudioBatchRow`
    - the audio arms in update, fill and find_metadata, and in `parse_type_str`
    - trait `set_audio_duration` (`:239`, impl `:1804-1835`)
  - `catalog/index_scope.rs:60`: the error text lists `video, html, text, document, comic, image`
  - `playback/mime.rs:19-26`
  - `playback/thumbnail.rs`: drop the `V: CoverArtReader` generic and field, the Audio arm (`:168-205`), `FakeCoverArt` and the audio tests
  - `playback/test_support.rs:204`
  - `services.rs`: remove the Lofty imports and readers from the `Default*Handler` aliases
- Modify (http and ffi):
  - `alexandria-http/src/routes/browse.rs:143`
  - `alexandria-ffi/src/lib.rs:3027` (`parse_file_type`)
- Modify (Cargo): remove `lofty` from the workspace `Cargo.toml` and from core deps, http dev-deps and ffi dev-deps
- Modify (tests):
  - `crates/alexandria-core/tests/common/mod.rs`: remove these
    - the `FakeAudioMetadataReader`, `InterruptingAudioMetadataReader` and `ConcurrencyTrackingAudioMetadataReader` fakes
    - `audio_durations`
    - `set_audio_duration`

    Also make `a_cataloged_file` use `FileType::Text`.
  - `crates/alexandria-http/tests/common/mod.rs:330-395`: remove `write_minimal_wav`, `write_audio_with_cover_art` and `write_audio_without_cover_art`
  - Any other core, http or ffi test that uses `.mp3`/`.flac` or `FileType::Audio`: delete the audio-specific tests the explore pass named, and refit the rest.
    - Specific tests to delete:
      - in `catalog/index.rs`: `given_tagged_audio_file…`, `given_untagged_audio_file…`, `given_non_audio_file…`, `audio_library`, `album_with_cover_art`
      - the audio fill tests in `refresh.rs`
      - `migrations.rs:220`
      - the audio cases in `browse_batching.rs`, `edit_metadata.rs`, `browse.rs`, `throughput.rs` (and its `write_audio_tags`)
      - the audio cases in `playback_thumbnail_api.rs`, `catalog_api.rs`, `smoke.rs` and `parity.rs`
    - How to refit the rest:
      - Where an audio file stood in for "some file that isn't X", use `.txt` (`FileType::Text`).
      - Where a test needs two distinct types (the scope tests at `catalog/index.rs:3583-3895`, `index_scope.rs` units), use `.png` (`FileType::Image`) as the second type.

**Interfaces:** Produces: `FileType` with six variants; `IndexScope::parse_list("audio")` returns `DomainError::InvalidInput`. Task 6 relies on that error.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/alexandria-core/src/catalog/classify.rs (unit)
#[test]
fn given_an_audio_extension_when_classified_then_it_is_unrecognised() {
    for ext in ["mp3", "flac", "wav", "ogg", "oga", "m4a", "aac", "opus", "wma"] {
        assert_eq!(classify_by_extension(Path::new(&format!("a.{ext}"))), None);
    }
}
```
```rust
// crates/alexandria-core/tests/catalog/index.rs
#[tokio::test]
async fn given_a_folder_mixing_audio_video_and_text_when_indexed_then_only_video_and_text_are_cataloged() {
    // folder: song.mp3, track.flac, film.mp4, notes.txt
    // assert: catalog lists exactly film.mp4 (video) and notes.txt (text)
    // assert: run status is completed and catalog_run_failures for the run is empty
}
```
```rust
// crates/alexandria-core/src/catalog/index_scope.rs (unit)
#[test]
fn given_audio_in_a_scope_when_parsed_then_it_is_rejected() {
    assert!(matches!(IndexScope::parse_list("audio"), Err(DomainError::InvalidInput(_))));
}
```

- [ ] **Step 2: Run them.** Expected: all three fail.
- [ ] **Step 3: Remove `Audio` and `lofty`, and refit the fixtures as listed above.**
- [ ] **Step 4: Check.** Run the API verification, plus `rg -n "lofty|FileType::Audio|SubtypeMetadata::Audio|\\.mp3|\\.flac" crates`. Expected: no hits except the classify test's extension list.
- [ ] **Step 5: Commit** `refactor!: remove the audio file type`

### Task 6: Migration that removes stored music

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000025_remove_audio.sql`
- Test: `crates/alexandria-core/tests/migrations.rs`. Reuse its pattern: apply migrations to version 24 one by one, seed, then `run_migrations`.

The SQL is fixed by SQLite's rules, so it's written out here. The first line must be `-- no-transaction`, because `PRAGMA foreign_keys=OFF` does nothing inside a transaction. With FKs on, `DROP TABLE files` would cascade-delete every subtype row.

```sql
-- no-transaction
PRAGMA foreign_keys = OFF;
BEGIN;

DELETE FROM catalog_run_failures WHERE run_id IN (SELECT id FROM catalog_runs WHERE scope = 'audio');
DELETE FROM catalog_runs WHERE scope = 'audio';
UPDATE catalog_runs
   SET scope = TRIM(REPLACE(',' || scope || ',', ',audio,', ','), ',')
 WHERE ',' || scope || ',' LIKE '%,audio,%';

DELETE FROM reading_progress WHERE item_file_id IN (SELECT id FROM files WHERE type = 'audio');
DELETE FROM watch_progress   WHERE video_file_id IN (SELECT id FROM files WHERE type = 'audio');

DROP TABLE playlist_entries;
DROP TABLE playlists;
DROP TABLE track_lyrics;
DROP TABLE track_energy;
DROP TABLE play_events;
DROP TABLE artist_images;
DROP TABLE audio_files;

DELETE FROM files WHERE type = 'audio';

CREATE TABLE files_new ( /* every current column of files, in order, identical except:
   type TEXT NOT NULL CHECK (type IN ('video','html','text','document','comic','image')) */ );
INSERT INTO files_new SELECT * FROM files;   -- list columns explicitly
DROP TABLE files;
ALTER TABLE files_new RENAME TO files;
CREATE INDEX idx_files_type    ON files(type);
CREATE INDEX idx_files_state   ON files(state);
CREATE INDEX idx_files_library ON files(library_id);

COMMIT;
PRAGMA foreign_keys = ON;
```

The `files` columns, in order:
- `id`, `uuid`, `path`, `name`, `type`, `content_hash`, `size_bytes`, `mtime`
- `state` (CHECK `active`/`deleted`, default `active`), `deleted_at`, `indexed_at`, `collection_id`
- `missing_at` (002), `library_id` (020), `metadata_version INTEGER NOT NULL DEFAULT 0` (022)

Copy each column's type and constraints from `migrations/00000000000001_catalog.sql:10-28` and from the later `ALTER`s. Before writing the deletes, grep the migrations for any other table holding a file id. If one exists, add a `DELETE … WHERE … IN (SELECT id FROM files WHERE type='audio')` for it.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn given_a_database_with_music_when_migrated_then_music_is_gone_and_everything_else_survives() {
    // Seed at version 24:
    //   files: audio (id A) with audio_files, playlist + entry, play_event, track_lyrics,
    //   track_energy rows; artist_images row; video (id V) with video_files row and
    //   watch_progress; text (id T) with text_files row.
    //   catalog_runs: r1 scope 'audio' status 'paused' with a catalog_run_failures row,
    //   r2 'audio,image', r3 'image,audio,video', r4 'video', r5 NULL.
    // Run run_migrations.
    // Assert:
    //   files ids == {V, T}; video_files and text_files rows intact; watch_progress for V intact
    //   tables audio_files, playlists, playlist_entries, track_lyrics, track_energy,
    //     play_events, artist_images do not exist (sqlite_master)
    //   catalog_runs: r1 and its failure gone; r2 scope 'image'; r3 'image,video'; r4 'video'; r5 NULL
    //   PRAGMA foreign_key_check returns no rows; PRAGMA foreign_keys returns 1
    //   INSERT INTO files(... type='audio' ...) fails with a CHECK constraint error
}

#[tokio::test]
async fn given_a_migrated_database_with_mixed_scope_runs_when_runs_are_listed_then_none_is_corrupt() {
    // Seed as above. Migrate. Read r2 and r3 through the runs repository (the path row_to_run uses).
    // Assert: Ok, and scopes parse to {Image} and {Image, Video}.
}
```

- [ ] **Step 2: Run** `cargo test -p alexandria-core --test migrations`. Expected: FAIL.
- [ ] **Step 3: Write the migration** as specified above.
- [ ] **Step 4: Run the API verification.** Expected: PASS, including the existing `given_migrated_database_when_connected_then_wal_and_foreign_keys_enabled`.
- [ ] **Step 5: Commit** `feat: delete stored music on upgrade`

### Task 7: API docs and version 0.5.0

**Files:**
- Modify:
  - `Cargo.toml:25` → `0.5.0`, and add a line to the history comment at `:10-21`. Run `cargo build` to refresh `Cargo.lock`.
  - `alexandria-ffi/src/lib.rs:301`: update the doc example version
  - `CHANGELOG.md`: change `(currently 0.4.0)` to 0.5.0. Under `## [Unreleased]`, add `### Removed` listing:
    - the audio type
    - tag reading
    - playlists
    - play history and stats
    - enrichment (and the `[metadata]` config)
    - track energy
    - the removed HTTP routes and FFI functions
    - the upgrade migration's data deletion

    Edit existing Added and Fixed bullets that describe now-removed music features so they don't describe shipping features.
  - `README.md`:
    - intro `:4`
    - feature table `:36-49`: drop F-10 energy, F-11, F-13 and F-14, and fix the "last four" sentence
    - the matching sections at `:179-215`
  - `docs/requirements/Use Case Specification Document.md`:
    - mark UC-49 and UC-50 Removed
    - drop audio from `:116, 120, 215, 230, 678, 845`, UC-40 `:1245-1265` and `:1900-1910`
  - `docs/requirements/System Requirements Document.md`:
    - mark §3.8 (audio parts), §3.9, §3.10 and §3.11 Removed
    - §4.12-4.13, §5.9-5.12 and traceability `:845-893`
    - plus `:30, 84, 97, 108-109, 159, 172`
  - `docs/requirements/Vision Document.md`, `Technology Stack Document.md:93` (lofty row), `Testing Specification Document.md:285, 338`
  - `docs/System Behavior Document.md` (`:337`, §5.12-5.14, §6.3)
  - `docs/initial/Business Rules.md`, `Project Overview.md`, `Brainstorm.md`: describe Alexandria as a library to read and watch, and name Orpheus as the home for music

- [ ] **Step 1: Make the edits.**
- [ ] **Step 2: Check.** Run `rg -n -i "audio|music|playlist|lyric|enrichment|lofty" README.md CHANGELOG.md config.toml.example docs --glob '!docs/superpowers/**'`. Expected: hits only for video audio tracks, the Removed markers, the CHANGELOG Removed section, and Orpheus pointers.
- [ ] **Step 3: Run the API verification.** Expected: PASS.
- [ ] **Step 4: Run `cargo build -p alexandria-ffi`.** This regenerates `crates/alexandria-ffi/src/header.h` (gitignored), which Task 13 needs.
- [ ] **Step 5: Commit** `docs: describe alexandria without music`

---

## Part B — alexandria-ui

Before Task 8:
```bash
git switch develop && git switch -c feature/remove-music
```
Ignore the existing stat-only `M` entries on `linux/` and `windows/flutter/generated_*`; they have no content diff. Tasks 8–12 run against the current 0.4 core. Build it with `tools/dev.ps1` from an `alexandria-api` checkout of `develop`.

### Task 8: Remove enrichment, lyrics and artist portraits

**Files:**
- Delete:
  - `lib/features/enrichment/`
  - `test/features/enrichment/`, `test/support/fake_enrichment_gateway.dart`
  - `integration_test/enrichment/`
- Modify:
  - `core/di/providers.dart`: remove these providers
    - `enrichmentGatewayProvider`, `trackEnrichmentControllerProvider`
    - `artistImageControllerProvider`, `artistPortraitBackfillProvider`
    - `enrichmentSweepControllerProvider`, `enrichmentRunControllerProvider`

    Also remove their imports.
  - `core_client.dart` and `core_isolate.dart`: remove `enrichmentRun`, `enrichmentReadTrack`, `artistImage` and `artistImageFetch`, and remove the `musicLookup` parameter from `CoreClient.initialize`
  - `core_environment.dart`: remove these
    - `MusicLookup`, `defaultMusicLookupContact`, `ensureMusicLookup`
    - `coreImageCacheDirVariable` and the `artist-images` entry in `cacheDirectoriesIn`
    - `prepareCoreEnvironment(musicLookup:)`
  - `startup_controller.dart`: remove `MusicLookupApplication`, `musicLookup`, `applyMusicLookup` and `_appliedMusicLookup`
  - Settings: remove `musicLookupEnabled` and `musicLookupContact` from these files
    - `settings_store.dart`, `shared_preferences_settings_store.dart`
    - `preferences_state.dart`, `preferences_controller.dart`, `preferences_dialog.dart:115-151`

    Stored keys are simply no longer read; nothing deletes them.
  - `active_runs_controller.dart:287-294` (`retryDeferredMusicLookup`)
  - `background_activity_strip.dart` (artist-portrait rows)
  - `library_menu.dart` (`EnrichmentSweepScreen` entry)
  - `CoreStatusFamily.enrichment` and `_mapEnrichment`
  - l10n: the `enrichment*`, `lyrics*` and `artistPortraits*` keys in `app_en.arb` / `app_pt.arb`
  - `git rm` the stray tracked `artist-images/*.jpg`
  - Tests:
    - `core_environment_test` (groups at `:81`, `:154`), `startup_controller_test:234`, `preferences_dialog_test:550`, `background_activity_strip` test
    - `MusicLookup.off` arguments in `core_boundary_test`, `catalog/index_scope_test`, `listing_shape_test`, `library_boundary_test`, `auth/log_in_test`, `auth/sign_up_test`
- Test: `test/core/settings/shared_preferences_settings_store_test.dart`

- [ ] **Step 1: Write the test.** `GivenStoredMusicLookupKeys_WhenSettingsLoad_ThenTheyAreIgnored`: seed shared_preferences with `musicLookupEnabled=true` and `musicLookupContact`, load settings, and expect success. It passes after removal and pins Review Focus 4.
- [ ] **Step 2: Delete and edit the files above.**
- [ ] **Step 3: Run the UI verification.** Expected: PASS.
- [ ] **Step 4: Commit** `refactor: remove music enrichment`

### Task 9: Remove music stats and play recording

**Files:**
- Delete: `lib/features/stats/`, `test/features/stats/`, `test/support/fake_stats_gateway.dart`
- Modify:
  - `providers.dart`: remove `statsGatewayProvider`, `playRecorderProvider` and `musicStatsControllerProvider`
  - Delete each call site of `playRecorderProvider`
  - `core_client`/`core_isolate`: remove `playRecord` and `musicStats`
  - `library_menu.dart` (`MusicStatsScreen`)
  - `CoreStatusFamily.play` and `_mapPlay`
  - l10n: the `musicStats*` keys
- Test: no new test; deletion only.

- [ ] **Step 1: Delete and edit the files above.**
- [ ] **Step 2: Run the UI verification.** Expected: PASS.
- [ ] **Step 3: Commit** `refactor: remove music stats`

### Task 10: Remove playlists

**Files:**
- Delete:
  - `lib/features/playlists/`, `test/features/playlists/`, `test/support/fake_playlist_gateway.dart`
  - `integration_test/playlists/`
- Modify:
  - `providers.dart`: remove `playlistGatewayProvider`, `playlistsControllerProvider`, `playlistsFormProvider` and `playlistDetailControllerProvider`
  - `core_client`/`core_isolate`: remove every `playlist*` method; `core_client_test:74-170`
  - `file_details_view.dart:280` (`AddToPlaylistButton`) and its test case `GivenAnAudioFile_…JoinAPlaylist`
  - `library_menu.dart` (`PlaylistsScreen`)
  - `CoreStatusFamily.playlist` and `_mapPlaylist`
  - l10n: the `playlist*` keys
- Test: no new test; deletion only.

- [ ] **Step 1: Delete and edit the files above.**
- [ ] **Step 2: Run the UI verification.** Expected: PASS.
- [ ] **Step 3: Commit** `refactor: remove playlists`

### Task 11: Remove music playback, the player bar and the music library

**Files:**
- Delete (from `lib/features/playback/`):
  - application: `album_art_controller`, `album_cover_controller`, `audio_playback_controller`, `audio_playback_session`, `music_browse_controller`, `music_layout_controller`, `music_library_controller`, `track_energy_controller`, `playback_session_activity`
  - data: `core_energy_gateway`
  - domain: `album_cover`, `music_browse`, `music_grouping`, `playback_queue`, `sleeve_design`, `track_energy`
  - presentation: `album_visor`, `music_library_view`, `music_rows`, `now_playing_screen`, `sound_bars`
- Delete (outside `lib/features/playback/`):
  - `lib/features/shell/presentation/playback_bar.dart`
- Delete (tests):
  - `test/features/playback/`:
    - application: `album_cover_controller`, `audio_playback_controller`, `music_browse_controller`, `music_layout_controller`, `music_library_controller`, `playback_session_activity`
    - data: `core_energy_gateway`
    - domain: `music_browse`, `music_grouping`, `sleeve_design`, `track_energy`
    - presentation: `album_visor`, `audio_player`, `music_context_menu`, `music_library_view`, `music_rows`, `now_playing_screen`, `sound_bars`
  - `integration_test/playback/track_energy_test.dart`
  - `integration_test/catalog/metadata_backfill_test.dart`
  - `integration_test/support/flac_fixture.dart`, `wav_fixture.dart`
- Keep: the video files the explore pass listed (`video_playback_*`, `media_kit_player`, `playback_session` with `PlaybackMedium`, `video_player_screen`, `video_surface`, the position store, and `playback_colors`). `music_display_name.dart` stays until Task 12.
- Modify:
  - `providers.dart`:
    - remove `audioPlayerProvider`, `musicLibraryProvider`, `musicBrowseControllerProvider`, `musicLayoutControllerProvider`, `albumArtControllerProvider`, `shuffleRandomProvider`, `audioPlaybackControllerProvider`, `albumCoverControllerProvider`, `energyGatewayProvider` and `trackEnergyControllerProvider`
    - drop `AudioPlaybackSession` from `playbackSessionsProvider`, `AudioFileHold` from `fileHoldsProvider`, and `PlaybackSessionActivity` from `sessionActivitiesProvider`
  - `playback_file_holds.dart:25` (`AudioFileHold`)
  - `PlaybackMedium`: drop the `audio` value if nothing else uses it
  - `catalog_session_activity.dart` and `catalog_projections.dart:27`: remove the music invalidations
  - `shell_screen.dart`:
    - the now-playing listener at `:68-75`
    - `PlaybackBar` at `:102`
    - `MusicLibraryView` at `:158-160`
  - `ShellDestination.music` and its uses in `shell_navigation_panel.dart` and `file_type.dart:61`
  - `file_details_view.dart:191, 551`: play buttons and the audio open path. Audio is no longer a type after Task 12, so here just remove the music controller calls.
  - `core_client`/`core_isolate`: remove `trackEnergy`
  - Settings: remove `opensPlayerOnPlay` from the settings stores and from the preferences state, controller and dialog
  - `pubspec.yaml`: remove `media_kit_libs_audio` and the `AlexandriaSleeve` font, then run `flutter pub get` to regenerate `windows/flutter/generated_plugin*`
  - `test/flutter_test_config.dart:29`
  - l10n: the `music*`, `audio*`, `albumCoverLabel`, `tracks`, `playerAutoOpen*` and `destinationMusic` keys. Keep `videoAudioTracks` and `videoNoAudioTracks`.
  - Tests:
    - `video_player_test:488`: rewrite as `GivenAVideoIsPlaying_WhenAnotherVideoStarts…`, or delete it if it only covered audio-to-video handoff
    - `media_kit_test:31` (`…AudioEngine…`)
    - the shell tests
  - Goldens: run `flutter test --update-goldens test/features/shell`, then look at the 6 PNGs to confirm the Music entry is gone and nothing else moved oddly

- [ ] **Step 1: Write the test.** `GivenStoredOpensPlayerOnPlay_WhenSettingsLoad_ThenItIsIgnored` in the settings store test.
- [ ] **Step 2: Delete and edit the files above, and regenerate the goldens.**
- [ ] **Step 3: Run the UI verification.** Expected: PASS.
- [ ] **Step 4: Manual check on Windows.** Run `flutter run -d windows`, play a video that has sound, and confirm you can hear it and that the audio-track menu lists its tracks.
- [ ] **Step 5: Commit** `refactor: remove music playback`

### Task 12: Remove the audio file type, the music metadata editor, and audio scopes from tracked folders

**Files:**
- Create: `lib/features/library_sources/domain/audio_scope_cleanup.dart`
- Delete:
  - `lib/features/catalog/application/music_metadata_editor.dart`, `domain/music_metadata.dart`, `presentation/music_metadata_form.dart`
  - `lib/features/playback/presentation/music_display_name.dart`
  - `test/features/catalog/domain/music_metadata_test.dart`, `catalog/presentation/music_metadata_form_test.dart`
- Modify:
  - `file_type.dart`: remove `audio('audio')`, and change the "Seven" comments to six
  - `catalog_search_view.dart:185, 396`
  - `file_details_view.dart`: remove `:615` and the imports at `:14-17`
  - `home_dashboard.dart:89-97`
  - `delete_record_button.dart:52`, `missing_files_screen.dart:144`
  - `index_scope_dialog.dart:13`
  - `music_library_controller` references, if any remain
  - `libraries/domain/library.dart:4,51` (`MusicMetadata? metadata`)
  - `CatalogGateway.editMusicMetadata` and `MetadataEditOutcome.saved(MusicMetadata)`, in `catalog_gateway.dart:52,161` and `core_catalog_gateway.dart:122`
  - `providers.dart` (`musicMetadataEditorProvider`)
  - l10n: `fileTypeAudio`
  - `startup_controller.dart`: call the cleanup (Step 3)
  - Tests:
    - `catalog_search_test:326`, `file_details_test:583-650`, `home_dashboard_test:180`, `catalog_listing_test:331`
    - `text_editor_test:175`, `delete_record_test:200`, `missing_files_test:148`
    - `index_scope_test:105`, `index_scope_dialog_test:224`
    - fixture uses in the listed tests and `test/support/` fakes: replace audio with text or image the same way as Task 5
- Test: `test/features/library_sources/domain/audio_scope_cleanup_test.dart`, `test/core/startup/startup_controller_test.dart`

**Interfaces:**
- Produces: `({List<LibrarySource> kept, List<LibrarySource> removed}) cleanUpAudioScopes(List<LibrarySource> sources)`.
- It works on the raw `scope` wire strings. It must not use `scopeTypes`, which already drops unknown names.
  - Any source whose `scope` is non-empty and contains only `'audio'` goes to `removed`.
  - Any other source goes to `kept`, with `'audio'` removed from its `scope`.
  - Order is preserved.

- [ ] **Step 1: Write the failing tests**

```dart
test('GivenAMusicOnlyFolder_WhenCleanedUp_ThenItIsRemoved', () {
  final r = cleanUpAudioScopes([source('/music', scope: ['audio'])]);
  expect(r.removed.map((s) => s.path), ['/music']);
  expect(r.kept, isEmpty);
});
test('GivenAMixedFolder_WhenCleanedUp_ThenAudioIsDroppedAndTheRestKept', () {
  final r = cleanUpAudioScopes([source('/mix', scope: ['audio', 'image'])]);
  expect(r.kept.single.scope, ['image']);
  expect(r.removed, isEmpty);
});
test('GivenAnEveryTypeFolder_WhenCleanedUp_ThenItIsUnchanged', () {
  final r = cleanUpAudioScopes([source('/all', scope: [])]);
  expect(r.kept.single.scope, isEmpty);
});
```
In `startup_controller_test`, add `GivenAStoredMusicOnlyFolder_WhenTheAppStarts_ThenItIsUnregisteredAndTheMixedOneLosesAudio`:
- Seed `'librarySources'` with `/music` (`['audio']`, `libraryName: 'Music'`) and `/mix` (`['audio','video']`), then start.
- Expect the store to hold only `/mix` with scope `['video']`.
- Expect the core library removal to have been called for `/music`.

- [ ] **Step 2: Run them.** Expected: FAIL.
- [ ] **Step 3: Implement and wire the cleanup.** Implement `cleanUpAudioScopes`. Then call it in `StartupController.start()` after preferences load (`_step3LoadPreferences`) and after the core is initialised:
  - Read the `LibrarySourceStore`.
  - If anything changed, write the `kept` list back.
  - For each `removed` source, unregister it through the same code path the existing unregister-folder flow uses. That path calls core `libraryRemove` when the source has a `libraryName`.

  It runs on every launch. Once the stored data is clean it changes nothing, so no flag is needed.
- [ ] **Step 4: Remove `FileType.audio` and the editor, and refit the tests.**
- [ ] **Step 5: Run the UI verification.** Also run `rg -n "FileType.audio|MusicMetadata|music_display_name" lib test integration_test`. Expected: PASS, no hits.
- [ ] **Step 6: Commit** `refactor!: remove the audio file type`

### Task 13: Switch the UI to the 0.5 core

**Files:**
- Modify:
  - `native/include/alexandria_ffi.h`: copy it verbatim from `../alexandria-api/crates/alexandria-ffi/src/header.h` (built in Task 7 on the api `feature/remove-music` branch)
  - `ffigen.yaml`: drop the `PLAYLIST_.*`, `PLAY_.*` and `ENRICHMENT_.*` macro includes
  - `lib/core/bindings/alexandria_bindings.dart`: regenerate it
  - `lib/core/startup/core_version.dart:17,20`: set `minimum = '0.5.0'` and `exclusiveMaximum = '0.6.0'`
  - `core_client.dart:21` doc; `core_version_test`
- Leave `CORE_REF` in `.github/workflows/ci.yml:46` and `release.yml:83` alone for now. It must name an api commit that exists on GitHub; see Task 15.

- [ ] **Step 1: Update `core_version_test`.** It should expect `0.5.0` accepted, and `0.4.9` and `0.6.0` rejected. Run it; expected: FAIL.
- [ ] **Step 2: Rebuild and regenerate.** Run `tools/dev.ps1` against `../alexandria-api` on `feature/remove-music`, copy the header, run `dart run ffigen --config ffigen.yaml`, then bump the version.
- [ ] **Step 3: Run the UI verification.** Then run the core-boundary integration tests one at a time:
  - `integration_test/**/core_boundary_test.dart`
  - `library_boundary_test.dart`
  - `catalog/index_scope_test.dart`

  Expected: PASS against the 0.5 core.
- [ ] **Step 4: Commit** `build: require core 0.5`

### Task 14: UI docs and packaging

**Files:**
- Modify:
  - `README.md`:
    - intro `:7`; features `:22, 30-37, 42, 57`
    - roadmap `:148-173`; backlog `:237, 247-248`
    - M-12 section `:297-309`: mark it Removed
  - `CHANGELOG.md`: under `## [Unreleased]`, add `### Removed` (music playback, the player bar, the music library, playlists, stats, lyrics and portraits, the music metadata editor, the audio type) and `### Changed` (needs core 0.5; music-only folders are unregistered on upgrade)
  - `docs/requirements/Use Case Specification Document.md`:
    - mark UC-15 (`:646`), UC-20 (`:823`), UC-21 (`:859`), UC-46 (`:1764`), UC-47 (`:1825`), UC-48 (`:1886`) and UC-50 (`:1999`) Removed
    - drop audio from the diagram (`:78-109`), UC-19 and the other hits, traceability `:2081-2116`, and §4.5 `:2209`
  - `docs/requirements/System Requirements Document.md`:
    - mark FR-CT-13/14, FR-ME-01, FR-PL-05…07 and 11…17, FR-TR-15…20 and FR-UX-13 Removed
    - FFI table `:535-541`, screens table `:497-512`, intro `:31-33`
  - `Vision`, `Operations & Infrastructure`, `Technology Stack` and `Testing Specification` docs
  - `docs/initial/*`: same framing as Task 7, naming Orpheus as the home for music
  - Packaging:
    - `metainfo.xml:11`: change to "plays video"
    - `.desktop` file and `install.sh:433`: change `Categories=AudioVideo;Player` to `AudioVideo;Video;Player`
    - Keep `--socket=pulseaudio` (video needs it) and update the comment at `io.github.artur_rios.Alexandria.yaml:39`

- [ ] **Step 1: Make the edits.**
- [ ] **Step 2: Check.** Run `rg -n -i "music|playlist|lyric|enrichment|now playing" README.md CHANGELOG.md docs packaging --glob '!docs/superpowers/**'`. Expected: only Removed markers, the CHANGELOG, and Orpheus pointers.
- [ ] **Step 3: Commit** `docs: describe alexandria without music`

---

## Part C — alexandria-docs and release wiring

### Task 15: Docs site, then release wiring

**Files (alexandria-docs, branch `feature/remove-music` from `main`):**
- `content/en/_index.md:20-21, 34-35`: "Read and watch", and remove the persistent audio player feature
- `content/en/about/_index.md:14`: drop music from the list, and point to Orpheus for music
- `content/en/docs/overview/_index.md`:
  - delete the Audio row at `:29`; keep `:30`
  - delete `:45-46`
  - `:49`: change to "Edit video metadata"
  - `:65`: change to "No video re-encoding"
- `content/en/docs/usage/_index.md`:
  - `:58`: drop "music"
  - delete the Audio paragraph at `:70-72`
  - `:84`: change to **Video metadata**; keep `:67`

- [ ] **Step 1: Make the edits.** Then run `npm ci && hugo --minify && test -s public/index.html`. Expected: the build succeeds.
- [ ] **Step 2: Check.** Run `rg -n -i "audio|music|playlist|lyric" content --glob '!**/superpowers/**'`. Expected: only video audio-track mentions and Orpheus pointers.
- [ ] **Step 3: Commit** `docs: describe alexandria without music`
- [ ] **Step 4: Stop and ask the owner before pushing anything.** Once the owner agrees:
  - Push the api `feature/remove-music` branch and open its PR into `develop`.
  - After it merges, set `CORE_REF` in the UI's `ci.yml:46` and `release.yml:83` to the merged api commit. Commit that as `build: pin core to the music-free api`, push the UI branch, and open its PR into `develop`.
  - Open the docs PR into `main`.

## Final verification

- API: run the API verification on `feature/remove-music`. Then run a server against a copy of a real 0.4 database and check:
  - `/v1/files?type=video` still lists the videos
  - `/v1/files/{uuid}/stream` serves a video
  - GET `/v1/playlists` returns 404
- UI: run the UI verification, plus every integration test touched. Then launch the app against that same migrated database and check:
  - no Music nav entry and no player bar
  - videos play with sound
  - a former music-only folder is gone from Library sources
- Docs: run `hugo --minify`.
