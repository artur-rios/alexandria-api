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

/// The year `lofty::tag::Accessor::year()` used to return.
///
/// lofty 0.25 dropped that convenience: "year" is ambiguous between a
/// dedicated `Year` item, which few formats map, and the year part of a
/// `RecordingDate`, which most of them do. Reimplemented here rather than
/// replaced with a single key read, so the catalog records exactly what it
/// has always recorded — prefer `Year`, fall back to `RecordingDate`, and
/// take the first four leading ASCII digits of whichever answered.
///
/// That last rule is what makes `1983` and `1983-04-01` both read as 1983,
/// tolerates leading whitespace, and makes anything with fewer than four
/// leading digits read as nothing at all rather than as a truncated year.
fn year_of(tag: &lofty::tag::Tag) -> Option<u32> {
    let raw = tag
        .get_string(lofty::tag::ItemKey::Year)
        .or_else(|| tag.get_string(lofty::tag::ItemKey::RecordingDate))?;
    let (digits, year) = raw
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .take(4)
        .fold((0usize, 0u32), |(digits, year), c| {
            (digits + 1, year * 10 + c.to_digit(10).expect("ascii digit"))
        });
    (digits == 4).then_some(year)
}

impl AudioTags {
    /// `None` when every field is `None` — nothing worth writing, so the
    /// caller skips the `update_metadata` call entirely.
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

/// Real audio-tag reader backed by `lofty`, covering ID3v1/v2 (MP3, WAV),
/// Vorbis comments (FLAC, OGG/OGA, Opus), and MP4 atoms (M4A, AAC-in-MP4) —
/// all but one (`.wma`, ASF/Windows Media, unsupported by `lofty`) of the
/// extensions `classify_by_extension` maps to `FileType::Audio`. A `.wma`
/// file simply gets no extracted metadata, the same graceful degradation as
/// any unparseable file.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyAudioMetadataReader;

impl LoftyAudioMetadataReader {
    /// The synchronous probe. `read` runs it on the blocking pool — see
    /// [`crate::catalog::read_blocking`].
    fn parse(path: &str) -> Option<AudioTags> {
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

        let tag = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())?;

        let tags = AudioTags {
            title: tag
                .title()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty()),
            artist: tag
                .artist()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty()),
            album: tag
                .album()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty()),
            year: year_of(tag).map(i64::from),
            genre: tag
                .genre()
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty()),
            track: tag.track().map(i64::from),
        };

        tags.clone()
            .into_subtype_metadata()
            .is_some()
            .then_some(tags)
    }
}

impl AudioMetadataReader for LoftyAudioMetadataReader {
    async fn read(&self, path: &str) -> Option<AudioTags> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

/// Read-only port over an audio file's embedded front-cover picture
/// (issue #117). A sibling of [`AudioMetadataReader`] rather than an
/// extension of it: the two are read at different times for different
/// reasons — tags are extracted once, at first index, to prefill editable
/// metadata (FR-FC-25); a cover picture is read fresh on every uncached
/// thumbnail request (UC-40, FR-MP-05), because it is not a field an owner
/// edits and there is nothing to prefill. Injected into `ThumbnailHandler`
/// the way its other collaborators are, so "no picture", "wrong type", and
/// the auth/state checks that run before it is even consulted stay
/// unit-testable against a fake with no file I/O (Testing Specification
/// §6.2); the real implementation is `lofty`-backed and wired in
/// `services.rs`, beside [`LoftyAudioMetadataReader`].
#[allow(async_fn_in_trait)]
pub trait CoverArtReader: Send + Sync {
    /// Best-effort read of the embedded front-cover picture's raw,
    /// still-encoded bytes (JPEG or PNG, whatever the tag itself carries).
    /// `None` covers "no picture embedded", "the tag has pictures but none
    /// is usable", and "couldn't parse this file" alike — the caller never
    /// needs to tell them apart; extraction failure is never a run failure
    /// or a panic, only ever a "there is nothing to show" the caller maps to
    /// `InvalidInput`.
    async fn read(&self, path: &str) -> Option<Vec<u8>>;
}

/// Real cover-art reader backed by `lofty`, covering the same formats
/// [`LoftyAudioMetadataReader`] does.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyCoverArtReader;

impl LoftyCoverArtReader {
    /// The synchronous probe. `read` runs it on the blocking pool — see
    /// [`crate::catalog::read_blocking`].
    fn parse(path: &str) -> Option<Vec<u8>> {
        Self::parse_capped(path, crate::playback::MAX_PLAYBACK_READ_BYTES)
    }

    /// `parse`'s body, with the cap broken out as a parameter so a test can
    /// drive the over-cap branch against a fixture of a few bytes rather
    /// than allocating a picture past the real, 256 MiB
    /// `MAX_PLAYBACK_READ_BYTES` — the same reason `playback::read_capped`
    /// takes its cap as a parameter instead of reading the constant itself.
    fn parse_capped(path: &str, cap: u64) -> Option<Vec<u8>> {
        use lofty::file::TaggedFileExt;
        use lofty::picture::PictureType;
        use lofty::probe::Probe;

        let tagged_file = match Probe::open(path).and_then(|probe| probe.read()) {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse audio file for cover art");
                return None;
            }
        };

        let tag = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())?;

        // The picture the tag itself calls the front cover is what a sleeve
        // wants; where it carries pictures but names none of them the front
        // cover (a lone icon or back-cover-only release), the first is a
        // better answer than nothing — the design's own call, matching
        // "cover art" loosely rather than refusing a file that clearly has
        // some artwork embedded.
        let pictures = tag.pictures();
        let picture = pictures
            .iter()
            .find(|p| p.pic_type() == PictureType::CoverFront)
            .or_else(|| pictures.first())?;

        let data = picture.data();

        // Nothing in ID3v2, Vorbis comments, or MP4 atoms bounds an embedded
        // picture's size — that is a property of the container format, not
        // of pictures. `Probe::open` reads the audio file sequentially
        // rather than loading it whole, but the picture's own bytes are
        // materialized in full once the tag is parsed, so an oversized one
        // must be refused here rather than handed on: a file claiming a
        // multi-gigabyte "cover" must not cost that much memory before
        // `ImageThumbnailRenderer` ever gets a chance to reject it. Capped
        // at the same `MAX_PLAYBACK_READ_BYTES` the image thumbnail arm
        // bounds its own read by (in production; `parse_capped`'s caller
        // decides which), so both arms share one ceiling for "how large a
        // source image a thumbnail request will decode."
        if data.len() as u64 > cap {
            tracing::warn!(
                path,
                size = data.len(),
                cap,
                "embedded cover art exceeds the playback read cap; refusing"
            );
            return None;
        }

        Some(data.to_vec())
    }
}

impl CoverArtReader for LoftyCoverArtReader {
    async fn read(&self, path: &str) -> Option<Vec<u8>> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        // What `set_year` did for an ID3v2 tag: that format maps no dedicated
        // `Year` item, so the year goes to `RecordingDate`, which is where
        // `year_of` falls back to reading it.
        tag.insert_text(lofty::tag::ItemKey::RecordingDate, "2020".to_string());
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

    /// Write a WAV with a blank (empty-string) title/artist/album/genre but
    /// a non-blank year, so the frames exist but carry no text.
    fn write_blank_string_tags(path: &std::path::Path) {
        use lofty::config::WriteOptions;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};

        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title(String::new());
        tag.set_artist("   ".to_string());
        tag.set_album(String::new());
        tag.set_genre(String::new());
        // What `set_year` did for an ID3v2 tag: that format maps no dedicated
        // `Year` item, so the year goes to `RecordingDate`, which is where
        // `year_of` falls back to reading it.
        tag.insert_text(lofty::tag::ItemKey::RecordingDate, "2021".to_string());
        tag.save_to_path(path, WriteOptions::default())
            .expect("save tag");
    }

    #[tokio::test]
    async fn given_blank_string_tags_when_read_then_string_fields_are_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("blank.wav");
        write_minimal_wav(&path);
        write_blank_string_tags(&path);

        let reader = LoftyAudioMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("year alone is enough to write metadata");

        assert_eq!(tags.title, None, "empty string must not become Some(\"\")");
        assert_eq!(
            tags.artist, None,
            "whitespace-only string must not become Some(\"   \")"
        );
        assert_eq!(tags.album, None);
        assert_eq!(tags.genre, None);
        assert_eq!(tags.year, Some(2021));
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = LoftyAudioMetadataReader;

        let tags = reader.read("/no/such/file.wav").await;

        assert!(tags.is_none());
    }

    /// A tiny, real, decodable JPEG — deterministic per `seed` so a test can
    /// recompute the exact bytes it expects back without threading them
    /// through a second channel. Mirrors `jpeg_bytes_for` in the HTTP test
    /// suite's `common` helpers, kept local here because this module has no
    /// dependency on that crate.
    fn jpeg_bytes_for(seed: u8) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([seed, 100, 200]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut out)
            .encode_image(&image::DynamicImage::ImageRgb8(img))
            .expect("encode jpeg");
        out
    }

    /// Write a fresh ID3v2 tag carrying one picture of `pic_type` onto
    /// `path`, mirroring `write_test_tags`'s shape: a new in-memory `Tag`,
    /// populated, then saved.
    fn write_picture(path: &std::path::Path, pic_type: lofty::picture::PictureType, data: Vec<u8>) {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture};
        use lofty::tag::{Tag, TagExt, TagType};

        let mut tag = Tag::new(TagType::Id3v2);
        let picture = Picture::unchecked(data)
            .pic_type(pic_type)
            .mime_type(MimeType::Jpeg)
            .build();
        tag.push_picture(picture);
        tag.save_to_path(path, WriteOptions::default())
            .expect("save tag with picture");
    }

    #[tokio::test]
    async fn given_front_cover_when_read_then_its_bytes_returned() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cover.wav");
        write_minimal_wav(&path);
        let cover = jpeg_bytes_for(11);
        write_picture(
            &path,
            lofty::picture::PictureType::CoverFront,
            cover.clone(),
        );

        // Act
        let reader = LoftyCoverArtReader;
        let bytes = reader
            .read(path.to_str().unwrap())
            .await
            .expect("picture extracted");

        // Assert
        assert_eq!(bytes, cover);
    }

    #[tokio::test]
    async fn given_only_a_back_cover_when_read_then_it_is_returned_anyway() {
        // Arrange — no `CoverFront` picture exists, but one picture does.
        // The design calls the first picture a better answer than nothing
        // when the tag names no front cover.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("back_only.wav");
        write_minimal_wav(&path);
        let back = jpeg_bytes_for(22);
        write_picture(&path, lofty::picture::PictureType::CoverBack, back.clone());

        // Act
        let reader = LoftyCoverArtReader;
        let bytes = reader
            .read(path.to_str().unwrap())
            .await
            .expect("fallback picture extracted");

        // Assert
        assert_eq!(bytes, back);
    }

    #[tokio::test]
    async fn given_untagged_wav_when_cover_read_then_none() {
        // Arrange — no tag written at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("no_picture.wav");
        write_minimal_wav(&path);

        // Act
        let reader = LoftyCoverArtReader;
        let bytes = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert!(bytes.is_none());
    }

    #[tokio::test]
    async fn given_tag_with_no_pictures_when_read_then_none() {
        // Arrange — a tag exists (title set) but carries no picture at all,
        // proving "tag present, no picture" is told apart from "no tag".
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("titled_no_picture.wav");
        write_minimal_wav(&path);
        write_test_tags(&path);

        // Act
        let reader = LoftyCoverArtReader;
        let bytes = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert!(bytes.is_none());
    }

    #[tokio::test]
    async fn given_missing_file_when_cover_read_then_none_not_panic() {
        let reader = LoftyCoverArtReader;

        let bytes = reader.read("/no/such/file.wav").await;

        assert!(bytes.is_none());
    }

    #[tokio::test]
    async fn given_picture_over_cap_when_parsed_then_none() {
        // Arrange — a real 16-byte picture against an 8-byte cap. `cap` is a
        // parameter to `parse_capped` precisely so this fixture stays tiny;
        // the request path calls `parse`, which passes the real 256 MiB
        // `MAX_PLAYBACK_READ_BYTES`. Reaching this branch at all proves the
        // rejection runs before the bytes are cloned out to the caller.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("over_cap.wav");
        write_minimal_wav(&path);
        write_picture(
            &path,
            lofty::picture::PictureType::CoverFront,
            vec![0xFFu8; 16],
        );

        // Act
        let bytes = LoftyCoverArtReader::parse_capped(path.to_str().unwrap(), 8);

        // Assert
        assert!(bytes.is_none(), "a picture over cap must be refused");
    }

    #[tokio::test]
    async fn given_picture_exactly_at_cap_when_parsed_then_returned() {
        // Arrange — a picture the same size as the cap is legal; the extra
        // byte `read_capped`'s sibling logic allows must not turn a
        // right-at-the-line picture into a rejection.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("at_cap.wav");
        write_minimal_wav(&path);
        write_picture(
            &path,
            lofty::picture::PictureType::CoverFront,
            vec![0xFFu8; 16],
        );

        // Act
        let bytes = LoftyCoverArtReader::parse_capped(path.to_str().unwrap(), 16);

        // Assert
        assert_eq!(bytes, Some(vec![0xFFu8; 16]));
    }
}
