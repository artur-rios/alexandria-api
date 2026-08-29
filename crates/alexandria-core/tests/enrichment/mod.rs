//! Shared fixtures for the enrichment tests.
//!
//! Every port is faked here, and that is the point of the module: the
//! decisions worth testing — what is skipped, what score is accepted, what
//! outcome is recorded — must not need a network, an API key, or three
//! public services to be up while the suite runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alexandria_core::catalog::clock::Clock;
use alexandria_core::config::MetadataSettings;
use alexandria_core::enrichment::commands::ArtistImageStore;
use alexandria_core::enrichment::model::{ArtistImage, EnrichmentScope, TrackLyrics};
use alexandria_core::enrichment::providers::{
    ArtistIdentityProvider, ArtistImageAsset, ArtistImageProvider, ArtistMatch, LyricsMatch,
    LyricsProvider, LyricsQuery, ProviderError, RecordingIdentityProvider, RecordingMatch,
};
use alexandria_core::enrichment::repos::{EnrichmentCandidate, EnrichmentRepository};
use alexandria_core::errors::DomainError;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

/// A clock that never moves, so a stored `fetched_at` is assertable.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap()
    }
}

/// Settings with enrichment switched on and a contact filled in — the only
/// configuration under which a run is allowed to start.
pub fn available_settings() -> MetadataSettings {
    MetadataSettings {
        enabled: true,
        contact: "owner@example.com".to_string(),
        image_cache_dir: "artist-images".to_string(),
    }
}

/// An in-memory enrichment repository.
///
/// Shared through `Arc<Mutex<…>>` so a test can clone it, hand the original
/// to the handler, and read what was written afterwards — the same shape
/// `FakeCatalogRepository` uses.
#[derive(Debug, Default, Clone)]
pub struct FakeEnrichmentRepository {
    pub candidates: Arc<Mutex<Vec<EnrichmentCandidate>>>,
    pub images: Arc<Mutex<HashMap<String, ArtistImage>>>,
    pub lyrics: Arc<Mutex<HashMap<Uuid, TrackLyrics>>>,
    /// Answer every lyrics write with `NotFound`, standing in for a file
    /// purged between being listed as a candidate and the write.
    pub lyrics_file_vanished: bool,
}

impl FakeEnrichmentRepository {
    pub fn with_candidates(candidates: Vec<EnrichmentCandidate>) -> Self {
        Self {
            candidates: Arc::new(Mutex::new(candidates)),
            ..Default::default()
        }
    }

    pub fn stored_image(&self, artist: &str) -> Option<ArtistImage> {
        self.images.lock().unwrap().get(artist).cloned()
    }

    pub fn stored_lyrics(&self, file_uuid: Uuid) -> Option<TrackLyrics> {
        self.lyrics.lock().unwrap().get(&file_uuid).cloned()
    }
}

impl EnrichmentRepository for FakeEnrichmentRepository {
    async fn candidates(
        &self,
        _scope: &EnrichmentScope,
    ) -> Result<Vec<EnrichmentCandidate>, DomainError> {
        Ok(self.candidates.lock().unwrap().clone())
    }

    async fn artist_image(&self, artist_name: &str) -> Result<Option<ArtistImage>, DomainError> {
        Ok(self.images.lock().unwrap().get(artist_name).cloned())
    }

    async fn put_artist_image(&self, image: ArtistImage) -> Result<(), DomainError> {
        self.images
            .lock()
            .unwrap()
            .insert(image.artist_name.clone(), image);
        Ok(())
    }

    async fn lyrics(&self, file_uuid: Uuid) -> Result<Option<TrackLyrics>, DomainError> {
        Ok(self.lyrics.lock().unwrap().get(&file_uuid).cloned())
    }

    async fn put_lyrics(&self, lyrics: TrackLyrics) -> Result<(), DomainError> {
        if self.lyrics_file_vanished {
            return Err(DomainError::NotFound);
        }
        self.lyrics.lock().unwrap().insert(lyrics.file_uuid, lyrics);
        Ok(())
    }

    async fn pending_count(&self) -> Result<u32, DomainError> {
        // The fake answers a canned candidate list rather than querying, so
        // there is nothing here to count down: what a test asserts about
        // `remaining` belongs against the real repository.
        Ok(0)
    }
}

/// An identity provider that counts how often it was asked.
///
/// The count is the assertion for "one lookup per artist, not one per track":
/// nothing else observable distinguishes a run that asked twelve times from
/// one that asked once and reused the answer.
#[derive(Debug, Default, Clone)]
pub struct FakeIdentity {
    pub answer: Option<ArtistMatch>,
    pub fails: bool,
    pub asked: Arc<Mutex<Vec<String>>>,
    /// What a recording lookup answers, and every query it was given.
    pub recording: Option<RecordingMatch>,
    pub recordings_asked: Arc<Mutex<Vec<LyricsQuery>>>,
}

impl FakeIdentity {
    pub fn matching(mbid: &str, name: &str, score: u8) -> Self {
        Self {
            answer: Some(ArtistMatch {
                mbid: mbid.to_string(),
                name: name.to_string(),
                score,
            }),
            ..Default::default()
        }
    }

    pub fn unreachable() -> Self {
        Self {
            fails: true,
            ..Default::default()
        }
    }

    pub fn ask_count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    /// Also answer a recording, at `score`.
    pub fn with_recording(mut self, mbid: &str, score: u8) -> Self {
        self.recording = Some(RecordingMatch {
            mbid: mbid.to_string(),
            score,
        });
        self
    }

    pub fn recording_ask_count(&self) -> usize {
        self.recordings_asked.lock().unwrap().len()
    }
}

impl RecordingIdentityProvider for FakeIdentity {
    async fn find_recording(
        &self,
        query: &LyricsQuery,
    ) -> Result<Option<RecordingMatch>, ProviderError> {
        self.recordings_asked.lock().unwrap().push(query.clone());
        if self.fails {
            return Err(ProviderError::Unreachable("test".to_string()));
        }
        Ok(self.recording.clone())
    }
}

impl ArtistIdentityProvider for FakeIdentity {
    async fn find_artist(&self, name: &str) -> Result<Option<ArtistMatch>, ProviderError> {
        self.asked.lock().unwrap().push(name.to_string());
        if self.fails {
            return Err(ProviderError::Unreachable("test".to_string()));
        }
        Ok(self.answer.clone())
    }
}

/// An image provider answering fixed bytes, nothing, or a failure.
#[derive(Debug, Default, Clone)]
pub struct FakeImages {
    pub answer: Option<ArtistImageAsset>,
    pub fails: bool,
}

impl FakeImages {
    pub fn with_image() -> Self {
        Self {
            answer: Some(ArtistImageAsset {
                source_url: "https://commons.example/portrait.jpg".to_string(),
                bytes: vec![1, 2, 3],
                extension: "jpg".to_string(),
            }),
            fails: false,
        }
    }

    pub fn with_nothing() -> Self {
        Self::default()
    }
}

impl ArtistImageProvider for FakeImages {
    async fn image_for(&self, _mbid: &str) -> Result<Option<ArtistImageAsset>, ProviderError> {
        if self.fails {
            return Err(ProviderError::Unreachable("test".to_string()));
        }
        Ok(self.answer.clone())
    }
}

/// A lyrics provider answering fixed text, nothing, or a failure, and
/// counting the queries it was given.
#[derive(Debug, Default, Clone)]
pub struct FakeLyrics {
    pub answer: Option<LyricsMatch>,
    pub fails: bool,
    pub asked: Arc<Mutex<Vec<LyricsQuery>>>,
}

impl FakeLyrics {
    /// Deliberately not real lyrics: a fixture only has to be non-empty, and
    /// putting somebody's copyrighted words in a test file would be
    /// redistributing them.
    pub fn with_text() -> Self {
        Self {
            answer: Some(LyricsMatch {
                plain: Some("first line\nsecond line".to_string()),
                synced: None,
                source: "fake".to_string(),
            }),
            ..Default::default()
        }
    }

    pub fn with_nothing() -> Self {
        Self::default()
    }

    pub fn unreachable() -> Self {
        Self {
            fails: true,
            ..Default::default()
        }
    }

    pub fn ask_count(&self) -> usize {
        self.asked.lock().unwrap().len()
    }

    pub fn last_query(&self) -> Option<LyricsQuery> {
        self.asked.lock().unwrap().last().cloned()
    }
}

impl LyricsProvider for FakeLyrics {
    async fn lyrics_for(&self, query: &LyricsQuery) -> Result<Option<LyricsMatch>, ProviderError> {
        self.asked.lock().unwrap().push(query.clone());
        if self.fails {
            return Err(ProviderError::Unreachable("test".to_string()));
        }
        Ok(self.answer.clone())
    }
}

/// An image store that keeps what it was given in memory.
#[derive(Debug, Default, Clone)]
pub struct FakeImageStore {
    pub stored: Arc<Mutex<Vec<String>>>,
    pub fails: bool,
}

impl FakeImageStore {
    pub fn failing() -> Self {
        Self {
            fails: true,
            ..Default::default()
        }
    }
}

impl ArtistImageStore for FakeImageStore {
    async fn store(&self, mbid: &str, asset: &ArtistImageAsset) -> Result<String, DomainError> {
        if self.fails {
            return Err(DomainError::Disk("no space".to_string()));
        }
        let name = format!("{mbid}.{}", asset.extension);
        self.stored.lock().unwrap().push(name.clone());
        Ok(name)
    }
}

/// One audio track's worth of tags.
pub fn candidate(title: &str, album_artist: &str) -> EnrichmentCandidate {
    EnrichmentCandidate {
        file_uuid: Uuid::new_v4(),
        title: Some(title.to_string()),
        artist: Some(album_artist.to_string()),
        album_artist: Some(album_artist.to_string()),
        album: Some("Kind of Blue".to_string()),
        duration_seconds: Some(545),
    }
}
