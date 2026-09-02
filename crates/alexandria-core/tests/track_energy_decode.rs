//! The analyzer against a real file (UC-21, FR-MP-07).
//!
//! Everything else about the energy envelope is measured against samples this
//! workspace generated in memory. This is the other half: ffmpeg opening a
//! file off disk, decoding it, resampling it, and the meter measuring what
//! comes out — the path that actually runs when an owner presses play, and
//! the one no unit test of the transform can stand in for.

use std::io::Write;

use alexandria_core::playback::energy::{EnergyAnalyzer, FfmpegEnergyAnalyzer, ENERGY_BANDS};
use tempfile::tempdir;

/// A mono 16-bit PCM WAV of a sine at `hertz`, `seconds` long.
///
/// Uncompressed, because the point is the decode path rather than any one
/// codec: forty-four bytes of header and then the samples.
fn sine_wav(hertz: f32, seconds: f32, rate: u32) -> Vec<u8> {
    let samples = (rate as f32 * seconds) as usize;
    let mut data = Vec::with_capacity(samples * 2);
    for index in 0..samples {
        let phase = index as f32 / rate as f32;
        let value = (std::f32::consts::TAU * hertz * phase).sin() * 0.66 * 32767.0;
        data.extend_from_slice(&(value as i16).to_le_bytes());
    }

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);

    wav
}

#[test]
fn a_tone_on_disk_is_measured_where_it_belongs() {
    let dir = tempdir().expect("temp dir");
    let path = dir.path().join("tone.wav");
    let mut file = std::fs::File::create(&path).expect("create");
    file.write_all(&sine_wav(1_000.0, 1.0, 44_100))
        .expect("write");
    drop(file);

    let levels = FfmpegEnergyAnalyzer
        .analyze(path.to_str().expect("utf-8 path"))
        .expect("measured");

    assert!(
        levels.len() >= ENERGY_BANDS * 5,
        "a second of audio is several frames, got {} bytes",
        levels.len()
    );
    assert_eq!(levels.len() % ENERGY_BANDS, 0);

    // The middle frame, past the window filling at the start.
    let frames = levels.len() / ENERGY_BANDS;
    let middle = frames / 2;
    let frame = &levels[middle * ENERGY_BANDS..(middle + 1) * ENERGY_BANDS];
    let loudest = frame
        .iter()
        .enumerate()
        .max_by_key(|(_, &level)| level)
        .map(|(band, _)| band)
        .expect("a loudest band");
    let average: u32 =
        frame.iter().map(|&level| u32::from(level)).sum::<u32>() / ENERGY_BANDS as u32;

    assert!(
        u32::from(frame[loudest]) > average * 2,
        "a pure tone belongs in one band: {frame:?}"
    );
}
