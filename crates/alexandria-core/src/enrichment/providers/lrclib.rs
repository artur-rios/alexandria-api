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
const SEARCH_URL: &str = "https://lrclib.net/api/search";
const SOURCE: &str = "lrclib";

/// How far a search result's length may sit from the file's and still be
/// taken for the same recording, in seconds.
///
/// Generous enough for the ragged edges of real files — a few seconds of
/// silence, a different master, a tag rounded to the minute — and far short
/// of what separates versions anyone would notice getting wrong: a radio
/// edit runs a minute under its album cut, a live take further still.
const DURATION_TOLERANCE_SECONDS: i64 = 20;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsResponse {
    #[serde(default)]
    plain_lyrics: Option<String>,
    #[serde(default)]
    synced_lyrics: Option<String>,
}

/// One row of a search answer: the same lyrics fields, plus what they are of.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    duration: Option<f64>,
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
    /// Where a miss on [`Self::get_url`] is asked again, less strictly.
    search_url: String,
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
            search_url: SEARCH_URL.to_string(),
        })
    }

    /// The same client against `get_url`, for tests. The search endpoint is
    /// derived from it, since the stub serves both from one address.
    #[cfg(test)]
    pub(crate) fn against(contact: &str, get_url: &str) -> Result<Self, ProviderError> {
        Ok(Self {
            search_url: get_url.replace("/api/get", "/api/search"),
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

        // A 404 from the exact lookup is not the end of it. `/api/get`
        // matches on the strings it is given, whole — so a file tagged the
        // way real files are tagged misses a recording the service plainly
        // has: `Many Men (Wish Death) (Explicit)` finds nothing where
        // `Many Men (Wish Death)` is sitting there with synced lyrics on it,
        // and an owner whose library carries `(Explicit)`, `(Remastered)` or
        // a different apostrophe in the album title gets nothing for any
        // track, forever. So a miss is asked again, less strictly.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return self.search_for(query).await;
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

impl LrclibClient {
    /// The same recording, looked for rather than named (`/api/search`).
    ///
    /// Free text rather than the fielded parameters: their `track_name` is
    /// as strict in search as it is in `get`, and the whole point here is to
    /// get past a title that does not match character for character.
    ///
    /// What comes back is ranked by their relevance and filtered by ours,
    /// because a search that answers something is worse than one that
    /// answers nothing if the something is the wrong recording — lyrics
    /// against the wrong track are a defect an owner has to notice to
    /// correct. So a candidate has to be recognisably the same artist, and
    /// close enough in length to be the same recording.
    async fn search_for(&self, query: &LyricsQuery) -> Result<Option<LyricsMatch>, ProviderError> {
        self.gate.admit().await;

        let response = self
            .http
            .get(&self.search_url)
            .query(&[("q", format!("{} {}", query.artist, query.title))])
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

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

        let results: Vec<SearchResult> = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        Ok(best_match(query, results))
    }
}

/// The result that is most plausibly the recording `query` describes, or
/// `None` when none of them is.
fn best_match(query: &LyricsQuery, results: Vec<SearchResult>) -> Option<LyricsMatch> {
    results
        .into_iter()
        .filter(|result| result.plain_lyrics.is_some() || result.synced_lyrics.is_some())
        .filter(|result| names_match(&query.artist, &result.artist_name))
        // The title as well as the artist. Their search is free text and
        // ranks by its own relevance, so an artist's *other* songs come back
        // for a title it does not hold — and a track quietly showing another
        // song's words is worse than one showing none, because the owner has
        // to notice before they can correct it.
        .filter(|result| names_match(&query.title, &result.track_name))
        .filter(|result| within_tolerance(query.duration_seconds, result.duration))
        // The closest length wins, and a result with no length at all loses
        // to any that has one: their catalog holds several masters of a
        // popular recording, and length is the only thing that tells them
        // apart.
        .min_by_key(|result| distance(query.duration_seconds, result.duration))
        .map(|result| LyricsMatch {
            plain: result.plain_lyrics,
            synced: result.synced_lyrics,
            source: SOURCE.to_string(),
        })
}

/// Whether two names — two artists, or two titles — are the same thing.
///
/// Containment either way, case-insensitively. Their catalog spells one
/// artist `50 Cent` and `50 CENT`, and a file tagged `50 Cent feat. Nate
/// Dogg` names the same person as a row tagged `50 Cent`; a file titled
/// `Many Men (Wish Death) (Explicit)` is the recording they hold as
/// `Many Men (Wish Death)`. Equality alone refuses all of those, which is
/// the strictness this whole path exists to get past — and containment is
/// as far as it goes, because two names that share no substring are two
/// different things however well a search engine ranks them.
fn names_match(query: &str, candidate: &str) -> bool {
    let query = query.trim().to_lowercase();
    let candidate = candidate.trim().to_lowercase();

    !query.is_empty()
        && !candidate.is_empty()
        && (query.contains(&candidate) || candidate.contains(&query))
}

/// Whether a candidate's length is close enough to the file's to be the same
/// recording. A file whose length the catalog does not know accepts any.
fn within_tolerance(query_seconds: Option<u32>, candidate: Option<f64>) -> bool {
    distance(query_seconds, candidate) <= DURATION_TOLERANCE_SECONDS
}

/// How far apart two lengths are, for ranking. A candidate with no length is
/// ranked last rather than rejected.
fn distance(query_seconds: Option<u32>, candidate: Option<f64>) -> i64 {
    match (query_seconds, candidate) {
        (Some(query), Some(candidate)) => (i64::from(query) - candidate.round() as i64).abs(),
        // Nothing to compare: accepted by `within_tolerance` — a file whose
        // length the catalog does not know must not be refused every
        // candidate — and ranked last here, behind anything that can be
        // measured.
        _ => i64::MAX,
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

    /// A stub answering `/api/get` with `get_status`/`get_body` and
    /// `/api/search` with `search_body`, recording what each was asked.
    ///
    /// Two endpoints because the fallback is the interesting path: a miss on
    /// the first has to become a question to the second, and nothing else in
    /// the suite would notice if it stopped.
    async fn stub_with_search(
        get_status: u16,
        get_body: &'static str,
        search_body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::RawQuery;
        use axum::routing::get;

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let get_recorder = asked.clone();
        let search_recorder = asked.clone();

        let app = axum::Router::new()
            .route(
                "/api/get",
                get(move |RawQuery(query): RawQuery| {
                    let recorder = get_recorder.clone();
                    async move {
                        recorder
                            .lock()
                            .unwrap()
                            .push(format!("get?{}", query.unwrap_or_default()));
                        (
                            axum::http::StatusCode::from_u16(get_status).unwrap(),
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            get_body,
                        )
                    }
                }),
            )
            .route(
                "/api/search",
                get(move |RawQuery(query): RawQuery| {
                    let recorder = search_recorder.clone();
                    async move {
                        recorder
                            .lock()
                            .unwrap()
                            .push(format!("search?{}", query.unwrap_or_default()));
                        (
                            axum::http::StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            search_body,
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

    /// The owner's own case, which is what this whole path is for: a file
    /// tagged `Many Men (Wish Death) (Explicit)` on `Get Rich Or Die Tryin'`
    /// against a service holding `Many Men (Wish Death)`.
    fn a_tagged_query() -> LyricsQuery {
        LyricsQuery {
            title: "Many Men (Wish Death) (Explicit)".to_string(),
            artist: "50 Cent".to_string(),
            album: Some("Get Rich Or Die Tryin\'".to_string()),
            duration_seconds: Some(256),
        }
    }

    const SEARCH_HIT: &str = r#"[
        {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":256.3,
         "plainLyrics":"many men","syncedLyrics":"[00:01.00] many men"}
    ]"#;

    #[tokio::test]
    async fn given_the_exact_lookup_misses_when_fetched_then_it_is_searched_for() {
        // The defect an owner reported as "lyrics never work": `/api/get`
        // matches the strings it is given, whole, and no real library is
        // tagged the way a catalog spells things.
        let (url, asked) = stub_with_search(404, "{}", SEARCH_HIT).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        let found = client.lyrics_for(&a_tagged_query()).await.unwrap();

        let asked = asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 2, "the miss must become a second question");
        assert!(asked[1].starts_with("search?"), "{asked:?}");
        assert!(
            asked[1].contains("50+Cent") && asked[1].contains("Many+Men"),
            "the search asks in free text, artist and title together: {asked:?}"
        );
        assert_eq!(
            found.map(|found| found.synced),
            Some(Some("[00:01.00] many men".to_string()))
        );
    }

    #[tokio::test]
    async fn given_the_exact_lookup_answers_when_fetched_then_nothing_is_searched() {
        let (url, asked) = stub_with_search(200, r#"{"plainLyrics":"a line"}"#, SEARCH_HIT).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        client.lyrics_for(&a_tagged_query()).await.unwrap().unwrap();

        assert_eq!(
            asked.lock().unwrap().len(),
            1,
            "a hit must cost one request, not two"
        );
    }

    #[tokio::test]
    async fn given_a_search_hit_by_another_artist_when_fetched_then_it_is_refused() {
        // Lyrics against the wrong track are worse than none: the owner has
        // to notice before they can correct it.
        const OTHER: &str = r#"[
            {"trackName":"Many Men","artistName":"Someone Else","duration":256.0,
             "plainLyrics":"not this"}
        ]"#;
        let (url, _) = stub_with_search(404, "{}", OTHER).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert!(client
            .lyrics_for(&a_tagged_query())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn given_a_search_hit_of_another_song_when_fetched_then_it_is_refused() {
        // Their search is free text and ranks by its own relevance, so an
        // artist's other songs come back for a title they do not hold. A
        // track quietly showing another song's words is the worst outcome
        // available here.
        const ANOTHER_SONG: &str = r#"[
            {"trackName":"In Da Club","artistName":"50 Cent","duration":253.0,
             "plainLyrics":"go shorty"}
        ]"#;
        let (url, _) = stub_with_search(404, "{}", ANOTHER_SONG).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert!(client
            .lyrics_for(&a_tagged_query())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn given_a_search_hit_of_another_length_when_fetched_then_it_is_refused() {
        // A radio edit is not the album cut, and their catalog holds both.
        const EDIT: &str = r#"[
            {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":180.0,
             "plainLyrics":"the short one"}
        ]"#;
        let (url, _) = stub_with_search(404, "{}", EDIT).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert!(client
            .lyrics_for(&a_tagged_query())
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn given_several_masters_when_searched_then_the_closest_length_wins() {
        // Their catalog holds a handful of masters of a popular recording,
        // seconds apart. Length is the only thing that tells them apart.
        const MASTERS: &str = r#"[
            {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":268.0,
             "plainLyrics":"the far one"},
            {"trackName":"Many Men (Wish Death)","artistName":"50 CENT","duration":256.2,
             "plainLyrics":"the near one"},
            {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":262.0,
             "plainLyrics":"the middle one"}
        ]"#;
        let (url, _) = stub_with_search(404, "{}", MASTERS).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        let found = client.lyrics_for(&a_tagged_query()).await.unwrap().unwrap();

        assert_eq!(found.plain.as_deref(), Some("the near one"));
    }

    #[tokio::test]
    async fn given_a_search_row_with_no_lyrics_when_searched_then_it_is_passed_over() {
        // A row in their catalog with neither form is a recording nobody has
        // contributed words for. Taking it would settle the track as found
        // and show the owner nothing.
        const EMPTY_THEN_FULL: &str = r#"[
            {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":256.1},
            {"trackName":"Many Men (Wish Death)","artistName":"50 Cent","duration":257.0,
             "plainLyrics":"the words"}
        ]"#;
        let (url, _) = stub_with_search(404, "{}", EMPTY_THEN_FULL).await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        let found = client.lyrics_for(&a_tagged_query()).await.unwrap().unwrap();

        assert_eq!(found.plain.as_deref(), Some("the words"));
    }

    #[tokio::test]
    async fn given_the_search_finds_nothing_when_fetched_then_it_is_a_clean_no() {
        // Still `None`, not an error: a recording nobody has words for is an
        // answer, and the caller settles it rather than asking forever.
        let (url, _) = stub_with_search(404, "{}", "[]").await;
        let client = LrclibClient::against("owner@example.com", &url).unwrap();

        assert!(client
            .lyrics_for(&a_tagged_query())
            .await
            .unwrap()
            .is_none());
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
