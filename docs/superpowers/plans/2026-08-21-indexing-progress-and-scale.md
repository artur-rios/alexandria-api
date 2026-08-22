# Indexing Progress, Run Control, and Scale — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make indexing a 418 GB / 12,264-file library take seconds instead of tens of minutes, report live progress while it runs, and be pausable and resumable across application restarts.

**Architecture:** Change detection moves from full-file SHA-256 to `(size_bytes, mtime)` captured during the directory walk, which makes the per-file cost independent of file size. `content_hash` becomes nullable and is computed only on demand. A process-local `RunRegistry` holds per-run atomics for progress and a control signal; a flusher persists them into `catalog_runs` periodically so a paused run survives a restart. Pause, cancel, and resume are three verbs over that one signal.

**Tech Stack:** Rust 2021, tokio, sqlx (SQLite), axum (HTTP), `#[no_mangle] extern "C"` FFI, `walkdir`, `sha2`, `futures_util::stream`.

**Spec:** `docs/superpowers/specs/2026-08-21-indexing-progress-and-scale-design.md`

## Global Constraints

- Baseline migrations are **amended in place**, not stacked. `sqlx::migrate!` checksums migration files, so every existing database — including `.dev/catalog.db` in the `alexandria-ui` checkout — must be **deleted**, not migrated. State this in every commit message that touches a migration.
- FFI and HTTP stay at parity (FR-FC-24). Every FFI call added here gets its HTTP twin in Task 12, and the JSON bodies must be byte-for-byte identical between the two surfaces.
- Unit tests use trait fakes — no real database, filesystem, or auth service (Testing Specification §6.2). Integration tests that need real collaborators live in `crates/alexandria-core/tests/`.
- Test names follow the existing convention: `given_<condition>_when_<action>_then_<outcome>`.
- Per-file failures are counted in `failed`, logged at `warn`, and the walk continues. Only a failure to list the root at all fails the run. This is existing behaviour and no task may change it.
- `DomainError::InvalidState` already exists (`crates/alexandria-core/src/errors.rs:47`). Do not add a new variant for illegal run transitions.
- Free FFI status code: `RUN_ERR_*` currently uses 1, 2, 3, 4, 9. `RUN_ERR_INVALID_STATE` is **5**.
- Run `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before every commit.
- **Test fixtures.** `crates/alexandria-core/tests/common/mod.rs` provides `FakeAuth`, `FakeCatalogRepository`, `FakeCatalogRunRepository`, `FakeFilesystem` (via `FakeFilesystem::builder().with_root(..).with_file(root, path, name, hash)`), the five `Fake*MetadataReader`s, `fixed_clock()`, and `now()`. `tests/catalog/index.rs` has a local `handler(..)` function that wires all nine, and the constants `ROOT = "/library"`, `TOKEN = "bearer-token"`, `TEST_CONCURRENCY = 4`. **No unit test in `tests/catalog/` touches a real filesystem or database.** The snippets below show the new fixture calls and the assertions; copy the arrange block verbatim from the nearest existing test in the same file rather than inventing a second harness.

---

## File Structure

**Modified — core:**

| File | Responsibility after this plan |
| --- | --- |
| `crates/alexandria-core/migrations/00000000000001_catalog.sql` | `files` gains `size_bytes`, `mtime`; `content_hash` becomes nullable |
| `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql` | `catalog_runs` gains progress, pause, priority columns |
| `crates/alexandria-core/src/catalog/model.rs` | `File`/`NewFile` carry stat fields; `content_hash` optional |
| `crates/alexandria-core/src/catalog/repos.rs` | Persist and read stat fields; `ensure_content_hash` |
| `crates/alexandria-core/src/catalog/fs.rs` | `FileEntry` carries size and mtime from the walk |
| `crates/alexandria-core/src/catalog/runs.rs` | Statuses, counters, progress persistence, `pause_running`, `list_active` |
| `crates/alexandria-core/src/catalog/commands/index.rs` | No byte reads; honours the signal; publishes progress |
| `crates/alexandria-core/src/catalog/commands/refresh.rs` | Stat comparison; honours the signal; publishes progress |
| `crates/alexandria-core/src/catalog/queries/run_status.rs` | Overlays the live registry cell onto the persisted row |
| `crates/alexandria-core/src/playback/thumbnail.rs` | Cache key becomes `uuid-mtime-maxdim` |
| `crates/alexandria-core/src/catalog/commands/edit_content.rs` | Uses `ensure_content_hash` |
| `crates/alexandria-core/src/config.rs` | `indexing.low_priority_concurrency` |
| `crates/alexandria-core/src/services.rs` | Wires the registry and the three control handlers |

**Created — core:**

| File | Responsibility |
| --- | --- |
| `crates/alexandria-core/src/catalog/run_registry.rs` | Per-run process state: progress atomics, phase, control signal |
| `crates/alexandria-core/src/catalog/commands/run_control.rs` | Pause, resume, cancel; enforces the state machine |
| `crates/alexandria-core/src/catalog/queries/active_runs.rs` | Every non-terminal run |

**Modified — surfaces:**

- `crates/alexandria-ffi/src/lib.rs`, `native/include/alexandria_ffi.h` (regenerated)
- `crates/alexandria-http/src/routes/runs.rs`, `index.rs`, `refresh.rs`, `mod.rs`, `crates/alexandria-http/src/lib.rs`

**Modified — tests and docs:**

- `crates/alexandria-core/tests/catalog/{index,refresh,runs,run_status}.rs`, `tests/throughput.rs`
- `crates/alexandria-ffi/tests/parity.rs`, `crates/alexandria-http/tests/catalog_api.rs`
- `docs/requirements/{System Requirements,Use Case Specification}.md`

---

### Task 1: Capture size and mtime during the walk

Adds the two columns and threads them from `walkdir` into the catalog. `content_hash` is untouched — still `NOT NULL`, still computed. This task alone changes no behaviour; it makes Tasks 2–4 possible.

**Files:**
- Modify: `crates/alexandria-core/migrations/00000000000001_catalog.sql:1-14`
- Modify: `crates/alexandria-core/src/catalog/fs.rs:33-38` (`FileEntry`), `:133-158` (`collect`)
- Modify: `crates/alexandria-core/src/catalog/model.rs:64-71` (`NewFile`), `:295-311` (`File`)
- Modify: `crates/alexandria-core/src/catalog/repos.rs` (the `SELECT` column lists, `insert_file`, the row mapper)
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs:365-378` (`index_entry`)
- Test: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `FileEntry { path: String, name: String, size_bytes: i64, modified_at: Option<DateTime<Utc>> }`; `NewFile` and `File` both gain `size_bytes: Option<i64>` and `mtime: Option<DateTime<Utc>>`.

- [ ] **Step 1: Write the failing test**

The builder needs a way to seed the two new fields. Add to `FakeFilesystemBuilder` in `tests/common/mod.rs`:

```rust
    /// Seed the stat a `FileEntry` carries out of the walk. Without this an
    /// entry reports zero bytes and no modification time — what a filesystem
    /// that could not answer would give.
    pub fn with_stat(
        mut self,
        path: &str,
        size_bytes: i64,
        modified_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.fs
            .stat_by_path
            .insert(path.to_string(), (size_bytes, modified_at));
        self
    }
```

and have `FakeFilesystem::list_files` read `stat_by_path` when it builds each `FileEntry`. Then add to `crates/alexandria-core/tests/catalog/index.rs`:

```rust
#[tokio::test]
async fn given_a_file_on_disk_when_indexed_then_its_size_and_mtime_are_recorded() {
    // Arrange: copy the nine-fake `handler(..)` wiring from the nearest test
    // above, substituting this filesystem.
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .with_stat("/library/song.txt", 10, Some(now()))
        .build();

    let IndexStarted { run_id } = handler
        .start(IndexRequest { root: ROOT.into() }, TOKEN)
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let file = repo
        .find_by_path("/library/song.txt")
        .await
        .unwrap()
        .expect("file should be cataloged");

    assert_eq!(file.size_bytes, Some(10));
    assert_eq!(file.mtime, Some(now()), "mtime is captured from the walk");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p alexandria-core --test catalog given_a_file_on_disk_when_indexed_then_its_size_and_mtime_are_recorded`
Expected: FAIL to compile — `no field 'size_bytes' on type 'File'`.

- [ ] **Step 3: Add the columns to the baseline migration**

In `crates/alexandria-core/migrations/00000000000001_catalog.sql`, inside `CREATE TABLE IF NOT EXISTS files`, after the `content_hash` line:

```sql
    size_bytes    INTEGER,
    mtime         TEXT,
```

- [ ] **Step 4: Widen `FileEntry` and capture the metadata in the walk**

In `crates/alexandria-core/src/catalog/fs.rs`, replace the `FileEntry` struct:

```rust
/// A discovered file ready to be classified and recorded.
///
/// `size_bytes` and `modified_at` come from the directory entry's own
/// metadata, which `walkdir` has already fetched during the walk — reading
/// them here costs nothing and is what lets the indexer decide whether a file
/// changed without opening it.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
    pub size_bytes: i64,
    /// `None` when the platform or filesystem could not report a modification
    /// time. Change detection falls back to size alone for such a file.
    pub modified_at: Option<DateTime<Utc>>,
}
```

Add `use chrono::{DateTime, Utc};` to the top of the file. In `StdFilesystem::collect`, replace the `entries.push(...)` call:

```rust
            let metadata = entry.metadata().ok();
            let size_bytes = metadata.as_ref().map(|m| m.len() as i64).unwrap_or(0);
            let modified_at = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(DateTime::<Utc>::from);
            entries.push(FileEntry {
                path: path.to_string_lossy().into_owned(),
                name,
                size_bytes,
                modified_at,
            });
```

- [ ] **Step 5: Add the fields to the models**

In `crates/alexandria-core/src/catalog/model.rs`, add to both `NewFile` and `File`, after `content_hash`:

```rust
    /// The file's size in bytes at index time. With `mtime`, this is what
    /// re-index compares to decide whether a file changed (FR-FC-10).
    pub size_bytes: Option<i64>,
    /// The file's on-disk modification time at index time.
    pub mtime: Option<DateTime<Utc>>,
```

- [ ] **Step 6: Persist and read them**

In `crates/alexandria-core/src/catalog/repos.rs`, add `size_bytes, mtime` to every `SELECT` column list that names `content_hash` (there are five, at roughly lines 296, 307, 359, 532, and the two projections near 1184 and 1219 — grep for `content_hash` and update each). Add the two columns and their `?` placeholders to the `INSERT INTO files (...)` at line 321, bind them with `.bind(new_file.size_bytes)` and `.bind(new_file.mtime.map(|t| t.to_rfc3339()))`, and read them back in the row mapper:

```rust
        size_bytes: row.try_get::<Option<i64>, _>("size_bytes")?,
        mtime: row
            .try_get::<Option<String>, _>("mtime")?
            .map(|raw| parse_time(&raw, "mtime"))
            .transpose()?,
```

If `repos.rs` has no `parse_time` helper of its own, copy the one from `runs.rs:176-181` rather than importing it — the two modules do not currently depend on each other, and a shared time-parsing helper is a refactor this task does not need.

- [ ] **Step 7: Populate them at index time**

In `crates/alexandria-core/src/catalog/commands/index.rs`, in `index_entry`, add to the `NewFile` literal:

```rust
            size_bytes: Some(entry.size_bytes),
            mtime: entry.modified_at,
```

- [ ] **Step 8: Fix every remaining construction site**

Run `cargo build --workspace --all-targets`. Every `NewFile { .. }` and `File { .. }` literal in test support and fixtures now needs the two fields; add `size_bytes: None, mtime: None` to each. `crates/alexandria-core/src/playback/test_support.rs:228` is one of them.

- [ ] **Step 9: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including the new test.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat(catalog): record file size and mtime at index time

Captured from the directory entry's own metadata during the walk, where
walkdir has already fetched it. Nothing reads them yet; they are what
lets re-index decide a file changed without opening it.

Amends the baseline migration rather than stacking a new one: existing
databases must be deleted, not migrated."
```

---

### Task 2: Re-key the thumbnail cache off uuid and mtime

Must land before `content_hash` goes nullable. Otherwise the first thumbnail of every video forces a full-file SHA-256 — moving the 418 GB out of indexing and into browsing.

**Files:**
- Modify: `crates/alexandria-core/src/playback/thumbnail.rs:85-87`
- Test: `crates/alexandria-core/tests/playback.rs`

**Interfaces:**
- Consumes: `File.mtime` from Task 1.
- Produces: cache keys of the form `{uuid}-{mtime_rfc3339_or_"none"}-{THUMBNAIL_MAX_DIM}`.

- [ ] **Step 1: Write the failing test**

Add to `crates/alexandria-core/tests/playback.rs`:

```rust
#[tokio::test]
async fn given_a_file_whose_mtime_changed_when_a_thumbnail_is_requested_then_the_cache_is_not_reused() {
    let harness = ThumbnailHarness::new().await;
    let uuid = harness.an_image_file(Some(t(1))).await;

    let first = harness.thumbnail(uuid).await.unwrap();
    assert_eq!(harness.cache.hits(), 0, "first request cannot be a hit");

    harness.set_mtime(uuid, Some(t(2))).await;
    let second = harness.thumbnail(uuid).await.unwrap();

    assert_eq!(
        harness.cache.hits(),
        0,
        "a changed mtime must produce a different cache key"
    );
    assert_eq!(first.uuid, second.uuid);
}
```

Follow the harness and fake-cache helpers already in that file; `a_file` in `playback/test_support.rs:209` is the canned `File` those tests build on, and it will need an `mtime` parameter.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p alexandria-core --test playback given_a_file_whose_mtime_changed`
Expected: FAIL — the second request is a cache hit, because the key still derives from an unchanged `content_hash`.

- [ ] **Step 3: Change the key**

In `crates/alexandria-core/src/playback/thumbnail.rs`, replace lines 85-87:

```rust
        // Keyed on uuid and mtime rather than on the content hash. The hash is
        // computed on demand now (FR-FC-09), so keying on it would make the
        // first thumbnail of a multi-gigabyte video pay for hashing the whole
        // file — moving indexing's old cost into browsing, one unpredictable
        // stall at a time. The uuid is already unique and stable, and mtime
        // gives back the invalidation-on-change the hash was providing.
        let mtime = file
            .mtime
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "none".to_string());
        let key = format!("{}-{}-{}", file.uuid, mtime, THUMBNAIL_MAX_DIM);
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p alexandria-core --test playback`
Expected: PASS. The test at `thumbnail.rs:493` asserting the old `"abc"`-derived key needs updating to the new shape.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(playback): key thumbnails on uuid and mtime

The content hash is about to become lazily computed. A cache keyed on it
would make the first thumbnail of every video hash the whole file, which
is the indexing cost this work exists to remove, relocated somewhere the
owner cannot predict it. uuid is stable and unique; mtime restores the
invalidation-on-change the hash was providing."
```

---

### Task 3: Make `content_hash` nullable and computed on demand

This is where the speedup lands. After this task an index run reads no file bytes at all.

**Files:**
- Modify: `crates/alexandria-core/migrations/00000000000001_catalog.sql:7`
- Modify: `crates/alexandria-core/src/catalog/model.rs` (`File`, `NewFile`)
- Modify: `crates/alexandria-core/src/catalog/repos.rs` (trait + Sqlite impl)
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs:365-378`
- Modify: `crates/alexandria-core/src/catalog/commands/edit_content.rs:90-96`
- Test: `crates/alexandria-core/tests/catalog/index.rs`, `tests/catalog/edit_content.rs`

**Interfaces:**
- Consumes: Task 1's stat fields.
- Produces: `File.content_hash: Option<String>`; `NewFile.content_hash: Option<String>`; new trait method `CatalogRepository::ensure_content_hash(&self, uuid: Uuid) -> Result<String, DomainError>`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/alexandria-core/tests/catalog/index.rs`:

```rust
#[tokio::test]
async fn given_a_file_when_indexed_then_no_content_hash_is_computed() {
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .build();

    let IndexStarted { run_id } = handler
        .start(IndexRequest { root: ROOT.into() }, TOKEN)
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let file = repo.find_by_path("/library/song.txt").await.unwrap().unwrap();

    assert_eq!(
        file.content_hash, None,
        "indexing must not read file bytes; the hash is computed on demand"
    );
    assert_eq!(fs.hash_calls(), 0, "the filesystem's hash port is never reached");
}
```

`FakeFilesystem` needs a `hash_calls()` counter for that last assertion — an `AtomicUsize` on `FakeFsState`, bumped by `content_hash`. Task 4 asserts on it too.

Add to `crates/alexandria-core/tests/catalog/edit_content.rs`:

```rust
#[tokio::test]
async fn given_a_file_with_no_stored_hash_when_content_is_edited_then_the_hash_is_computed_first() {
    let harness = EditContentHarness::new().await;
    let uuid = harness.a_text_file_with_no_hash("hello").await;

    harness.edit(uuid, "goodbye").await.expect("edit should succeed");

    let file = harness.repo.find_by_uuid(uuid).await.unwrap().unwrap();
    assert_eq!(file.content_hash, Some(sha256_hex(b"goodbye")));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog content_hash`
Expected: FAIL to compile — `expected 'String', found 'Option<String>'`.

- [ ] **Step 3: Make the column nullable**

In `crates/alexandria-core/migrations/00000000000001_catalog.sql`, line 7:

```sql
    content_hash  TEXT,
```

- [ ] **Step 4: Make the model fields optional**

In `crates/alexandria-core/src/catalog/model.rs`, on both `File` and `NewFile`:

```rust
    /// SHA-256 of the file's bytes, lowercase hex. `None` means "not computed
    /// yet": indexing never reads file bytes (FR-FC-09), so the hash is filled
    /// in by `ensure_content_hash` the first time something genuinely needs
    /// it — which, after re-index stopped hashing, is only UC-33's
    /// optimistic-concurrency check on a text edit.
    pub content_hash: Option<String>,
```

- [ ] **Step 5: Add `ensure_content_hash` to the repository port**

In `crates/alexandria-core/src/catalog/repos.rs`, add to the `CatalogRepository` trait:

```rust
    /// The file's content hash, computing and storing it if it is not yet
    /// known. Returns `NotFound` when no file row carries the UUID.
    ///
    /// Hashing is deliberately not part of indexing (FR-FC-09), so this is
    /// where the cost lands: on the one caller that needs the value, for the
    /// one file it needs it for, while a person is already waiting on that
    /// file. Callers that only need to know whether a file *changed* must use
    /// `size_bytes` and `mtime` instead and must not call this.
    async fn ensure_content_hash(&self, uuid: Uuid) -> Result<String, DomainError>;
```

Implement it on `SqliteCatalogRepository`. It takes a `Filesystem` to hash with, which the repository does not currently hold — rather than widening the repository's dependencies, give it the hash:

```rust
    async fn ensure_content_hash(&self, uuid: Uuid) -> Result<String, DomainError> {
        let file = self.find_by_uuid(uuid).await?.ok_or(DomainError::NotFound)?;
        if let Some(hash) = file.content_hash {
            return Ok(hash);
        }
        let hash = crate::catalog::fs::StdFilesystem.content_hash(&file.path).await?;
        sqlx::query("UPDATE files SET content_hash = ? WHERE uuid = ?")
            .bind(&hash)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await?;
        Ok(hash)
    }
```

`StdFilesystem` is a unit struct (`#[derive(Debug, Default, Clone, Copy)] pub struct StdFilesystem;`), so naming it directly costs nothing and keeps the trait's signature free of a filesystem parameter every other method would ignore. Bring `use crate::catalog::fs::Filesystem;` into scope for the `content_hash` call.

Add the same method to whatever in-memory fake the unit tests use, backed by a `HashMap<Uuid, String>` the test seeds.

- [ ] **Step 6: Stop hashing at index time**

In `crates/alexandria-core/src/catalog/commands/index.rs`, in `index_entry`, delete the `let content_hash = self.fs.content_hash(&entry.path).await?;` line and set the field:

```rust
            // Not computed here, and deliberately: reading every byte of every
            // file is what made a 418 GB library take tens of minutes. Size and
            // mtime are the change signal now (FR-FC-10), and the hash is
            // filled in on demand by `ensure_content_hash`.
            content_hash: None,
```

- [ ] **Step 7: Make the text editor compute it**

In `crates/alexandria-core/src/catalog/commands/edit_content.rs`, replace the pre-write hash comparison at lines 90-96 so the *stored* side comes from `self.repo.ensure_content_hash(file.uuid).await?` rather than from `file.content_hash`. The post-write verification still calls `self.fs.content_hash(&file.path)` directly — it is checking bytes that were just written, and there is nothing stored to reuse.

- [ ] **Step 8: Fix every remaining call site**

Run `cargo build --workspace --all-targets` and follow the errors. Construction sites take `content_hash: None` (or `Some("abc".to_string())` where a test asserts on a known hash).

- [ ] **Step 9: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "perf(catalog): stop hashing file bytes during indexing

index_entry hashed every byte of every file, which made the cost of a
scan scale with the library's size rather than its file count: a 418 GB
library meant 418 GB of reads regardless of the 12,264 files in it.

content_hash is now nullable and computed by ensure_content_hash on the
one path that still needs it — UC-33's concurrency check on a text edit.

Amends the baseline migration: existing databases must be deleted."
```

---

### Task 4: Re-index compares stats instead of hashing

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/refresh.rs:225-245`
- Modify: `crates/alexandria-core/src/catalog/repos.rs` (`refresh_hash` → `refresh_stat`)
- Modify: `crates/alexandria-core/src/catalog/fs.rs` (add `stat` to the `Filesystem` port)
- Test: `crates/alexandria-core/tests/catalog/refresh.rs`

**Interfaces:**
- Consumes: Task 1's stat fields, Task 3's nullable hash.
- Produces: `Filesystem::stat(&self, path: &str) -> Result<Option<FileStat>, DomainError>` where `FileStat { size_bytes: i64, modified_at: Option<DateTime<Utc>> }` and `None` means the file is gone; `CatalogRepository::refresh_stat(&self, path: &str, size_bytes: i64, mtime: Option<DateTime<Utc>>, indexed_at: DateTime<Utc>)` which also sets `content_hash = NULL`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/alexandria-core/tests/catalog/refresh.rs`:

```rust
#[tokio::test]
async fn given_an_unchanged_file_when_refreshed_then_it_is_unchanged_and_no_bytes_are_read() {
    // Arrange: copy the fake wiring from the nearest test above. `repo` is
    // seeded with a cataloged file whose size and mtime match what the
    // filesystem is about to report.
    let repo = FakeCatalogRepository::default();
    repo.seed(a_cataloged_file("/library/song.flac", 4096, Some(now())))
        .await;
    let fs = FakeFilesystem::builder()
        .with_stat("/library/song.flac", 4096, Some(now()))
        .build();

    let RefreshStarted { run_id } = handler.start(TOKEN).await.unwrap();
    let outcome = handler.execute(run_id).await.unwrap();

    assert_eq!(outcome.unchanged, 1);
    assert_eq!(outcome.refreshed, 0);
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
}

#[tokio::test]
async fn given_a_file_whose_size_changed_when_refreshed_then_it_is_refreshed_and_its_hash_is_cleared() {
    let repo = FakeCatalogRepository::default();
    let uuid = repo
        .seed(a_cataloged_file_with_hash("/library/song.flac", 4096, Some(now()), "abc"))
        .await;
    // Same mtime, different size: either one differing is a change.
    let fs = FakeFilesystem::builder()
        .with_stat("/library/song.flac", 8192, Some(now()))
        .build();

    let RefreshStarted { run_id } = handler.start(TOKEN).await.unwrap();
    let outcome = handler.execute(run_id).await.unwrap();

    assert_eq!(outcome.refreshed, 1);
    let file = repo.find_by_uuid(uuid).await.unwrap().unwrap();
    assert_eq!(file.size_bytes, Some(8192));
    assert_eq!(file.content_hash, None, "a stale hash must not outlive the bytes");
    assert_eq!(fs.hash_calls(), 0, "refresh must not hash");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog refreshed`
Expected: FAIL — `no method named 'stat'`, and `hash_calls()` is 1 rather than 0.

- [ ] **Step 3: Add `stat` to the filesystem port**

In `crates/alexandria-core/src/catalog/fs.rs`:

```rust
/// One file's change signal (FR-FC-10). `None` from `stat` means the file is
/// not there at all, which is UC-02 AF-01's "marked missing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size_bytes: i64,
    pub modified_at: Option<DateTime<Utc>>,
}
```

Add to the `Filesystem` trait, with doc comment:

```rust
    /// The file's size and modification time, or `None` when it is gone
    /// (UC-02 AF-01). One `stat` syscall — this is what replaced reading and
    /// hashing every byte to answer "did this change?".
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, DomainError>;
```

Implement on `StdFilesystem`:

```rust
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, DomainError> {
        let path = path.to_string();
        blocking(move || match std::fs::metadata(&path) {
            Ok(metadata) => Ok(Some(FileStat {
                size_bytes: metadata.len() as i64,
                modified_at: metadata.modified().ok().map(DateTime::<Utc>::from),
            })),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(DomainError::disk(format!("stat {path:?}: {err}"))),
        })
        .await
    }
```

- [ ] **Step 4: Replace `refresh_hash` with `refresh_stat`**

In `crates/alexandria-core/src/catalog/repos.rs`, rename the trait method and change its body's SQL at line 374:

```sql
UPDATE files SET size_bytes = ?, mtime = ?, content_hash = NULL,
                 indexed_at = ?, missing_at = NULL WHERE path = ?
```

`content_hash = NULL` is the point: the recorded hash described bytes that have changed, and leaving it would let a stale value be served as current.

- [ ] **Step 5: Rewrite the comparison**

In `crates/alexandria-core/src/catalog/commands/refresh.rs`, replace the per-file body around lines 230-240:

```rust
            let Some(stat) = self.fs.stat(&file.path).await? else {
                // UC-02 AF-01 / FR-FC-11: the on-disk file is gone.
                self.repo.mark_missing(&file.path, now).await?;
                return EntryOutcome::MarkedMissing;
            };

            // FR-FC-10: size and mtime are the change signal. A file that
            // returned to disk while marked missing is refreshed even when its
            // stats match, because `missing_at` has to be cleared.
            let unchanged = file.size_bytes == Some(stat.size_bytes)
                && file.mtime == stat.modified_at
                && file.missing_at.is_none();
            if unchanged {
                return EntryOutcome::Unchanged;
            }

            self.repo
                .refresh_stat(&file.path, stat.size_bytes, stat.modified_at, now)
                .await?;
            EntryOutcome::Refreshed
```

Adapt the surrounding `match`/fold to the outcome enum that file already uses rather than introducing a second one.

- [ ] **Step 6: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "perf(catalog): re-index on size and mtime instead of hashing

Refresh recomputed the SHA-256 of every cataloged file to answer whether
it had changed, which cost a full read per file per run. One stat call
answers the same question.

A refreshed file's content_hash is set back to NULL: it described bytes
that are gone, and a stale hash must not be served as a current one."
```

---

### Task 5: Split `skipped` into `skipped` and `alreadyCataloged`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs` (`EntryOutcome`, `IndexOutcome`, the fold)
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (`RunCounts::Index`)
- Modify: `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql`
- Test: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Produces: `EntryOutcome::{Indexed, Skipped, AlreadyCataloged, Failed}`; `IndexOutcome` and `RunCounts::Index` both gain `already_cataloged: usize`, serialized as `alreadyCataloged`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn given_a_cataloged_file_and_an_unsupported_one_when_indexed_then_the_two_are_counted_apart() {
    let fs = FakeFilesystem::builder()
        .with_root(ROOT)
        .with_file(ROOT, "/library/song.txt", "song.txt", "hash-1")
        .with_file(ROOT, "/library/notes.xyz", "notes.xyz", "hash-2")
        .build();

    // Index once so song.txt is already cataloged, then again over the same
    // root — which is exactly what resume does.
    let IndexStarted { run_id } = handler
        .start(IndexRequest { root: ROOT.into() }, TOKEN)
        .await
        .unwrap();
    handler.execute(ROOT, run_id).await.unwrap();

    let IndexStarted { run_id } = handler
        .start(IndexRequest { root: ROOT.into() }, TOKEN)
        .await
        .unwrap();
    let outcome = handler.execute(ROOT, run_id).await.unwrap();

    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.already_cataloged, 1, "song.txt is already in the catalog");
    assert_eq!(outcome.skipped, 1, "notes.xyz has an unsupported extension");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p alexandria-core --test catalog counted_apart`
Expected: FAIL — `no field 'already_cataloged'`.

- [ ] **Step 3: Add the column**

In `00000000000011_catalog_runs.sql`, after `skipped`:

```sql
    already_cataloged INTEGER,
```

- [ ] **Step 4: Split the outcome**

In `index.rs`, add the variant with its reason:

```rust
/// What one scanned entry resolved to.
///
/// `Skipped` and `AlreadyCataloged` are two different facts and were one
/// counter until resume existed. A resumed run re-walks and re-skips
/// everything a previous segment indexed, so folding the two together made a
/// resumed run report thousands of files as "skipped" — a tally that
/// misdescribes what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryOutcome {
    Indexed,
    Skipped,
    AlreadyCataloged,
    Failed,
}
```

`index_entry` currently returns `Ok(false)` for both cases. Change its signature to return `Result<EntryOutcome, DomainError>` and have the already-cataloged branch return `EntryOutcome::AlreadyCataloged` while the classification branch in `execute` returns `EntryOutcome::Skipped`.

- [ ] **Step 5: Widen the fold, `IndexOutcome`, and `RunCounts::Index`**

Add `already_cataloged: usize` to both structs, extend the fold's tuple to four counters, and add the bind and the column read in `SqliteCatalogRunRepository::finish` and `get`.

- [ ] **Step 6: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(catalog): count already-cataloged files apart from skipped

index_entry returned the same outcome for an unsupported extension and
for a path already in the catalog. Resume re-walks and re-skips
everything an earlier segment did, so the two had to be told apart
before a resumed run's tally could be honest."
```

---

### Task 6: Live progress via a run registry

**Files:**
- Create: `crates/alexandria-core/src/catalog/run_registry.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs` (add `pub mod run_registry;`)
- Modify: `crates/alexandria-core/migrations/00000000000011_catalog_runs.sql`
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (`CatalogRun`, `record_progress`)
- Modify: `crates/alexandria-core/src/catalog/commands/{index,refresh}.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/run_status.rs`
- Test: `crates/alexandria-core/tests/catalog/run_status.rs`

**Interfaces:**
- Produces:

```rust
pub enum RunPhase { Discovering, Processing }

pub struct RunRegistry { /* Mutex<HashMap<Uuid, Arc<RunCell>>> */ }

impl RunRegistry {
    pub fn new() -> Self;
    pub fn open(&self, run_id: Uuid) -> Arc<RunCell>;
    pub fn get(&self, run_id: Uuid) -> Option<Arc<RunCell>>;
    pub fn close(&self, run_id: Uuid);
}

pub struct RunCell { /* AtomicUsize processed, total; AtomicU8 phase, signal */ }

impl RunCell {
    pub fn set_phase(&self, phase: RunPhase);
    pub fn set_total(&self, total: usize);
    pub fn advance(&self);              // processed += 1
    pub fn snapshot(&self) -> RunProgress;
}

pub struct RunProgress {
    pub phase: RunPhase,
    pub total: Option<usize>,
    pub processed: usize,
}
```

`CatalogRun` gains `phase: Option<RunPhase>`, `total: Option<usize>`, `processed: Option<usize>`, `active_millis: i64`, `paused_at: Option<DateTime<Utc>>`, and `paused_millis: i64`.

`paused_millis` carries `#[serde(skip)]`: it is the input `active_millis` is derived from, and a client that has `activeMillis` has no use for it. Task 8's resume test asserts on it through the struct, which is why it is a field rather than a value read straight out of the row.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn given_a_running_run_when_read_then_it_reports_live_progress() {
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();
    let cell = registry.open(run_id);
    cell.set_phase(RunPhase::Processing);
    cell.set_total(12_264);
    for _ in 0..8_412 {
        cell.advance();
    }

    let handler = GetRunStatusHandler::new(ok_auth(), a_running_run_repo(run_id), registry);
    let run = handler.get(run_id, "token").await.unwrap();

    assert_eq!(run.phase, Some(RunPhase::Processing));
    assert_eq!(run.total, Some(12_264));
    assert_eq!(run.processed, Some(8_412));
}

#[tokio::test]
async fn given_a_run_with_no_live_cell_when_read_then_it_reports_the_persisted_progress() {
    let registry = RunRegistry::new();
    let run_id = Uuid::new_v4();

    let handler = GetRunStatusHandler::new(
        ok_auth(),
        a_paused_run_repo(run_id, 12_264, 8_412),
        registry,
    );
    let run = handler.get(run_id, "token").await.unwrap();

    assert_eq!(run.processed, Some(8_412), "a restart falls back to the last flush");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog live_progress`
Expected: FAIL — `RunRegistry` does not exist.

- [ ] **Step 3: Add the columns**

In `00000000000011_catalog_runs.sql`:

```sql
    phase           TEXT,
    total           INTEGER,
    processed       INTEGER,
    paused_at       TEXT,
    paused_millis   INTEGER NOT NULL DEFAULT 0,
    concurrency     INTEGER,
```

- [ ] **Step 4: Write the registry**

Create `crates/alexandria-core/src/catalog/run_registry.rs` implementing the interface above. Use `AtomicUsize` for `processed` and `total` and `AtomicU8` for `phase`, all with `Ordering::Relaxed` — the counters are read for display, and a reader one increment behind is not a defect worth a fence. `total` uses `usize::MAX` as its "not yet known" value so `snapshot` can return `None` during discovery. Document that choice in the file.

- [ ] **Step 5: Publish progress from both handlers**

In `IndexHandler::execute`, open a cell before `list_files`, set the phase to `Discovering`, set the total to `entries.len()` and the phase to `Processing` after the walk, call `cell.advance()` at the end of each per-entry future, and `registry.close(run_id)` when the run terminates. Do the same in `RefreshHandler::execute`, whose "discovery" is `repo.list_all()`.

- [ ] **Step 6: Add the flusher**

Add `CatalogRunRepository::record_progress(&self, id: Uuid, progress: &RunProgress) -> Result<(), DomainError>` writing `phase`, `total`, `processed`. Spawn a `tokio::time::interval(Duration::from_secs(2))` task alongside each run that flushes the cell and exits when the registry no longer holds it. A flush that fails is logged at `warn` and does not fail the run — the in-memory cell is authoritative, and a missed flush costs accuracy after a restart, not correctness.

- [ ] **Step 7: Overlay in the query**

`GetRunStatusHandler` gains a `RunRegistry` field. After loading the row, if `registry.get(run_id)` returns a cell, overwrite the row's `phase`, `total`, and `processed` from `cell.snapshot()`. Compute `active_millis` as `(finished_at.unwrap_or(now) - started_at).num_milliseconds() - paused_millis`.

- [ ] **Step 8: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(catalog): report live indexing progress

A run in flight had status 'running' and nothing else — counts were
written once, at the end, so no client could draw a progress bar because
the core published no number to draw one from.

Progress is now an in-memory cell of atomics per run, flushed into
catalog_runs every two seconds and overlaid onto the persisted row when
the status query runs. A live query is exact; a query after a restart
falls back to the last flush."
```

---

### Task 7: Pause and cancel

**Files:**
- Create: `crates/alexandria-core/src/catalog/commands/run_control.rs`
- Modify: `crates/alexandria-core/src/catalog/run_registry.rs` (the signal)
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (`RunStatus`, `pause`, `cancel`)
- Modify: `crates/alexandria-core/src/catalog/commands/{index,refresh}.rs`
- Test: `crates/alexandria-core/tests/catalog/runs.rs`

**Interfaces:**
- Produces: `RunSignal::{None, Pause, Cancel}`; `RunCell::signal(&self) -> RunSignal` and `RunCell::raise(&self, signal: RunSignal)`; `RunControlHandler::{pause, cancel}(&self, run_id: Uuid, token: &str) -> Result<(), DomainError>`. `RunStatus` gains `Paused` and `Cancelled`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_running_run_when_paused_then_it_stops_and_keeps_its_tally() {
    let harness = ControlHarness::with_running_run().await;

    harness.control.pause(harness.run_id, "token").await.unwrap();
    harness.await_settled().await;

    let run = harness.run().await;
    assert_eq!(run.status, RunStatus::Paused);
    assert!(run.paused_at.is_some());
    assert!(run.processed.unwrap() > 0, "the tally survives the pause");
}

#[tokio::test]
async fn given_a_paused_run_when_paused_again_then_invalid_state() {
    let harness = ControlHarness::with_paused_run().await;
    let result = harness.control.pause(harness.run_id, "token").await;
    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_a_completed_run_when_cancelled_then_invalid_state() {
    let harness = ControlHarness::with_completed_run().await;
    let result = harness.control.cancel(harness.run_id, "token").await;
    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_pausing_then_unauthorized_not_invalid_state() {
    let harness = ControlHarness::with_completed_run().await;
    let result = harness.control.pause(harness.run_id, "bad-token").await;
    assert!(matches!(result, Err(DomainError::Unauthorized)));
}
```

That last one matters: authentication is checked before the state machine, so a bad token never learns anything about the run.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog when_paused`
Expected: FAIL — `RunControlHandler` does not exist.

- [ ] **Step 3: Add the statuses**

In `runs.rs`, add `Paused` and `Cancelled` to `RunStatus` with their `as_str`/`parse` arms. Leave `Interrupted` in place for now; Task 8 removes it.

- [ ] **Step 4: Add the signal**

In `run_registry.rs`, add an `AtomicU8` signal to `RunCell` with `raise` and `signal`.

- [ ] **Step 5: Honour the signal in both handlers**

At the top of each per-entry future, before any work:

```rust
                match cell.signal() {
                    RunSignal::None => {}
                    // The window drains rather than aborting: entries already
                    // in flight are a stat and a header read each, so this
                    // costs milliseconds. Draining is what lets the tally be
                    // written once, correctly, after the last one lands.
                    RunSignal::Pause | RunSignal::Cancel => return EntryOutcome::Halted,
                }
```

Add `EntryOutcome::Halted`, which contributes to no counter. After the fold, branch on the signal: `Pause` writes `runs.pause(run_id, now)`, `Cancel` writes `runs.cancel(run_id, now)`, and `None` keeps today's `runs.finish`.

During `Discovering` the signal is checked once, after `list_files` returns and before the processing loop begins — `walkdir`'s collect is a single blocking call with no interruption point, and discovery is seconds.

- [ ] **Step 6: Write the control handler**

Create `run_control.rs` with a `RunControlHandler<A, RR>` holding auth, the run repository, and the registry. `pause` and `cancel` each: authenticate, load the run (`NotFound` if absent), reject unless the status permits the transition (`DomainError::InvalidState`), raise the signal on the live cell if there is one, and — when there is no live cell, which is a run whose process is gone — write the terminal state directly.

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(catalog): pause and cancel an index or re-index run

One control signal in the run registry, read by each per-entry future
before it does work. The buffer_unordered window drains rather than
aborting, which costs milliseconds now that per-file work is a stat and
a header read rather than a full-file hash.

Pause keeps the tally and records paused_at; cancel is terminal."
```

---

### Task 8: Resume, and pausing runs at startup

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/run_control.rs`
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (`interrupt_running` → `pause_running`; drop `Interrupted`)
- Modify: `crates/alexandria-core/src/services.rs` (the startup reconciliation call site)
- Test: `crates/alexandria-core/tests/catalog/runs.rs`

**Interfaces:**
- Produces: `RunControlHandler::resume(&self, run_id: Uuid, token: &str) -> Result<RunResumed, DomainError>` where `RunResumed { run_id: Uuid, root: Option<String>, kind: RunKind, concurrency: u32 }`; `CatalogRunRepository::pause_running(&self, now: DateTime<Utc>) -> Result<u64, DomainError>`.

`resume` records the state change and returns what the caller needs to spawn `execute` — it does not spawn anything itself, matching how `start` and `execute` are already separated so the FFI and HTTP layers own the spawn.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_paused_run_when_resumed_then_it_runs_again_from_zero_with_the_paused_time_banked() {
    let harness = ControlHarness::with_paused_run_at(t(1), 8_412).await;
    harness.clock.set(t(1) + Duration::minutes(30));

    let resumed = harness.control.resume(harness.run_id, "token").await.unwrap();

    let run = harness.run().await;
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.paused_at, None);
    assert_eq!(run.paused_millis, 30 * 60 * 1000, "paused time is banked, not counted as work");
    assert_eq!(run.processed, Some(0), "the segment's counter restarts");
    assert_eq!(resumed.root, Some("D:/music".to_string()));
}

#[tokio::test]
async fn given_a_cancelled_run_when_resumed_then_invalid_state() {
    let harness = ControlHarness::with_cancelled_run().await;
    let result = harness.control.resume(harness.run_id, "token").await;
    assert!(matches!(result, Err(DomainError::InvalidState)));
}

#[tokio::test]
async fn given_a_run_still_marked_running_at_startup_when_reconciled_then_it_is_paused_not_lost() {
    let repo = a_sqlite_run_repo().await;
    let run_id = Uuid::new_v4();
    repo.start(run_id, RunKind::Index, Some("D:/music"), t(1)).await.unwrap();

    let reconciled = repo.pause_running(t(2)).await.unwrap();

    assert_eq!(reconciled, 1);
    assert_eq!(repo.get(run_id).await.unwrap().unwrap().status, RunStatus::Paused);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog when_resumed`
Expected: FAIL — `no method named 'resume'`.

- [ ] **Step 3: Implement `resume`**

Authenticate, load the run, reject unless `status == Paused`, then write in one statement: `status = 'running'`, `paused_at = NULL`, `paused_millis = paused_millis + (now - paused_at)`, `processed = 0`, `total = NULL`, `phase = 'discovering'`. Return `RunResumed` carrying the root, kind, and stored concurrency so the caller can spawn `execute` on the same run id.

- [ ] **Step 4: Replace interruption with pausing**

Rename `interrupt_running` to `pause_running`, change its `UPDATE` to set `status = 'paused'` and `paused_at = ?`, and remove `Interrupted` from `RunStatus` entirely — including its `as_str` and `parse` arms. Update the call site in `services.rs` and its log line, which should now say runs were paused and can be resumed rather than that they were interrupted.

- [ ] **Step 5: Make `execute` resumable**

`IndexHandler::execute` and `RefreshHandler::execute` need no change for resume — they re-walk, and already-cataloged entries fall out as `AlreadyCataloged` from Task 5. Confirm this by running the existing index tests; if any of them assert that a second `execute` over the same root is refused, that assertion is now wrong and must be updated.

- [ ] **Step 6: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(catalog): resume a paused run, and pause rather than interrupt at startup

Resume re-walks instead of checkpointing a cursor. Already-cataloged
entries fall out as AlreadyCataloged in seconds, so there is nothing to
keep honest and no drift to correct.

Startup reconciliation now leaves a run paused rather than interrupted:
closing the application mid-scan leaves work to resume instead of work
to redo. The Interrupted status is gone."
```

---

### Task 9: Run priority

**Files:**
- Modify: `crates/alexandria-core/src/config.rs:280-300`, `:455-470`
- Modify: `crates/alexandria-core/src/catalog/commands/{index,refresh}.rs` (`start` signatures)
- Modify: `crates/alexandria-core/src/catalog/runs.rs` (persist `concurrency`)
- Modify: `crates/alexandria-core/src/services.rs`
- Test: `crates/alexandria-core/tests/catalog/index.rs`, `tests/config.rs`

**Interfaces:**
- Produces: `RunPriority::{Normal, Low}` in `runs.rs`; `IndexRequest` gains `priority: RunPriority`; `RefreshHandler::start(&self, priority: RunPriority, token: &str)`; config gains `indexing.low_priority_concurrency: u32` (default 1) with the `ALEXANDRIA_INDEXING_LOW_PRIORITY_CONCURRENCY` override.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_low_priority_index_when_started_then_the_run_records_the_low_concurrency() {
    // Arrange: the nine-fake `handler(..)`, built with normal concurrency 4
    // and low-priority concurrency 1.
    let IndexStarted { run_id } = handler
        .start(
            IndexRequest { root: ROOT.into(), priority: RunPriority::Low },
            TOKEN,
        )
        .await
        .unwrap();

    assert_eq!(runs.get(run_id).await.unwrap().unwrap().concurrency, Some(1));
}

#[test]
fn given_no_configured_low_priority_concurrency_when_loaded_then_it_defaults_to_one() {
    let settings = Settings::default();
    assert_eq!(settings.indexing.low_priority_concurrency, 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-core low_priority`
Expected: FAIL — `RunPriority` does not exist.

- [ ] **Step 3: Add the config key**

In `config.rs`, mirroring the existing `concurrency` field exactly — `#[serde(default = "…")]`, a `default_…()` function returning 1, the `Default` impl arm, and the `ALEXANDRIA_INDEXING_LOW_PRIORITY_CONCURRENCY` branch in the environment-override block.

- [ ] **Step 4: Add `RunPriority` and thread it through**

```rust
/// How hard a run should push (FR-FC-08). `Low` is for a large scan the owner
/// wants running while they use the library; it maps to
/// `indexing.low_priority_concurrency` rather than to a raw thread count,
/// because the client should not have to invent a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunPriority {
    #[default]
    Normal,
    Low,
}
```

Both handlers hold both concurrency values and pick with `match priority`. `start` writes the chosen number to the run's `concurrency` column, and `execute` reads it from the run rather than from a field, so a resumed run reuses what it was started with.

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(catalog): start a run at normal or low priority

A semantic knob rather than a thread count: Low maps to the new
indexing.low_priority_concurrency (default 1), Normal to the existing
indexing.concurrency (default 4).

Chosen at start and stored on the run, so resume reuses it. Not a live
slider — buffer_unordered fixes its width when the stream is built, and
changing your mind now costs a pause and a resume."
```

---

### Task 10: Query every outstanding run

**Files:**
- Create: `crates/alexandria-core/src/catalog/queries/active_runs.rs`
- Modify: `crates/alexandria-core/src/catalog/queries/mod.rs`, `runs.rs`, `services.rs`
- Test: `crates/alexandria-core/tests/catalog/run_status.rs`

**Interfaces:**
- Produces: `CatalogRunRepository::list_active(&self) -> Result<Vec<CatalogRun>, DomainError>` returning every run whose status is `running` or `paused`, newest first; `GetActiveRunsHandler::list(&self, token: &str) -> Result<Vec<CatalogRun>, DomainError>`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn given_runs_in_every_state_when_active_ones_are_listed_then_only_running_and_paused_are_returned() {
    let harness = ActiveRunsHarness::new().await;
    let running = harness.a_run(RunStatus::Running).await;
    let paused = harness.a_run(RunStatus::Paused).await;
    harness.a_run(RunStatus::Complete).await;
    harness.a_run(RunStatus::Failed).await;
    harness.a_run(RunStatus::Cancelled).await;

    let active = harness.handler.list("token").await.unwrap();

    let ids: Vec<_> = active.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&running) && ids.contains(&paused));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p alexandria-core --test catalog only_running_and_paused`
Expected: FAIL — `GetActiveRunsHandler` does not exist.

- [ ] **Step 3: Implement it**

`list_active` is `SELECT … FROM catalog_runs WHERE status IN ('running','paused') ORDER BY started_at DESC`, reusing the same row mapper `get` uses. The handler authenticates and then overlays each row with its live registry cell, exactly as `GetRunStatusHandler` does — a caller listing outstanding runs wants current numbers, not the last flush.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(catalog): list every outstanding run

A client showing background activity, or offering to resume at launch,
needs one question answered across all runs at once. Tracking run ids
client-side cannot answer it — only the core knows what is outstanding."
```

---

### Task 11: FFI surface

**Files:**
- Modify: `crates/alexandria-ffi/src/lib.rs`
- Modify: `native/include/alexandria_ffi.h` (regenerated by `cbindgen`; check how `tools/` does it in this repo)
- Test: `crates/alexandria-ffi/tests/parity.rs`, `tests/smoke.rs`

**Interfaces:**
- Produces:

| Symbol | Signature |
| --- | --- |
| `alexandria_index_pause` | `(run_id: *const c_char, token: *const c_char) -> c_int` |
| `alexandria_index_resume` | `(run_id: *const c_char, token: *const c_char) -> IndexStartResult` |
| `alexandria_index_cancel` | `(run_id: *const c_char, token: *const c_char) -> c_int` |
| `alexandria_index_runs_active_json` | `(token: *const c_char) -> RunJsonResult` |
| `alexandria_index_start` | gains `priority: *const c_char` |
| `alexandria_index_refresh_start` | gains `priority: *const c_char` |
| `RUN_ERR_INVALID_STATE` | `c_int = 5` |

`priority` is a string (`"normal"` / `"low"`), not an int: the JSON bodies on the HTTP side use the same lowercase words, and parity means the two surfaces spell it identically. A NULL or unrecognised value is `Normal`.

- [ ] **Step 1: Write the failing test**

In `crates/alexandria-ffi/tests/parity.rs`:

```rust
#[test]
fn given_a_running_run_when_paused_over_ffi_then_the_status_body_matches_http() {
    let harness = FfiHarness::new();
    let run_id = harness.start_index("normal");

    assert_eq!(harness.pause(&run_id), 0);

    let body = harness.run_status(&run_id);
    assert_eq!(body["status"], "paused");
    assert!(body["processed"].is_number());
    assert!(body["activeMillis"].is_number());
}

#[test]
fn given_a_completed_run_when_paused_over_ffi_then_invalid_state() {
    let harness = FfiHarness::new();
    let run_id = harness.completed_run();
    assert_eq!(harness.pause(&run_id), RUN_ERR_INVALID_STATE);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p alexandria-ffi given_a_running_run_when_paused_over_ffi`
Expected: FAIL — `alexandria_index_pause` is not defined.

- [ ] **Step 3: Add the three control calls**

Follow `alexandria_index_run_status_json` at `lib.rs:3756` for the shape: take the services slot, return `RUN_ERR_NOT_INITIALIZED` when empty, parse the uuid (`RUN_ERR_INVALID_INPUT` on failure), `runtime().block_on` the handler, and map `DomainError` to codes — adding `DomainError::InvalidState => RUN_ERR_INVALID_STATE`.

`alexandria_index_resume` additionally spawns `execute` on the returned root and run id, exactly as `alexandria_index_start` at `lib.rs:250-262` does, and returns `IndexStartResult::ok(&run_id.to_string())` with the *same* run id it was given.

- [ ] **Step 4: Add the priority parameter**

Add `priority: *const c_char` as the last parameter of both start functions, parsed with `cstr_lossy` and mapped `"low" => RunPriority::Low`, anything else to `Normal`.

- [ ] **Step 5: Add the active-runs accessor**

`alexandria_index_runs_active_json` mirrors `alexandria_index_run_status_json` but serializes a `Vec<CatalogRun>`.

- [ ] **Step 6: Regenerate the header and run the tests**

Regenerate `native/include/alexandria_ffi.h`, then:

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(ffi): expose run pause, resume, cancel, and active runs

The two start calls gain a priority argument, spelled with the same
lowercase words the HTTP bodies use so the surfaces stay at parity
(FR-FC-24). Breaking to embedders; the header is regenerated and the
front end's ffigen bindings regenerate with it."
```

---

### Task 12: HTTP surface at parity

**Files:**
- Modify: `crates/alexandria-http/src/routes/runs.rs`, `index.rs`, `refresh.rs`, `mod.rs`
- Modify: `crates/alexandria-http/src/lib.rs` (route table)
- Test: `crates/alexandria-http/tests/catalog_api.rs`

**Interfaces:**
- Produces: `POST /v1/index/runs/{runId}/pause|resume|cancel`, `GET /v1/index/runs?status=active`, `priority` accepted on `POST /v1/index` and `POST /v1/index/refresh`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_running_run_when_paused_then_200_and_the_run_reads_paused() {
    let app = TestApp::new().await;
    let run_id = app.start_index("D:/music", "normal").await;

    let response = app.post(&format!("/v1/index/runs/{run_id}/pause")).await;
    assert_eq!(response.status(), StatusCode::OK);

    let run = app.get_json(&format!("/v1/index/runs/{run_id}")).await;
    assert_eq!(run["status"], "paused");
}

#[tokio::test]
async fn given_a_completed_run_when_paused_then_409() {
    let app = TestApp::new().await;
    let run_id = app.completed_run().await;
    let response = app.post(&format!("/v1/index/runs/{run_id}/pause")).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_runs_in_every_state_when_active_ones_are_requested_then_only_those_come_back() {
    let app = TestApp::new().await;
    app.a_run_in_every_state().await;
    let runs = app.get_json("/v1/index/runs?status=active").await;
    assert_eq!(runs.as_array().unwrap().len(), 2);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alexandria-http when_paused`
Expected: FAIL — 404, the routes do not exist.

- [ ] **Step 3: Add the routes**

In `routes/runs.rs`, add `pause_run`, `resume_run`, `cancel_run`, and `active_runs`, following `run_status`'s existing shape exactly — `Result<Path<Uuid>, PathRejection>` for the uuid, `bearer_token(&headers)`, `.map_err(ApiError)`. `resume_run` returns `202` with the run id, matching how `POST /v1/index` answers; the other two return `200`.

Confirm `ApiError` already maps `DomainError::InvalidState` to `409`; if it does not, add that arm in `middleware/error.rs`.

- [ ] **Step 4: Accept `priority` on the two start bodies**

Add `#[serde(default)] priority: RunPriority` to both request structs. `RunPriority` derives `Default = Normal`, so an omitted field keeps every existing caller working.

- [ ] **Step 5: Register the routes and run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(http): pause, resume, cancel, and list active runs

Parity with the FFI surface (FR-FC-24): the same verbs, the same JSON
bodies, and the same lowercase priority words. InvalidState answers 409."
```

---

### Task 13: A throughput case that would have caught this

**Files:**
- Modify: `crates/alexandria-core/tests/throughput.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the test**

The existing NFR-02 fixtures are small text files, which is exactly why the suite could not have caught a size-dominated cost. Add a case whose fixture is large, asserting that the *rate* barely moves:

```rust
/// NFR-02 restated: the indexing rate is independent of the library's size.
///
/// The other cases in this file use small text files, which is why they could
/// not have caught the regression this test exists to prevent — a per-file
/// cost proportional to the file's bytes is invisible to a size-free fixture.
/// A library of 200 × 8 MB files is 1.6 GB; if indexing reads the bytes, this
/// takes seconds to minutes, and if it stats them, it takes about as long as
/// the same count of empty files.
#[tokio::test]
#[ignore = "throughput floor; see the module docs"]
async fn given_large_files_when_indexed_then_the_rate_matches_the_small_file_rate() {
    let small = measure_rate(200, 0).await;
    let large = measure_rate(200, 8 * 1024 * 1024).await;

    println!("small: {small:.0} files/sec, large: {large:.0} files/sec");
    assert!(
        large > small / 4.0,
        "indexing rate collapsed on large files ({large:.0} vs {small:.0} files/sec): \
         something is reading file bytes during a scan"
    );
}
```

Write `measure_rate(count: usize, bytes_each: usize) -> f64` alongside it, reusing the real-collaborator setup the neighbouring tests already build.

The factor-of-four floor is deliberately loose. This test guards against a regression that would show up as two orders of magnitude, not as noise, and a tight bound on a personal machine buys flakiness.

- [ ] **Step 2: Run it**

Run: `cargo test -p alexandria-core --test throughput --release -- --ignored --nocapture`
Expected: PASS, with the two rates within a factor of four.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test(throughput): assert the indexing rate is size-independent

Every existing NFR-02 fixture is a small text file, which is why the
suite never caught a per-file cost proportional to the file's bytes.
This case indexes 1.6 GB across 200 files and fails if the rate
collapses against the small-file baseline."
```

---

### Task 14: Requirement documents

**Files:**
- Modify: `docs/requirements/System Requirements Document.md`
- Modify: `docs/requirements/Use Case Specification Document.md`
- Modify: `docs/requirements/Testing Specification Document.md:200-220`

- [ ] **Step 1: Rewrite the affected requirements**

- **FR-FC-09** — indexing records path, name, type, size, and mtime, and does not read file bytes.
- **FR-FC-10** — re-index compares size and mtime; a difference is a change, and a changed file's stored hash is cleared.
- **FR-FC-27** — the run record's statuses are `running`, `paused`, `complete`, `failed`, `cancelled`; its counters are `scanned`, `indexed`, `skipped`, `alreadyCataloged`, `failed` for an index and `refreshed`, `markedMissing`, `unchanged`, `failed` for a refresh; it carries `phase`, `total`, `processed`, and `activeMillis` while in flight.
- **FR-FC-29** — a run found `running` at startup is recorded `paused` and offered for resume; nothing resumes by itself.
- **NFR-02** — "at least 500 files per second on a personal machine, independent of the total size of the library, without blocking read/query operations."

- [ ] **Step 2: Add the new requirements**

New FRs for: starting a run at a priority; pausing an in-flight run; resuming a paused one; cancelling a run; and querying every outstanding run.

- [ ] **Step 3: Add the use case**

A new use case for pausing and resuming a run, with alternate flows for pausing a run that is not running, resuming one that is not paused, and resuming after an application restart.

- [ ] **Step 4: Update the testing specification**

§9 describes what `throughput.rs` measures; add the size-independence case from Task 13 and say why the existing fixtures could not have caught it.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: record indexing progress, run control, and the scale change

FR-FC-09 and FR-FC-10 move to stat-based change detection, FR-FC-27
gains the new statuses and counters, FR-FC-29 pauses rather than
interrupts, and NFR-02's rate is now explicitly independent of library
size. New FRs and a use case cover priority, pause, resume, cancel, and
querying outstanding runs."
```

---

## What this plan does not cover

The front end. Auto-indexing on folder registration, the progress bar, the background activity strip, the resume prompt, the library-tools button placement, and the `alexandria_desktop` → Alexandria renaming are all `alexandria-ui` work, specified separately once this lands and the regenerated header is available for `ffigen`.
