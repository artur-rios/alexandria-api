/// Tags read from an image file's embedded EXIF data (issue #44 image
/// slice). `width`/`height` are written via `CatalogRepository::set_image_dimensions`
/// (they live outside `SubtypeMetadata::Image`, which only covers the
/// owner-editable `title`/`caption`); `title` is written via the existing
/// `update_metadata` when present. `caption` has no EXIF-native tag and is
/// never populated by extraction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageTags {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub title: Option<String>,
}

/// Read-only port over an image file's embedded EXIF data (issue #44 image
/// slice). Generic-parameter-injected into `IndexHandler` so the decision
/// logic is unit-tested against a fake with no real file I/O (Testing
/// Specification §6.2); wired with the real `ExifImageMetadataReader` at
/// runtime (services.rs).
#[allow(async_fn_in_trait)]
pub trait ImageMetadataReader: Send + Sync {
    /// Best-effort read of embedded EXIF data. `None` covers both "no EXIF
    /// present" and "couldn't parse this file" — the caller never needs to
    /// tell them apart; extraction failure is never a run failure.
    async fn read(&self, path: &str) -> Option<ImageTags>;
}

/// Real image-EXIF reader backed by `kamadak-exif`, covering JPEG, TIFF,
/// HEIC, and PNG's `eXIf` chunk — 4 of the 9 extensions
/// `classify_by_extension` maps to `FileType::Image` (jpg/jpeg/tif/tiff, and
/// PNG when it carries an `eXIf` chunk). gif/webp/bmp/svg have no EXIF to
/// extract and always yield no metadata — the same graceful degradation the
/// audio slice established for `.wma`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExifImageMetadataReader;

impl ExifImageMetadataReader {
    /// The synchronous EXIF parse. `read` runs it on the blocking pool — see
    /// [`crate::catalog::read_blocking`].
    fn parse(path: &str) -> Option<ImageTags> {
        let file = std::fs::File::open(path).ok()?;
        let mut bufreader = std::io::BufReader::new(&file);
        let exif = match exif::Reader::new().read_from_container(&mut bufreader) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse image EXIF data");
                return None;
            }
        };

        let width = exif
            .get_field(exif::Tag::PixelXDimension, exif::In::PRIMARY)
            .or_else(|| exif.get_field(exif::Tag::ImageWidth, exif::In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
            .map(i64::from);
        let height = exif
            .get_field(exif::Tag::PixelYDimension, exif::In::PRIMARY)
            .or_else(|| exif.get_field(exif::Tag::ImageLength, exif::In::PRIMARY))
            .and_then(|f| f.value.get_uint(0))
            .map(i64::from);
        let title = exif
            .get_field(exif::Tag::ImageDescription, exif::In::PRIMARY)
            .and_then(|f| match &f.value {
                exif::Value::Ascii(vecs) => vecs
                    .first()
                    .map(|b| String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if width.is_none() && height.is_none() && title.is_none() {
            return None;
        }

        Some(ImageTags {
            width,
            height,
            title,
        })
    }
}

impl ImageMetadataReader for ExifImageMetadataReader {
    async fn read(&self, path: &str) -> Option<ImageTags> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny real JPEG (4x3 pixels, arbitrary solid color) using the
    /// `image` crate — a real, valid JPEG file, not hand-crafted bytes.
    fn write_minimal_jpeg(path: &std::path::Path) {
        let img = image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]));
        img.save(path).expect("encode jpeg");
    }

    /// Write EXIF tags (pixel dimensions + an ImageDescription) into an
    /// existing JPEG using `little_exif`.
    fn write_test_exif(path: &std::path::Path, width: u32, height: u32, description: &str) {
        use little_exif::exif_tag::ExifTag;
        use little_exif::metadata::Metadata;

        let mut metadata = Metadata::new();
        metadata.set_tag(ExifTag::ImageDescription(description.to_string()));
        // little_exif names these `ExifImageWidth`/`ExifImageHeight`, but
        // they write tag IDs 0xa002/0xa003 — the same IDs `kamadak-exif`
        // reads back as `Tag::PixelXDimension`/`Tag::PixelYDimension`.
        metadata.set_tag(ExifTag::ExifImageWidth(vec![width]));
        metadata.set_tag(ExifTag::ExifImageHeight(vec![height]));
        metadata.write_to_file(path).expect("write exif");
    }

    #[tokio::test]
    async fn given_tagged_jpeg_when_read_then_dimensions_and_title_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.jpg");
        write_minimal_jpeg(&path);
        write_test_exif(&path, 64, 48, "Test Description");

        let reader = ExifImageMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.width, Some(64));
        assert_eq!(tags.height, Some(48));
        assert_eq!(tags.title.as_deref(), Some("Test Description"));
    }

    #[tokio::test]
    async fn given_untagged_jpeg_when_read_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("untagged.jpg");
        write_minimal_jpeg(&path);

        let reader = ExifImageMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none(), "no EXIF written, no EXIF read");
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = ExifImageMetadataReader;

        let tags = reader.read("/no/such/file.jpg").await;

        assert!(tags.is_none());
    }
}
