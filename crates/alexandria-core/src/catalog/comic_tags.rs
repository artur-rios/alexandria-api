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
            let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                page_count += 1;
            }
        }

        let (title, series, issue_number) = match comic_info_index {
            Some(i) => {
                let mut entry = archive.by_index(i).ok()?;
                let mut xml = String::new();
                std::io::Read::read_to_string(&mut entry, &mut xml).ok()?;
                parse_comic_info(&xml)
            }
            None => (None, None, None),
        };

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
                current_tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().into_owned();
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

impl ComicMetadataReader for CbzComicMetadataReader {
    async fn read(&self, path: &str) -> Option<ComicTags> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".cbz") {
            Self::read_cbz(path)
        } else {
            None
        }
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
}
