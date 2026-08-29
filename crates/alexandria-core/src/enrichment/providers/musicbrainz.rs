//! MusicBrainz: the identity half. It has no images and no lyrics — what it
//! has is the mapping from a tag's spelling to a stable id, which is what
//! makes the other two lookups possible at all.

use reqwest::Client;
use serde::Deserialize;

use super::{
    user_agent, ArtistIdentityProvider, ArtistMatch, LyricsQuery, ProviderError, RateGate,
    RecordingIdentityProvider, RecordingMatch,
};

const SEARCH_URL: &str = "https://musicbrainz.org/ws/2/artist";
const RECORDING_URL: &str = "https://musicbrainz.org/ws/2/recording";

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

/// A name as a quoted Lucene phrase, safe to interpolate into `query=`.
///
/// MusicBrainz's search parameter is Lucene syntax, and a tag is just text
/// the owner's ripper wrote. `AC/DC`, `Sunn O)))`, `!!!` and `Godspeed You!
/// Black Emperor` all carry reserved characters, and sending them raw
/// produces a malformed query that the server answers `400` — mapped here to
/// `Failed`, the one *unsettled* outcome, so every later run re-asks the same
/// broken question at one second a go and those artists can never get an
/// image at all.
///
/// Quoting rather than escaping each reserved character individually: inside
/// a phrase only `\` and `"` are special, so the rule is two replacements
/// instead of a table of fifteen that has to stay in step with Lucene's.
fn lucene_phrase(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Resolves artist names against MusicBrainz.
///
/// Holds the [`RateGate`] rather than taking one per call: the limit is per
/// client, not per request, and a gate that did not outlive the call would
/// enforce nothing.
pub struct MusicBrainzClient {
    http: Client,
    /// Borrowed, not owned: the limit is per process, not per client.
    gate: &'static RateGate,
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
            gate: RateGate::shared_musicbrainz(),
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

        let query = format!("artist:{}", lucene_phrase(trimmed));

        let response = self
            .http
            .get(SEARCH_URL)
            // `limit=1`: only the top hit is ever considered, and asking for
            // more would be fetching candidates this code has no rule for
            // choosing between. The score on that one hit is what decides
            // whether it is used (`MIN_ARTIST_SCORE`).
            .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "1")])
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

#[derive(Debug, Deserialize)]
struct RecordingResponse {
    #[serde(default)]
    recordings: Vec<SearchedRecording>,
}

#[derive(Debug, Deserialize)]
struct SearchedRecording {
    id: String,
    #[serde(default)]
    score: u32,
}

impl RecordingIdentityProvider for MusicBrainzClient {
    async fn find_recording(
        &self,
        query: &LyricsQuery,
    ) -> Result<Option<RecordingMatch>, ProviderError> {
        if query.title.trim().is_empty() || query.artist.trim().is_empty() {
            return Ok(None);
        }

        // Every field quoted, for the reason the artist search quotes its
        // one: these are tags, and `AC/DC` or a title with a `!` in it is
        // Lucene syntax the server answers 400 to.
        let mut terms = vec![
            format!("recording:{}", lucene_phrase(query.title.trim())),
            format!("artist:{}", lucene_phrase(query.artist.trim())),
        ];
        if let Some(album) = query.album.as_deref().map(str::trim) {
            if !album.is_empty() {
                terms.push(format!("release:{}", lucene_phrase(album)));
            }
        }
        let search = terms.join(" AND ");

        self.gate.admit().await;

        let response = self
            .http
            .get(RECORDING_URL)
            .query(&[("query", search.as_str()), ("fmt", "json"), ("limit", "1")])
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

        let body: RecordingResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        Ok(body
            .recordings
            .into_iter()
            .next()
            .map(|recording| RecordingMatch {
                mbid: recording.id,
                score: recording.score.min(100) as u8,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_name_with_lucene_syntax_when_quoted_then_it_is_a_phrase() {
        // The names that actually break this are ordinary bands, not
        // adversarial input.
        assert_eq!(lucene_phrase("AC/DC"), "\"AC/DC\"");
        assert_eq!(lucene_phrase("Sunn O)))"), "\"Sunn O)))\"");
        assert_eq!(lucene_phrase("!!!"), "\"!!!\"");
    }

    #[test]
    fn given_a_name_with_a_quote_when_quoted_then_it_cannot_close_the_phrase() {
        // A `"` left raw would end the phrase and turn the rest of the name
        // into loose query syntax.
        assert_eq!(lucene_phrase("The \"Band\""), "\"The \\\"Band\\\"\"");
    }

    #[test]
    fn given_a_name_with_a_backslash_when_quoted_then_it_is_escaped_first() {
        // Escaped before the quote replacement, or a name ending in `\`
        // would escape the closing quote instead of itself.
        assert_eq!(lucene_phrase("AC\\DC"), "\"AC\\\\DC\"");
    }
}
