//! LRCLIB: lyrics, plain and synchronized.
//!
//! Chosen over the better-known services for a reason that is legal rather
//! than technical. Lyrics are somebody's copyright, and most providers'
//! terms forbid storing what they serve — which would make the cache this
//! design depends on unlawful and force a network round trip on every play.
//! LRCLIB permits retention. Nothing fetched here is redistributed,
//! exported, or written into any backup this application makes; it is shown
//! to the one local owner and cached so it is not re-fetched.

use reqwest::Client;
use serde::Deserialize;

use super::{user_agent, LyricsMatch, LyricsProvider, LyricsQuery, ProviderError};

const GET_URL: &str = "https://lrclib.net/api/get";
const SOURCE: &str = "lrclib";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsResponse {
    #[serde(default)]
    plain_lyrics: Option<String>,
    #[serde(default)]
    synced_lyrics: Option<String>,
}

/// Fetches lyrics from LRCLIB.
pub struct LrclibClient {
    http: Client,
}

impl LrclibClient {
    pub fn new(contact: &str) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(user_agent(contact))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        Ok(Self { http })
    }
}

impl LyricsProvider for LrclibClient {
    async fn lyrics_for(&self, query: &LyricsQuery) -> Result<Option<LyricsMatch>, ProviderError> {
        let mut params: Vec<(&str, String)> = vec![
            ("track_name", query.title.clone()),
            ("artist_name", query.artist.clone()),
        ];
        if let Some(album) = &query.album {
            params.push(("album_name", album.clone()));
        }
        // Duration is what tells a radio edit from an album cut. Sent when
        // the catalog knows it, because a provider matching on title and
        // artist alone will answer with whichever version it happens to hold.
        if let Some(duration) = query.duration_seconds {
            params.push(("duration", duration.to_string()));
        }

        let response = self
            .http
            .get(GET_URL)
            .query(&params)
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        // A 404 is an answer — this recording has no lyrics here — and is
        // reported as `None` so the caller settles it and stops asking.
        // Anything else non-success is the absence of an answer.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited);
        }
        if !response.status().is_success() {
            return Err(ProviderError::Unreachable(format!(
                "status {}",
                response.status()
            )));
        }

        let body: LyricsResponse = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        let found = LyricsMatch {
            plain: body.plain_lyrics,
            synced: body.synced_lyrics,
            source: SOURCE.to_string(),
        };

        // A 200 carrying two blank fields is a "no", not a find.
        Ok(if found.is_empty() { None } else { Some(found) })
    }
}
