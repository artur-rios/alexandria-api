# Libraries Are the Only Way In — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a registered library the only way a file enters the catalog, delete what lies outside libraries on upgrade, and bring audio back as a playable type.

**Architecture:** Registration validates the folder and starts its first index run through the existing `IndexHandler`; every run and every file carries its library; the free-form index call, the type scope and the type-panel exclusion are removed; removing a library deletes its records. Audio is restored from commit `8867aa8`'s parent, without the music-manager features. Both transports (HTTP, FFI) change at parity.

**Tech Stack:** Rust 1.99.1, tokio, axum, sqlx (SQLite), lofty (restored), ffmpeg-next, cbindgen.

**Spec:** `docs/superpowers/specs/2026-11-20-libraries-only-design.md`

## Global Constraints

- Workspace version `0.5.0` → `0.6.0` (`Cargo.toml`, `[workspace.package]`).
- Migrations are forward-only; never edit an existing `migrations/*.sql` (sqlx checksums them). New ones: `00000000000027_run_library.sql`, `00000000000028_libraries_only.sql`.
- HTTP↔FFI parity (FR-FC-24) for every new or changed call: same inputs, same refusals, same JSON.
- Nothing on disk is ever deleted or moved by this work — database rows only.
- After Task 4, no code path may write a `files` row without a library.
- Not restored: playlists, plays, enrichment, energy, recording identification, `[metadata]` config, `reqwest`.
- Test names `given_…_when_…_then_…` (the repository's convention). Write the failing test first.
- Commits: lowercase Conventional Commits, subject ≤ 50 chars, body wrapped at 72 saying why, ending with the `Co-Authored-By` / `Claude-Session` lines.
- One heavy build at a time; the machine is capped at 6 CPUs / 8 GB.
- ffmpeg's dev libraries are not on this machine: build and test inside the dev container (Task 1). The final gate is `ci-check lint-and-test`.
- Unfinished run = status `running` or `paused` (an interrupted run is `paused` after startup's `pause_running`).

## Review Focus

1. **A library at a path with a trailing slash or backslashes** (`/media/course/`, `D:\course`) — registration, overlap and the run's root must agree on one spelling; the stored `root_path` is the trimmed input, and every file the run writes is under it. Pinned in Task 2 (`given_a_root_with_a_trailing_separator_when_registered_then_its_run_indexes_the_folder`).
2. **Re-indexing a library whose files are already catalogued** — a second run must count them `already_cataloged`, not fail them on the `path` unique constraint and not write duplicates. Pinned in Task 2 (`given_an_indexed_library_when_reindexed_then_existing_files_count_as_already_cataloged`).
3. **Removing a library whose files are in a collection, a watchlist and a reading list** — the collection and lists survive, minus those items, and other libraries' items are untouched. Pinned in Task 5.
4. **Upgrading a database where a library's root is inside an older index run's root** — the outer run (outside every library) is deleted, its failures with it, while the library's files survive. Pinned in Task 4's migration test.
5. **An audio file with no tags at all** — it is still catalogued, named by its filename, with its duration, and the thumbnail endpoint answers "no thumbnail" rather than an error. Pinned in Task 7.

---

## Dev loop (used by every task)

`dt` below means:

```bash
docker exec -w /work alexandria-dev
```

so `dt cargo test …` runs inside the container. Test filters:

- core: `dt cargo test --locked -p alexandria-core --test integration <module>::`
- core unit: `dt cargo test --locked -p alexandria-core --lib <path>`
- http: `dt cargo test --locked -p alexandria-http --test api <file_stem>::`
- ffi: `dt cargo test --locked -p alexandria-ffi --test smoke` / `--test parity`

Each task ends with `dt cargo clippy --locked --workspace --all-targets -- -D warnings` and `dt cargo fmt --all -- --check` before its commit.

---

### Task 1: Dev container

**Files:** none in the repository.

- [ ] **Step 1: Start the container**

```bash
docker volume create alexandria-target && docker volume create alexandria-cargo
docker run -d --name alexandria-dev --cpus 4 --memory 6g \
  -v ~/repositories/alexandria-api:/work \
  -v alexandria-target:/target -v alexandria-cargo:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e CARGO_INCREMENTAL=0 \
  -e CARGO_PROFILE_DEV_DEBUG=line-tables-only -e CARGO_BUILD_JOBS=4 -e RUSTFLAGS="-D warnings" \
  rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c sleep infinity
docker exec alexandria-dev bash -c 'apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends libavformat-dev libavcodec-dev libavutil-dev libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev pkg-config clang && rustup component add clippy rustfmt'
```

- [ ] **Step 2: Baseline**

Run: `dt cargo test --locked --workspace 2>&1 | tail -5`
Expected: every suite `ok`. Record any pre-existing failure before changing code.

---

### Task 2: Registering a library starts its first run; per-library re-index; one run per library

Additive: the free-form index path still exists after this task.

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000027_run_library.sql`
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (run row gains library), `catalog/commands/index.rs` (`IndexRequest`, `LibraryRunStarter`), `catalog/commands/run_control.rs` (`RunResumed`), `libraries/commands.rs`, `libraries/model.rs`, `services.rs`
- Modify: `crates/alexandria-http/src/routes/libraries.rs`, `crates/alexandria-http/src/lib.rs` (route), `routes/runs.rs` (resume spawn)
- Modify: `crates/alexandria-ffi/src/lib.rs` (`alexandria_library_register`, new `alexandria_library_index_start`, resume spawn)
- Test: `crates/alexandria-core/tests/libraries/indexing.rs`, `tests/catalog/runs.rs`, `crates/alexandria-http/tests/api/libraries_api.rs`, `crates/alexandria-ffi/tests/smoke.rs`, `tests/parity.rs`

**Interfaces:**
- Produces:
  - Migration 27: `ALTER TABLE catalog_runs ADD COLUMN library_id INTEGER;` plus `CREATE INDEX idx_catalog_runs_library ON catalog_runs (library_id);`
  - `IndexRequest { root: String, priority: RunPriority, scope: IndexScope, library_uuid: Option<Uuid> }` (made required in Task 3)
  - `CatalogRunRepository::start(…, scope: Option<&str>, library_uuid: Option<Uuid>)`. The row stores `(SELECT id FROM libraries WHERE uuid = ?)`.
  - `CatalogRun.library_uuid: Option<Uuid>`, serialized `libraryUuid`, skipped when `None`. Read with a join on `libraries`.
  - `CatalogRunRepository::unfinished_for_library(&self, library: Uuid) -> Result<Option<CatalogRun>, DomainError>`: newest index run of that library with status `running` or `paused`.
  - `RunResumed.library_uuid: Option<Uuid>`
  - In `catalog/commands/index.rs`:
    ```rust
    #[allow(async_fn_in_trait)]
    pub trait LibraryRunStarter: Send + Sync {
        async fn start_library_run(&self, library: &Library, priority: RunPriority, token: &str)
            -> Result<IndexStarted, DomainError>;
    }
    ```
    implemented for `IndexHandler<…>` (builds an `IndexRequest` from `library.root_path` / `library.uuid`, every-type scope) and for `Arc<T: LibraryRunStarter>`.
  - In `libraries/model.rs`: `RegisteredLibrary { #[serde(flatten)] library: Library, #[serde(rename = "runId")] run_id: Uuid }`
  - `RegisterLibraryHandler<A, R, S: LibraryRunStarter>::new(auth, repo, starter, library_root: LibraryRoot)`. There is no filesystem generic: the existence check is `IndexHandler::start`'s (Step 3).
  - `register(&self, name: &str, root_path: &str, priority: RunPriority, token: &str) -> Result<RegisteredLibrary, DomainError>`
  - `IndexLibraryHandler<A, R, S, RR>::index(&self, uuid: Uuid, priority: RunPriority, token: &str) -> Result<(Library, IndexStarted), DomainError>`
  - `MoveLibraryHandler` gains `RR: CatalogRunRepository` and refuses when `unfinished_for_library` is `Some`.
  - Conflict copy, verbatim: `format!("the library has an unfinished run {run_id}; cancel it first")`
  - HTTP: `POST /v1/libraries` body gains `priority` (`deserialize_priority`) and answers `201` with `RegisteredLibrary`. New `POST /v1/libraries/{uuid}/index`, body `{ "priority"? }` (absent body = normal), answers `202 {"runId"}`.
  - FFI: `alexandria_library_register` reads `"priority"` from its JSON body (`parse_priority`) and answers `RegisteredLibrary` JSON. New:
    ```rust
    pub extern "C" fn alexandria_library_index_start(uuid: *const c_char, token: *const c_char, priority: *const c_char) -> IndexStartResult
    ```
    with new codes `INDEX_ERR_NOT_FOUND = 6`, `INDEX_ERR_CONFLICT = 7`.
  - Both transports spawn `index_handler.execute(&library.root_path, run_id, &IndexScope::all())` after a successful register or index, and resume spawns as today.

- [ ] **Step 1: Write the failing core tests** in `tests/libraries/indexing.rs`, against the real SQLite services the module already uses:
  - `given_a_folder_when_registered_then_a_run_indexes_its_readable_files_and_skips_the_rest`: the folder holds `a.md`, `b.pdf`, `c.zip`. After waiting for the returned `run_id` to be terminal: status `complete`, `indexed == 2`, `skipped == 1`, and both files have `library_uuid == Some(library.uuid)`.
  - `given_a_root_with_a_trailing_separator_when_registered_then_its_run_indexes_the_folder`: register `"<dir>/"`. The stored `root_path` has no trailing `/`, and the run indexes the files.
  - `given_a_folder_that_does_not_exist_when_registered_then_invalid_input_and_nothing_is_stored`: `Err(InvalidInput)`, `list_all()` is empty, `list_active()` is empty.
  - `given_a_folder_outside_filesystem_root_when_registered_then_refused_and_nothing_is_stored`
  - `given_an_overlapping_folder_when_registered_then_conflict_naming_the_library_and_no_run`
  - `given_an_indexed_library_when_reindexed_then_existing_files_count_as_already_cataloged`: second run `already_cataloged == 2`, `failed == 0`, file count unchanged.
  - `given_a_paused_run_when_the_library_is_reindexed_then_conflict_naming_the_run`: the message contains the run id.
  - `given_a_paused_run_when_the_library_is_moved_then_conflict_naming_the_run`
  - `given_a_library_run_when_its_status_is_read_then_it_carries_the_library_uuid`, and the same for `list_active`.
  - `given_a_refresh_run_when_its_status_is_read_then_it_has_no_library`

- [ ] **Step 2: Run them to see them fail**

Run: `dt cargo test --locked -p alexandria-core --test integration libraries::`
Expected: compile errors on `register`'s new arity / `run_id`.

- [ ] **Step 3: Implement**
  - Migration 27 and the run repository changes.
  - `LibraryRunStarter` for `IndexHandler`.
  - Register checks, in this order: blank name, blank root, `LibraryRoot::check_root`, overlap. Then insert, then `start_library_run`. If `start_library_run` returns `Err`, delete the library row and return the error. The existence check comes free: `IndexHandler::start` already refuses a missing root with `InvalidInput("root path does not exist")`. Because a failed start deletes the row, nothing is stored, and no `Filesystem` generic is needed.
  - Keep `claim_files` for now; it goes in Task 3.
  - Wire `services.rs`: `register_library_handler` holds `Arc<DefaultIndexHandler>`. Add a new `index_library_handler: Arc<DefaultIndexLibraryHandler>`.

- [ ] **Step 4: Run the core tests**

Run: `dt cargo test --locked -p alexandria-core --test integration libraries:: catalog::runs`
Expected: PASS.

- [ ] **Step 5: Transport tests (failing first), then the code**
  - `libraries_api.rs`:
    - `given_a_folder_when_posted_then_201_with_the_library_and_a_run_id_that_completes`
    - `given_a_library_when_index_is_posted_then_202_with_a_run_id`
    - `given_an_unknown_library_when_index_is_posted_then_404`
    - `given_a_paused_run_when_index_is_posted_then_409`
  - `smoke.rs`: the same four over `alexandria_library_register` / `alexandria_library_index_start` (`INDEX_ERR_NOT_FOUND`, `INDEX_ERR_CONFLICT`).
  - `parity.rs`: `given_the_same_folder_when_registered_on_both_surfaces_then_the_json_matches_except_uuids`, comparing key sets and `name` / `rootPath`.

Run: `dt cargo test --locked -p alexandria-http --test api libraries_api::` and `dt cargo test --locked -p alexandria-ffi`
Expected: PASS.

- [ ] **Step 6: Lint and commit**

```bash
git add -A crates && git commit -m "feat: start indexing when a library is added"
```

---

### Task 3: Remove free-form indexing and the type scope

**Files:**
- Delete: `crates/alexandria-core/src/catalog/index_scope.rs`, `crates/alexandria-http/src/routes/index.rs`
- Modify:
  - `catalog/mod.rs`, `catalog/commands/index.rs`, `catalog/commands/run_control.rs`, `catalog/runs.rs` (drop the `scope` field and parameter; the column stays and is written NULL)
  - `libraries/commands.rs`, `libraries/repos.rs` (remove `claim_files`)
  - `crates/alexandria-http/src/lib.rs`, `routes/mod.rs`, `routes/runs.rs`
  - `crates/alexandria-ffi/src/lib.rs` (remove `alexandria_index_start` and `parse_scope`)
- Test: every call site listed by `grep -rn "IndexRequest {\|/v1/index\"\|alexandria_index_start(\|IndexScope\|claim_files" crates`

**Interfaces:**
- Consumes: Task 2's `LibraryRunStarter`, `RegisteredLibrary`, `alexandria_library_register`.
- Produces:
  - `IndexRequest { library_uuid: Uuid, root: String, priority: RunPriority }`
  - `IndexHandler::execute(&self, root: &str, run_id: Uuid) -> Result<IndexOutcome, DomainError>`
  - `CatalogRunRepository::start(id, kind, root, started_at, concurrency, library_uuid: Option<Uuid>)`
  - Test helpers:
    - core `tests/common/mod.rs`: `pub async fn index_as_library(services: &Services, root: &Path, token: &str) -> (Library, Uuid /*run*/)`, which registers and waits for a terminal run.
    - http `tests/api/common/mod.rs`: `pub async fn index_folder(app: &Router, token: &str, root: &Path) -> (String /*libraryUuid*/, String /*runId*/)`, which posts `/v1/libraries` and waits with `wait_for_run_terminal`.
    - ffi `tests/smoke.rs` and `tests/parity.rs`: `fn index_folder(root: &Path, token: &CString) -> (String, String)` over `alexandria_library_register`.

- [ ] **Step 1: Write the failing tests**
  - http `catalog_api.rs`: `given_the_old_index_route_when_posted_then_it_is_not_found`. Assert status is `404` or `405`.
  - `crates/alexandria-ffi/tests/smoke.rs`: `given_the_header_when_read_then_it_declares_no_free_form_index_start`. Read `src/header.h` and assert it does not contain `alexandria_index_start(` but does contain `alexandria_library_index_start(`.

- [ ] **Step 2: Run them to see them fail**

Run: `dt cargo test --locked -p alexandria-http --test api catalog_api::given_the_old_index_route` → FAIL (202 or 400).

- [ ] **Step 3: Remove the code and move every test call site to the helpers above.** Tests that existed only to check the scope (`types`, `IndexScope`) are deleted. Tests that check `filesystem.root`, missing roots, unauthorized and not-initialized move to registration; their names change from `…_index_start_…` to `…_library_register_…`. The low-priority tests move to registration with `"priority":"low"`.

- [ ] **Step 4: Run the whole workspace**

Run: `dt cargo test --locked --workspace 2>&1 | grep -E "test result|FAILED|panicked" | tail -20`
Expected: all `ok`.

- [ ] **Step 5: Lint and commit**

```bash
git commit -am "refactor!: index only through a library"
```

The body must include `BREAKING CHANGE: POST /v1/index, alexandria_index_start and the types scope are removed.`

---

### Task 4: Every file belongs to a library; clean up on upgrade

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000028_libraries_only.sql` (`-- no-transaction`, with the `PRAGMA foreign_keys` / `BEGIN` / `COMMIT` frame copied from migration 25)
- Modify: `catalog/model.rs` (`NewFile`), `catalog/repos.rs` (`insert_file`), `catalog/commands/index.rs` (`index_entry`), `playback/test_support.rs`
- Test: `crates/alexandria-core/tests/migrations/mod.rs`, `tests/catalog/repos_integrity.rs`, and every `NewFile {` site: `browse_batching.rs`, `catalog/refresh.rs`, `refresh_after_edit.rs`, `libraries/persistence.rs`, ffi `smoke.rs`, http `libraries_api.rs`, `playback_stream_api.rs`

**Interfaces:**
- Consumes: `IndexRequest.library_uuid` (Task 3).
- Produces:
  - `NewFile.library_uuid: Uuid`
  - `insert_file` resolves `library_id = (SELECT id FROM libraries WHERE uuid = ?)`. When no library matches it returns `DomainError::InvalidInput("unknown library")`, checked before the insert inside the same transaction.
  - `IndexHandler::execute(&self, root: &str, run_id: Uuid, library_uuid: Uuid)`. The transports pass the registered or indexed library's uuid. A resume passes `RunResumed.library_uuid` and refuses to spawn an index run whose `library_uuid` is `None`, logging an error.
  - Core test helper: `pub async fn seed_library(pool: &SqlitePool, root: &str) -> Uuid`, which inserts a `libraries` row directly.

Migration 28, in order (the spec's §6):
1. Delete, for `files` rows with `library_id IS NULL`: `reading_progress` (`item_file_id`), `watch_progress` (`video_file_id`), each subtype table (`video_files`, `html_pages`, `text_files`, `documents`, `comic_books`, `images`), then those `files` rows.
2. Among index runs (`root IS NOT NULL`): delete `catalog_run_failures` and then `catalog_runs` rows whose root is neither a library's `root_path` nor under one. Use the separator-safe `substr` prefix test from `insert_file`. Set `library_id` on the remaining index runs with the same test, longest root first.
3. Delete `catalog_run_failures` rows whose `path` is not under any library root (failures on files that are now gone).
4. Rebuild `files` as migration 25 does, with `type CHECK (type IN ('video','html','text','document','comic','image','audio'))` and `library_id INTEGER NOT NULL`. Keep the `sqlite_sequence` carry-over and recreate the three indexes.
5. `CREATE TABLE IF NOT EXISTS audio_files (file_id INTEGER PRIMARY KEY, title TEXT, artist TEXT, album TEXT, year INTEGER, genre TEXT, track INTEGER, duration_seconds REAL, album_artist TEXT, FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE);`

Read the actual column names of `catalog_run_failures` and the progress tables from their migrations before writing the SQL.

- [ ] **Step 1: Write the failing migration test** in `tests/migrations/mod.rs`, following the module's existing pattern (migrate to 27, seed with raw SQL, run the rest):
  - `given_files_inside_and_outside_libraries_when_upgraded_then_only_library_files_remain`
    - Seed library L at `/lib`. Seed files `/lib/a.mp4` (in L) and `/other/b.mp4` (NULL), each with a `video_files` row.
    - Seed watch progress on both, reading progress on a `/other/c.pdf`, and a collection holding both videos.
    - Seed index runs: one with root `/` (status `paused`), with a failure on `/other/x.md`; one with root `/lib/sub` (status `complete`), with a failure on `/lib/sub/y.md`. Seed one refresh run (root NULL).
    - After migrating: only `/lib/a.mp4` remains, with its `video_files` row, watch progress and collection. The `/` run and its failure are gone. The `/lib/sub` run remains with `library_id` = L, with its failure. The refresh run remains with `library_id` NULL.
  - `given_the_upgraded_schema_when_a_file_without_a_library_is_inserted_then_it_is_refused`: a raw `INSERT` with NULL `library_id` fails.
  - `given_the_upgraded_schema_when_an_audio_file_is_inserted_then_it_is_accepted`: a raw insert of type `audio` plus an `audio_files` row both succeed.
  - `given_the_upgraded_schema_when_new_rows_are_inserted_then_ids_continue_after_the_old_high_water_mark`

- [ ] **Step 2: Run it to see it fail**

Run: `dt cargo test --locked -p alexandria-core --test integration migrations::`
Expected: FAIL (migration 28 missing).

- [ ] **Step 3: Write migration 28; change `NewFile` and `insert_file`; thread `library_uuid` through `execute` → `index_entry`; update every `NewFile {` site to use `seed_library`.**

- [ ] **Step 4: Add the repository test** in `repos_integrity.rs`: `given_an_unknown_library_when_a_file_is_inserted_then_invalid_input_and_no_row`

- [ ] **Step 5: Run the whole workspace**

Run: `dt cargo test --locked --workspace 2>&1 | grep -E "test result|FAILED|panicked" | tail -20`
Expected: all `ok`.

- [ ] **Step 6: Lint and commit**

```bash
git commit -am "feat!: keep every file inside a library"
```

The body explains the upgrade deletes records outside libraries, and ends with a `BREAKING CHANGE:` line.

---

### Task 5: Removing a library removes its records

**Files:**
- Modify: `libraries/repos.rs` (`delete`), `libraries/commands.rs` (`RemoveLibraryHandler` gains `RR: CatalogRunRepository`), `services.rs`
- Modify: `crates/alexandria-http/src/routes/libraries.rs` (doc comment: `409` while a run is running), `crates/alexandria-ffi/src/lib.rs` (`alexandria_library_remove` maps `Conflict` → `LIBRARY_ERR_CONFLICT`, as `map_library_err` already does)
- Test: `tests/libraries/persistence.rs`, `crates/alexandria-http/tests/api/libraries_api.rs`, ffi `smoke.rs`

**Interfaces:**
- Produces:
  - `LibraryRepository::delete(&self, uuid: Uuid) -> Result<u32 /*files removed*/, DomainError>`. One write transaction:
    1. progress rows and subtype rows (now including `audio_files`) for the library's files;
    2. the library's files;
    3. `catalog_run_failures` for the library's runs, and those whose `path` is under its root;
    4. the library's `catalog_runs`;
    5. the `libraries` row.
  - `RemoveLibraryHandler::remove`: `NotFound` for an unknown uuid. When `unfinished_for_library` returns a `Running` run, `Conflict` with Task 2's copy. A `Paused` run does not block removal.

- [ ] **Step 1: Write the failing tests** (core, with real SQLite and real temp files):
  - `given_a_library_in_a_collection_and_lists_when_removed_then_its_items_go_and_the_groupings_stay`
    - Two libraries, A and B, each with a video and a PDF.
    - One collection holds A's video and B's video. A watchlist holds both videos, with progress. A reading list holds both PDFs, with progress.
    - Remove A. The collection, watchlist and reading list still exist and list only B's items. A's files are absent from `GET`-by-uuid (`NotFound`). B's files are untouched.
  - `given_a_library_when_removed_then_every_file_stays_on_disk`
  - `given_a_running_run_when_the_library_is_removed_then_conflict_and_nothing_is_deleted`. Hold the run `running` the way `tests/catalog/run_control.rs` does.
  - `given_a_paused_run_when_the_library_is_removed_then_the_run_is_gone`: `runs.get(run_id)` is `None`.
  - `given_a_refresh_run_when_a_library_is_removed_then_the_refresh_run_remains`
  - Transports: `given_a_library_when_deleted_then_204_and_its_files_are_gone` (HTTP and FFI).

- [ ] **Step 2: Run the tests to see them fail**

Run: `dt cargo test --locked -p alexandria-core --test integration libraries::persistence`
Expected: FAIL (files still present).

- [ ] **Step 3: Implement.** Delete the old "files return to the type panels" tests, or rewrite them to the new behaviour.

- [ ] **Step 4: Run the tests**

Run: `dt cargo test --locked -p alexandria-core --test integration libraries::` and `dt cargo test --locked -p alexandria-http --test api libraries_api::` and `dt cargo test --locked -p alexandria-ffi`
Expected: PASS.

- [ ] **Step 5: Lint and commit**

```bash
git commit -am "feat!: delete a library's records when removed"
```

---

### Task 6: The listing loses the exclusion and gains a library filter

**Files:**
- Modify:
  - core: `catalog/queries/browse.rs` (`FileFilter`), `catalog/repos.rs` (`list_filter_where_clause`; remove `LibraryScope`)
  - http: `crates/alexandria-http/src/routes/browse.rs`
  - ffi: `crates/alexandria-ffi/src/lib.rs` (`alexandria_files_list`'s JSON filter parse)
- Test: `tests/catalog/browse.rs`, http `catalog_api.rs`, ffi `parity.rs`

**Interfaces:**
- Produces:
  - `FileFilter::with_library(self, uuid: Uuid) -> Self`; `everywhere()` and `LibraryScope` are removed.
  - SQL: `library_id = (SELECT id FROM libraries WHERE uuid = ?)`, bound after the collection placeholder.
  - HTTP query `libraryUuid` and FFI JSON key `"libraryUuid"`. Malformed → `400` / `FILE_ERR_INVALID_INPUT` with `invalid libraryUuid: <value>`. Well-formed but unknown → `[]`.

- [ ] **Step 1: Write the failing tests**
  - core:
    - `given_two_libraries_when_listed_with_one_library_then_only_its_files`
    - `given_library_files_when_listed_without_a_filter_then_all_are_answered`
    - `given_an_unknown_library_uuid_when_listed_then_empty`
  - http and ffi:
    - `given_a_malformed_library_uuid_when_listed_then_invalid_input`
    - parity `given_a_library_filter_when_listed_on_both_surfaces_then_the_json_matches`

- [ ] **Step 2: Run the tests to see them fail**

Run: `dt cargo test --locked -p alexandria-core --test integration catalog::browse`
Expected: compile error (`with_library`).

- [ ] **Step 3: Implement.** Delete the `includeLibraries` tests and the parsing for it on both surfaces (`parse_include_libraries`, FFI's `"includeLibraries"`).

- [ ] **Step 4: Run the tests**

Run the three suites.
Expected: PASS.

- [ ] **Step 5: Lint and commit**

```bash
git commit -am "feat!: filter the file listing by library"
```

---

### Task 7: Restore audio

Reference for every step: `git show 8867aa8` (what was removed) and `git show 8867aa8^:<path>` (the file before). Re-apply by hand against today's code. Do not use `git revert`: later commits changed the same files.

**Files:**
- Create: `crates/alexandria-core/src/catalog/audio_tags.rs` (from `8867aa8^`)
- Modify:
  - `Cargo.toml` and `crates/alexandria-core/Cargo.toml` (`lofty`, the version from `git show 8867aa8 -- Cargo.toml`; then `cargo update -p lofty --precise <that version>` so `Cargo.lock` is written once, inside the container)
  - core: `catalog/mod.rs`, `catalog/classify.rs`, `catalog/model.rs` (`FileType::Audio`, `SubtypeMetadata::Audio`), `catalog/extraction.rs`, `catalog/repos.rs` (the audio arms listed in `8867aa8`'s message), `catalog/commands/index.rs` and `refresh.rs` (the audio reader generic), `playback/mime.rs`, `playback/thumbnail.rs` (cover-art arm), `playback/test_support.rs`, `services.rs`
  - `crates/alexandria-http/Cargo.toml` (lofty dev-dependency, if the restored HTTP fixtures need it)
- Test: restore the audio cases removed by `8867aa8` in `tests/catalog/{index,browse,edit_metadata,refresh}.rs`, `tests/common/mod.rs`, `tests/playback/`, http `catalog_api.rs` / `playback_*`, and ffi `parity.rs` / `smoke.rs`, where they still apply. Index them through a library (Task 3's helpers).

**Interfaces:**
- Produces: `FileType::Audio` with wire name `"audio"`; the `SubtypeMetadata::Audio { title, artist, album, album_artist, year, genre, track }` shape as at `8867aa8^`; `FileView.duration_seconds` filled for audio.

- [ ] **Step 1: Restore the tests first.** Replace `classify.rs`'s `given_an_audio_extension_when_classified_then_it_is_unrecognised` with `given_an_audio_extension_when_classified_then_audio`, covering all nine extensions. Add:
  - `given_an_audio_file_with_no_tags_when_indexed_then_it_is_catalogued_by_name_with_its_duration`
  - `given_an_audio_file_with_no_cover_when_its_thumbnail_is_asked_then_no_thumbnail` (HTTP, matching what the endpoint answers today for a type with no thumbnail)
  - `given_an_audio_file_when_streamed_with_a_range_then_206_and_an_audio_mime_type`, for `.mp3` → `audio/mpeg` and `.flac` → `audio/flac` (the values at `8867aa8^:crates/alexandria-core/src/playback/mime.rs`)
  - `given_audio_as_a_type_filter_when_listed_then_only_audio`, on both surfaces

- [ ] **Step 2: Run the tests to see them fail**

Run: `dt cargo test --locked -p alexandria-core --lib catalog::classify`
Expected: FAIL.

- [ ] **Step 3: Restore the code.** Check that nothing named `playlist`, `play_event`, `enrichment`, `energy`, `lyrics`, `musicbrainz` or `reqwest` came back:

Run: `grep -rniE "playlist|play_event|enrichment|energy|lyrics|musicbrainz|reqwest" crates --include=*.rs --include=Cargo.toml`
Expected: no match outside comments that predate this work.

- [ ] **Step 4: Run the whole workspace**

Run: `dt cargo test --locked --workspace 2>&1 | grep -E "test result|FAILED|panicked" | tail -20`
Expected: all `ok`.

- [ ] **Step 5: Lint and commit**

```bash
git commit -am "feat: catalogue and play audio files again"
```

---

### Task 8: Documentation, CHANGELOG, version

**Files:**
- Modify: `Cargo.toml` (`version = "0.6.0"`); `Cargo.lock` follows from `dt cargo build --locked` (it fails if stale, in which case `dt cargo update -w`)
- Modify: `README.md`, `CHANGELOG.md`, `config.toml.example`, `CONTRIBUTING.md` (only where it mentions `POST /v1/index` or type panels), `docs/System Behavior Document.md`, `docs/initial/*.md`, `docs/requirements/*.md`

What each must say comes from the spec's "Documentation" section. Specifically:
- **System Requirements:**
  - FR-FC-01 and -02 are indexing a library, with no scope.
  - FR-FC-12 is listing filters, including `libraryUuid`.
  - FR-FC-26: registration is bounded too.
  - FR-FC-36 now includes "starts its first run" and refuses a missing folder.
  - FR-FC-37 is rewritten: every file belongs to exactly one library.
  - FR-FC-38 is rewritten: no exclusion; library filter.
  - FR-FC-40 is rewritten: removal deletes records, never disk files, and is refused while a run is running.
  - FR-FC-41 adds the refusal while a run is unfinished.
  - New FR-FC-42: one unfinished run per library.
  - The §5 route table loses `POST /v1/index` and gains `POST /v1/libraries/{uuid}/index`.
  - Audio returns to the type lists.
- **Use Case Specification:**
  - UC-01 becomes "Index a library".
  - UC-51 becomes "Add a library", with the flows for registration, re-index, move and remove, and the alternate flows for each refusal.
  - UC-03 is browsing through libraries and search.
  - Traceability table updated.
  - UC-49 and UC-50 stay removed.
- **README:** introduction (library manager for things you read, watch and listen to; music *management* stays in Orpheus); project status, with F-12 described as the entry point; audio in the type list.
- **CHANGELOG** `[Unreleased]`: Changed / Added / Removed as in the spec, plus an **Upgrade** note: "Starting 0.6.0 deletes every catalog record outside a library, with its progress and collection membership; files on disk are untouched. Back up `alexandria.db` first to keep them." The Removed entry for audio is reworded so it no longer contradicts.

- [ ] **Step 1: Find every stale statement**

Run: `grep -rniE "type panel|includeLibraries|/v1/index\"|POST /v1/index[^/]|alexandria_index_start|index scope|\btypes\b.*scope|no longer catalogued|reading and watching|read and watch" README.md CHANGELOG.md CONTRIBUTING.md config.toml.example docs --include=*.md --include=*.example | grep -v "docs/superpowers/"`
Expected afterwards: only historical mentions inside the CHANGELOG's own Removed entries.

- [ ] **Step 2: Edit the documents**

- [ ] **Step 3: Check the version**

Run: `dt cargo build --locked`
Expected: success.

- [ ] **Step 4: Commit**

```bash
git commit -am "docs: describe alexandria as a library manager"
```

---

### Task 9: Final verification and pull request

- [ ] **Step 1: Run CI's own check**

Run: `ci-check lint-and-test`
Expected: exit 0, fmt/clippy/test all green.

- [ ] **Step 2: Clean up the dev container**

```bash
docker rm -f alexandria-dev && docker volume rm alexandria-target alexandria-cargo
```

- [ ] **Step 3: Push and open the pull request into `develop`**

```bash
git push -u origin feature/libraries-only
tea pr create --base develop --head feature/libraries-only --title "feat!: make libraries the only way in"
```

The description covers what changed and why (the spec), how it was tested (the `ci-check` run), and what the owner must do: back up `alexandria.db` before the first 0.6.0 start, and the alexandria-ui and alexandria-docs follow-ups. It ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.

- [ ] **Step 4: Watch Jenkins**

Run: `tea api repos/artur-rios/alexandria-api/commits/<sha>/status`
Expected: `success`. Do not merge: the owner approves merges.
