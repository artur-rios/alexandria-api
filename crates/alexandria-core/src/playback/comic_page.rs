//! UC-39 — Read a comic book page (FR-MP-04).

use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::comic_tags::is_page_entry;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::playback::mime::mime_for_path;
use crate::playback::{resolve_playable, MAX_PLAYBACK_READ_BYTES};

/// One page of a comic archive. `bytes` are the archive entry's own bytes,
/// undecoded and unmodified (FR-MP-03) — a CBZ page is already a JPEG or
/// PNG, so there is nothing to convert.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComicPage {
    pub uuid: Uuid,
    pub page: u32,
    pub page_count: u32,
    pub mime_type: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// Comic archive port. Unit tests substitute a fake; the real
/// implementation reads a ZIP.
#[allow(async_fn_in_trait)]
pub trait ComicArchive: Send + Sync {
    /// Names of the archive entries that count as pages, in whatever order
    /// the archive stores them. The handler sorts.
    async fn page_names(&self, path: &str) -> Result<Vec<String>, DomainError>;
    /// The raw bytes of one entry.
    async fn read_entry(&self, path: &str, entry: &str) -> Result<Vec<u8>, DomainError>;
}

/// Real `ComicArchive`, reading CBZ (ZIP) on the blocking pool.
#[derive(Clone, Copy)]
pub struct ZipComicArchive;

impl ComicArchive for ZipComicArchive {
    async fn page_names(&self, path: &str) -> Result<Vec<String>, DomainError> {
        let owned = path.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&owned)
                .map_err(|e| DomainError::disk(format!("cannot open {owned}: {e}")))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| DomainError::disk(format!("cannot read {owned}: {e}")))?;
            let mut names = Vec::new();
            for i in 0..archive.len() {
                let entry = archive
                    .by_index(i)
                    .map_err(|e| DomainError::disk(format!("cannot read entry {i}: {e}")))?;
                let name = entry.name().to_string();
                if is_page_entry(&name) {
                    names.push(name);
                }
            }
            Ok(names)
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!("archive task failed: {err}"))),
        }
    }

    async fn read_entry(&self, path: &str, entry: &str) -> Result<Vec<u8>, DomainError> {
        let owned = path.to_string();
        let wanted = entry.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&owned)
                .map_err(|e| DomainError::disk(format!("cannot open {owned}: {e}")))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| DomainError::disk(format!("cannot read {owned}: {e}")))?;
            let zip_entry = archive
                .by_name(&wanted)
                .map_err(|e| DomainError::disk(format!("cannot read entry {wanted}: {e}")))?;
            read_entry_capped(zip_entry, MAX_PLAYBACK_READ_BYTES)
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!("archive task failed: {err}"))),
        }
    }
}

/// Read one archive entry, refusing anything that decompresses past `cap`.
///
/// A ZIP entry's decompressed size is whatever the archive claims it is, and
/// the bytes keep coming whatever the header said: `read_to_end` on a 900 KB
/// CBZ whose entry inflates to 40 GB allocates until the process dies. `take`
/// bounds the reader itself, so the allocation can never exceed the cap
/// regardless of what the entry declares.
///
/// `cap + 1` bytes are read so a file *at* the cap is still served and a file
/// past it is detected rather than truncated — a short JPEG decodes into a
/// garbage page, which is worse than an error. Over-cap is `InvalidInput`,
/// consistent with every other "this file is not something we can work with"
/// rejection on this path, and the message names no entry because
/// `InvalidInput` messages reach the client.
///
/// `cap` is a parameter, not a read of [`MAX_PLAYBACK_READ_BYTES`], so a test
/// can drive the over-cap branch against a fixture of a few hundred bytes.
pub(crate) fn read_entry_capped<R: std::io::Read>(
    reader: R,
    cap: u64,
) -> Result<Vec<u8>, DomainError> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader.take(cap + 1), &mut bytes)
        .map_err(|e| DomainError::disk(format!("cannot read comic entry: {e}")))?;

    if bytes.len() as u64 > cap {
        return Err(DomainError::InvalidInput(format!(
            "comic page is larger than the {cap}-byte playback read limit"
        )));
    }

    Ok(bytes)
}

/// Which archive entry is page `page` (1-based) of the comic at `path`, and
/// how many pages the comic has.
///
/// The single implementation of "what is page N of a comic", shared by
/// UC-39's page route and UC-40's comic thumbnail — the design puts the
/// thumbnail on the UC-39 path precisely so the two can never disagree. When
/// this lived in both handlers, they had already drifted: only UC-39 guarded
/// CBR, so a `.cbr` thumbnail reached `ZipComicArchive`, failed to open, and
/// became a 500 where the error table promises a 400.
///
/// Three rules, in order:
///
/// 1. CBR is RAR: proprietary, no viable pure-Rust reader. The same
///    graceful-degradation line `comic_tags.rs` already draws — except here
///    the caller asked for something specific, so it is told the format is
///    unsupported rather than silently getting nothing.
/// 2. Archive-storage order is not page order — nothing obliges a writer to
///    store entries in sequence. Sort case-insensitively by name, which is
///    what comic readers conventionally do and what the zero-padded
///    filenames CBZ archives use are designed for.
/// 3. Pages are 1-based, so 0 and `count + 1` are both out of range. A comic
///    with no page entries at all has every page out of range, including
///    page 1.
///
/// Every rejection is `InvalidInput`: each describes the request or the
/// file's format, never a fault in reading it.
pub async fn select_page<C: ComicArchive>(
    archive: &C,
    uuid: Uuid,
    path: &str,
    page: u32,
) -> Result<(String, u32), DomainError> {
    if !path.to_ascii_lowercase().ends_with(".cbz") {
        return Err(DomainError::InvalidInput(format!(
            "comic {uuid} is not a CBZ archive; page extraction supports CBZ only"
        )));
    }

    let mut names = archive.page_names(path).await?;
    names.sort_by_key(|name| name.to_ascii_lowercase());

    let page_count = names.len() as u32;
    if page == 0 || page > page_count {
        return Err(DomainError::InvalidInput(format!(
            "page {page} is out of range; comic {uuid} has {page_count} pages"
        )));
    }

    Ok((names.swap_remove((page - 1) as usize), page_count))
}

/// UC-39 — return page `page` (1-based) of a CBZ ComicBook.
pub struct ComicPageHandler<A, R, C> {
    auth: A,
    repo: R,
    archive: C,
}

impl<A, R, C> ComicPageHandler<A, R, C>
where
    A: AuthService,
    R: CatalogRepository,
    C: ComicArchive,
{
    pub fn new(auth: A, repo: R, archive: C) -> Self {
        Self {
            auth,
            repo,
            archive,
        }
    }

    pub async fn read_page(
        &self,
        uuid: Uuid,
        page: u32,
        token: &str,
    ) -> Result<ComicPage, DomainError> {
        let file = resolve_playable(&self.auth, &self.repo, uuid, token).await?;

        if file.file_type != FileType::Comic {
            return Err(DomainError::InvalidInput(format!(
                "file {uuid} is not a comic book"
            )));
        }

        let (entry, page_count) = select_page(&self.archive, uuid, &file.path, page).await?;
        let bytes = self.archive.read_entry(&file.path, &entry).await?;

        Ok(ComicPage {
            uuid: file.uuid,
            page,
            page_count,
            mime_type: mime_for_path(&entry).to_string(),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::FileState;
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};

    /// Archive fake: entries deliberately supplied out of order, to prove
    /// the handler sorts rather than trusting archive order.
    #[derive(Clone)]
    struct FakeArchive;

    impl ComicArchive for FakeArchive {
        async fn page_names(&self, _path: &str) -> Result<Vec<String>, DomainError> {
            Ok(vec![
                "page003.jpg".to_string(),
                "page001.jpg".to_string(),
                "page002.png".to_string(),
            ])
        }

        async fn read_entry(&self, _path: &str, entry: &str) -> Result<Vec<u8>, DomainError> {
            Ok(entry.as_bytes().to_vec())
        }
    }

    #[tokio::test]
    async fn given_cbz_when_page_requested_then_pages_are_lexicographically_ordered() {
        // Arrange — archive order is 3, 1, 2; page order must be 1, 2, 3.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbz",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let handler = ComicPageHandler::new(FakeAuth { good: "t" }, repo, FakeArchive);

        // Act
        let page = handler
            .read_page(Uuid::nil(), 1, "t")
            .await
            .expect("page 1");

        // Assert
        assert_eq!(page.bytes, b"page001.jpg".to_vec());
        assert_eq!(page.mime_type, "image/jpeg");
        assert_eq!(page.page_count, 3);
    }

    #[tokio::test]
    async fn given_cbz_when_last_page_requested_then_its_own_mime_returned() {
        // Arrange — page 2 is a PNG; the MIME comes from the entry, not the
        // archive.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbz",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let handler = ComicPageHandler::new(FakeAuth { good: "t" }, repo, FakeArchive);

        // Act
        let page = handler
            .read_page(Uuid::nil(), 2, "t")
            .await
            .expect("page 2");

        // Assert
        assert_eq!(page.mime_type, "image/png");
    }

    #[tokio::test]
    async fn given_page_index_out_of_range_when_requested_then_invalid_input() {
        // Arrange — 1-based indexing: 0 and count+1 are both out of range.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbz",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let handler = ComicPageHandler::new(FakeAuth { good: "t" }, repo, FakeArchive);

        // Act
        let zero = handler.read_page(Uuid::nil(), 0, "t").await;
        let past_end = handler.read_page(Uuid::nil(), 4, "t").await;

        // Assert
        assert!(matches!(zero, Err(DomainError::InvalidInput(_))));
        assert!(matches!(past_end, Err(DomainError::InvalidInput(_))));
    }

    /// A real one-entry ZIP in memory, so the cap test reads through the
    /// `zip` crate's own decompressing reader rather than a stand-in.
    fn zip_with_entry(name: &str, bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("start entry");
        writer.write_all(bytes).expect("write entry");
        writer.finish().expect("finish zip").into_inner()
    }

    #[test]
    fn given_entry_larger_than_cap_when_read_then_invalid_input() {
        // Arrange — a zip bomb in miniature: 4096 highly compressible bytes
        // stored in a far smaller archive, read against a 64-byte cap. `cap`
        // is a parameter so the fixture stays small; the request path passes
        // `MAX_PLAYBACK_READ_BYTES`.
        let archive_bytes = zip_with_entry("page001.jpg", &vec![0u8; 4096]);
        assert!(archive_bytes.len() < 4096, "entry must inflate on read");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(archive_bytes)).expect("open zip");
        let entry = archive.by_index(0).expect("entry");

        // Act
        let result = read_entry_capped(entry, 64);

        // Assert — an error, not 64 truncated bytes labelled `image/jpeg`.
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[test]
    fn given_entry_within_cap_when_read_then_bytes_returned() {
        // Arrange — the cap must not reject a legitimate page.
        let archive_bytes = zip_with_entry("page001.jpg", b"tiny page");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(archive_bytes)).expect("open zip");
        let entry = archive.by_index(0).expect("entry");

        // Act
        let bytes = read_entry_capped(entry, 64);

        // Assert
        assert_eq!(bytes.expect("read"), b"tiny page".to_vec());
    }

    #[tokio::test]
    async fn given_non_comic_file_when_page_requested_then_invalid_input() {
        // Arrange
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let handler = ComicPageHandler::new(FakeAuth { good: "t" }, repo, FakeArchive);

        // Act
        let result = handler.read_page(Uuid::nil(), 1, "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_cbr_comic_when_page_requested_then_invalid_input() {
        // Arrange — RAR has no viable pure-Rust reader, the same precedent
        // `comic_tags.rs` set. The file exists and is genuinely a comic, so
        // this is an unsupported *format*, not a missing record.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbr",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let handler = ComicPageHandler::new(FakeAuth { good: "t" }, repo, FakeArchive);

        // Act
        let result = handler.read_page(Uuid::nil(), 1, "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }
}
