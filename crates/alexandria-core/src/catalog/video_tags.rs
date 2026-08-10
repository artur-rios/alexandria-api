/// Tags read from a video file's embedded metadata (container-level
/// duration and format metadata dictionary — issue #44 video slice).
/// `resolution` is formatted `"{width}x{height}"` from the best video
/// stream's dimensions (e.g. `"1920x1080"`). There is no `media_kind`
/// field — movie-vs-series is not inferable from the file itself, so
/// extraction never sets it; the field stays owner-only via UC-04.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTags {
    pub title: Option<String>,
    pub year: Option<i64>,
    pub resolution: Option<String>,
    pub duration_seconds: Option<VideoDuration>,
}

/// Wraps an `f64` so `VideoTags` can derive `PartialEq`/`Eq` (raw `f64`
/// implements neither). Holds a duration in fractional seconds. `Eq` is
/// sound here because a duration read from a real file is always a
/// finite, non-NaN value — this type is not used for arbitrary float
/// arithmetic, only for carrying and comparing an extracted duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoDuration(pub f64);

impl Eq for VideoDuration {}

#[allow(async_fn_in_trait)]
pub trait VideoMetadataReader: Send + Sync {
    /// Best-effort read of embedded video metadata. `None` covers
    /// "couldn't open the container", "no video stream", and "no metadata
    /// present" alike — the caller never needs to tell them apart.
    async fn read(&self, path: &str) -> Option<VideoTags>;
}

/// Real video reader covering every extension `classify_by_extension`
/// maps to `FileType::Video` (mp4, m4v, mkv, avi, mov, webm, mpg, mpeg,
/// wmv, flv) via `ffmpeg-next` — unlike every prior slice, no extension
/// subset is left unextracted; ffmpeg's container/codec coverage is broad
/// enough that one reader handles all ten.
#[derive(Debug, Default, Clone, Copy)]
pub struct FfmpegVideoMetadataReader;

impl FfmpegVideoMetadataReader {
    /// The synchronous container probe. `read` runs it on the blocking pool —
    /// see [`crate::catalog::read_blocking`]. This is the slowest of the five
    /// readers: ffmpeg may read a long way into a file to find the best video
    /// stream, so it is the one that most needs to stay off the runtime.
    fn parse(path: &str) -> Option<VideoTags> {
        if ffmpeg_next::init().is_err() {
            return None;
        }

        let ictx = match ffmpeg_next::format::input(path) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::debug!(path, error = %err, "could not open video container");
                return None;
            }
        };

        let stream = ictx.streams().best(ffmpeg_next::media::Type::Video)?;

        let params = stream.parameters();
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(params).ok()?;
        let decoder = codec_ctx.decoder().video().ok()?;
        let (width, height) = (decoder.width(), decoder.height());
        let resolution = if width > 0 && height > 0 {
            Some(format!("{width}x{height}"))
        } else {
            None
        };

        let duration_seconds = {
            let duration = ictx.duration();
            if duration > 0 {
                Some(VideoDuration(
                    duration as f64 / f64::from(ffmpeg_next::ffi::AV_TIME_BASE),
                ))
            } else {
                None
            }
        };

        let metadata = ictx.metadata();
        let title = metadata
            .get("title")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let year = metadata
            .get("date")
            .and_then(|s| s.get(..4))
            .and_then(|y| y.parse::<i64>().ok());

        Some(VideoTags {
            title,
            year,
            resolution,
            duration_seconds,
        })
    }
}

impl VideoMetadataReader for FfmpegVideoMetadataReader {
    async fn read(&self, path: &str) -> Option<VideoTags> {
        crate::catalog::read_blocking(path, Self::parse).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid MP4 with `ffmpeg-next` itself: a few frames of
    /// a tiny raw video stream, encoded and muxed to a real file, with a
    /// `title`/`date` tag set on the output format context. This is a
    /// real, playable (if trivial) video file — not hand-crafted bytes.
    fn write_minimal_mp4(path: &std::path::Path, title: &str, width: u32, height: u32) {
        ffmpeg_next::init().expect("ffmpeg init");

        let mut octx = ffmpeg_next::format::output(path).expect("create output context");
        octx.set_metadata({
            let mut dict = ffmpeg_next::Dictionary::new();
            dict.set("title", title);
            dict.set("date", "2024-01-01T00:00:00Z");
            dict
        });

        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::MPEG4)
            .expect("mpeg4 encoder available");
        let mut ost = octx.add_stream(codec).expect("add video stream");
        let mut encoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .expect("video encoder context");
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        encoder.set_time_base(ffmpeg_next::Rational(1, 25));
        let mut encoder = encoder.open().expect("open encoder");
        ost.set_parameters(&encoder);

        octx.write_header().expect("write header");

        let mut frame =
            ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
        for plane in 0..frame.planes() {
            frame.data_mut(plane).fill(16);
        }

        // 10 frames at 25fps = 0.4s of video, plenty for a duration/
        // resolution/title extraction test.
        for i in 0..10 {
            frame.set_pts(Some(i));
            encoder.send_frame(&frame).expect("send frame");
            let mut packet = ffmpeg_next::Packet::empty();
            while encoder.receive_packet(&mut packet).is_ok() {
                packet.set_stream(0);
                packet.write_interleaved(&mut octx).expect("write packet");
            }
        }
        encoder.send_eof().expect("send eof");
        let mut packet = ffmpeg_next::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
        octx.write_trailer().expect("write trailer");
    }

    #[tokio::test]
    async fn given_tagged_mp4_when_read_then_title_year_resolution_and_duration_extracted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tagged.mp4");
        write_minimal_mp4(&path, "Test Title", 320, 240);

        let reader = FfmpegVideoMetadataReader;
        let tags = reader
            .read(path.to_str().unwrap())
            .await
            .expect("tags extracted");

        assert_eq!(tags.title.as_deref(), Some("Test Title"));
        assert_eq!(tags.year, Some(2024));
        assert_eq!(tags.resolution.as_deref(), Some("320x240"));
        assert!(
            tags.duration_seconds.is_some(),
            "a real encoded video must report a non-None duration"
        );
        let VideoDuration(seconds) = tags.duration_seconds.unwrap();
        assert!(
            seconds > 0.0,
            "10 frames at 25fps must report a positive duration, got {seconds}"
        );
    }

    #[tokio::test]
    async fn given_missing_file_when_read_then_none_not_panic() {
        let reader = FfmpegVideoMetadataReader;

        let tags = reader.read("/no/such/file.mp4").await;

        assert!(tags.is_none());
    }

    #[tokio::test]
    async fn given_non_video_file_when_read_then_none_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-video.txt");
        std::fs::write(&path, b"just some text, not a container at all").expect("write stub");

        let reader = FfmpegVideoMetadataReader;
        let tags = reader.read(path.to_str().unwrap()).await;

        assert!(tags.is_none());
    }
}
