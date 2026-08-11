//! UC-40 — Get a file thumbnail (FR-MP-05).

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::playback::comic_page::ComicArchive;
use crate::playback::{resolve_playable, MAX_PLAYBACK_READ_BYTES};

/// The one thumbnail size. Not a config key and not a query parameter —
/// there is one size until something needs a second. The cache key includes
/// it anyway, precisely so introducing a second size later cannot collide
/// with entries written under the first.
pub const THUMBNAIL_MAX_DIM: u32 = 320;

/// A generated thumbnail. Always JPEG.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Thumbnail {
    pub uuid: Uuid,
    pub mime_type: String,
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// Thumbnail rendering port — decode and downscale.
///
/// `from_*` here names the *source* a thumbnail is derived from, not a
/// conversion of `Self`, so clippy's constructor convention does not apply.
#[allow(async_fn_in_trait)]
#[allow(clippy::wrong_self_convention)]
pub trait ThumbnailRenderer: Send + Sync {
    /// Decode `bytes` as an image and downscale to fit `max_dim`,
    /// preserving aspect ratio. Returns JPEG.
    async fn from_image_bytes(&self, bytes: &[u8], max_dim: u32) -> Result<Vec<u8>, DomainError>;
    /// Grab a keyframe from the video at `path` and downscale it.
    /// Returns JPEG.
    async fn from_video(&self, path: &str, max_dim: u32) -> Result<Vec<u8>, DomainError>;
}

/// Thumbnail cache port. `get` returns `Option` rather than `Result`: a
/// cache that cannot answer is a miss, never an error the caller must
/// handle — the thumbnail is always re-derivable from the file.
#[allow(async_fn_in_trait)]
pub trait ThumbnailCache: Send + Sync {
    async fn get(&self, key: &str) -> Option<Vec<u8>>;
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), DomainError>;
}

/// UC-40 — return a downscaled thumbnail for a video, image, or comic.
pub struct ThumbnailHandler<A, R, C, T, K> {
    auth: A,
    repo: R,
    archive: C,
    renderer: T,
    cache: K,
}

impl<A, R, C, T, K> ThumbnailHandler<A, R, C, T, K>
where
    A: AuthService,
    R: CatalogRepository,
    C: ComicArchive,
    T: ThumbnailRenderer,
    K: ThumbnailCache,
{
    pub fn new(auth: A, repo: R, archive: C, renderer: T, cache: K) -> Self {
        Self {
            auth,
            repo,
            archive,
            renderer,
            cache,
        }
    }

    pub async fn thumbnail(&self, uuid: Uuid, token: &str) -> Result<Thumbnail, DomainError> {
        let file = resolve_playable(&self.auth, &self.repo, uuid, token).await?;

        // Keying on the content hash makes re-index invalidation automatic
        // and free: different bytes, different key.
        let key = format!("{}-{}", file.content_hash, THUMBNAIL_MAX_DIM);

        // The cache is consulted before anything is decoded — a hit must
        // cost one read and no rendering at all.
        if let Some(bytes) = self.cache.get(&key).await {
            return Ok(Thumbnail {
                uuid: file.uuid,
                mime_type: "image/jpeg".to_string(),
                bytes,
            });
        }

        let bytes = match file.file_type {
            FileType::Video => {
                self.renderer
                    .from_video(&file.path, THUMBNAIL_MAX_DIM)
                    .await?
            }
            FileType::Image => {
                // SVG is the one extension `classify_by_extension` maps to
                // `FileType::Image` that has no raster decoder and never
                // will: `image` decodes rasters, and rasterizing vector
                // artwork would mean a new dependency. Reject it in the
                // same shape as an unsupported *type*, so the caller gets
                // "not supported" rather than a decoder error it cannot
                // act on.
                if file.path.to_ascii_lowercase().ends_with(".svg") {
                    return Err(DomainError::InvalidInput(format!(
                        "file {uuid} is an SVG; SVG thumbnails are not supported"
                    )));
                }
                // Bounded: `tokio::fs::read` would allocate the whole source
                // first, so a 3 GB TIFF costs 3 GB before `image`'s own
                // decode guard ever runs (`MAX_PLAYBACK_READ_BYTES`).
                let raw = crate::playback::read_capped(&file.path, MAX_PLAYBACK_READ_BYTES).await?;
                self.renderer
                    .from_image_bytes(&raw, THUMBNAIL_MAX_DIM)
                    .await?
            }
            FileType::Comic => {
                // Page 1 via UC-39's own path, not a second copy of it: the
                // CBZ guard is `ensure_cbz`, and the case-insensitive sort
                // and range check live in `select_page`, which every
                // `ComicArchive` resolves through. A comic's thumbnail is
                // therefore the same image `GET /pages/1` returns, by
                // construction, and a `.cbr` is rejected here — before the
                // archive is opened — with the same `InvalidInput` the page
                // route gives instead of failing inside the ZIP reader as a
                // `Disk` error.
                crate::playback::comic_page::ensure_cbz(uuid, &file.path)?;
                let first = self.archive.read_page(&file.path, 1).await?;
                self.renderer
                    .from_image_bytes(&first.bytes, THUMBNAIL_MAX_DIM)
                    .await?
            }
            _ => {
                return Err(DomainError::InvalidInput(format!(
                    "file {uuid} has no thumbnail; thumbnails cover video, image, and comic"
                )))
            }
        };

        // A cache write that fails must not fail a request whose thumbnail
        // is already rendered and correct. `get` deliberately treats
        // failure as a miss — "a cache that cannot answer is a miss, never
        // an error" — and `put` is the same bargain from the other side: a
        // full disk or a read-only cache directory costs the next caller a
        // re-render, not this caller a 500.
        if let Err(e) = self.cache.put(&key, &bytes).await {
            tracing::warn!(
                uuid = %file.uuid,
                key = %key,
                error = %e,
                "could not cache thumbnail; returning it uncached"
            );
        }

        Ok(Thumbnail {
            uuid: file.uuid,
            mime_type: "image/jpeg".to_string(),
            bytes,
        })
    }
}

/// Real renderer: `image` for stills, `ffmpeg-next` for video keyframes.
#[derive(Clone, Copy)]
pub struct ImageThumbnailRenderer;

impl ImageThumbnailRenderer {
    /// Downscale an already-decoded image to fit `max_dim` and encode JPEG.
    ///
    /// Downscale only. `DynamicImage::thumbnail` would happily *enlarge* a
    /// source smaller than the box — its ratio is not clamped to 1.0 — which
    /// would return a blocky upsample that is both worse-looking and larger
    /// than the original. FR-MP-05 asks for a downscaled thumbnail, so an
    /// image already inside the box is encoded at its own size.
    fn encode(img: image::DynamicImage, max_dim: u32) -> Result<Vec<u8>, DomainError> {
        use image::codecs::jpeg::JpegEncoder;
        let fits = img.width() <= max_dim && img.height() <= max_dim;
        // `thumbnail` preserves aspect ratio, fitting inside the box.
        let scaled = if fits {
            img
        } else {
            img.thumbnail(max_dim, max_dim)
        };
        let mut out = Vec::new();
        JpegEncoder::new(&mut out)
            .encode_image(&scaled)
            .map_err(|e| DomainError::internal(format!("thumbnail encode failed: {e}")))?;
        Ok(out)
    }
}

impl ThumbnailRenderer for ImageThumbnailRenderer {
    async fn from_image_bytes(&self, bytes: &[u8], max_dim: u32) -> Result<Vec<u8>, DomainError> {
        let owned = bytes.to_vec();
        let handle = tokio::task::spawn_blocking(move || {
            let img = image::load_from_memory(&owned)
                .map_err(|e| DomainError::InvalidInput(format!("cannot decode image: {e}")))?;
            Self::encode(img, max_dim)
        });
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!(
                "thumbnail task failed: {err}"
            ))),
        }
    }

    async fn from_video(&self, path: &str, max_dim: u32) -> Result<Vec<u8>, DomainError> {
        let owned = path.to_string();
        let handle = tokio::task::spawn_blocking(move || decode_video_keyframe(&owned, max_dim));
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(DomainError::internal(format!(
                "thumbnail task failed: {err}"
            ))),
        }
    }
}

/// Decode the first frame of a video's best video stream and hand it to
/// [`ImageThumbnailRenderer::encode`].
///
/// Initialization, best-video-stream selection and decoder construction
/// follow `catalog::video_tags` exactly — `ffmpeg_next::init`, then
/// `streams().best(Type::Video)`, then a decoder built from the stream's
/// parameters. Every ffmpeg failure maps to `Disk`, matching how
/// `video_tags` treats extraction failure: a video that will not decode is
/// a property of the file on disk, not of the request.
///
/// Runs synchronously; `from_video` calls it on the blocking pool, the same
/// arrangement `FfmpegVideoMetadataReader::parse` uses.
fn decode_video_keyframe(path: &str, max_dim: u32) -> Result<Vec<u8>, DomainError> {
    use ffmpeg_next::software::scaling::{Context as Scaler, Flags};

    ffmpeg_next::init().map_err(|e| DomainError::disk(format!("ffmpeg init failed: {e}")))?;

    let mut ictx = ffmpeg_next::format::input(path)
        .map_err(|e| DomainError::disk(format!("cannot open {path}: {e}")))?;

    let (stream_index, codec_ctx) = {
        let stream = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .ok_or_else(|| DomainError::disk(format!("{path} has no video stream")))?;
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| DomainError::disk(format!("cannot read codec for {path}: {e}")))?;
        (stream.index(), codec_ctx)
    };

    let mut decoder = codec_ctx
        .decoder()
        .video()
        .map_err(|e| DomainError::disk(format!("cannot open decoder for {path}: {e}")))?;

    // The first frame the decoder yields. It is a keyframe by construction:
    // a decoder cannot emit anything before the first I-frame.
    let mut frame = ffmpeg_next::frame::Video::empty();
    let mut decoded = false;
    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        if decoder.receive_frame(&mut frame).is_ok() {
            decoded = true;
            break;
        }
    }

    // Short files can hold every frame in the decoder's queue until flush.
    if !decoded {
        let _ = decoder.send_eof();
        decoded = decoder.receive_frame(&mut frame).is_ok();
    }

    if !decoded {
        return Err(DomainError::disk(format!(
            "no decodable video frame in {path}"
        )));
    }

    let (width, height) = (frame.width(), frame.height());
    if width == 0 || height == 0 {
        return Err(DomainError::disk(format!(
            "video frame in {path} has no dimensions"
        )));
    }

    let mut scaler = Scaler::get(
        frame.format(),
        width,
        height,
        ffmpeg_next::format::Pixel::RGB24,
        width,
        height,
        Flags::BILINEAR,
    )
    .map_err(|e| DomainError::disk(format!("cannot convert frame from {path}: {e}")))?;

    let mut rgb = ffmpeg_next::frame::Video::empty();
    scaler
        .run(&frame, &mut rgb)
        .map_err(|e| DomainError::disk(format!("cannot convert frame from {path}: {e}")))?;

    // ffmpeg pads each row out to the frame's stride; `image` wants rows
    // packed end to end, so copy row by row rather than wholesale.
    let stride = rgb.stride(0);
    let row_bytes = width as usize * 3;
    let data = rgb.data(0);
    let mut packed = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        let row = data
            .get(start..start + row_bytes)
            .ok_or_else(|| DomainError::disk(format!("truncated video frame in {path}")))?;
        packed.extend_from_slice(row);
    }

    let img = image::RgbImage::from_raw(width, height, packed)
        .ok_or_else(|| DomainError::disk(format!("malformed video frame in {path}")))?;

    ImageThumbnailRenderer::encode(image::DynamicImage::ImageRgb8(img), max_dim)
}

/// Real cache: one file per key under a configured directory.
#[derive(Clone)]
pub struct DiskThumbnailCache {
    root: String,
}

impl DiskThumbnailCache {
    pub fn new(root: String) -> Self {
        Self { root }
    }

    /// The cache key is a hex content hash plus a number, so it is always a
    /// safe filename — no path separators can appear in it.
    fn path_for(&self, key: &str) -> std::path::PathBuf {
        std::path::Path::new(&self.root).join(format!("{key}.jpg"))
    }

    /// A scratch path in the *same* directory as the target, so the rename
    /// that follows stays on one volume and is therefore atomic. Unique per
    /// call: two writers must never share a temp file, or one would truncate
    /// the other's bytes and the rename would publish the damage.
    fn temp_path_for(&self, key: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::path::Path::new(&self.root).join(format!("{key}.{}.{n}.tmp", std::process::id()))
    }
}

impl ThumbnailCache for DiskThumbnailCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.path_for(key)).await.ok()
    }

    /// Write-then-rename, never write in place.
    ///
    /// `tokio::fs::write` is `File::create` followed by `write_all` — two
    /// syscalls, with a window in between where the file is zero bytes. A
    /// concurrent `get` on the same key would read that window and succeed:
    /// `read_to_end` returns `Ok` for a short read, so the caller would be
    /// handed an empty or truncated body labelled `image/jpeg`, with nothing
    /// to catch it. Writing to a unique temp file and renaming closes the
    /// window: `rename(2)` is atomic on POSIX, and Rust's `fs::rename` uses
    /// `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` on Windows, so readers
    /// only ever observe a whole file — the old one or the new one.
    ///
    /// Two concurrent renders of the same key both write and both rename;
    /// last one wins and the bytes are identical, so that costs a wasted CPU
    /// cycle and nothing else. It is only the *partial* file that was ever a
    /// correctness problem.
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), DomainError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|e| DomainError::disk(format!("cannot create {}: {e}", self.root)))?;

        let temp = self.temp_path_for(key);
        if let Err(e) = tokio::fs::write(&temp, bytes).await {
            // Leave no scratch file behind on a failed write.
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(DomainError::disk(format!("cannot write thumbnail: {e}")));
        }

        if let Err(e) = tokio::fs::rename(&temp, self.path_for(key)).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(DomainError::disk(format!("cannot publish thumbnail: {e}")));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::FileState;
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};
    use std::sync::{Arc, Mutex};

    /// Archive fake: pages deliberately supplied out of order, so the
    /// "page 1" assertion proves the ordering rule ran rather than the
    /// archive's own storage order being taken. Like the real
    /// `ZipComicArchive`, it resolves the page number through `select_page`,
    /// and it echoes the chosen entry's name as its bytes.
    #[derive(Clone)]
    struct FakeArchive;

    impl ComicArchive for FakeArchive {
        async fn read_page(
            &self,
            _path: &str,
            page: u32,
        ) -> Result<crate::playback::comic_page::ArchivePage, DomainError> {
            let names = vec!["p2.jpg".to_string(), "p1.jpg".to_string()];
            let (position, page_count) = crate::playback::comic_page::select_page(&names, page)?;
            Ok(crate::playback::comic_page::ArchivePage {
                bytes: names[position].as_bytes().to_vec(),
                entry: names[position].clone(),
                page_count,
            })
        }
    }

    /// Renderer fake recording which path it was asked to take. The log is
    /// an `Arc` the test keeps its own clone of, so assertions read it
    /// directly instead of through a test-only accessor on the handler.
    struct FakeRenderer {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ThumbnailRenderer for FakeRenderer {
        async fn from_image_bytes(
            &self,
            bytes: &[u8],
            _max_dim: u32,
        ) -> Result<Vec<u8>, DomainError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("image:{}", String::from_utf8_lossy(bytes)));
            Ok(b"JPEG".to_vec())
        }

        async fn from_video(&self, path: &str, _max_dim: u32) -> Result<Vec<u8>, DomainError> {
            self.calls.lock().unwrap().push(format!("video:{path}"));
            Ok(b"JPEG".to_vec())
        }
    }

    /// Shared cache contents: key to bytes, in insertion order.
    type Entries = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// In-memory cache fake. Like the renderer log, its entries live behind
    /// an `Arc` the test holds a clone of.
    struct FakeCache {
        entries: Entries,
    }

    impl ThumbnailCache for FakeCache {
        async fn get(&self, key: &str) -> Option<Vec<u8>> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }

        async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), DomainError> {
            self.entries
                .lock()
                .unwrap()
                .push((key.to_string(), bytes.to_vec()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn given_video_when_thumbnailed_then_video_renderer_used_and_cached() {
        // Arrange — `content_hash` is `"abc"` in the shared `a_file` helper
        // on purpose: the cache-key assertions below are written against it.
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let entries = Arc::new(Mutex::new(Vec::new()));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::clone(&calls),
            },
            FakeCache {
                entries: Arc::clone(&entries),
            },
        );

        // Act
        let thumb = handler.thumbnail(Uuid::nil(), "t").await.expect("thumb");

        // Assert
        assert_eq!(thumb.bytes, b"JPEG".to_vec());
        assert_eq!(thumb.mime_type, "image/jpeg");
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["video:/lib/movie.mp4".to_string()]
        );
        assert_eq!(
            *entries.lock().unwrap(),
            vec![(format!("abc-{THUMBNAIL_MAX_DIM}"), b"JPEG".to_vec())]
        );
    }

    #[tokio::test]
    async fn given_comic_when_thumbnailed_then_first_page_rendered() {
        // Arrange — pages sort to p1, p2; the thumbnail is page 1.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbz",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::clone(&calls),
            },
            FakeCache {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
        );

        // Act
        handler.thumbnail(Uuid::nil(), "t").await.expect("thumb");

        // Assert
        assert_eq!(*calls.lock().unwrap(), vec!["image:p1.jpg".to_string()]);
    }

    /// Cache fake whose `put` always fails — a full disk, or a cache
    /// directory that is not writable. `get` is always a miss.
    struct FailingPutCache;

    impl ThumbnailCache for FailingPutCache {
        async fn get(&self, _key: &str) -> Option<Vec<u8>> {
            None
        }

        async fn put(&self, _key: &str, _bytes: &[u8]) -> Result<(), DomainError> {
            Err(DomainError::disk("no space left on device"))
        }
    }

    #[tokio::test]
    async fn given_cache_write_failure_when_thumbnailed_then_thumbnail_still_returned() {
        // Arrange — by the time `put` runs, the JPEG is rendered and
        // correct. A cache that cannot be written costs the next caller a
        // re-render; it must not cost this caller the response.
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FailingPutCache,
        );

        // Act
        let thumb = handler.thumbnail(Uuid::nil(), "t").await.expect("thumb");

        // Assert
        assert_eq!(thumb.bytes, b"JPEG".to_vec());
        assert_eq!(thumb.mime_type, "image/jpeg");
    }

    #[tokio::test]
    async fn given_cbr_comic_when_thumbnailed_then_invalid_input() {
        // Arrange — CBR has no viable pure-Rust reader. The page route has
        // always said so; before the selection was shared, the thumbnail
        // instead reached the ZIP reader and failed as `Disk`, which the
        // HTTP surface renders as 500. The error table promises 400 on both
        // routes.
        let repo = FakeRepo::with_file(a_file(
            "/lib/issue.cbr",
            FileType::Comic,
            FileState::Active,
            None,
        ));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeCache {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
        );

        // Act
        let result = handler.thumbnail(Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_comic_when_thumbnailed_then_same_entry_as_page_one() {
        // Arrange — one archive, two handlers. The thumbnail must be
        // derived from exactly the entry the `pages/1` route serves, or a
        // future change to page ordering would silently make "the
        // thumbnail" and "page 1" different images.
        use crate::playback::comic_page::ComicPageHandler;
        let file = a_file("/lib/issue.cbz", FileType::Comic, FileState::Active, None);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thumbnails = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            FakeRepo::with_file(file.clone()),
            FakeArchive,
            FakeRenderer {
                calls: Arc::clone(&calls),
            },
            FakeCache {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let pages = ComicPageHandler::new(
            FakeAuth { good: "t" },
            FakeRepo::with_file(file),
            FakeArchive,
        );

        // Act
        thumbnails.thumbnail(Uuid::nil(), "t").await.expect("thumb");
        let page_one = pages.read_page(Uuid::nil(), 1, "t").await.expect("page 1");

        // Assert — `FakeArchive::read_entry` echoes the entry name, and
        // `FakeRenderer` logs the bytes it was handed, so both sides name
        // the entry each route chose.
        assert_eq!(
            *calls.lock().unwrap(),
            vec![format!(
                "image:{}",
                String::from_utf8_lossy(&page_one.bytes)
            )]
        );
    }

    #[tokio::test]
    async fn given_cached_thumbnail_when_requested_then_renderer_not_called() {
        // Arrange — the cache already holds an entry for this content hash.
        let repo = FakeRepo::with_file(a_file(
            "/lib/movie.mp4",
            FileType::Video,
            FileState::Active,
            None,
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let cache = FakeCache {
            entries: Arc::new(Mutex::new(vec![(
                format!("{}-{}", "abc", THUMBNAIL_MAX_DIM),
                b"CACHED".to_vec(),
            )])),
        };
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::clone(&calls),
            },
            cache,
        );

        // Act
        let thumb = handler.thumbnail(Uuid::nil(), "t").await.expect("thumb");

        // Assert
        assert_eq!(thumb.bytes, b"CACHED".to_vec());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn given_unsupported_type_when_thumbnailed_then_invalid_input() {
        // Arrange — FR-MP-05 covers video, image, and comic only. A PDF
        // would need a rasterizer, which is out of scope.
        let repo = FakeRepo::with_file(a_file(
            "/lib/book.pdf",
            FileType::Document,
            FileState::Active,
            None,
        ));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            FakeCache {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
        );

        // Act
        let result = handler.thumbnail(Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn given_svg_image_when_thumbnailed_then_invalid_input_and_no_render() {
        // Arrange — SVG classifies as `FileType::Image`, but no raster
        // decoder can read it. It must be rejected before the renderer is
        // reached, so the caller sees "not supported" and not a decode error.
        let repo = FakeRepo::with_file(a_file(
            "/lib/logo.svg",
            FileType::Image,
            FileState::Active,
            None,
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = ThumbnailHandler::new(
            FakeAuth { good: "t" },
            repo,
            FakeArchive,
            FakeRenderer {
                calls: Arc::clone(&calls),
            },
            FakeCache {
                entries: Arc::new(Mutex::new(Vec::new())),
            },
        );

        // Act
        let result = handler.thumbnail(Uuid::nil(), "t").await;

        // Assert
        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn given_image_smaller_than_max_dim_when_encoded_then_dimensions_unchanged() {
        // Arrange — a 64x64 source, well inside the 320 box. `thumbnail`
        // alone would enlarge it to 320x320: its ratio is not clamped to
        // 1.0. FR-MP-05 asks for a *downscaled* thumbnail, so a source that
        // already fits must come back untouched. In-memory PNG only — no
        // real file, no ffmpeg, the same shape `catalog::image_tags`' tests
        // already use.
        let source = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            64,
            image::Rgb([10, 200, 30]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode source png");

        // Act
        let bytes = ImageThumbnailRenderer
            .from_image_bytes(png.get_ref(), THUMBNAIL_MAX_DIM)
            .await
            .expect("thumbnail");

        // Assert
        let out = image::load_from_memory(&bytes).expect("valid jpeg");
        assert_eq!((out.width(), out.height()), (64, 64));
    }
}
