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

/// The outcome of trying to read an audio file's embedded front-cover
/// picture. Not a plain `Option`: the two ways a caller ends up with no
/// picture bytes need different handling downstream. A file that parses
/// fine but simply carries no picture is "not supported for this file" —
/// `InvalidInput`, the same 400 the SVG and `.cbr` rejections already use.
/// A file that could not be opened, stat'd, or parsed as audio at all —
/// missing on disk despite being marked present, corrupt, or a format
/// `lofty` does not support (`.wma`) — is a property of the file on disk,
/// the same `Disk` classification `video_tags` and the video thumbnail arm
/// already give a source that will not decode. Collapsing the two into one
/// `None`, as the first version of this port did, would have told an owner
/// whose file has actually gone missing that it "has no cover art" —
/// technically true and actively misleading about where the real problem
/// is (caught in review of issue #117).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverArtRead {
    /// The embedded front-cover picture (or, failing that, the tag's first
    /// picture — see [`LoftyCoverArtReader::parse_capped`]), raw and
    /// still-encoded (JPEG or PNG, whatever the tag itself carries).
    Found(Vec<u8>),
    /// The file parsed; its tag carries no picture, or none that fits
    /// under the read cap.
    NoPicture,
    /// The file could not be opened, stat'd, or parsed as audio at all.
    Unreadable,
}

/// Read-only port over an audio file's embedded front-cover picture
/// (issue #117). A sibling of [`AudioMetadataReader`] rather than an
/// extension of it: the two are read at different times for different
/// reasons — tags are extracted once, at first index, to prefill editable
/// metadata (FR-FC-25); a cover picture is read fresh on every uncached
/// thumbnail request (UC-40, FR-MP-05), because it is not a field an owner
/// edits and there is nothing to prefill. Injected into `ThumbnailHandler`
/// the way its other collaborators are, so "no picture", "unreadable",
/// "wrong type", and the auth/state checks that run before it is even
/// consulted stay unit-testable against a fake with no file I/O (Testing
/// Specification §6.2); the real implementation is `lofty`-backed and
/// wired in `services.rs`, beside [`LoftyAudioMetadataReader`].
#[allow(async_fn_in_trait)]
pub trait CoverArtReader: Send + Sync {
    /// Best-effort read of the embedded front-cover picture. Extraction
    /// failure is never a panic; see [`CoverArtRead`] for how its two
    /// non-picture outcomes are told apart and why that distinction exists.
    async fn read(&self, path: &str) -> CoverArtRead;
}

/// Real cover-art reader backed by `lofty`, covering the same formats
/// [`LoftyAudioMetadataReader`] does.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyCoverArtReader;

impl LoftyCoverArtReader {
    /// The synchronous probe, adapted to [`crate::catalog::read_blocking`]'s
    /// `Option`-returning contract: that function's own `None` means "the
    /// blocking task panicked," never a value this probe itself produces,
    /// so it always wraps its [`CoverArtRead`] in `Some`. `read` unwraps
    /// that outer layer and maps a genuine panic to
    /// [`CoverArtRead::Unreadable`] — a task that panicked parsing one file
    /// could not extract a picture from it either way.
    fn parse(path: &str) -> Option<CoverArtRead> {
        Some(Self::parse_capped(
            path,
            crate::playback::MAX_PLAYBACK_READ_BYTES,
        ))
    }

    /// `parse`'s body, with the cap broken out as a parameter so a test can
    /// drive the over-cap branch against a fixture of a few bytes rather
    /// than allocating a picture past the real, 256 MiB
    /// `MAX_PLAYBACK_READ_BYTES` — the same reason `playback::read_capped`
    /// takes its cap as a parameter instead of reading the constant itself.
    ///
    /// Bounds the *file*, not just the picture it hands back, and bounds it
    /// before `lofty` ever parses a byte of it. An earlier version of this
    /// function checked only the extracted picture's length after
    /// `Probe::read()` had already returned — but `lofty`'s ID3v2 frame
    /// reader materializes a frame's *declared* size into an owned buffer
    /// before it has read that many bytes back off disk
    /// (`try_vec![0; size]`, sized straight from the frame header, ahead of
    /// the `read_exact` that would actually fail on a lie), for every
    /// picture the tag carries, not only the one this function selects. A
    /// check that runs after that call has already paid the allocation it
    /// meant to refuse (caught in review of issue #117). `lofty` 0.25's
    /// `ParseOptions` has no per-frame or per-picture size limit to ask for
    /// instead — `read_cover_art` is a bool, not a bound — so the file's
    /// own size on disk is the best available bound, not a complete one: a
    /// real embedded picture cannot be larger than the file that contains
    /// it, which closes the realistic version of the design's named risk
    /// ("a file claiming a multi-gigabyte cover"), but a maliciously
    /// crafted *small* file could still declare a frame size up to the
    /// format's own ~4 GiB ceiling and cost a transient allocation of that
    /// order before `read_exact` fails on it. The post-parse check below
    /// stays as a second bound for exactly that residual window, even
    /// though for any file that already passed the size check here, the
    /// picture it yields — being a subset of that file's own bytes — can
    /// never itself exceed the same cap.
    fn parse_capped(path: &str, cap: u64) -> CoverArtRead {
        use lofty::file::TaggedFileExt;
        use lofty::picture::PictureType;
        use lofty::probe::Probe;

        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > cap => {
                tracing::warn!(
                    path,
                    size = meta.len(),
                    cap,
                    "audio file exceeds the playback read cap; refusing before parsing"
                );
                return CoverArtRead::Unreadable;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(path, error = %err, "could not stat audio file for cover art");
                return CoverArtRead::Unreadable;
            }
        }

        let tagged_file = match Probe::open(path).and_then(|probe| probe.read()) {
            Ok(f) => f,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not parse audio file for cover art");
                return CoverArtRead::Unreadable;
            }
        };

        let Some(tag) = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())
        else {
            return CoverArtRead::NoPicture;
        };

        // The picture the tag itself calls the front cover is what a sleeve
        // wants; where it carries pictures but names none of them the front
        // cover (a lone icon or back-cover-only release), the first is a
        // better answer than nothing — the design's own call, matching
        // "cover art" loosely rather than refusing a file that clearly has
        // some artwork embedded.
        let pictures = tag.pictures();
        let Some(picture) = pictures
            .iter()
            .find(|p| p.pic_type() == PictureType::CoverFront)
            .or_else(|| pictures.first())
        else {
            return CoverArtRead::NoPicture;
        };

        let data = picture.data();

        if Self::exceeds_cap(data.len(), cap) {
            tracing::warn!(
                path,
                size = data.len(),
                cap,
                "embedded cover art exceeds the playback read cap; refusing"
            );
            return CoverArtRead::NoPicture;
        }

        CoverArtRead::Found(data.to_vec())
    }

    /// The picture-length comparison the post-parse check above runs, split
    /// out so a unit test can drive its boundary directly rather than
    /// through a real file — the file-size precheck makes this branch
    /// unreachable via `parse_capped` for any real fixture (see that
    /// function's own doc comment), so this is what stays testable of it.
    fn exceeds_cap(len: usize, cap: u64) -> bool {
        len as u64 > cap
    }
}

impl CoverArtReader for LoftyCoverArtReader {
    async fn read(&self, path: &str) -> CoverArtRead {
        crate::catalog::read_blocking(path, Self::parse)
            .await
            .unwrap_or(CoverArtRead::Unreadable)
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

    /// A tiny, real, decodable JPEG, one distinct picture per `seed`. Local
    /// to this module rather than reused from the HTTP test suite's helper
    /// of the same name — this crate has no dependency on that one, and its
    /// `&str` seed is not needed here, where a `u8` is enough to keep two
    /// pictures in the same test apart.
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
        write_pictures(path, &[(pic_type, data)]);
    }

    /// Write a fresh ID3v2 tag carrying every `(pic_type, data)` pair onto
    /// `path`, in the order given — so a test can prove selection picks the
    /// `CoverFront` one specifically, rather than merely "a" picture,
    /// regardless of where in the tag it lands.
    fn write_pictures(path: &std::path::Path, pictures: &[(lofty::picture::PictureType, Vec<u8>)]) {
        use lofty::config::WriteOptions;
        use lofty::picture::{MimeType, Picture};
        use lofty::tag::{Tag, TagExt, TagType};

        let mut tag = Tag::new(TagType::Id3v2);
        for (pic_type, data) in pictures {
            let picture = Picture::unchecked(data.clone())
                .pic_type(*pic_type)
                .mime_type(MimeType::Jpeg)
                .build();
            tag.push_picture(picture);
        }
        tag.save_to_path(path, WriteOptions::default())
            .expect("save tag with pictures");
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
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert_eq!(outcome, CoverArtRead::Found(cover));
    }

    #[tokio::test]
    async fn given_front_cover_after_another_picture_when_read_then_front_cover_wins() {
        // Arrange — a back cover written first, a front cover second. With
        // only one picture in the tag (every other fixture in this module),
        // `find(CoverFront).or_else(pictures.first)` and a plain
        // `pictures.first()` are indistinguishable; this is the one test
        // that actually exercises the `find` branch rather than only its
        // fallback.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("front_after_back.wav");
        write_minimal_wav(&path);
        let back = jpeg_bytes_for(33);
        let front = jpeg_bytes_for(44);
        write_pictures(
            &path,
            &[
                (lofty::picture::PictureType::CoverBack, back),
                (lofty::picture::PictureType::CoverFront, front.clone()),
            ],
        );

        // Act
        let reader = LoftyCoverArtReader;
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert — the front cover, not the back cover written first.
        assert_eq!(outcome, CoverArtRead::Found(front));
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
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert_eq!(outcome, CoverArtRead::Found(back));
    }

    #[tokio::test]
    async fn given_untagged_wav_when_cover_read_then_no_picture() {
        // Arrange — no tag written at all.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("no_picture.wav");
        write_minimal_wav(&path);

        // Act
        let reader = LoftyCoverArtReader;
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert_eq!(outcome, CoverArtRead::NoPicture);
    }

    #[tokio::test]
    async fn given_tag_with_no_pictures_when_read_then_no_picture() {
        // Arrange — a tag exists (title set) but carries no picture at all,
        // proving "tag present, no picture" is told apart from "no tag".
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("titled_no_picture.wav");
        write_minimal_wav(&path);
        write_test_tags(&path);

        // Act
        let reader = LoftyCoverArtReader;
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert_eq!(outcome, CoverArtRead::NoPicture);
    }

    #[tokio::test]
    async fn given_missing_file_when_cover_read_then_unreadable_not_panic() {
        // Arrange — a file that cannot be stat'd at all: "no picture" would
        // be misleading here (the file itself is the problem), so this must
        // land on `Unreadable`, not `NoPicture` (issue #117 review).
        let reader = LoftyCoverArtReader;

        let outcome = reader.read("/no/such/file.wav").await;

        assert_eq!(outcome, CoverArtRead::Unreadable);
    }

    #[tokio::test]
    async fn given_garbage_bytes_when_cover_read_then_unreadable_not_panic() {
        // Arrange — a file that exists and is small enough to pass the size
        // check, but is not a RIFF/WAVE container (or anything else `lofty`
        // recognizes) at all. Distinct from the missing-file case above: that
        // one fails at `std::fs::metadata`/`Probe::open`'s file-open step,
        // this one fails inside `Probe::read`'s actual parse.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("garbage.wav");
        std::fs::write(&path, b"not a real audio file, just filler bytes").expect("write garbage");

        // Act
        let reader = LoftyCoverArtReader;
        let outcome = reader.read(path.to_str().unwrap()).await;

        // Assert
        assert_eq!(outcome, CoverArtRead::Unreadable);
    }

    #[tokio::test]
    async fn given_file_larger_than_cap_when_parsed_then_unreadable_before_parsing() {
        // Arrange — a real, validly tagged file, rejected purely on its own
        // size before `Probe::read` ever runs. Proves the precheck bounds the
        // *file*, independent of whether it would otherwise parse cleanly —
        // the fix for the read that used to run unbounded (issue #117
        // review): a file bigger than `cap` must never reach `Probe::read`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("too_big.wav");
        write_minimal_wav(&path);
        write_picture(
            &path,
            lofty::picture::PictureType::CoverFront,
            jpeg_bytes_for(55),
        );
        let actual_size = std::fs::metadata(&path).expect("stat fixture").len();

        // Act — a cap one byte under the fixture's real size.
        let outcome = LoftyCoverArtReader::parse_capped(path.to_str().unwrap(), actual_size - 1);

        // Assert
        assert_eq!(outcome, CoverArtRead::Unreadable);
    }

    #[tokio::test]
    async fn given_file_exactly_at_cap_when_parsed_then_parsed_normally() {
        // Arrange — the same fixture, capped at exactly its own size. The
        // precheck must not reject a file that merely equals the cap.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("at_cap.wav");
        write_minimal_wav(&path);
        let cover = jpeg_bytes_for(66);
        write_picture(
            &path,
            lofty::picture::PictureType::CoverFront,
            cover.clone(),
        );
        let actual_size = std::fs::metadata(&path).expect("stat fixture").len();

        // Act
        let outcome = LoftyCoverArtReader::parse_capped(path.to_str().unwrap(), actual_size);

        // Assert
        assert_eq!(outcome, CoverArtRead::Found(cover));
    }

    #[test]
    fn given_picture_length_over_cap_then_exceeds_cap_true() {
        // The post-parse picture-length check itself, isolated from the
        // file-size precheck that makes it unreachable via `parse_capped`
        // for any real fixture (see that function's own doc comment) — this
        // is the only way left to drive its boundary directly.
        assert!(LoftyCoverArtReader::exceeds_cap(17, 16));
    }

    #[test]
    fn given_picture_length_at_cap_then_exceeds_cap_false() {
        assert!(!LoftyCoverArtReader::exceeds_cap(16, 16));
    }
}
