# Design: Extract audio metadata during indexing (pilot for issue #44)

**Date:** 2026-08-06
**Status:** Approved, ready for implementation planning
**Tracks:** [Issue #44](https://github.com/artur-rios/alexandria-api/issues/44) — pilot scope only (audio)

## Context

Issue #44 tracks reading embedded type-specific metadata at index time instead
of leaving every subtype field for the owner to enter manually via UC-04
(Edit file metadata). The issue covers five independent file-type families —
audio, image, document, video, comic — each needing its own parsing
dependency and each a separate failure surface.

This design scopes the first slice: **audio only**. Once implemented, the
same pattern (a read-only metadata-reader port, wired into `IndexHandler`,
reused `update_metadata` call) repeats for the remaining four types as
separate follow-up issues rather than one large PR.

## Decisions

1. **Precedence rule: extract once, at first index only.** Extraction runs
   exclusively inside `index_entry` when a file is newly cataloged. Re-index
   / refresh (`refresh.rs`) is untouched and continues to never write
   metadata — matching its current behavior. Owner edits via UC-04 are the
   only thing that can change subtype metadata after first index. This
   avoids any provenance-tracking schema addition and sidesteps the
   "owner-edited vs. extracted" conflict entirely, since extraction never
   runs a second time against an already-cataloged path.
2. **Extraction failure is not a run failure.** A file with no tags, a
   corrupt tag block, or an unparseable container is still indexed
   successfully with an empty subtype row — exactly like today. It is never
   counted in `IndexOutcome::failed` and never logged above `debug`.
3. **Pilot type: audio**, via the `lofty` crate — one dependency covering
   ID3v1/v2 (MP3, WAV), Vorbis comments (FLAC, OGG/OGA, Opus), and MP4
   atoms (M4A, AAC-in-MP4), i.e. all 9 extensions `classify.rs` already
   recognizes as `FileType::Audio`.

## Architecture

### New port: `AudioMetadataReader`

`crates/alexandria-core/src/catalog/audio_tags.rs` (new file):

```rust
pub struct AudioTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub year: Option<i64>,
    pub genre: Option<String>,
    pub track: Option<i64>,
}

impl AudioTags {
    /// `None` when every field is `None` — nothing worth writing.
    pub fn into_subtype_metadata(self) -> Option<SubtypeMetadata> { ... }
}

#[allow(async_fn_in_trait)]
pub trait AudioMetadataReader: Send + Sync {
    /// Best-effort read of embedded tags. `None` covers both "no tags
    /// present" and "couldn't parse this file" — the caller never needs to
    /// tell them apart (decision 2).
    async fn read(&self, path: &str) -> Option<AudioTags>;
}
```

Concrete implementation `LoftyAudioMetadataReader` wraps `lofty::probe::Probe`
+ `TaggedFile::primary_tag()`, mapping `lofty`'s `ItemKey`s
(`TrackTitle`, `TrackArtist`, `AlbumTitle`, `Year`/`RecordingDate`, `Genre`,
`TrackNumber`) onto `AudioTags`. Any `lofty` `Err` or absent tag is logged at
`debug` and mapped to `None` — `lofty` is designed for untrusted input and
returns `Result` rather than panicking, so no additional guarding is needed.

Follows the same trait-port pattern already used for `Filesystem`,
`JwksProvider`, and `LocalCredentialRepository`: real impl for production,
fake impl for unit tests (Testing Specification §6.2).

### `IndexHandler` wiring

`IndexHandler<A, R, F, C>` gains a fifth generic parameter, `M: AudioMetadataReader`,
constructed alongside the existing four in `services.rs`. `index_entry`
changes to call it after `insert_file`, only for `FileType::Audio`:

```rust
async fn index_entry(&self, entry: FileEntry, file_type: FileType, now: DateTime<Utc>) -> Result<bool, DomainError> {
    if self.repo.find_by_path(&entry.path).await?.is_some() {
        return Ok(false);
    }
    let content_hash = self.fs.content_hash(&entry.path).await?;
    let file = self.repo.insert_file(NewFile {
        uuid: Uuid::new_v4(),
        path: entry.path.clone(),
        name: entry.name,
        file_type,
        content_hash,
        indexed_at: now,
    }).await?;

    if file_type == FileType::Audio {
        if let Some(metadata) = self.audio_tags.read(&entry.path).await.and_then(AudioTags::into_subtype_metadata) {
            // Best-effort: a write failure here doesn't fail indexing
            // (decision 2) and isn't counted in `failed`.
            if let Err(err) = self.repo.update_metadata(file.uuid, &metadata).await {
                tracing::warn!(path = %entry.path, error = %err, "indexed but failed to write extracted audio tags");
            }
        }
    }
    Ok(true)
}
```

No repository trait changes, no schema migration, no `NewFile` field
addition. It reuses `CatalogRepository::update_metadata` — the exact method
UC-04's `PATCH /v1/files/{uuid}/metadata` already calls — immediately after
insert, while the subtype row is still all-`NULL`. This is the same "full
replace" semantics UC-04 already has, applied once by the system instead of
by the owner.

`refresh.rs` requires **no changes** — it already never touches subtype
metadata, and decision 1 keeps it that way.

## Error handling / failure isolation

- `AudioMetadataReader::read` never returns `Err`; all failure modes collapse
  to `None` (decision 2).
- The `update_metadata` write failure branch is defensive rather than
  expected — the file row was just inserted in the same call (`NotFound`
  can't happen) and the metadata variant always matches the just-inserted
  `file_type` (the type-mismatch branch can't happen either). Logged at
  `warn`, swallowed, does not fail `index_entry` or increment
  `IndexOutcome::failed`.
- Reading the file twice (once for `content_hash`, once for tag parsing) is
  an accepted minor inefficiency for this pilot — not worth changing
  `Filesystem::content_hash`'s signature to share the byte buffer.

## Testing strategy

1. **Unit tests** — `IndexHandler` against a `FakeAudioMetadataReader`
   (canned `AudioTags` per path), covering: tags present → subtype row
   populated; `None` → subtype row stays empty and `update_metadata` is
   never called; partial tags → only present fields written, rest `NULL`;
   non-audio file → reader never invoked.
2. **`LoftyAudioMetadataReader` unit test** — against a small checked-in
   fixture file (`crates/alexandria-core/tests/fixtures/tagged.mp3`, a few
   KB, known ID3v2 tag values) asserting the real parse round-trips
   correctly. This is the one place real bytes are needed; the fake covers
   the handler's decision logic everywhere else.
3. **HTTP/FFI integration + parity** — index a temp library containing the
   same fixture MP3 through both surfaces, then assert `GET /v1/files/{uuid}`
   (and its FFI equivalent) return the extracted metadata — reusing the
   existing UC-01/UC-03 parity test pattern.

## Out of scope (this pilot)

- Image (EXIF), Document (PDF/EPUB), Video (resolution/`mediaKind`), Comic
  (`ComicInfo.xml`) — tracked as follow-up issues once this pattern is
  proven.
- `.pdf` comic-vs-book classification — already explicitly out of scope per
  issue #44 (stays extension-based, see FR-FC-06).
- Any provenance/re-extraction behavior on refresh — ruled out by decision 1.
