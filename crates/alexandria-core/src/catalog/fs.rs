use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::errors::DomainError;

/// Run a blocking filesystem operation on Tokio's blocking pool.
///
/// Every `StdFilesystem` method underneath is `std::fs`, which parks the
/// calling thread. Called directly from an `async fn` that would block a
/// runtime worker — and with the indexer now running several files at once
/// (`IndexHandler`'s `concurrency`), blocking workers is what would stop the
/// server answering reads during a scan (FR-FC-08). `spawn_blocking` moves
/// the work to the blocking pool instead, which is also what turns the
/// indexer's concurrency into real parallel hashing rather than interleaved
/// waiting.
///
/// A panic inside the closure surfaces as `Internal` — it is a bug in this
/// crate, not a disk condition, and must not be mistaken for one.
async fn blocking<T, F>(operation: F) -> Result<T, DomainError>
where
    F: FnOnce() -> Result<T, DomainError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(err) => Err(DomainError::internal(format!(
            "filesystem task failed to run: {err}"
        ))),
    }
}

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

/// One file's change signal (FR-FC-10). `None` from `stat` means the file is
/// not there at all, which is UC-02 AF-01's "marked missing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size_bytes: i64,
    pub modified_at: Option<DateTime<Utc>>,
}

/// Filesystem port — the indexer's view of the on-disk store. The real
/// implementation walks the tree and streams bytes through SHA-256; unit
/// tests substitute an in-memory fake returning canned entries and hashes
/// (Testing Specification §6.2 — no real filesystem in unit tests).
#[allow(async_fn_in_trait)]
pub trait Filesystem: Send + Sync {
    async fn path_exists(&self, root: &str) -> bool;
    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError>;
    async fn content_hash(&self, path: &str) -> Result<String, DomainError>;
    /// The file's size and modification time, or `None` when it is gone
    /// (UC-02 AF-01). One `stat` syscall — this is what replaced reading and
    /// hashing every byte to answer "did this change?" (Task 4). Also used
    /// by UC-33's post-write refresh (`EditTextFileContentHandler`) to record
    /// the new size/mtime alongside the hash it just verified, so the very
    /// next re-index sees them match and does not clobber a hash it stored.
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, DomainError>;
    /// Rename `from` to `to` on disk (UC-05 / FR-FC-19). Atomic on a single
    /// volume; fails with `Disk` when the source is missing, the parent
    /// directory is not writable, or the target already exists (the latter is
    /// OS-dependent, so callers also pre-check it). Used by the rename
    /// handler, which leaves the catalog untouched if this fails (AF-02).
    async fn rename(&self, from: &str, to: &str) -> Result<(), DomainError>;
    /// Delete the file at `path` (UC-09 / FR-FC-23). `Ok(true)` — the file was
    /// present and is now gone. `Ok(false)` — no file was there (AF-01); the
    /// caller still removes the record and reports the absence. `Err(Disk)` —
    /// the delete failed (permission denied, AF-02); nothing was removed.
    async fn remove_file(&self, path: &str) -> Result<bool, DomainError>;
    /// Read the file at `path` as UTF-8 text (UC-32 / FR-TX-01). Fails with
    /// `Disk` when the file is missing, unreadable (permission), or its
    /// bytes are not valid UTF-8 (AF-02).
    async fn read_file(&self, path: &str) -> Result<String, DomainError>;
    /// Write `content` to `path`, replacing its bytes (UC-33 / FR-TX-02).
    /// Fails with `Disk` when the write cannot complete (disk full,
    /// permission denied — AF-02); the caller is responsible for leaving
    /// the catalog untouched when this fails.
    async fn write_file(&self, path: &str, content: &str) -> Result<(), DomainError>;
}

/// SHA-256 of `bytes`, lowercase hex (UC-01/UC-02/UC-33). Extracted so
/// `StdFilesystem::content_hash` and the UC-33 handler's pre-write hash
/// computation share one implementation and can never silently diverge —
/// UC-33 AF-03 relies on comparing this exact output against a post-write
/// `content_hash` read back from disk.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finish_hex(hasher)
}

/// Render a finished SHA-256 as lowercase hex, byte by byte. digest 0.11
/// returns an `Array` that no longer implements `LowerHex`, so `{:x}` is
/// unavailable — but the output must stay identical to what earlier versions
/// produced, because these hashes are persisted and compared on every
/// re-index.
fn finish_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Hash a file by streaming it through SHA-256 in fixed-size chunks.
///
/// Blocking — callers go through [`blocking`]. Streaming rather than
/// `fs::read` matters because the catalog hashes whatever the library holds,
/// including multi-gigabyte video files, and the indexer now hashes several
/// of them at once: buffering each one whole would multiply the largest file
/// in the library by the configured concurrency. The digest is identical to
/// `sha256_hex` over the same bytes — chunking changes nothing about SHA-256's
/// output, which the persisted hashes depend on.
fn hash_file_blocking(path: &str) -> Result<String, DomainError> {
    // A failed open/read is a disk error, not an internal one — the same
    // classification `read_file`/`write_file`/`remove_file` use, so a
    // caller cannot tell an unhashable file from an unreadable one by
    // error variant (UC-01 AF-04, UC-02 AF-03, UC-33 AF-02).
    let file =
        std::fs::File::open(path).map_err(|e| DomainError::disk(format!("read {path:?}: {e}")))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| DomainError::disk(format!("read {path:?}: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(finish_hex(hasher))
}

/// Real on-disk filesystem backed by `walkdir` and `sha2`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFilesystem;

impl StdFilesystem {
    fn collect(root: &Path) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            let name = entry
                .file_name()
                .to_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
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
        }
        entries
    }
}

// Every method below hands its `std::fs` work to `blocking`, so no filesystem
// call ever parks a runtime worker. Each closure needs owned arguments
// (`spawn_blocking` requires `'static`), which is why the `&str` parameters are
// copied into `String`s first — a per-call allocation that is immaterial next
// to the syscall it precedes.
impl Filesystem for StdFilesystem {
    async fn path_exists(&self, root: &str) -> bool {
        let root = root.to_string();
        // A stat that cannot run at all is reported as "does not exist",
        // preserving the trait's infallible signature; the callers that need
        // to distinguish the two (UC-01 `start`) get their real error from the
        // subsequent `list_files`.
        blocking(move || Ok(Path::new(&root).exists()))
            .await
            .unwrap_or(false)
    }

    async fn list_files(&self, root: &str) -> Result<Vec<FileEntry>, DomainError> {
        let root = root.to_string();
        blocking(move || {
            let root = PathBuf::from(root);
            if !root.exists() {
                return Err(DomainError::InvalidInput("root path does not exist".into()));
            }
            Ok(Self::collect(&root))
        })
        .await
    }

    async fn content_hash(&self, path: &str) -> Result<String, DomainError> {
        let path = path.to_string();
        blocking(move || hash_file_blocking(&path)).await
    }

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

    async fn rename(&self, from: &str, to: &str) -> Result<(), DomainError> {
        let (from, to) = (from.to_string(), to.to_string());
        blocking(move || {
            std::fs::rename(&from, &to)
                .map_err(|e| DomainError::disk(format!("rename {from:?} -> {to:?}: {e}")))
        })
        .await
    }

    async fn remove_file(&self, path: &str) -> Result<bool, DomainError> {
        let path = path.to_string();
        blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(DomainError::disk(format!("remove {path:?}: {e}"))),
        })
        .await
    }

    async fn read_file(&self, path: &str) -> Result<String, DomainError> {
        let path = path.to_string();
        blocking(move || {
            std::fs::read_to_string(&path)
                .map_err(|e| DomainError::disk(format!("read {path:?}: {e}")))
        })
        .await
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), DomainError> {
        let (path, content) = (path.to_string(), content.to_string());
        blocking(move || {
            std::fs::write(&path, content.as_bytes())
                .map_err(|e| DomainError::disk(format!("write {path:?}: {e}")))
        })
        .await
    }
}

impl FileEntry {
    /// Builds an entry with no stat — what a filesystem that could not
    /// report size or modification time would give. Test fakes that need a
    /// real stat call `Filesystem::list_files` after seeding one instead.
    pub fn new(path: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            name: name.into(),
            size_bytes: 0,
            modified_at: None,
        }
    }
}
