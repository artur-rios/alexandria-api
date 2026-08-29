//! MusicBrainz: the identity half. It has no images and no lyrics — what it
//! has is the mapping from a tag's spelling to a stable id, which is what
//! makes the other two lookups possible at all.

use reqwest::Client;
use serde::Deserialize;

use super::{user_agent, ArtistIdentityProvider, ArtistMatch, ProviderError, RateGate};

const SEARCH_URL: &str = "https://musicbrainz.org/ws/2/artist";

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    artists: Vec<SearchedArtist>,
}

#[derive(Debug, Deserialize)]
struct SearchedArtist {
    id: String,
    name: String,
    #[serde(default)]
    score: u32,
}

/// Resolves artist names against MusicBrainz.
///
/// Holds the [`RateGate`] rather than taking one per call: the limit is per
/// client, not per request, and a gate that did not outlive the call would
/// enforce nothing.
pub struct MusicBrainzClient {
    http: Client,
    gate: RateGate,
}

impl MusicBrainzClient {
    /// Build a client identifying itself with `contact`.
    ///
    /// `contact` is required by MusicBrainz's terms, and the caller has
    /// already refused to start enrichment without one
    /// (`MetadataSettings::unavailable_reason`) — this does not re-check,
    /// because a second, differently-worded refusal here would be a second
    /// place the rule lives.
    pub fn new(contact: &str) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(user_agent(contact))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        Ok(Self {
            http,
            gate: RateGate::musicbrainz(),
        })
    }
}

impl ArtistIdentityProvider for MusicBrainzClient {
    async fn find_artist(&self, name: &str) -> Result<Option<ArtistMatch>, ProviderError> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        // Every MusicBrainz request, without exception, waits its turn.
        self.gate.admit().await;

        let response = self
            .http
            .get(SEARCH_URL)
            // `limit=1`: only the top hit is ever considered, and asking for
            // more would be fetching candidates this code has no rule for
            // choosing between. The score on that one hit is what decides
            // whether it is used (`MIN_ARTIST_SCORE`).
            .query(&[("query", trimmed), ("fmt", "json"), ("limit", "1")])
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited);
        }
        if !response.status().is_success() {
            return Err(ProviderError::Unreachable(format!(
                "status {}",
                response.status()
            )));
        }

        let body: SearchResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        Ok(body.artists.into_iter().next().map(|artist| ArtistMatch {
            mbid: artist.id,
            name: artist.name,
            // Clamped rather than cast: MusicBrainz documents 0-100, and a
            // value outside it would otherwise wrap into a small number and
            // read as a bad match.
            score: artist.score.min(100) as u8,
        }))
    }
}
