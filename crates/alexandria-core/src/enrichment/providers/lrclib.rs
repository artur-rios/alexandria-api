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

use super::{user_agent, LyricsMatch, LyricsProvider, LyricsQuery, ProviderError, RateGate};

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
    /// The endpoint. Only the tests ever change it — see
    /// `MusicBrainzClient::base_url`.
    get_url: String,
    /// LRCLIB publishes no rate limit, which is why this is here rather than
    /// left out: a run over a large library would otherwise burst every
    /// track at it at once, earn a 429, and record all of them as retryable
    /// so the next run bursts again.
    gate: RateGate,
}

impl LrclibClient {
    pub fn new(contact: &str) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(user_agent(contact))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        Ok(Self {
            http,
            gate: RateGate::courteous(),
            get_url: GET_URL.to_string(),
        })
    }

    /// The same client against `get_url`, for tests.
    #[cfg(test)]
    pub(crate) fn against(contact: &str, get_url: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            get_url: get_url.to_string(),
            ..Self::new(contact)?
        })
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

        self.gate.admit().await;

        let response = self
            .http
            .get(&self.get_url)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub LRCLIB answering `body` with `status`, recording the query.
    async fn stub(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::RawQuery;
        use axum::routing::get;

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorder = asked.clone();

        let app = axum::Router::new().route(
            "/api/get",
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
        let url = format!("http://{}/api/get", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (url, asked)
    }

    fn a_query() -> LyricsQuery {
        LyricsQuery {
            title: "So What".to_string(),
            artist: "Miles Davis".to_string(),
            album: Some("Kind of Blue".to_string()),
            duration_seconds: Some(545),
        }
    }

    #[tokio::test]
    async fn given_lyrics_when_fetched_then_both_forms_are_parsed() {
        // The wire field names are `plainLyrics` and `syncedLyrics`, which
        // nothing but this test checks: the handler suite hands the command a
        // fake, so a rename on their side would be invisible until a real
        // lookup returned nothing.
        let (url, _) = stub(
            200,
            r#"{"plainLyrics":"first line\nsecond line","syncedLyrics":"[00:01.00] first line"}"#,
        )
        .await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        let found = client.lyrics_for(&a_query()).await.unwrap().unwrap();

        assert_eq!(found.plain.as_deref(), Some("first line\nsecond line"));
        assert_eq!(found.synced.as_deref(), Some("[00:01.00] first line"));
        assert_eq!(found.source, SOURCE);
    }

    #[tokio::test]
    async fn given_a_track_when_fetched_then_the_duration_is_on_the_wire() {
        // Duration is what separates a radio edit from an album cut. Sent
        // under the name their API takes, in whole seconds.
        let (url, asked) = stub(200, r#"{"plainLyrics":"a line"}"#).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        client.lyrics_for(&a_query()).await.unwrap();

        let query = asked.lock().unwrap().first().cloned().unwrap();
        assert!(query.contains("duration=545"), "{query}");
        assert!(query.contains("track_name=So+What"), "{query}");
        assert!(query.contains("artist_name=Miles+Davis"), "{query}");
    }

    #[tokio::test]
    async fn given_no_duration_when_fetched_then_it_is_simply_absent() {
        // A library indexed before the duration column existed still looks
        // up; the field is omitted rather than sent empty.
        let (url, asked) = stub(200, r#"{"plainLyrics":"a line"}"#).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        let query = LyricsQuery {
            duration_seconds: None,
            ..a_query()
        };
        client.lyrics_for(&query).await.unwrap();

        assert!(!asked.lock().unwrap()[0].contains("duration"));
    }

    #[tokio::test]
    async fn given_no_such_recording_when_fetched_then_it_is_an_answer_not_a_failure() {
        // A 404 here means "this recording has no lyrics with us", which the
        // caller settles and never re-asks. Treated as a failure it would be
        // retried forever at a request a time.
        let (url, _) = stub(404, "").await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert_eq!(client.lyrics_for(&a_query()).await, Ok(None));
    }

    #[tokio::test]
    async fn given_a_blank_body_when_fetched_then_there_is_nothing_to_store() {
        // A 200 carrying two empty fields is a "no". Stored as a find it
        // would cache an empty string and never ask again.
        let (url, _) = stub(200, r#"{"plainLyrics":"   ","syncedLyrics":null}"#).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert_eq!(client.lyrics_for(&a_query()).await, Ok(None));
    }

    #[tokio::test]
    async fn given_a_rate_limit_when_fetched_then_it_is_reported_as_one() {
        let (url, _) = stub(429, "").await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert_eq!(
            client.lyrics_for(&a_query()).await,
            Err(ProviderError::RateLimited)
        );
    }
}
