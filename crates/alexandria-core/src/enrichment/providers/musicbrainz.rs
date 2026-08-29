//! MusicBrainz: the identity half. It has no images and no lyrics — what it
//! has is the mapping from a tag's spelling to a stable id, which is what
//! makes the other two lookups possible at all.

use reqwest::Client;
use serde::Deserialize;

use super::{
    user_agent, ArtistIdentityProvider, ArtistMatch, LyricsQuery, ProviderError, RateGate,
    RecordingIdentityProvider, RecordingMatch,
};

const BASE_URL: &str = "https://musicbrainz.org/ws/2";

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
    /// The `ws/2` root. Only the tests ever change it — pointing the client
    /// at a local stub is the only way to cover the query it builds and the
    /// payload it parses without calling MusicBrainz from a test suite.
    base_url: String,
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
            base_url: BASE_URL.to_string(),
        })
    }

    /// The same client against `base_url`, for tests.
    #[cfg(test)]
    pub(crate) fn against(contact: &str, base_url: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            ..Self::new(contact)?
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
            .get(format!("{}/artist", self.base_url))
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
            .get(format!("{}/recording", self.base_url))
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

    /// A stub MusicBrainz answering `body` with `status`, and recording the
    /// query string it was asked with.
    ///
    /// The point of these tests: the handler suite replaces this client
    /// wholesale with a fake, so the query it builds and the payload it
    /// parses are covered by nothing at all otherwise. Calling the real
    /// service from a test suite would be slow, flaky, and rude to a host
    /// that rate-limits to one request per second.
    async fn stub(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::RawQuery;
        use axum::routing::get;

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = asked.clone();

        let app = axum::Router::new().route(
            "/artist",
            get(move |RawQuery(query): RawQuery| {
                let recorder = recorder.clone();
                async move {
                    recorder.lock().unwrap().push(query.unwrap_or_default());
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (base, asked)
    }

    #[tokio::test]
    async fn given_an_artist_when_searched_then_the_top_hit_is_parsed() {
        let (base, _) = stub(
            200,
            r#"{"artists":[{"id":"mb-1","name":"Miles Davis","score":100}]}"#,
        )
        .await;
        let client = MusicBrainzClient::against("owner@example.com", &base).unwrap();

        let found = client.find_artist("Miles Davis").await.unwrap().unwrap();

        assert_eq!(found.mbid, "mb-1");
        assert_eq!(found.score, 100);
    }

    #[tokio::test]
    async fn given_a_slashed_name_when_searched_then_it_is_sent_as_a_phrase() {
        // `AC/DC` is Lucene syntax. Sent raw it is a malformed query the
        // server answers 400 to, which this client records as retryable — so
        // the band is re-asked, wastefully, on every run and never resolved.
        let (base, asked) = stub(200, r#"{"artists":[]}"#).await;
        let client = MusicBrainzClient::against("owner@example.com", &base).unwrap();

        client.find_artist("AC/DC").await.unwrap();

        let query = asked.lock().unwrap().first().cloned().unwrap();
        assert!(
            query.contains("artist%3A%22AC%2FDC%22"),
            "the name was not sent as a quoted phrase: {query}"
        );
    }

    #[tokio::test]
    async fn given_an_empty_result_when_searched_then_there_is_no_match() {
        let (base, _) = stub(200, r#"{"artists":[]}"#).await;
        let client = MusicBrainzClient::against("owner@example.com", &base).unwrap();

        assert!(client.find_artist("Nobody At All").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn given_a_rate_limit_when_searched_then_it_is_reported_as_one() {
        // Its own error rather than a generic failure: it is the one that
        // says the gate is set wrong, and it reads very differently in a log.
        let (base, _) = stub(429, "").await;
        let client = MusicBrainzClient::against("owner@example.com", &base).unwrap();

        assert_eq!(
            client.find_artist("Miles Davis").await,
            Err(ProviderError::RateLimited)
        );
    }

    #[tokio::test]
    async fn given_an_unreadable_payload_when_searched_then_it_is_unusable() {
        let (base, _) = stub(200, "not json").await;
        let client = MusicBrainzClient::against("owner@example.com", &base).unwrap();

        assert!(matches!(
            client.find_artist("Miles Davis").await,
            Err(ProviderError::Unusable(_))
        ));
    }

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
