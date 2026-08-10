use crate::catalog::model::FormatKind;

/// Tags read from a document file's embedded metadata (PDF `/Info`
/// dictionary or EPUB OPF metadata — issue #44 document slice).
/// `format_kind` is set unconditionally whenever either format was
/// identified at all — `Book` for PDF, `Ebook` for EPUB — independent of
/// whether `title`/`author`/`year` were found. `page_count` only ever comes
/// from PDF; EPUB (reflowable text, no fixed pages) always leaves it
/// `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentTags {
    pub title: Option<String>,
    pub author: Option<String>,
    pub year: Option<i64>,
    pub format_kind: Option<FormatKind>,
    pub page_count: Option<i64>,
}

/// Read-only port over a document file's embedded metadata (issue #44
/// document slice). Generic-parameter-injected into `IndexHandler` so the
/// decision logic is unit-tested against a fake with no real file I/O
/// (Testing Specification §6.2); wired with the real `PdfEpubMetadataReader`
/// at runtime (services.rs).
#[allow(async_fn_in_trait)]
pub trait DocumentMetadataReader: Send + Sync {
    /// Best-effort read of embedded document metadata. `None` covers
    /// "unsupported extension (mobi/azw/azw3)", "no metadata present", and
    /// "couldn't parse this file" alike — the caller never needs to tell
    /// them apart; extraction failure is never a run failure.
    async fn read(&self, path: &str) -> Option<DocumentTags>;
}

/// Real document reader covering PDF (via `lopdf`) and EPUB (via the
/// `epub` crate) — 2 of the 5 extensions `classify_by_extension` maps to
/// `FileType::Document`. `.mobi`/`.azw`/`.azw3` (proprietary Amazon Kindle
/// formats) have no workable pure-Rust library and always yield `None` —
/// the same graceful degradation the audio and image slices established
/// for `.wma` and non-EXIF image formats.
#[derive(Debug, Default, Clone, Copy)]
pub struct PdfEpubMetadataReader;

impl PdfEpubMetadataReader {
    fn read_pdf(path: &str) -> Option<DocumentTags> {
        let doc = match lopdf::Document::load(path) {
            Ok(d) => d,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse pdf");
                return None;
            }
        };

        let page_count = doc.get_pages().len() as i64;

        let info_dict = doc
            .trailer
            .get(b"Info")
            .ok()
            .and_then(|obj| obj.as_reference().ok())
            .and_then(|id| doc.get_dictionary(id).ok());

        let title = info_dict
            .and_then(|d| d.get(b"Title").ok())
            .and_then(|v| v.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let author = info_dict
            .and_then(|d| d.get(b"Author").ok())
            .and_then(|v| v.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Some(DocumentTags {
            title,
            author,
            year: None,
            format_kind: Some(FormatKind::Book),
            page_count: Some(page_count),
        })
    }

    fn read_epub(path: &str) -> Option<DocumentTags> {
        let doc = match epub::doc::EpubDoc::new(path) {
            Ok(d) => d,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse epub");
                return None;
            }
        };

        let title = doc
            .mdata("title")
            .map(|item| item.value.trim().to_string())
            .filter(|s| !s.is_empty());
        let author = doc
            .mdata("creator")
            .map(|item| item.value.trim().to_string())
            .filter(|s| !s.is_empty());

        Some(DocumentTags {
            title,
            author,
            year: None,
            format_kind: Some(FormatKind::Ebook),
            page_count: None,
        })
    }
}

impl PdfEpubMetadataReader {
    /// The synchronous parse. `read` runs it on the blocking pool — see
    /// [`crate::catalog::read_blocking`].
    fn parse(path: &str) -> Option<DocumentTags> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".pdf") {
            Self::read_pdf(path)
        } else if lower.ends_with(".epub") {
            Self::read_epub(path)
        } else {
            None
        }
    }
}

impl DocumentMetadataReader for PdfEpubMetadataReader {
    async fn read(&self, path: &str) -> Option<DocumentTags> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid PDF with `lopdf` itself: a catalog, a single
    /// empty page, and an `/Info` dictionary carrying Title/Author. This
    /// is a real, valid PDF file — not hand-crafted bytes.
    fn write_minimal_pdf(path: &std::path::Path, title: &str, author: &str) {
        use lopdf::{dictionary, Document, Object};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = lopdf::content::Content { operations: vec![] };
        let content_id = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            content.encode().expect("encode content"),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(title),
            "Author" => Object::string_literal(author),
        });
        doc.trailer.set("Info", info_id);

        doc.save(path).expect("save pdf");
    }

    /// Build a minimal valid EPUB by hand-constructing the zip structure
    /// (mimetype, container.xml, a content.opf with Dublin Core metadata,
    /// one empty chapter, and a stub NCX) using the `zip` crate. `epub` is
    /// read-only, so this is the fixture-generation path — a real zip with
    /// real EPUB structure, not a checked-in binary.
    fn write_minimal_epub(path: &std::path::Path, title: &str, author: &str) {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let file = std::fs::File::create(path).expect("create epub file");
        let mut zip = zip::ZipWriter::new(file);

        zip.start_file(
            "mimetype",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .expect("start mimetype");
        zip.write_all(b"application/epub+zip")
            .expect("write mimetype");

        zip.start_file("META-INF/container.xml", SimpleFileOptions::default())
            .expect("start container.xml");
        zip.write_all(
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .expect("write container.xml");

        zip.start_file("OEBPS/content.opf", SimpleFileOptions::default())
            .expect("start content.opf");
        let opf = format!(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="BookId" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>{title}</dc:title>
    <dc:creator>{author}</dc:creator>
    <dc:identifier id="BookId">urn:uuid:test-fixture</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="chap1" href="chap1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="chap1"/>
  </spine>
</package>"#
        );
        zip.write_all(opf.as_bytes()).expect("write content.opf");

        zip.start_file("OEBPS/chap1.xhtml", SimpleFileOptions::default())
            .expect("start chap1.xhtml");
        zip.write_all(b"<html><body><p>Test</p></body></html>")
            .expect("write chap1.xhtml");

        zip.start_file("OEBPS/toc.ncx", SimpleFileOptions::default())
            .expect("start toc.ncx");
        zip.write_all(
            br#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head></head>
  <docTitle><text>Test</text></docTitle>
  <navMap></navMap>
</ncx>"#,
        )
        .expect("write toc.ncx");

        zip.finish().expect("finish epub zip");
    }

    #[tokio::test]
    async fn given_tagged_pdf_when_read_then_title_author_and_page_count_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.pdf");
        write_minimal_pdf(&path, "Test Title", "Test Author");

        let reader = PdfEpubMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.author.as_deref(), Some("Test Author"));
        assert_eq!(tags.page_count, Some(1));
        assert_eq!(tags.format_kind, Some(FormatKind::Book));
    }

    #[tokio::test]
    async fn given_tagged_epub_when_read_then_title_and_author_extracted_no_page_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.epub");
        write_minimal_epub(&path, "Test Title", "Test Author");

        let reader = PdfEpubMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.author.as_deref(), Some("Test Author"));
        assert_eq!(tags.page_count, None, "EPUB never sets page_count");
        assert_eq!(tags.format_kind, Some(FormatKind::Ebook));
    }

    #[tokio::test]
    async fn given_unsupported_extension_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("book.mobi");
        std::fs::write(&path, b"not a real mobi file").expect("write stub");

        let reader = PdfEpubMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), ".mobi is not attempted at all");
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = PdfEpubMetadataReader;

        let tags = reader.read("/no/such/file.pdf").await;

        assert!(tags.is_none());
    }
}
