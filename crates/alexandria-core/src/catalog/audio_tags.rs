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
}
