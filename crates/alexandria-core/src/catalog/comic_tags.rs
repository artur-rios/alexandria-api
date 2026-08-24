/// Tags read from a comic archive's embedded metadata (`ComicInfo.xml`,
/// the de-facto ComicRack/ComicVine standard, plus an archive-entry image
/// count — issue #44 comic slice). `page_count` is always computed by
/// counting image entries in the archive, independent of whether
/// `ComicInfo.xml` exists at all; `title`/`series`/`issue_number` come
/// only from `ComicInfo.xml` and are `None` when it's absent or
/// unparseable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComicTags {
    pub title: Option<String>,
    pub series: Option<String>,
    pub issue_number: Option<i64>,
    pub page_count: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait ComicMetadataReader: Send + Sync {
    /// Best-effort read of embedded comic metadata. `None` covers only
    /// "couldn't open the archive at all" — a readable archive with no
    /// `ComicInfo.xml` still yields `Some` with `page_count` set and the
    /// other three fields `None`.
    async fn read(&self, path: &str) -> Option<ComicTags>;
}

/// Real comic reader covering `.cbz` (ZIP-based) archives — 1 of the 2
/// extensions `classify_by_extension` maps to `FileType::Comic`. `.cbr`
/// (RAR, proprietary, no viable pure-Rust reader) always yields `None` —
/// the same graceful degradation the document slice established for
/// `.mobi`/`.azw`/`.azw3`. `title`/`series`/`issue_number` come from a
/// `ComicInfo.xml` entry when present (matched case-insensitively);
/// `page_count` is always the count of image-extension entries in the
/// archive, regardless of whether `ComicInfo.xml` exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct CbzComicMetadataReader;

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp"];

/// Does this archive entry count as a comic page?
///
/// The single source of truth for that question, shared by
/// `read_cbz`'s page count (FR-FC-25) and UC-39's page index (FR-MP-04).
/// If these two ever disagreed, "page 7" and `comicPageCount` would be
/// describing different things.
pub fn is_page_entry(entry_name: &str) -> bool {
    if entry_name.eq_ignore_ascii_case("ComicInfo.xml") {
        return false;
    }

    // macOS's Archive Utility writes one AppleDouble sidecar per file — a
    // few KB of resource fork and Finder metadata, stored under
    // `__MACOSX/` with the original name prefixed `._`. They carry the
    // page's extension, so without this a CBZ zipped on a Mac counts and
    // serves twice its real page count, and `._page001.jpg` sorts *before*
    // `page001.jpg` (`_` is 0x5F, `p` is 0x70) — so page 1 and the
    // thumbnail both become an undecodable blob labelled `image/jpeg`.
    let mut components = entry_name.split(['/', '\\']);
    if components.any(|part| part.eq_ignore_ascii_case("__MACOSX")) {
        return false;
    }
    let basename = entry_name.rsplit(['/', '\\']).next().unwrap_or(entry_name);
    if basename.starts_with("._") {
        return false;
    }

    let ext = entry_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
}

impl CbzComicMetadataReader {
    fn read_cbz(path: &str) -> Option<ComicTags> {
        let file = std::fs::File::open(path).ok()?;
        let mut archive = zip::ZipArchive::new(file).ok()?;

        let mut page_count = 0i64;
        let mut comic_info_index: Option<usize> = None;
        for i in 0..archive.len() {
            let entry = archive.by_index(i).ok()?;
            let name = entry.name().to_string();
            if name.eq_ignore_ascii_case("ComicInfo.xml") {
                comic_info_index = Some(i);
                continue;
            }
            if is_page_entry(&name) {
                page_count += 1;
            }
        }

        let (title, series, issue_number) = comic_info_index
            .and_then(|i| {
                let mut entry = archive.by_index(i).ok()?;
                let mut xml = String::new();
                std::io::Read::read_to_string(&mut entry, &mut xml).ok()?;
                Some(parse_comic_info(&xml))
            })
            .unwrap_or((None, None, None));

        Some(ComicTags {
            title,
            series,
            issue_number,
            page_count: Some(page_count),
        })
    }
}

/// Parse `<Title>`/`<Series>`/`<Number>` out of a `ComicInfo.xml` document.
/// Malformed XML or a missing element collapses that field to `None`
/// rather than erroring — the caller already treats every field as
/// best-effort.
fn parse_comic_info(xml: &str) -> (Option<String>, Option<String>, Option<i64>) {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut title = None;
    let mut series = None;
    let mut issue_number = None;
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                // quick-xml 0.42 decodes names to `str` on the way out, so
                // there are no longer bytes here to convert lossily.
                current_tag = e.name().as_ref().to_owned();
            }
            Ok(Event::Text(e)) => {
                // 0.42 also split what `BytesText::unescape` used to do in one
                // step: the event carries the decoded content with line
                // endings normalised, and resolving `&amp;` and friends is now
                // the free function in `quick_xml::escape`. An unresolvable
                // entity still collapses that field to nothing, exactly as
                // `unwrap_or_default` did before — every field here is
                // best-effort.
                let content = e.xml10_content();
                let text = quick_xml::escape::unescape(content.as_ref())
                    .unwrap_or_default()
                    .into_owned();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                match current_tag.as_str() {
                    "Title" => title = Some(text.to_string()),
                    "Series" => series = Some(text.to_string()),
                    "Number" => issue_number = text.parse::<i64>().ok(),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    (title, series, issue_number)
}

impl CbzComicMetadataReader {
    /// The synchronous archive read. `read` runs it on the blocking pool —
    /// see [`crate::catalog::read_blocking`].
    fn parse(path: &str) -> Option<ComicTags> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".cbz") {
            Self::read_cbz(path)
        } else {
            None
        }
    }
}

impl ComicMetadataReader for CbzComicMetadataReader {
    async fn read(&self, path: &str) -> Option<ComicTags> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid CBZ with the `zip` crate itself: an optional
    /// `ComicInfo.xml` entry plus `page_count` dummy `.jpg` entries (each
    /// just a few bytes — the reader only counts entries by extension, it
    /// never decodes image data). This is a real, valid ZIP archive — not
    /// hand-crafted bytes.
    fn write_minimal_cbz(
        path: &std::path::Path,
        comic_info: Option<(&str, &str, &str)>,
        page_count: usize,
    ) {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let file = std::fs::File::create(path).expect("create cbz file");
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();

        if let Some((title, series, number)) = comic_info {
            zip.start_file("ComicInfo.xml", options)
                .expect("start ComicInfo.xml");
            let xml = format!(
                r#"<?xml version="1.0"?>
<ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>{title}</Title>
  <Series>{series}</Series>
  <Number>{number}</Number>
</ComicInfo>"#
            );
            zip.write_all(xml.as_bytes()).expect("write ComicInfo.xml");
        }

        for i in 0..page_count {
            zip.start_file(format!("page-{i:03}.jpg"), options)
                .expect("start page");
            zip.write_all(b"not-a-real-jpeg-just-bytes")
                .expect("write page");
        }

        zip.finish().expect("finish cbz zip");
    }

    #[tokio::test]
    async fn given_tagged_cbz_when_read_then_title_series_issue_and_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.cbz");
        write_minimal_cbz(&path, Some(("Test Title", "Test Series", "3")), 5);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.series.as_deref(), Some("Test Series"));
        assert_eq!(tags.issue_number, Some(3));
        assert_eq!(tags.page_count, Some(5));
    }

    #[tokio::test]
    async fn given_cbz_without_comicinfo_when_read_then_only_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("untagged.cbz");
        write_minimal_cbz(&path, None, 7);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title, None);
        assert_eq!(tags.series, None);
        assert_eq!(tags.issue_number, None);
        assert_eq!(
            tags.page_count,
            Some(7),
            "page_count must be computed even with no ComicInfo.xml"
        );
    }

    #[tokio::test]
    async fn given_cbz_with_non_integer_issue_number_when_read_then_issue_number_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("annual.cbz");
        write_minimal_cbz(&path, Some(("Annual Special", "Test Series", "Annual")), 1);

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Annual Special"));
        assert_eq!(
            tags.issue_number, None,
            "a non-integer <Number> must not error, just leave issue_number None"
        );
    }

    #[tokio::test]
    async fn given_cbz_with_unreadable_comicinfo_when_read_then_page_count_survives() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad-comicinfo.cbz");

        let file = std::fs::File::create(&path).expect("create cbz file");
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();

        // ComicInfo.xml entry present but containing invalid UTF-8 bytes,
        // so `read_to_string` fails on it even though the archive itself
        // opens fine.
        zip.start_file("ComicInfo.xml", options)
            .expect("start ComicInfo.xml");
        zip.write_all(&[0xFF, 0xFE, 0x00, 0x41, 0x00, 0x42])
            .expect("write invalid utf-8 ComicInfo.xml");

        for i in 0..4 {
            zip.start_file(format!("page-{i:03}.jpg"), options)
                .expect("start page");
            zip.write_all(b"not-a-real-jpeg-just-bytes")
                .expect("write page");
        }

        zip.finish().expect("finish cbz zip");

        let reader = CbzComicMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("archive opens fine, so page_count must still be returned");

        assert_eq!(tags.title, None);
        assert_eq!(tags.series, None);
        assert_eq!(tags.issue_number, None);
        assert_eq!(
            tags.page_count,
            Some(4),
            "page_count must survive even when ComicInfo.xml fails to read"
        );
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = CbzComicMetadataReader;

        let tags = reader.read("/no/such/file.cbz").await;

        assert!(tags.is_none());
    }

    #[tokio::test]
    async fn given_unsupported_extension_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.cbr");
        std::fs::write(&path, b"not a real cbr file").expect("write stub");

        let reader = CbzComicMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), ".cbr is not attempted at all");
    }

    #[test]
    fn given_archive_entry_names_when_tested_then_only_images_are_pages() {
        // Arrange / Act / Assert — the metadata file is not a page, nor is a
        // non-image sidecar; every image extension the catalog recognizes is.
        assert!(!is_page_entry("ComicInfo.xml"));
        assert!(!is_page_entry("comicinfo.xml"));
        assert!(!is_page_entry("notes.txt"));
        assert!(is_page_entry("page001.jpg"));
        assert!(is_page_entry("page002.JPEG"));
        assert!(is_page_entry("sub/dir/page003.png"));
        assert!(is_page_entry("page004.webp"));
    }

    #[test]
    fn given_appledouble_sidecars_when_tested_then_not_pages() {
        // Arrange / Act / Assert — a CBZ zipped on macOS carries one
        // AppleDouble sidecar per page, under `__MACOSX/` and named `._` +
        // the original. Both forms must be excluded, or a 20-page comic
        // reports 40 pages and `pages/1` serves a metadata blob (`._` sorts
        // before the real name) labelled `image/jpeg`.
        assert!(!is_page_entry("__MACOSX/._page001.jpg"));
        assert!(!is_page_entry("__MACOSX/sub/._page002.png"));
        assert!(!is_page_entry("._page003.jpg"));
        assert!(!is_page_entry("sub/dir/._page004.png"));
        // The real pages beside them are still pages.
        assert!(is_page_entry("page001.jpg"));
        assert!(is_page_entry("sub/page002.png"));
    }
}
