//! UC-21 — the sound of a track, in numbers a visualiser can draw.
//!
//! The player's bars used to be invented: three sine waves per bar, seeded
//! from the track so no two songs moved alike. They moved *with* the music
//! only in the sense that they moved while it played, which is not what
//! anybody means by a visualiser. Nothing in the playing path could do
//! better — the engine reports position and nothing else, and a level drawn
//! from data nobody has is a lie told sixty times a second.
//!
//! This is the data. The file is decoded once, its samples are cut into
//! tenth-of-a-second frames, and each frame is measured in sixteen bands
//! from the bottom of the bass to the top of what a small speaker renders.
//! What comes out is one byte per band per frame — the whole track in about
//! nine kilobytes a minute — stored beside the file and read back at the
//! position playing.
//!
//! Computed on the first play rather than at index time, deliberately: a
//! library of ten thousand files would be hours of decoding for tracks
//! nobody has listened to, where this is a second or two for the one track
//! somebody just started.

use chrono::Utc;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::FileType;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::playback::resolve_playable;

/// How many bands each frame is measured in.
///
/// Sixteen is what a row of bars reads as: enough that the bass end and the
/// treble end visibly disagree, few enough that one band is a wide enough
/// slice of the spectrum to hold something at any moment. A visualiser with
/// sixty-four bands mostly draws sixty-four bars of nothing.
pub const ENERGY_BANDS: usize = 16;

/// How long one frame covers, in milliseconds.
///
/// Ten frames a second. The eye reads a level meter as continuous from about
/// eight, and the player interpolates between frames, so this is the point
/// where more frames stop buying anything and start costing storage.
pub const ENERGY_FRAME_MS: u32 = 100;

/// What the analysis below is. Stored with every envelope.
///
/// An envelope computed by an older analysis is not wrong, it is scaled by
/// rules that have since changed — bands at different edges, or levels
/// against a different reference. Rather than draw somebody else's numbers,
/// a core whose analysis has moved on recomputes.
pub const ENERGY_VERSION: i64 = 1;

/// The rate the analysis runs at, in hertz.
///
/// The file is resampled to this before anything is measured. Half of CD
/// rate: the top band ends below eleven kilohertz anyway, so the samples
/// above it are work with nothing to show for it — and a fixed rate is what
/// makes two files of the same music produce the same envelope whatever they
/// were encoded at.
const ANALYSIS_RATE: u32 = 22_050;

/// How many samples each measurement covers.
///
/// A power of two, because the transform below is radix-2, and 1024 at this
/// rate is a 46-millisecond window: long enough to resolve the bass bands
/// (the lowest edge is 40 Hz, two periods of which fit), short enough that a
/// drum hit lands in one frame rather than smearing across three.
const WINDOW: usize = 1024;

/// The lowest and highest frequency the bands span, in hertz.
///
/// Below 40 Hz is rumble most speakers do not render; above 11 kHz is air
/// and cymbal shimmer that would leave the top bars nearly still on most
/// music. The bands between them are spaced logarithmically, which is how
/// pitch is heard and therefore how a spectrum has to be drawn to look like
/// the music sounds.
const BAND_LOW_HZ: f32 = 40.0;

/// See [`BAND_LOW_HZ`].
const BAND_HIGH_HZ: f32 = 11_000.0;

/// A track's sound, as levels (UC-21).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEnergy {
    pub uuid: Uuid,
    /// How many levels each frame carries.
    pub bands: i64,
    /// How long one frame covers, in milliseconds.
    pub frame_ms: i64,
    /// Row-major: every band of the first frame, then the next frame.
    #[serde(skip)]
    pub levels: Vec<u8>,
}

impl TrackEnergy {
    /// How many frames the envelope holds.
    pub fn frames(&self) -> usize {
        if self.bands <= 0 {
            return 0;
        }

        self.levels.len() / self.bands as usize
    }
}

/// Analysis port — decode a file and measure it.
///
/// Blocking on purpose, and called from the blocking pool: decoding is CPU
/// work measured in seconds, and the runtime this core answers requests on
/// has other callers waiting on it.
pub trait EnergyAnalyzer: Send + Sync {
    /// The levels for the audio at `path`, row-major, [`ENERGY_BANDS`] per
    /// frame of [`ENERGY_FRAME_MS`].
    fn analyze(&self, path: &str) -> Result<Vec<u8>, DomainError>;
}

/// Storage port for computed envelopes.
#[allow(async_fn_in_trait)]
pub trait EnergyStore: Send + Sync {
    /// What is stored for a file, if the analysis that wrote it is the
    /// analysis running now.
    async fn get(&self, file_uuid: Uuid, version: i64) -> Result<Option<TrackEnergy>, DomainError>;

    /// Write (or replace) a file's envelope.
    async fn put(&self, energy: &TrackEnergy, version: i64) -> Result<(), DomainError>;
}

/// UC-21 — answer a track's envelope, computing it the first time.
pub struct EnergyHandler<A, R, S, N> {
    auth: A,
    repo: R,
    store: S,
    analyzer: N,
}

impl<A, R, S, N> EnergyHandler<A, R, S, N>
where
    A: AuthService,
    R: CatalogRepository,
    S: EnergyStore,
    N: EnergyAnalyzer + Clone + 'static,
{
    pub fn new(auth: A, repo: R, store: S, analyzer: N) -> Self {
        Self {
            auth,
            repo,
            store,
            analyzer,
        }
    }

    /// The levels for `uuid`, from storage or freshly measured.
    pub async fn energy(&self, uuid: Uuid, token: &str) -> Result<TrackEnergy, DomainError> {
        let file = resolve_playable(&self.auth, &self.repo, uuid, token).await?;

        if file.file_type != FileType::Audio {
            return Err(DomainError::InvalidInput(format!(
                "file {uuid} is not audio; there is no sound to measure"
            )));
        }

        // Storage first, always: a hit must cost one read and no decoding at
        // all, which is the whole reason the envelope is kept.
        if let Some(stored) = self.store.get(uuid, ENERGY_VERSION).await? {
            return Ok(stored);
        }

        let analyzer = self.analyzer.clone();
        let path = file.path.clone();
        let levels = tokio::task::spawn_blocking(move || analyzer.analyze(&path))
            .await
            .map_err(|err| DomainError::Internal(format!("energy analysis panicked: {err}")))??;

        let energy = TrackEnergy {
            uuid: file.uuid,
            bands: ENERGY_BANDS as i64,
            frame_ms: i64::from(ENERGY_FRAME_MS),
            levels,
        };

        // A store that will not take it is not a reason to withhold what was
        // just measured: the envelope is correct, it will simply be measured
        // again next time.
        if let Err(err) = self.store.put(&energy, ENERGY_VERSION).await {
            tracing::warn!(%uuid, error = %err, "could not store the track's energy envelope");
        }

        Ok(energy)
    }
}

/// The Sqlite store.
pub struct SqliteEnergyStore {
    pool: SqlitePool,
}

impl SqliteEnergyStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl EnergyStore for SqliteEnergyStore {
    async fn get(&self, file_uuid: Uuid, version: i64) -> Result<Option<TrackEnergy>, DomainError> {
        let row = sqlx::query(
            "SELECT e.bands, e.frame_ms, e.levels
             FROM track_energy e
             JOIN files f ON f.id = e.file_id
             WHERE f.uuid = ? AND e.version = ?",
        )
        .bind(file_uuid.to_string())
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| TrackEnergy {
            uuid: file_uuid,
            bands: row.get::<i64, _>("bands"),
            frame_ms: row.get::<i64, _>("frame_ms"),
            levels: row.get::<Vec<u8>, _>("levels"),
        }))
    }

    async fn put(&self, energy: &TrackEnergy, version: i64) -> Result<(), DomainError> {
        sqlx::query(
            "INSERT INTO track_energy
                 (file_id, bands, frame_ms, version, levels, computed_at)
             SELECT f.id, ?, ?, ?, ?, ? FROM files f WHERE f.uuid = ?
             ON CONFLICT(file_id) DO UPDATE SET
                 bands = excluded.bands,
                 frame_ms = excluded.frame_ms,
                 version = excluded.version,
                 levels = excluded.levels,
                 computed_at = excluded.computed_at",
        )
        .bind(energy.bands)
        .bind(energy.frame_ms)
        .bind(version)
        .bind(&energy.levels)
        .bind(Utc::now().to_rfc3339())
        .bind(energy.uuid.to_string())
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// How far below the track's own loudest moment a level still registers, in
/// decibels.
///
/// Levels are measured against the track rather than against full scale, so a
/// quiet recording fills the bars exactly as a loud one does — what an owner
/// wants to see is the shape of *this* music, not how hot it was mastered.
/// Forty-five decibels is roughly the range a level meter shows: below it is
/// the noise floor, and a bar that twitched down there would be drawing
/// silence.
const RANGE_DB: f32 = 45.0;

/// Measures a stream of mono samples into levels, a frame at a time.
///
/// Fed incrementally rather than handed the whole track: an hour of audio at
/// the analysis rate is three hundred megabytes of samples and eight hundred
/// kilobytes of levels, so the samples are measured and dropped as they
/// arrive and only the levels are kept.
pub struct EnergyMeter {
    /// The most recent [`WINDOW`] samples, oldest first.
    window: std::collections::VecDeque<f32>,
    /// How many samples have arrived since the last frame was measured.
    since_frame: usize,
    /// Every frame's bands, row-major, before they are scaled to bytes.
    frames: Vec<f32>,
    /// The loudest band of any frame, which is what the levels are scaled
    /// against.
    peak: f32,
}

impl Default for EnergyMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl EnergyMeter {
    pub fn new() -> Self {
        Self {
            window: std::collections::VecDeque::with_capacity(WINDOW),
            since_frame: 0,
            frames: Vec::new(),
            peak: 0.0,
        }
    }

    /// How many samples separate one frame from the next.
    fn hop() -> usize {
        (ANALYSIS_RATE as usize * ENERGY_FRAME_MS as usize) / 1000
    }

    /// Takes more samples, measuring a frame whenever enough have arrived.
    pub fn push(&mut self, samples: &[f32]) {
        for &sample in samples {
            if self.window.len() == WINDOW {
                self.window.pop_front();
            }
            self.window.push_back(sample);
            self.since_frame += 1;

            if self.since_frame >= Self::hop() {
                self.since_frame = 0;
                self.measure();
            }
        }
    }

    /// Measures what is in the window into one frame's bands.
    fn measure(&mut self) {
        let mut re = [0.0f32; WINDOW];
        let mut im = [0.0f32; WINDOW];

        // Zero-padded at the front while the first window is still filling,
        // which is what the leading silence of a track looks like anyway.
        let offset = WINDOW - self.window.len();
        for (index, &sample) in self.window.iter().enumerate() {
            // Hann, so a sine that does not fit the window a whole number of
            // times lands in its own band instead of smearing across all of
            // them.
            let phase = (offset + index) as f32 / (WINDOW - 1) as f32;
            let hann = 0.5 - 0.5 * (std::f32::consts::TAU * phase).cos();
            re[offset + index] = sample * hann;
        }

        transform(&mut re, &mut im);

        let edges = band_edges();
        for band in 0..ENERGY_BANDS {
            let (from, to) = (edges[band], edges[band + 1]);
            let mut total = 0.0f32;
            for bin in from..to {
                total += (re[bin] * re[bin] + im[bin] * im[bin]).sqrt();
            }

            // The mean rather than the sum: the bands are logarithmically
            // spaced, so the top one covers twenty times the bins of the
            // bottom one and summing would make treble tower over bass on
            // every track ever recorded.
            let level = if to > from {
                total / (to - from) as f32
            } else {
                0.0
            };
            self.peak = self.peak.max(level);
            self.frames.push(level);
        }
    }

    /// The finished envelope, scaled to bytes.
    pub fn finish(self) -> Vec<u8> {
        if self.peak <= 0.0 {
            // Silence — or a file of nothing but silence, which is the same
            // answer: every band at rest, and no division by zero to get
            // there.
            return vec![0; self.frames.len()];
        }

        self.frames
            .iter()
            .map(|&level| {
                if level <= 0.0 {
                    return 0;
                }

                let decibels = 20.0 * (level / self.peak).log10();
                let scaled = (decibels + RANGE_DB) / RANGE_DB;

                (scaled.clamp(0.0, 1.0) * 255.0).round() as u8
            })
            .collect()
    }
}

/// Where each band starts and ends, as transform bins.
///
/// Logarithmically spaced, because pitch is heard that way: an octave is a
/// doubling whether it sits at the bottom of the bass or the top of the
/// treble, and bands spaced evenly in hertz would give eleven of sixteen
/// bars to frequencies most music barely occupies.
fn band_edges() -> [usize; ENERGY_BANDS + 1] {
    let mut edges = [0usize; ENERGY_BANDS + 1];
    let ratio = (BAND_HIGH_HZ / BAND_LOW_HZ).powf(1.0 / ENERGY_BANDS as f32);

    for (band, edge) in edges.iter_mut().enumerate() {
        let hertz = BAND_LOW_HZ * ratio.powi(band as i32);
        let bin = (hertz * WINDOW as f32 / ANALYSIS_RATE as f32).round() as usize;
        // Strictly increasing, so no band is empty: the lowest bands are
        // narrower than one bin at this window, and two edges landing on the
        // same bin would leave a bar that never moves.
        *edge = bin.max(band + 1).min(WINDOW / 2 - 1);
    }

    edges
}

/// An in-place radix-2 fast Fourier transform.
///
/// Written here rather than taken from a crate, and it is worth saying why:
/// this is one textbook routine over a fixed power-of-two window, it is the
/// only numerical code in this workspace, and its correctness is checked
/// directly (a sine goes in, its own bin comes out). A dependency for fifty
/// lines would be a supply chain for a visualiser.
///
/// `re` and `im` are the real and imaginary parts, and both are rewritten.
fn transform(re: &mut [f32; WINDOW], im: &mut [f32; WINDOW]) {
    // Bit-reversal permutation: the butterflies below read pairs that are
    // adjacent only once the input is in this order.
    let mut target = 0usize;
    for source in 1..WINDOW {
        let mut bit = WINDOW >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target |= bit;

        if source < target {
            re.swap(source, target);
            im.swap(source, target);
        }
    }

    let mut span = 2;
    while span <= WINDOW {
        let angle = -std::f32::consts::TAU / span as f32;
        let (step_sin, step_cos) = angle.sin_cos();

        for start in (0..WINDOW).step_by(span) {
            let (mut twiddle_re, mut twiddle_im) = (1.0f32, 0.0f32);
            for offset in 0..span / 2 {
                let (a, b) = (start + offset, start + offset + span / 2);
                let product_re = re[b] * twiddle_re - im[b] * twiddle_im;
                let product_im = re[b] * twiddle_im + im[b] * twiddle_re;

                re[b] = re[a] - product_re;
                im[b] = im[a] - product_im;
                re[a] += product_re;
                im[a] += product_im;

                let next_re = twiddle_re * step_cos - twiddle_im * step_sin;
                twiddle_im = twiddle_re * step_sin + twiddle_im * step_cos;
                twiddle_re = next_re;
            }
        }

        span <<= 1;
    }
}

/// The real analyzer: ffmpeg decodes, [`EnergyMeter`] measures.
///
/// ffmpeg rather than a Rust decoder, because it is already here — the video
/// thumbnails and the video tag reader both go through it, the build already
/// links it on both platforms, and its coverage of what a music library
/// actually holds (mp3, flac, m4a, ogg, opus, wav, wma) is the reason
/// nothing else had to be added for those either.
#[derive(Debug, Default, Clone, Copy)]
pub struct FfmpegEnergyAnalyzer;

impl EnergyAnalyzer for FfmpegEnergyAnalyzer {
    fn analyze(&self, path: &str) -> Result<Vec<u8>, DomainError> {
        use ffmpeg_next::format::sample::Type as SampleType;
        use ffmpeg_next::format::Sample;
        use ffmpeg_next::{codec, frame, media, ChannelLayout};

        ffmpeg_next::init()
            .map_err(|err| DomainError::Internal(format!("ffmpeg would not start: {err}")))?;

        let mut input = ffmpeg_next::format::input(path)
            .map_err(|err| DomainError::Disk(format!("could not open {path}: {err}")))?;

        let stream = input
            .streams()
            .best(media::Type::Audio)
            .ok_or_else(|| DomainError::InvalidInput(format!("{path} carries no audio stream")))?;
        let index = stream.index();

        let context = codec::context::Context::from_parameters(stream.parameters())
            .map_err(|err| DomainError::Disk(format!("could not read the codec: {err}")))?;
        let mut decoder = context
            .decoder()
            .audio()
            .map_err(|err| DomainError::Disk(format!("could not open the decoder: {err}")))?;

        // Everything measured at one rate, in one channel, in one sample
        // format: the same music in flac and in mp3 has to produce the same
        // envelope, and it only does if the analysis never sees what the
        // file was encoded at.
        let mut resampler = decoder
            .resampler(
                Sample::F32(SampleType::Packed),
                ChannelLayout::MONO,
                ANALYSIS_RATE,
            )
            .map_err(|err| DomainError::Disk(format!("could not resample: {err}")))?;

        let mut meter = EnergyMeter::new();
        let mut decoded = frame::Audio::empty();
        let mut resampled = frame::Audio::empty();

        for (packet_stream, packet) in input.packets() {
            if packet_stream.index() != index {
                continue;
            }

            // A packet the decoder refuses is one damaged frame, not a
            // damaged file: the envelope of the rest of the track is still
            // worth drawing, and a track that will not decode at all comes
            // back as silence rather than as an error nobody can act on.
            if decoder.send_packet(&packet).is_err() {
                continue;
            }

            while decoder.receive_frame(&mut decoded).is_ok() {
                if resampler.run(&decoded, &mut resampled).is_err() {
                    continue;
                }
                meter.push(mono_samples(&resampled));
            }
        }

        let _ = decoder.send_eof();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if resampler.run(&decoded, &mut resampled).is_err() {
                continue;
            }
            meter.push(mono_samples(&resampled));
        }

        Ok(meter.finish())
    }
}

/// The samples of one resampled frame, as mono floats.
///
/// `plane(0)` is the whole of a packed frame, and packed is what the
/// resampler above was asked for — but ffmpeg sizes the plane by the buffer
/// it allocated rather than by the samples it filled, so the frame's own
/// count is what says where the audio ends.
fn mono_samples(frame: &ffmpeg_next::frame::Audio) -> &[f32] {
    let plane: &[f32] = frame.plane(0);
    let samples = frame.samples().min(plane.len());

    &plane[..samples]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::model::{FileState, FileType};
    use crate::playback::test_support::{a_file, FakeAuth, FakeRepo};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A tone at `hertz`, `seconds` long, at the analysis rate.
    fn tone(hertz: f32, seconds: f32) -> Vec<f32> {
        let count = (ANALYSIS_RATE as f32 * seconds) as usize;
        (0..count)
            .map(|index| {
                let phase = index as f32 / ANALYSIS_RATE as f32;
                (std::f32::consts::TAU * hertz * phase).sin()
            })
            .collect()
    }

    /// Which band a frequency belongs in, by the same edges the meter uses.
    fn band_of(hertz: f32) -> usize {
        let bin = (hertz * WINDOW as f32 / ANALYSIS_RATE as f32).round() as usize;
        let edges = band_edges();

        (0..ENERGY_BANDS)
            .find(|&band| bin >= edges[band] && bin < edges[band + 1])
            .expect("the tone is inside the measured range")
    }

    #[test]
    fn transform_finds_the_bin_a_sine_belongs_in() {
        // The one check the whole analysis rests on: a sine of exactly `k`
        // periods across the window has to come out as bin `k` and nothing
        // else. A transform with its butterflies wrong still produces
        // plausible-looking numbers, which is why this is asserted against
        // an input whose answer is known rather than against a golden.
        let mut re = [0.0f32; WINDOW];
        let mut im = [0.0f32; WINDOW];
        let periods = 8.0;
        for (index, slot) in re.iter_mut().enumerate() {
            *slot = (std::f32::consts::TAU * periods * index as f32 / WINDOW as f32).sin();
        }

        transform(&mut re, &mut im);

        let magnitudes: Vec<f32> = (0..WINDOW / 2)
            .map(|bin| (re[bin] * re[bin] + im[bin] * im[bin]).sqrt())
            .collect();
        let loudest = magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(bin, _)| bin)
            .expect("a peak");

        assert_eq!(loudest, periods as usize);
    }

    #[test]
    fn a_tone_lands_in_its_own_band() {
        // The bands are logarithmic and the frequencies are real: this is
        // what catches an edge table that drifts, which would show as bars
        // moving to the wrong music rather than as anything failing.
        let mut meter = EnergyMeter::new();
        meter.push(&tone(1_000.0, 1.0));
        let levels = meter.finish();

        assert_eq!(levels.len() % ENERGY_BANDS, 0);
        let frames = levels.len() / ENERGY_BANDS;
        assert!(frames >= 8, "a second is about ten frames, got {frames}");

        // The middle frame, past the window filling and before it empties.
        let middle = frames / 2;
        let frame = &levels[middle * ENERGY_BANDS..(middle + 1) * ENERGY_BANDS];
        let loudest = frame
            .iter()
            .enumerate()
            .max_by_key(|(_, &level)| level)
            .map(|(band, _)| band)
            .expect("a loudest band");

        assert_eq!(loudest, band_of(1_000.0));
    }

    #[test]
    fn silence_measures_as_silence() {
        // Not "close to zero": a file of digital silence has to come back as
        // a flat envelope, and the scaling has to reach that answer without
        // dividing by the peak it does not have.
        let mut meter = EnergyMeter::new();
        meter.push(&vec![0.0; ANALYSIS_RATE as usize]);
        let levels = meter.finish();

        assert!(!levels.is_empty());
        assert!(levels.iter().all(|&level| level == 0));
    }

    #[test]
    fn levels_are_scaled_against_the_track_itself() {
        // A quiet recording fills the bars exactly as a loud one does: what
        // an owner wants to see is the shape of this music, not how hot it
        // was mastered. The same tone at a tenth of the amplitude has to
        // produce the same envelope.
        let loud = {
            let mut meter = EnergyMeter::new();
            meter.push(&tone(1_000.0, 1.0));
            meter.finish()
        };
        let quiet = {
            let mut meter = EnergyMeter::new();
            let samples: Vec<f32> = tone(1_000.0, 1.0).iter().map(|s| s * 0.1).collect();
            meter.push(&samples);
            meter.finish()
        };

        assert_eq!(loud.len(), quiet.len());
        for (index, (&a, &b)) in loud.iter().zip(quiet.iter()).enumerate() {
            assert!(
                a.abs_diff(b) <= 1,
                "band {index}: {a} against {b} — the scaling is not relative"
            );
        }
    }

    /// An analyzer that counts how often it was asked, and answers a fixed
    /// envelope.
    #[derive(Clone)]
    struct CountingAnalyzer {
        calls: Arc<AtomicUsize>,
    }

    impl EnergyAnalyzer for CountingAnalyzer {
        fn analyze(&self, _path: &str) -> Result<Vec<u8>, DomainError> {
            self.calls.fetch_add(1, Ordering::SeqCst);

            Ok(vec![7; ENERGY_BANDS * 3])
        }
    }

    /// A store that holds whatever it is given, in memory.
    #[derive(Clone, Default)]
    struct MemoryStore {
        held: Arc<std::sync::Mutex<Option<(i64, TrackEnergy)>>>,
    }

    impl EnergyStore for MemoryStore {
        async fn get(&self, uuid: Uuid, version: i64) -> Result<Option<TrackEnergy>, DomainError> {
            let held = self.held.lock().expect("not poisoned");

            Ok(held.as_ref().and_then(|(stored, energy)| {
                (*stored == version && energy.uuid == uuid).then(|| energy.clone())
            }))
        }

        async fn put(&self, energy: &TrackEnergy, version: i64) -> Result<(), DomainError> {
            *self.held.lock().expect("not poisoned") = Some((version, energy.clone()));

            Ok(())
        }
    }

    fn handler(
        file: crate::catalog::model::File,
        store: MemoryStore,
        calls: Arc<AtomicUsize>,
    ) -> EnergyHandler<FakeAuth, FakeRepo, MemoryStore, CountingAnalyzer> {
        EnergyHandler::new(
            FakeAuth { good: "t" },
            FakeRepo::with_file(file),
            store,
            CountingAnalyzer { calls },
        )
    }

    #[tokio::test]
    async fn a_track_is_measured_once_and_kept() {
        // The whole reason the envelope is stored: decoding is seconds of
        // CPU, and a player that re-measured on every open would spend them
        // every time an owner pressed play.
        let calls = Arc::new(AtomicUsize::new(0));
        let store = MemoryStore::default();
        let handler = handler(
            a_file("/music/one.flac", FileType::Audio, FileState::Active, None),
            store.clone(),
            calls.clone(),
        );

        let first = handler.energy(Uuid::nil(), "t").await.expect("measured");
        let second = handler.energy(Uuid::nil(), "t").await.expect("stored");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.levels, second.levels);
        assert_eq!(first.frames(), 3);
    }

    #[tokio::test]
    async fn a_file_with_no_sound_is_refused() {
        // A video has audio in it and could be measured, and deliberately is
        // not: the player draws this for the music it is playing, and a
        // caller asking for the sound of a PDF has made a mistake worth
        // being told about.
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = handler(
            a_file("/docs/one.pdf", FileType::Document, FileState::Active, None),
            MemoryStore::default(),
            calls.clone(),
        );

        let error = handler.energy(Uuid::nil(), "t").await.expect_err("refused");

        assert!(matches!(error, DomainError::InvalidInput(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_stranger_is_told_nothing() {
        // FR-AU-07, on this route as on every other.
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = handler(
            a_file("/music/one.flac", FileType::Audio, FileState::Active, None),
            MemoryStore::default(),
            calls.clone(),
        );

        let error = handler
            .energy(Uuid::nil(), "wrong")
            .await
            .expect_err("refused");

        assert!(matches!(error, DomainError::Unauthorized));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
