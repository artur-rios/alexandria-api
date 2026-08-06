# Audio Metadata Extraction (UC-01 pilot, issue #44) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read embedded ID3/Vorbis/MP4 tags at index time and pre-populate an audio file's subtype metadata, instead of leaving every field for the owner to type in via UC-04.

**Architecture:** A new `AudioMetadataReader` trait port (mirroring the existing `Filesystem`/`JwksProvider` pattern) is added as a fifth generic collaborator on `IndexHandler`. After `insert_file` creates the (empty) subtype row, `index_entry` makes one best-effort call to the reader and, if it found anything, writes it via the *existing* `CatalogRepository::update_metadata` — the same method UC-04 already uses. No schema change, no new repository method, `refresh.rs` untouched.

**Tech Stack:** Rust, `lofty` (new dependency) for tag parsing, existing `sqlx`/`axum`/FFI stack unchanged.

## Global Constraints

- Spec doc: `docs/superpowers/specs/2026-08-06-audio-metadata-extraction-design.md` — read it first if anything below is ambiguous.
- Extraction runs **once, at first index only**. Never touch `refresh.rs`.
- Extraction failure (no tags, corrupt tags, unparseable file) is **never** a run failure: not counted in `IndexOutcome::failed`, logged at `debug` at most.
- No repository trait changes, no migration, no `NewFile` field addition — reuse `CatalogRepository::update_metadata`.
- `lofty`'s exact method names are for a specific version pinned below (`lofty = "0.22"`); if a method name has moved in the resolved version, fix it against `cargo doc -p lofty --open` — the shapes (`Probe`, `TaggedFileExt`, `Accessor`, `TagExt`) have been stable across recent `0.2x` releases.
- Every new/changed Rust file must pass `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` before its task is done.
- Branch: `feature/audio-metadata-extraction` off `main`. One PR at the end of Task 7, following this repo's established branch → PR → CI → squash-merge cycle (see recent merged PRs for the pattern).

---

### Task 1: `AudioTags` type and `AudioMetadataReader` trait

**Files:**
- Create: `crates/alexandria-core/src/catalog/audio_tags.rs`
- Modify: `crates/alexandria-core/src/catalog/mod.rs`

**Interfaces:**
- Produces: `pub struct AudioTags { pub title: Option<String>, pub artist: Option<String>, pub album: Option<String>, pub year: Option<i64>, pub genre: Option<String>, pub track: Option<i64> }`
- Produces: `impl AudioTags { pub fn into_subtype_metadata(self) -> Option<SubtypeMetadata> }`
- Produces: `#[allow(async_fn_in_trait)] pub trait AudioMetadataReader: Send + Sync { async fn read(&self, path: &str) -> Option<AudioTags>; }`

This task is pure logic (no I/O, no `lofty` dependency yet) — TDD it directly.

- [ ] **Step 1: Write the failing test**

Create `crates/alexandria-core/src/catalog/audio_tags.rs`:

```rust
use crate::catalog::model::SubtypeMetadata;

/// Tags read from an audio file's embedded metadata (ID3/Vorbis/MP4),
/// before being mapped onto a `SubtypeMetadata::Audio` write (issue #44
/// pilot). Every field is `Option` because a real file rarely has all six
/// populated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub track: Option<i64>,
}

impl AudioTags {
    /// `None` when every field is `None` — nothing worth writing, so the
    /// caller skips the `update_metadata` call entirely.
    pub fn into_subtype_metadata(self) -> Option<SubtypeMetadata> {
        todo!()
    }
}

/// Read-only port over an audio file's embedded tags (issue #44 pilot).
/// Generic-parameter-injected into `IndexHandler` so the decision logic is
/// unit-tested against a fake with no real file I/O (Testing Specification
/// §6.2); wired with the real `LoftyAudioMetadataReader` in `services.rs`.
#[allow(async_fn_in_trait)]
pub trait AudioMetadataReader: Send + Sync {
    /// Best-effort read of embedded tags. `None` covers both "no tags
    /// present" and "couldn't parse this file" — the caller never needs to
    /// tell them apart; extraction failure is never a run failure.
    async fn read(&self, path: &str) -> Option<AudioTags>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_all_fields_none_when_into_subtype_metadata_then_none() {
        let tags = AudioTags::default();
        assert_eq!(tags.into_subtype_metadata(), None);
    }

    #[test]
    fn given_some_fields_set_when_into_subtype_metadata_then_audio_variant_with_those_fields() {
        let tags = AudioTags {
            title: Some("Song".to_string()),
            artist: Some("Band".to_string()),
            album: None,
            year: Some(1999),
            genre: None,
            track: Some(3),
        };

        let metadata = tags.into_subtype_metadata().expect("some fields set");

        assert_eq!(
            metadata,
            SubtypeMetadata::Audio {
                title: Some("Song".to_string()),
                artist: Some("Band".to_string()),
                album: None,
                year: Some(1999),
                genre: None,
                track: Some(3),
            }
        );
    }
}
```

Add the module to `crates/alexandria-core/src/catalog/mod.rs` — it currently
reads:

```rust
pub mod classify;
pub mod clock;
pub mod commands;
pub mod fs;
pub mod model;
pub mod queries;
pub mod repos;
```

Change to:

```rust
pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod commands;
pub mod fs;
pub mod model;
pub mod queries;
pub mod repos;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::audio_tags -- --nocapture`
Expected: compiles (the `todo!()` is valid Rust), then both tests panic with
`not yet implemented`.

- [ ] **Step 3: Implement `into_subtype_metadata`**

Replace the `todo!()` body in `audio_tags.rs`:

```rust
impl AudioTags {
    pub fn into_subtype_metadata(self) -> Option<SubtypeMetadata> {
        if self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.year.is_none()
            && self.genre.is_none()
            && self.track.is_none()
        {
            return None;
        }
        Some(SubtypeMetadata::Audio {
            title: self.title,
            artist: self.artist,
            album: self.album,
            year: self.year,
            genre: self.genre,
            track: self.track,
        })
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::audio_tags`
Expected: `test result: ok. 2 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-core/src/catalog/audio_tags.rs crates/alexandria-core/src/catalog/mod.rs
git commit -m "feat: add AudioTags and AudioMetadataReader port"
```

---

### Task 2: `LoftyAudioMetadataReader` (real implementation)

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/alexandria-core/Cargo.toml`
- Modify: `crates/alexandria-core/src/catalog/audio_tags.rs`

**Interfaces:**
- Consumes: `AudioTags`, `AudioMetadataReader` from Task 1.
- Produces: `#[derive(Debug, Default, Clone, Copy)] pub struct LoftyAudioMetadataReader;` implementing `AudioMetadataReader`.

- [ ] **Step 1: Add the `lofty` dependency**

In `Cargo.toml` (workspace root), `[workspace.dependencies]` section — insert
alphabetically between `jsonwebtoken` and `reqwest`:

```toml
jsonwebtoken = "9"
lofty = "0.22"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

In `crates/alexandria-core/Cargo.toml`, `[dependencies]` section — insert
alphabetically after `jsonwebtoken`:

```toml
jsonwebtoken.workspace = true
lofty.workspace = true
reqwest.workspace = true
```

Run: `cargo build -p alexandria-core`
Expected: builds successfully, `Cargo.lock` updates to include `lofty` and
its transitive dependencies.

- [ ] **Step 2: Write the failing test (real file, real parse)**

Append to `crates/alexandria-core/src/catalog/audio_tags.rs`, inside the
existing `#[cfg(test)] mod tests` block (add the imports at the top of the
module too):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ... existing tests above stay unchanged ...

    /// Write a minimal valid single-channel 8-bit PCM WAV file — just
    /// enough of a real RIFF/WAVE container for `lofty` to recognize the
    /// format and accept a written tag. No real audio content is needed;
    /// the eight sample bytes are arbitrary.
    fn write_minimal_wav(path: &std::path::Path) {
        let sample_data: [u8; 8] = [0x80; 8];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36u32 + sample_data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
        bytes.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
        bytes.extend_from_slice(&1u16.to_le_bytes()); // block align
        bytes.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(sample_data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&sample_data);

        let mut file = std::fs::File::create(path).expect("create wav");
        file.write_all(&bytes).expect("write wav");
    }

    /// Write an ID3v2 tag with all six fields onto an existing WAV file.
    fn write_test_tags(path: &std::path::Path) {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};

        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title("Test Title".to_string());
        tag.set_artist("Test Artist".to_string());
        tag.set_album("Test Album".to_string());
        tag.set_genre("Test Genre".to_string());
        tag.set_year(2020);
        tag.set_track(7);
        tag.save_to_path(path, WriteOptions::default())
            .expect("save tag");
    }

    #[tokio::test]
    async fn given_tagged_wav_when_read_then_all_fields_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.wav");
        write_minimal_wav(&path);
        write_test_tags(&path);

        let reader = LoftyAudioMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.artist.as_deref(), Some("Test Artist"));
        assert_eq!(tags.album.as_deref(), Some("Test Album"));
        assert_eq!(tags.genre.as_deref(), Some("Test Genre"));
        assert_eq!(tags.year, Some(2020));
        assert_eq!(tags.track, Some(7));
    }

    #[tokio::test]
    async fn given_untagged_wav_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("untagged.wav");
        write_minimal_wav(&path);

        let reader = LoftyAudioMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), "no tag written, no tag read");
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = LoftyAudioMetadataReader;

        let tags = reader.read("/no/such/file.wav").await;

        assert!(tags.is_none());
    }
}
```

`tempfile` is already a dev-dependency of `alexandria-core` (see its
`Cargo.toml`), so no change needed there.

- [ ] **Step 2b: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --lib catalog::audio_tags`
Expected: fails to compile — `LoftyAudioMetadataReader` does not exist yet.

- [ ] **Step 3: Implement `LoftyAudioMetadataReader`**

Add above the `#[cfg(test)]` block in `audio_tags.rs`:

```rust
/// Real audio-tag reader backed by `lofty`, covering ID3v1/v2 (MP3, WAV),
/// Vorbis comments (FLAC, OGG/OGA, Opus), and MP4 atoms (M4A, AAC-in-MP4) —
/// every extension `classify_by_extension` maps to `FileType::Audio`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyAudioMetadataReader;

impl AudioMetadataReader for LoftyAudioMetadataReader {
    async fn read(&self, path: &str) -> Option<AudioTags> {
        use lofty::file::TaggedFileExt;
        use lofty::probe::Probe;
        use lofty::tag::Accessor;

        let tagged_file = match Probe::open(path).and_then(|probe| probe.read()) {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse audio tags");
                return None;
            }
        };

        let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag())?;

        let tags = AudioTags {
            title: tag.title().map(|s| s.to_string()),
            artist: tag.artist().map(|s| s.to_string()),
            album: tag.album().map(|s| s.to_string()),
            year: tag.year().map(i64::from),
            genre: tag.genre().map(|s| s.to_string()),
            track: tag.track().map(i64::from),
        };

        tags.into_subtype_metadata().is_some().then_some(tags)
    }
}
```

Note: `tag.year()`/`tag.track()` return `Option<u32>` in `lofty` — `i64::from`
widens without a cast. If the resolved `lofty` version's `Accessor` trait
has renamed a method (check `cargo doc -p lofty --open` →
`lofty::tag::Accessor`), fix the method name here; the overall shape stays
the same.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --lib catalog::audio_tags`
Expected: `test result: ok. 5 passed; 0 failed` (2 from Task 1 + 3 new ones).

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alexandria-core/Cargo.toml crates/alexandria-core/src/catalog/audio_tags.rs
git commit -m "feat: implement LoftyAudioMetadataReader"
```

---

### Task 3: `FakeAudioMetadataReader` test double

**Files:**
- Modify: `crates/alexandria-core/tests/common/mod.rs`

**Interfaces:**
- Consumes: `AudioMetadataReader`, `AudioTags` from `alexandria_core::catalog::audio_tags`.
- Produces: `FakeAudioMetadataReader::new()`, `.seed(path: &str, tags: AudioTags)`, implementing `AudioMetadataReader`.

No test-the-test needed here — this fake is exercised by Task 4's
`IndexHandler` tests. Add it directly, following the exact shape of
`FakeCatalogRepository` already in this file (`Arc<Mutex<HashMap<...>>>`,
`new()`, a seed method).

- [ ] **Step 1: Add the fake**

Add this import near the top of `crates/alexandria-core/tests/common/mod.rs`,
alongside the other `alexandria_core::...` imports (e.g. next to the
`alexandria_core::auth::local::...` import):

```rust
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
```

Append this new fake at the end of the file, after
`FakeSessionRepository`'s `impl SessionRepository for FakeSessionRepository`
block:

```rust
/// In-memory audio-tag reader (issue #44 pilot). `read()` answers `None`
/// for any path with no seeded tags, mirroring "no tags found / couldn't
/// parse" — the same outcome `LoftyAudioMetadataReader` produces for those
/// cases.
#[derive(Debug, Default, Clone)]
pub struct FakeAudioMetadataReader {
    tags: Arc<Mutex<HashMap<String, AudioTags>>>,
}

impl FakeAudioMetadataReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tags `read()` returns for `path`.
    pub fn seed(&self, path: &str, tags: AudioTags) -> &Self {
        self.tags.lock().unwrap().insert(path.to_string(), tags);
        self
    }
}

impl AudioMetadataReader for FakeAudioMetadataReader {
    async fn read(&self, path: &str) -> Option<AudioTags> {
        self.tags.lock().unwrap().get(path).cloned()
    }
}
```

- [ ] **Step 2: Confirm it compiles**

Run: `cargo test -p alexandria-core --test catalog -- --list`
Expected: compiles cleanly and lists the existing catalog tests (the fake
being unused so far is fine — `common/mod.rs` already has a module-level
`#![allow(dead_code)]`).

- [ ] **Step 3: Commit**

```bash
git add crates/alexandria-core/tests/common/mod.rs
git commit -m "test: add FakeAudioMetadataReader test double"
```

---

### Task 4: Wire `AudioMetadataReader` into `IndexHandler`

**Files:**
- Modify: `crates/alexandria-core/src/catalog/commands/index.rs`
- Modify: `crates/alexandria-core/tests/catalog/index.rs`

**Interfaces:**
- Consumes: `AudioMetadataReader`, `AudioTags` from Task 1; `FakeAudioMetadataReader` from Task 3.
- Produces: `IndexHandler<A, R, F, C, M>` (was `<A, R, F, C>`) — `M: AudioMetadataReader` is the new 5th parameter, with `pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M) -> Self` (was 4 params).

This changes `IndexHandler`'s public constructor arity, so every existing
call site (production `services.rs`, and every test in
`tests/catalog/index.rs`) needs the new argument. `services.rs` is Task 5;
this task covers the handler itself and its own test file.

- [ ] **Step 1: Write the failing tests**

In `crates/alexandria-core/tests/catalog/index.rs`, make these exact edits:

**1a.** Change the imports block at the top from:

```rust
use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::model::FileType;
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAuth, FakeCatalogRepository, FakeFilesystem,
};
```

to:

```rust
use uuid::Uuid;

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::audio_tags::{AudioMetadataReader, AudioTags};
use alexandria_core::catalog::classify::classify_by_extension;
use alexandria_core::catalog::clock::Clock;
use alexandria_core::catalog::commands::index::{IndexHandler, IndexRequest};
use alexandria_core::catalog::fs::Filesystem;
use alexandria_core::catalog::model::{FileType, SubtypeMetadata};
use alexandria_core::catalog::repos::CatalogRepository;
use alexandria_core::errors::DomainError;

use crate::common::{
    existing_file, fixed_clock, now, FakeAudioMetadataReader, FakeAuth, FakeCatalogRepository,
    FakeFilesystem,
};
```

**1b.** Change the `handler` helper function from:

```rust
fn handler<A, R, F, C>(auth: A, repo: R, fs: F, clock: C) -> IndexHandler<A, R, F, C>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
{
    IndexHandler::new(auth, repo, fs, clock)
}
```

to:

```rust
fn handler<A, R, F, C, M>(
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
) -> IndexHandler<A, R, F, C, M>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
{
    IndexHandler::new(auth, repo, fs, clock, audio_tags)
}
```

**1c.** Every existing call to `handler(...)` in this file passes exactly 4
arguments today, across 3 distinct literal call shapes. Make all three
replacements — each applies to every occurrence of that exact text in the
file (use your editor's "replace all" for each of the three, or repeat the
edit at each occurrence manually):

**Shape 1** — appears once, in
`given_valid_root_and_authenticated_when_start_then_returns_run_id`:

Change:
```rust
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
    );
```
to:
```rust
    let handler = handler(
        FakeAuth::Allowing,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
    );
```
This exact text also appears in `given_missing_root_when_start_then_invalid_input`
— apply the same change there too (both occurrences are identical, so one
find-all-replace covers both).

**Shape 2** — appears once, in
`given_unauthenticated_when_start_then_unauthorized`:

Change:
```rust
    let handler = handler(
        FakeAuth::Denying,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
    );
```
to:
```rust
    let handler = handler(
        FakeAuth::Denying,
        FakeCatalogRepository::new(),
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
    );
```

**Shape 3** — appears identically 5 times, in
`given_already_cataloged_path_when_execute_then_skipped_no_duplicate`,
`given_supported_files_when_execute_then_indexed_with_hash_and_indexedat`,
`given_unsupported_extension_when_execute_then_skipped`,
`given_unreadable_file_when_execute_then_run_continues_and_counts_failure`,
and `given_failing_repository_write_when_execute_then_run_continues_and_counts_failure`:

Change every occurrence of:
```rust
    let handler = handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()));
```
to:
```rust
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
    );
```
(all 5 occurrences get the identical replacement — a single find-all-replace
covers every one).

After these three replacements, confirm no call site still passes only 4
arguments:

Run: `grep -n "handler(" crates/alexandria-core/tests/catalog/index.rs`
Expected: every multi-line `handler(...)` call block now contains
`FakeAudioMetadataReader::new()` as its last argument before the closing
`);` — 8 call sites total (the 8 listed above), each now 5 arguments.

**1d.** Add these new tests at the end of the file, just before the closing
of the file (after the last existing test,
`given_fixed_clock_when_now_then_returns_seeded_time`):

```rust
#[tokio::test]
async fn given_tagged_audio_file_when_execute_then_subtype_metadata_written() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    let audio_tags = FakeAudioMetadataReader::new();
    audio_tags.seed(
        "/library/a.mp3",
        AudioTags {
            title: Some("Song".to_string()),
            artist: Some("Band".to_string()),
            album: Some("LP".to_string()),
            year: Some(2001),
            genre: Some("Rock".to_string()),
            track: Some(4),
        },
    );
    let handler = handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()), audio_tags);

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp3").expect("indexed");
    let metadata = repo_handle
        .metadata_for(a.uuid)
        .expect("metadata written from extracted tags");
    assert_eq!(
        metadata,
        SubtypeMetadata::Audio {
            title: Some("Song".to_string()),
            artist: Some("Band".to_string()),
            album: Some("LP".to_string()),
            year: Some(2001),
            genre: Some("Rock".to_string()),
            track: Some(4),
        }
    );
}

#[tokio::test]
async fn given_untagged_audio_file_when_execute_then_subtype_metadata_stays_empty() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/a.mp3", "a.mp3", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let repo_handle = repo.clone();
    // No tags seeded — FakeAudioMetadataReader::read returns None for any
    // unseeded path.
    let handler = handler(
        FakeAuth::Allowing,
        repo,
        fs,
        fixed_clock(now()),
        FakeAudioMetadataReader::new(),
    );

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let a = repo_handle.file_for("/library/a.mp3").expect("indexed");
    assert!(
        repo_handle.metadata_for(a.uuid).is_none(),
        "no tags found means no update_metadata call"
    );
}

#[tokio::test]
async fn given_non_audio_file_when_execute_then_reader_never_consulted() {
    let fs = FakeFilesystem::builder()
        .with_file(ROOT, "/library/notes.md", "notes.md", "h-a")
        .build();
    let repo = FakeCatalogRepository::new();
    let audio_tags = FakeAudioMetadataReader::new();
    // Seed a path that does not exist in this library — if the handler
    // mistakenly consulted the reader for a text file, this would only
    // matter if it also queried the wrong path, so the strongest check is
    // the outcome below: no metadata for the text file's uuid, and the
    // fake never needed a seed to prove Text files are skipped entirely.
    let repo_handle = repo.clone();
    let handler = handler(FakeAuth::Allowing, repo, fs, fixed_clock(now()), audio_tags);

    let outcome = handler
        .execute(ROOT, Uuid::new_v4())
        .await
        .expect("execute");

    assert_eq!(outcome.indexed, 1);
    let notes = repo_handle
        .file_for("/library/notes.md")
        .expect("indexed");
    assert_eq!(notes.file_type, FileType::Text);
    assert!(
        repo_handle.metadata_for(notes.uuid).is_none(),
        "Text has no SubtypeMetadata variant; extraction must not run for it"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: fails to compile — `IndexHandler::new` still takes 4 arguments,
`M`/`AudioMetadataReader` not wired.

- [ ] **Step 3: Implement the change in `index.rs`**

In `crates/alexandria-core/src/catalog/commands/index.rs`, make these exact
edits.

Change the imports at the top from:

```rust
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::fs::{FileEntry, Filesystem};
use crate::catalog::model::{FileType, NewFile};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
```

to:

```rust
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::audio_tags::AudioMetadataReader;
use crate::catalog::classify::classify_by_extension;
use crate::catalog::clock::Clock;
use crate::catalog::fs::{FileEntry, Filesystem};
use crate::catalog::model::{FileType, NewFile};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
```

Change the struct + constructor from:

```rust
pub struct IndexHandler<A, R, F, C> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
}

impl<A, R, F, C> IndexHandler<A, R, F, C>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
        }
    }
```

to:

```rust
pub struct IndexHandler<A, R, F, C, M> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
    audio_tags: M,
}

impl<A, R, F, C, M> IndexHandler<A, R, F, C, M>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
    C: Clock,
    M: AudioMetadataReader,
{
    pub fn new(auth: A, repo: R, fs: F, clock: C, audio_tags: M) -> Self {
        Self {
            auth,
            repo,
            fs,
            clock,
            audio_tags,
        }
    }
```

Change `index_entry` from:

```rust
    async fn index_entry(
        &self,
        entry: FileEntry,
        file_type: FileType,
        now: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        if self.repo.find_by_path(&entry.path).await?.is_some() {
            return Ok(false);
        }
        let content_hash = self.fs.content_hash(&entry.path).await?;
        self.repo
            .insert_file(NewFile {
                uuid: Uuid::new_v4(),
                path: entry.path,
                name: entry.name,
                file_type,
                content_hash,
                indexed_at: now,
            })
            .await?;
        Ok(true)
    }
```

to:

```rust
    async fn index_entry(
        &self,
        entry: FileEntry,
        file_type: FileType,
        now: DateTime<Utc>,
    ) -> Result<bool, DomainError> {
        if self.repo.find_by_path(&entry.path).await?.is_some() {
            return Ok(false);
        }
        let content_hash = self.fs.content_hash(&entry.path).await?;
        let file = self
            .repo
            .insert_file(NewFile {
                uuid: Uuid::new_v4(),
                path: entry.path.clone(),
                name: entry.name,
                file_type,
                content_hash,
                indexed_at: now,
            })
            .await?;

        // Best-effort audio tag prefill (issue #44 pilot). Extraction only
        // ever runs here, at first index — refresh never touches metadata.
        // A parse failure or a write failure here must not fail indexing
        // (it is not counted in `IndexOutcome::failed`).
        if file_type == FileType::Audio {
            if let Some(metadata) = self
                .audio_tags
                .read(&entry.path)
                .await
                .and_then(|tags| tags.into_subtype_metadata())
            {
                if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                    tracing::warn!(
                        path = %entry.path,
                        error = %err,
                        "indexed but failed to write extracted audio tags"
                    );
                }
            }
        }
        Ok(true)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria-core --test catalog -- index::`
Expected: all tests in this module pass, including the 3 new ones (count
will be 8 pre-existing `#[tokio::test]`s minus the 2 `BearerAuthService`
ones unaffected by this change, plus 2 non-async, plus 3 new — just confirm
`0 failed`).

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-core --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/catalog/commands/index.rs crates/alexandria-core/tests/catalog/index.rs
git commit -m "feat: extract audio tags into subtype metadata on first index"
```

---

### Task 5: Wire `LoftyAudioMetadataReader` into `services.rs`

**Files:**
- Modify: `crates/alexandria-core/src/services.rs`

**Interfaces:**
- Consumes: `LoftyAudioMetadataReader` from Task 2, `IndexHandler<A, R, F, C, M>::new` from Task 4.

- [ ] **Step 1: Add the import**

In `crates/alexandria-core/src/services.rs`, the imports block currently
has (among others):

```rust
use crate::catalog::commands::index::IndexHandler;
```

Add a new import line right above it:

```rust
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
use crate::catalog::commands::index::IndexHandler;
```

- [ ] **Step 2: Update the `DefaultIndexHandler` type alias**

Change:

```rust
pub type DefaultIndexHandler =
    IndexHandler<RuntimeAuthService, SqliteCatalogRepository, StdFilesystem, SystemClock>;
```

to:

```rust
pub type DefaultIndexHandler = IndexHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    LoftyAudioMetadataReader,
>;
```

- [ ] **Step 3: Update the construction site**

In `build_services`, change:

```rust
    let index_handler = Arc::new(IndexHandler::new(auth.clone(), repo.clone(), fs, clock));
```

to:

```rust
    let audio_tags = LoftyAudioMetadataReader;
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        audio_tags,
    ));
```

- [ ] **Step 4: Build and run the full workspace test suite**

Run: `cargo build --workspace`
Expected: builds cleanly.

Run: `cargo test --workspace`
Expected: every test passes, `0 failed` across every crate (this confirms
nothing else in the workspace constructed `IndexHandler` directly — only
`services.rs` and the two test files already updated do).

- [ ] **Step 5: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/alexandria-core/src/services.rs
git commit -m "feat: wire LoftyAudioMetadataReader into DefaultIndexHandler"
```

---

### Task 6: HTTP/FFI integration + parity test

**Files:**
- Modify: `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Consumes: the full indexing pipeline through both surfaces (unchanged public API — this task only adds a new test).

This reuses the exact `local_settings()` / `seed_session()` /
`build_services()` / `app()` / `alexandria_index_init` /
`alexandria_index_start` scaffolding every other test in this file already
uses (see e.g. `given_same_lib_when_indexed_via_http_and_ffi_then_files_rows_identical`
near the top of the file) — only the fixture and the final assertion are
new.

- [ ] **Step 1: Write the failing test**

Append to the end of `crates/alexandria-ffi/tests/parity.rs`:

```rust
/// Write a minimal valid single-channel 8-bit PCM WAV file (see
/// `alexandria-core`'s `catalog::audio_tags` unit tests for the same
/// helper) — just enough of a real RIFF/WAVE container for `lofty` to
/// recognize the format and accept a written tag.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_data: [u8; 8] = [0x80; 8];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32 + sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&sample_data);
    std::fs::write(path, bytes).expect("write wav");
}

fn write_test_tags(path: &std::path::Path) {
    use lofty::config::WriteOptions;
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title("Parity Title".to_string());
    tag.set_artist("Parity Artist".to_string());
    tag.set_album("Parity Album".to_string());
    tag.set_genre("Parity Genre".to_string());
    tag.set_year(2015);
    tag.set_track(2);
    tag.save_to_path(path, WriteOptions::default())
        .expect("save tag");
}

/// Issue #44 pilot parity — index a tagged audio file through both
/// transports and assert the extracted subtype metadata (written by the
/// indexer itself, not by a manual PATCH) is byte-for-byte identical.
#[tokio::test]
async fn given_tagged_audio_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_track = http_lib.path().join("song.wav");
    write_minimal_wav(&http_track);
    write_test_tags(&http_track);

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.wav")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_track = ffi_lib.path().join("song.wav");
    write_minimal_wav(&ffi_track);
    write_test_tags(&ffi_track);
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if alexandria_index_count_files() >= 1 {
                break;
            }
            if std::time::Instant::now() > dl {
                panic!("ffi never persisted 1 file");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let uuid_json = {
            let raw = alexandria_index_files_json();
            assert!(!raw.is_null());
            let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
            unsafe {
                alexandria_free_string(raw);
            }
            json
        };
        let files: serde_json::Value = serde_json::from_str(&uuid_json).unwrap();
        let ffi_uuid = files[0]["uuid"].as_str().unwrap().to_string();

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["metadata"], ffi_body["metadata"],
        "extracted audio metadata must match across surfaces"
    );
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["artist"], "Parity Artist");
    assert_eq!(http_body["metadata"]["album"], "Parity Album");
    assert_eq!(http_body["metadata"]["genre"], "Parity Genre");
    assert_eq!(http_body["metadata"]["year"], 2015);
    assert_eq!(http_body["metadata"]["track"], 2);
}
```

The signature used above is confirmed against
`crates/alexandria-ffi/src/lib.rs`: `alexandria_file_get_by_uuid(uuid: *const c_char, token: *const c_char) -> FileJsonResult`,
where `FileJsonResult { pub status: c_int, pub json: *mut c_char }` — matches
the `.status` / `.json` usage above exactly, no adjustment needed.

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p alexandria-ffi --test parity given_tagged_audio_file_when_indexed_via_http_and_ffi -- --nocapture`
Expected: `test result: ok. 1 passed`. This test only exercises code paths
that already exist (indexing, get-by-uuid) plus the new tag-extraction path
from Tasks 1–5, so unlike Tasks 1–4 there's no "write it failing first" step
that means anything — the assertions either hold given the prior tasks'
implementation, or reveal a real bug in it.

- [ ] **Step 3: Run the full parity suite to confirm no regression**

Run: `cargo test -p alexandria-ffi --test parity`
Expected: every test in the file passes (this file runs single-threaded
across its `SERIAL` mutex, so this also confirms no deadlock/ordering
issue from the new test).

- [ ] **Step 4: `fmt` + `clippy`**

Run: `cargo fmt --all && cargo clippy -p alexandria-ffi --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/alexandria-ffi/tests/parity.rs
git commit -m "test: add HTTP/FFI parity coverage for extracted audio metadata"
```

---

### Task 7: Full verification, PR, and merge

**Files:** none (verification + workflow only)

- [ ] **Step 1: Full workspace verification**

Run: `cargo fmt --all -- --check`
Expected: no diff.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo test --workspace`
Expected: every test passes across every crate, `0 failed`.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin feature/audio-metadata-extraction
```

```bash
gh pr create --title "feat: extract audio metadata during indexing (issue #44 pilot)" --body "$(cat <<'EOF'
## Summary
- Implements the audio pilot slice of issue #44: `GET`-time indexing now reads embedded ID3/Vorbis/MP4 tags via `lofty` and pre-populates the subtype row via the existing UC-04 `update_metadata` call, instead of leaving every field for the owner to enter manually.
- Extraction runs once, at first index only; `refresh.rs` is untouched. Extraction failure (no tags, corrupt tags, unparseable file) never fails the indexing run and is never counted in `IndexOutcome::failed`.
- No schema change, no new repository method, no `NewFile` field addition.
- Image/document/video/comic extraction are explicitly out of scope here — tracked as follow-up issues once this pattern is proven, per the design doc.

See `docs/superpowers/specs/2026-08-06-audio-metadata-extraction-design.md` for the full design and the decisions behind it.

Relates to #44 (does not close it — this is the audio pilot only).

## Test plan
- [x] `cargo test --workspace` — all green
- [x] `cargo fmt --all` / `cargo clippy --workspace --all-targets -- -D warnings`
- [x] Unit tests: `AudioTags::into_subtype_metadata`, `LoftyAudioMetadataReader` against a generated tagged WAV fixture, `IndexHandler` against `FakeAudioMetadataReader` (tagged/untagged/non-audio cases)
- [x] HTTP/FFI parity test: index the same tagged WAV through both surfaces, assert extracted metadata matches

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 3: Wait for CI, then merge**

Run: `gh pr checks <PR number> --watch`
Expected: all checks pass.

```bash
gh pr merge <PR number> --squash --delete-branch
```

- [ ] **Step 4: Sync `main` and confirm clean tree**

```bash
git switch main
git pull --ff-only
git status --short
```

Expected: no output from `git status --short` (clean tree), `main` at the
new merge commit.
