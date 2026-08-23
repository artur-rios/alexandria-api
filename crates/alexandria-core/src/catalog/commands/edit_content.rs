use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::clock::Clock;
use crate::catalog::fs::{sha256_hex, Filesystem};
use crate::catalog::model::{File, FileState, FileType};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-33 — Edit text file content (FR-TX-02, FR-TX-03). Writes new content
/// to a TextFile's on-disk path and refreshes its content hash.
///
/// The handler authenticates the caller (AF-05), looks the file up by its
/// public UUID (AF-04), rejects edits on a soft-deleted record (restore via
/// UC-07 first, mirroring `EditMetadataHandler`), verifies the file is a
/// TextFile (AF-01), and writes the new content to disk before touching the
/// catalog (AF-02 — if the write fails, no catalog change is ever made).
/// After a successful write it recomputes the SHA-256 hash from the bytes
/// actually on disk and compares it against the hash of the submitted
/// content; a mismatch triggers exactly one retry before surfacing an
/// integrity error (AF-03). Only once the hash is confirmed does it stat the
/// file it just wrote and update the catalog's `content_hash`/`size_bytes`/
/// `mtime`/`indexed_at` (FR-TX-03) via `refresh_hash`, then return the
/// refreshed record. This post-write `refresh_hash` is the only writer of
/// `content_hash` left after Task 3: indexing never computes one (FR-FC-09),
/// so a file's `content_hash` is `None` until — and unless — this handler
/// edits it; that is expected, not a gap.
///
/// The write also changes the file's size and mtime, so `refresh_hash`
/// records those too (Task 4 correction) — not just the hash. Re-index (Task
/// 4's `RefreshHandler`) now decides "changed?" purely from stat, never from
/// bytes; if this handler only updated `content_hash` and left the row's
/// stale size/mtime behind, the very next re-index would see a stat
/// mismatch, count this unedited-since file as changed, and null out the
/// hash just verified above. The stat is read via the same `Filesystem::stat`
/// re-index uses (Task 4), rather than introducing a second way to answer
/// "what are this file's stats" that could drift from it.
///
/// Generic over auth, catalog repository, filesystem, and clock so the same
/// decision logic is unit-tested against trait fakes (no real DB, fs, or
/// auth in unit tests), then wired with the concrete collaborators at
/// runtime (services.rs). Both the HTTP and FFI surfaces call this handler
/// so the two stay at parity (FR-FC-24 / NFR-09).
pub struct EditTextFileContentHandler<A, R, F, C> {
    auth: A,
    repo: R,
    fs: F,
    clock: C,
}

impl<A, R, F, C> EditTextFileContentHandler<A, R, F, C>
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

    /// Write `content` to the TextFile identified by `uuid`. Returns the
    /// updated `File` (with its refreshed `content_hash`) on success.
    pub async fn edit(
        &self,
        uuid: Uuid,
        content: String,
        token: &str,
    ) -> Result<File, DomainError> {
        // AF-05: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-04: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // Precondition: restore via UC-07 before editing a deleted record —
        // mirrors `EditMetadataHandler` (UC-04).
        if file.state == FileState::Deleted {
            return Err(DomainError::InvalidState);
        }

        // AF-01: the target file must be a TextFile.
        if file.file_type != FileType::Text {
            return Err(DomainError::InvalidInput(format!(
                "file {uuid} is not a text file"
            )));
        }

        let expected_hash = sha256_hex(content.as_bytes());

        // AF-02: write before touching the catalog. If this fails, no
        // catalog change was ever made.
        self.fs.write_file(&file.path, &content).await?;

        // AF-03: verify the bytes actually on disk hash to what was
        // submitted, re-attempting the write exactly once before giving up.
        let mut actual_hash = self.fs.content_hash(&file.path).await?;
        if actual_hash != expected_hash {
            self.fs.write_file(&file.path, &content).await?;
            actual_hash = self.fs.content_hash(&file.path).await?;
            if actual_hash != expected_hash {
                return Err(DomainError::integrity(format!(
                    "content hash mismatch for {uuid} after retry"
                )));
            }
        }

        // Read back the stat of the bytes just verified above, so the
        // catalog's size/mtime move together with the hash they describe
        // (Task 4 correction — see the doc comment above).
        let stat = self.fs.stat(&file.path).await?.ok_or_else(|| {
            DomainError::disk(format!(
                "{} vanished immediately after being written",
                file.path
            ))
        })?;

        let now = self.clock.now();
        self.repo
            .refresh_hash(
                &file.path,
                &actual_hash,
                stat.size_bytes,
                stat.modified_at,
                now,
            )
            .await?;

        self.repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)
    }
}
