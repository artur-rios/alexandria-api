# Remove Music and Audio: Follow-ups

Follow-up work left after removing music and audio across alexandria-api, alexandria-ui and alexandria-docs. Each item was found by a task review or the final whole-branch review. None is a correctness bug: the final review found no Critical issues and no code defects. Everything here is a test gap, a stale comment, a wording fix or hardening.

**Context**
- Spec: `docs/superpowers/specs/2026-10-21-remove-music-and-audio-design.md`
- Plan: `docs/superpowers/plans/2026-10-21-remove-music-and-audio.md`

**State when this was written (2026-10-21)**
- alexandria-api: artur-rios/alexandria-api#137 is merged into `develop` (`ab69754`). Workspace version 0.5.0.
- alexandria-ui: artur-rios/alexandria-ui#129 is open into `develop`. It is branch `feature/remove-music`, head `8ab02fa`, with `CORE_REF` pinned to `ab69754`.
- alexandria-docs: artur-rios/alexandria-docs#2 is open into `main`. It is branch `feature/remove-music`, head `022ba46`.

Line numbers are as they were at review time. Find each spot by its text if a line has moved.

## 0. Gates before merging alexandria-ui#129

These come first. They are checks, not code.

- [ ] **The Linux `test` job on #129 is green.** The six shell golden base PNGs in `test/features/shell/presentation/goldens/` are Windows renders.
  - `test/flutter_test_config.dart` (`_platformVariant`) treats base files as Linux renders. Windows renders belong in `.windows.png` variants.
  - The measured drift is 0.15–0.26%, against a 0.5% tolerance.
  - If the job fails, regenerate the bases on Linux (WSL, or a CI artifact) with `flutter test --update-goldens test/features/shell`, and keep the Windows renders as `.windows.png`.
  - Also update the stale typeface notes and the over-long line in `flutter_test_config.dart` (~:51-52, :57-60, :72, :133-136).
- [ ] **Video still has sound on Windows.** `media_kit_libs_audio` was dropped. Run `flutter run -d windows`, play a video that has sound, and confirm you hear it and that the audio-track menu lists its tracks.
- [ ] **Real-data walkthrough.** Copy a real 0.4 database, then run a 0.5 server and the UI against the copy. Check:
  - `/v1/files?type=video` still lists the videos.
  - `/v1/files/{uuid}/stream` serves a video.
  - `GET /v1/playlists` returns 404.
  - The UI has no Music entry and no player bar.
  - After sign-in, a former music-only folder is gone from Library sources.

## 1. Missing tests (do these first)

- [ ] **ui — the video time label has no test.** Its only assertions (`01:05`, `04:30`) lived in the deleted `audio_player_test`. `formatPlaybackPosition` was inlined into `video_player_screen.dart` (~:225). Add a widget assertion on the video controls' position label to `test/features/playback/presentation/video_player_test.dart`.
- [ ] **ui — nothing pins that signing out stops a playing video.** No test asserts that `sessionActivitiesProvider` includes `PlaybackSessionActivity`, or that `playbackSessionsProvider` includes `VideoPlaybackSession` (`lib/core/di/providers.dart` ~:504, ~:939). The activity and sign-out tests use fakes. Add a test that builds the real providers.
- [ ] **api — no transport test sends `audio`.** Add cases next to the existing `banana`/`sculpture` tests:
  - HTTP: `GET /v1/files?type=audio` → 400, and index `types:["audio"]` → 400.
  - FFI: the same through `files_list` and `alexandria_index_start`, giving `FILE_ERR_INVALID_INPUT` and `INDEX_ERR_INVALID_INPUT`.
- [ ] **api — migration 25 tests miss three cases** (`crates/alexandria-core/tests/migrations.rs`):
  - Seed a run whose scope has audio in last place (`'image,audio'`) and expect `image`.
  - The fresh-install test asserts only `is_err()` for an audio insert. Match the upgrade test and expect `CHECK constraint failed`.
  - The rerun test re-checks only `files` and `foreign_keys`. Also re-check `catalog_runs` and the progress tables.
- [ ] **api — thinner HTTP PATCH coverage.** The deleted audio PATCH test asserted the response envelope (`file.uuid`, `file.fileType`, `file.state`, `metadata.type`, `metadata.title`) and that an omitted field clears to NULL. The surviving video test (`crates/alexandria-http/tests/catalog_api.rs` ~:309) asserts only `mediaKind` plus the row. Fold those assertions into it.
- [ ] **ui — no `startup step 5` `CoreCallException` test.** `test/support/fake_core_client.dart` `failOnInitialize` (~:169-174) is now unused. Its doc still says `initialize` is called twice. Make it `final`, or use it for that test.

## 2. Hardening

- [ ] **ui — the sign-in folder cleanup can overwrite a write that lands in the middle.** `audio_library_cleanup_activity.dart` reads the store (~:53), awaits `browse`, `remove` and `reload`, then writes (~:86). A `lastRunOutcome` or a folder registered inside that window would be lost.
  - Fix: re-read the store just before writing, run `cleanUpAudioScopes` on the fresh read, and re-apply the retained folders by path.
  - The window is short and only open on the upgrade sign-in.
- [ ] **ui — an unauthorized `LibraryBrowseFailed` in the cleanup only logs** (~:115-120). Everywhere else, an `UnauthorizedFailure` invalidates the session (`libraries_controller.dart` ~:40-42). Do the same here.
- [ ] **api — old run-failure rows still name audio files.** Migration 25 keeps `catalog_run_failures` rows such as `song.mp3 unreadable` on surviving mixed or NULL-scope runs. They're harmless but confusing. Either add a migration that deletes failures whose `path` has an audio extension, or note the behaviour in the docs.

## 3. Wording that is wrong

- [ ] **api — the client/core direction is inverted.** `Cargo.toml` (~:13-14) and the CHANGELOG "Breaking for clients" line say "a client built against 0.4 is refused". It is the 0.4 client that refuses the 0.5 core. Rephrase both.
- [ ] **ui — the folder cleanup is described too narrowly.** `CHANGELOG.md` (~:26-28) and the System Requirements Document (~:151) say "first sign-in" and "their library is removed". It actually runs on every sign-in, retries a failed removal, and only removes a library when the folder was one.
- [ ] **ui — the CHANGELOG `### Removed` section doesn't name the "open the player on play" preference.** Keep a Changelog order also puts Changed before Removed.
- [ ] **ui — `README.md` (~:158-160) says `UC-46` … `UC-50` are "specified all the same".** Four of them are now Removed stubs.
- [ ] **ui — the System Requirements Document FFI table (~:530) lists `alexandria_file_thumbnail`,** which the UI no longer calls.
- [ ] **api — stale requirement ranges.** These still cite the Removed FR-FC-14 / FR-MP-07, while the README now says "FR-FC-15…18":
  - Use Case Specification Document ~:218, ~:1787
  - System Requirements Document ~:517, ~:746, ~:751
- [ ] **api — `config.toml.example` (~:85) still says "a tag-header read".**
- [ ] **docs — the site has no upgrade note.** Add one line to the overview or installation page: upgrading to 0.5 deletes stored music, playlists and play history, and files on disk are untouched.
- [ ] **docs — link Orpheus** in `content/en/about/_index.md` and `content/en/docs/overview/_index.md` once it has a public URL.

## 4. Stale comments

- [ ] **ui — music-lookup and re-initialization leftovers:**
  - `lib/core/bindings/core_environment.dart` ~:184-186 (re-initialization)
  - `lib/features/shell/presentation/preferences_dialog.dart` ~:40-46
  - `test/features/shell/presentation/preferences_dialog_test.dart` ~:497
  - `lib/core/startup/startup_controller.dart` ~:66-70 ("whether it may look music up")
- [ ] **ui — music wording:**
  - `active_runs_controller.dart` ~:342 ("music library")
  - `tracked_videos_controller.dart` ~:31
  - `run_estimate.dart` ~:24 ("tagged FLACs")
  - `listing_view_controller.dart` ~:16
  - `tools/clean.sh` ~:17 and `tools/clean.ps1` ~:18 ("indexed music")
- [ ] **ui — comments that cite removed UC/FR ids:**
  - `file_details_view.dart` ~:183-187, `catalog_session_activity.dart` ~:23-25 and `file_details.dart` ~:37 (UC-15)
  - `core_client.dart` ~:199, ~:211 (FR-ME-01)
  - `playback_session.dart` ~:9 and `video_playback_session.dart` ~:5-6 (UC-19 AF-05)
  - `playback_source.dart` ~:7, ~:33, ~:38 (FR-PL-05, UC-20 AF-01)
  - `media_player.dart` ~:105
  - `ffigen.yaml` ~:58 (UC-20)
  - `integration_test/catalog/index_scope_test.dart` ~:30
- [ ] **ui — `video_playback_controller.dart` (~:151-158) has a dead "stop the other medium" loop.**
  - `PlaybackMedium` now has one value.
  - The `stopForOtherMedium` doc (~:256) still says "another medium is starting".
  - Either fix the doc or remove the loop together with `PlaybackSession.medium`.
- [ ] **api — stale counts and labels:**
  - "five readers/subtypes" should be four at `catalog/commands/index.rs` ~:104, `commands/refresh.rs` ~:88, `catalog/video_tags.rs` ~:43 and `catalog/repos.rs` ~:145.
  - `tests/catalog/index.rs` ~:521 says "the unreadable mp3".
  - `crates/alexandria-http/tests/run_control_api.rs` ~:182 says "parses tags per file".
- [ ] **api — `tests/catalog/refresh.rs` ~:335:** the test is named `…_then_the_row_is_left_behind` but asserts that the row is stamped. Rename it.

## 5. Hygiene

- [ ] **api — rejected-kind tests use `"gallery"`,** which is a plausible future collection kind. Use `"not-a-kind"`. Places: `migrations.rs` ~:49, `collections_api.rs` ~:173, `collections_browse_api.rs` ~:334, and `parity.rs` ~:3830, ~:3855, ~:11945, ~:11983.
- [ ] **api — four removed-route 404 tests repeat one loop** (`catalog_api.rs` ~:1110-1157). Extract an `assert_routes_gone(&[(method, uri)])` helper.
- [ ] **api, pre-existing — duplication and dead code:**
  - `FileType::subtype_table` has no callers.
  - `parse_file_type` is duplicated in `alexandria-http/src/routes/browse.rs` and `alexandria-ffi/src/lib.rs`. Fold both into `FileType::from_wire`.
- [ ] **api, pre-existing — possible race.** The parity tests at `parity.rs` ~:727 and ~:1266 compare `metadataVersion` before the index run finishes. Wait for the run to finish, as the listing test already does.
- [ ] **ui — `DeleteFileButton`, `RecentFilesController` and the dashboard take `FileDetails` but read only `.file`.**
- [ ] **ui — leftover 0.4 values:** the fixture range in `core_status_mapper_test.dart` (~:195) and the `app_en.arb` placeholder example (~:129).
- [ ] **Over-long lines from reflowing:**
  - api: System Behavior Document ~:430, the Vision scope paragraph, `docs/initial/brainstorm.md`, the README intro
  - ui: System Requirements Document §1.2, `docs/initial/Project Overview.md` ~:8-10, `docs/initial/Technology Stack.md` ~:140-141, `library_menu.dart` ~:19-20, `core_version_test.dart` ~:33-35, `delete_record_test.dart` ~:254
  - docs: `content/en/_index.md` ~:35 and `content/en/docs/overview/_index.md` ~:62-63
- [ ] **ui — README milestone totals still count the Removed M-12 issues.**

## Known baseline (not follow-ups)

These are Windows-only and pre-existing:
- `migrations::given_a_path_with_url_metacharacters_when_migrated_then_that_exact_file_is_used` creates a directory named `what?`, which Windows rejects.
- clippy warns about an unused `use super::*` at `catalog/fs.rs` ~:314 under `cfg(windows)`.
- `flutter analyze` reports `analysis_options_deprecated_plugins` (Dart 3.13.5 and the `custom_lint` plugin block).
