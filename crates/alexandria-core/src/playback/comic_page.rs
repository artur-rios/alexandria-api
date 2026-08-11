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

/// One page as the archive resolved it: the entry's bytes, the entry's name
/// (the only thing the MIME table needs), and how many pages the archive
/// holds. Everything a caller can learn from a single open, returned from a
/// single open.
#[derive(Debug, Clone)]
pub struct ArchivePage {
    pub entry: String,
    pub bytes: Vec<u8>,
    pub page_count: u32,
}

/// Comic archive port. Unit tests substitute a fake; the real implementation
/// reads a ZIP.
///
/// One method, not the `page_names` + `read_entry` pair it replaced. That
/// pair made a single page request open and parse the archive twice — a
/// reader paging through a 200-page comic paid 400 central-directory parses —
/// and the two calls could observe different archive states. It was also
/// wrong for a CBZ holding two entries with the same name, which ZIP permits:
/// `page_names` counted both while `by_name` returned the first for either
/// index, so page 2 silently served page 1's bytes.
///
/// No policy moves into the port: implementations resolve the page number by
/// calling [`select_page`], the same pure function the fakes call, so the
/// ordering and bounds rules stay in one directly unit-testable place.
#[allow(async_fn_in_trait)]
pub trait ComicArchive: Send + Sync {
    /// Page `page` (1-based) of the archive at `path`, from one open.
    async fn read_page(&self, path: &str, page: u32) -> Result<ArchivePage, DomainError>;
}

/// Real `ComicArchive`, reading CBZ (ZIP) on the blocking pool.
#[derive(Clone, Copy)]
pub struct ZipComicArchive;

impl ComicArchive for ZipComicArchive {
    /// One `File::open`, one central-directory parse, one `spawn_blocking`.
    ///
    /// Entries are selected by *index*, never by name: names are not unique
    /// in a ZIP, and `by_name` would collapse duplicates onto the first of
    /// them. [`select_page`] returns the position it chose within the list it
    /// was given, and that list is built alongside the archive indices it
    /// came from, so the entry finally read is the exact one counted.
    async fn read_page(&self, path: &str, page: u32) -> Result<ArchivePage, DomainError> {
        let owned = path.to_string();
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&owned)
                .map_err(|e| DomainError::disk(format!("cannot open {owned}: {e}")))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|e| DomainError::disk(format!("cannot read {owned}: {e}")))?;

            let mut names = Vec::new();
            let mut indices = Vec::new();
            for i in 0..archive.len() {
                let entry = archive
                    .by_index(i)
                    .map_err(|e| DomainError::disk(format!("cannot read entry {i}: {e}")))?;
                let name = entry.name().to_string();
                if is_page_entry(&name) {
                    names.push(name);
                    indices.push(i);
                }
            }

            let (position, page_count) = select_page(&names, page)?;
            let entry_name = names[position].clone();
            let zip_entry = archive
                .by_index(indices[position])
                .map_err(|e| DomainError::disk(format!("cannot read entry {entry_name}: {e}")))?;
            let bytes = read_entry_capped(zip_entry, MAX_PLAYBACK_READ_BYTES)?;

            Ok(ArchivePage {
                entry: entry_name,
                bytes,
                page_count,
            })
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

/// Reject anything that is not a CBZ, before any archive is touched.
///
/// CBR is RAR: proprietary, with no viable pure-Rust reader. The same
/// graceful-degradation line `comic_tags.rs` already draws — except here the
/// caller asked for something specific, so it is told the format is
/// unsupported rather than silently getting nothing.
///
/// Shared by UC-39's page route and UC-40's comic thumbnail, and called by
/// both *handlers* rather than by the archive port: a `.cbr` must never reach
/// `ZipComicArchive`, where a failure to open would surface as a `Disk` error
/// and a 500 where the error table promises a 400. The two had already
/// drifted apart once on exactly this point.
pub fn ensure_cbz(uuid: Uuid, path: &str) -> Result<(), DomainError> {
    if !path.to_ascii_lowercase().ends_with(".cbz") {
        return Err(DomainError::InvalidInput(format!(
            "comic {uuid} is not a CBZ archive; page extraction supports CBZ only"
        )));
    }
    Ok(())
}

/// Which of `names` is page `page` (1-based), and how many pages there are.
///
/// The single implementation of "what is page N of a comic": every
/// `ComicArchive` — the real ZIP reader and every test fake — resolves the
/// page number through this one function, so the ordering rule cannot drift
/// between them and stays directly unit-testable without a filesystem. UC-39's
/// page route and UC-40's comic thumbnail therefore always agree on what page
/// 1 is, which is the whole reason the thumbnail sits on the UC-39 path.
///
/// Two rules:
///
/// 1. Archive-storage order is not page order — nothing obliges a writer to
///    store entries in sequence. Sort case-insensitively by name, which is
///    what comic readers conventionally do and what the zero-padded filenames
///    CBZ archives use are designed for. The sort is *stable*, so entries
///    sharing a name (legal in ZIP) keep their archive order and stay
///    distinct pages.
/// 2. Pages are 1-based, so 0 and `count + 1` are both out of range. A comic
///    with no page entries at all has every page out of range, including
///    page 1.
///
/// Returns an index into `names` rather than the name itself: a name does not
/// identify an entry in an archive that permits duplicates, and the caller
/// holds the archive index that matches each position.
///
/// The out-of-range message names neither the comic nor its path. It is
/// `InvalidInput` — it describes the request, not a fault in reading the file
/// — and `InvalidInput` messages are rendered into the client's error
/// envelope, where a library path has no business appearing.
pub fn select_page(names: &[String], page: u32) -> Result<(usize, u32), DomainError> {
    let page_count = names.len() as u32;
    if page == 0 || page > page_count {
        return Err(DomainError::InvalidInput(format!(
            "page {page} is out of range; the comic has {page_count} pages"
        )));
    }

    let mut order: Vec<usize> = (0..names.len()).collect();
    order.sort_by_key(|&i| names[i].to_ascii_lowercase());

    Ok((order[(page - 1) as usize], page_count))
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

        // Before the archive is opened, so a `.cbr` is a 400 and never a ZIP
        // reader failure.
        ensure_cbz(uuid, &file.path)?;

        // One call, one open: ordering, bounds and bytes all come from the
        // same view of the archive.
        let page_data = self.archive.read_page(&file.path, page).await?;

        Ok(ComicPage {
            uuid: file.uuid,
            page,
            page_count: page_data.page_count,
            mime_type: mime_for_path(&page_data.entry).to_string(),
            bytes: page_data.bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::FileState;
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};

    /// Archive fake: entries deliberately supplied out of order, to prove
    /// the ordering rule is applied rather than archive order trusted. It
    /// resolves the page number through the real `select_page`, exactly as
    /// `ZipComicArchive` does, and echoes the chosen entry's name as its
    /// bytes so a test can see which entry was picked.
    #[derive(Clone)]
    struct FakeArchive;

    impl ComicArchive for FakeArchive {
        async fn read_page(&self, _path: &str, page: u32) -> Result<ArchivePage, DomainError> {
            let names = vec![
                "page003.jpg".to_string(),
                "page001.jpg".to_string(),
                "page002.png".to_string(),
            ];
            let (position, page_count) = select_page(&names, page)?;
            Ok(ArchivePage {
                bytes: names[position].as_bytes().to_vec(),
                entry: names[position].clone(),
                page_count,
            })
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

    #[test]
    fn given_duplicate_entry_names_when_selected_then_each_page_is_a_distinct_entry() {
        // Arrange — ZIP permits two entries with the same name. The port used
        // to count both while `by_name` returned the first for either index,
        // so page 2 silently served page 1's bytes. Selection now returns a
        // *position*, which the archive maps to its own entry index, and the
        // sort is stable, so duplicates stay two distinct pages in archive
        // order. (No real CBZ fixture here: the `zip` crate's writer rejects
        // duplicate filenames outright with `InvalidArchive("Duplicate
        // filename")`, so such an archive cannot be built with it.)
        let names = vec![
            "page001.jpg".to_string(),
            "page001.jpg".to_string(),
            "page000.jpg".to_string(),
        ];

        // Act
        let first = select_page(&names, 1).expect("page 1");
        let second = select_page(&names, 2).expect("page 2");
        let third = select_page(&names, 3).expect("page 3");

        // Assert — three pages, three different entries, and the two
        // duplicates keep their archive order behind the entry that sorts
        // first.
        assert_eq!((first.1, second.1, third.1), (3, 3, 3));
        assert_eq!((first.0, second.0, third.0), (2, 0, 1));
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
