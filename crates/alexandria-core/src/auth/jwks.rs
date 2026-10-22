//! Heimdall's published signing keys, fetched and cached (UC-36 / FR-AU-02).
//!
//! Heimdall signs its tokens ES256 and publishes the public halves of its
//! keys as an RFC 7517 key set at `/.well-known/jwks.json`: the current key,
//! then the previous one while a rotation is in progress. Each token names its
//! key by `kid`, the key's RFC 7638 thumbprint.
//!
//! The set is fetched lazily and kept. It is fetched again when a token names
//! a key the cache does not hold — a rotation has started — or when the cached
//! set is older than [`MAX_AGE`], so a key Heimdall has withdrawn stops
//! verifying. Either refetch happens at most once per [`MIN_REFETCH_INTERVAL`],
//! whatever the tokens say: a caller cannot turn made-up `kid`s into a stream
//! of requests to Heimdall, which rate-limits the endpoint anyway. A fetch
//! that fails keeps the keys already known, so Heimdall being briefly
//! unreachable does not black out Alexandria.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::DecodingKey;
use tokio::sync::Mutex;

/// The longest a fetched key set is trusted before it is fetched again. The
/// same five minutes Heimdall's `Cache-Control: max-age` gives it.
pub const MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// The shortest gap between two fetches, successful or not.
pub const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// How long one fetch may take. It runs on a request path, so it is bounded
/// well below any client's own timeout.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a key set's JSON comes from: Heimdall over HTTP in production, a
/// canned answer in tests.
pub trait JwksSource: Send + Sync {
    /// The key set document, or why it could not be had.
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;
}

/// Fetches the key set from Heimdall's `/.well-known/jwks.json`.
pub struct HttpJwksSource {
    client: reqwest::Client,
    url: String,
}

impl HttpJwksSource {
    pub fn new(url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            // Only a TLS backend that cannot initialise fails here, and the
            // default client fails the same way on its first request instead.
            .unwrap_or_default();
        Self {
            client,
            url: url.into(),
        }
    }
}

impl JwksSource for HttpJwksSource {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let response = self
                .client
                .get(&self.url)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("answered {status}"));
            }
            response.text().await.map_err(|e| e.to_string())
        })
    }
}

/// A monotonic clock, injected so the refetch intervals can be tested without
/// waiting them out.
pub type MonotonicClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The keys known so far, and when the set was last fetched and last asked
/// for.
#[derive(Default)]
struct State {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
}

/// Heimdall's signing keys by `kid`, fetched on demand.
pub struct JwksCache {
    source: Box<dyn JwksSource>,
    now: MonotonicClock,
    // A tokio mutex, held across the fetch: requests that arrive while one is
    // in flight wait for its answer rather than starting their own.
    state: Mutex<State>,
}

impl JwksCache {
    pub fn new(source: Box<dyn JwksSource>, now: MonotonicClock) -> Self {
        Self {
            source,
            now,
            state: Mutex::new(State::default()),
        }
    }

    /// Fetches from Heimdall at `url`, on the system clock.
    pub fn over_http(url: &str) -> Self {
        Self::new(Box::new(HttpJwksSource::new(url)), Arc::new(Instant::now))
    }

    /// The verifying key named `kid`, fetching the set first when the cache
    /// does not hold it or has grown stale and a fetch is not too recent.
    /// `None` when no such key is known, which refuses the token.
    pub async fn key(&self, kid: &str) -> Option<DecodingKey> {
        let mut state = self.state.lock().await;
        let now = (self.now)();

        let stale = state
            .fetched_at
            .is_none_or(|at| now.duration_since(at) >= MAX_AGE);
        let wanted = stale || !state.keys.contains_key(kid);
        let allowed = state
            .attempted_at
            .is_none_or(|at| now.duration_since(at) >= MIN_REFETCH_INTERVAL);

        if wanted && allowed {
            state.attempted_at = Some(now);
            match self
                .source
                .fetch()
                .await
                .and_then(|body| parse_key_set(&body))
            {
                Ok(keys) => {
                    state.keys = keys;
                    state.fetched_at = Some(now);
                }
                Err(reason) => {
                    tracing::warn!(
                        "Heimdall's key set could not be fetched; keeping the {} key(s) \
                         already known: {reason}",
                        state.keys.len()
                    );
                }
            }
        }

        state.keys.get(kid).cloned()
    }
}

/// The ES256 verifying keys in a key set document, by `kid`.
///
/// Each key is read on its own, so a key in a shape this does not know — a
/// future algorithm, an encryption key — is skipped rather than failing the
/// whole set. Only a P-256 elliptic-curve key with a `kid`, meant for
/// signatures and, when it says, for ES256, is kept. A document with no such
/// key is an error, so the keys already known are not replaced by nothing.
fn parse_key_set(body: &str) -> Result<HashMap<String, DecodingKey>, String> {
    #[derive(serde::Deserialize)]
    struct Document {
        keys: Vec<serde_json::Value>,
    }

    let document: Document =
        serde_json::from_str(body).map_err(|e| format!("not a key set: {e}"))?;

    let keys: HashMap<String, DecodingKey> = document
        .keys
        .into_iter()
        .filter_map(|value| serde_json::from_value::<Jwk>(value).ok())
        .filter_map(|jwk| {
            let is_p256 = matches!(
                &jwk.algorithm,
                AlgorithmParameters::EllipticCurve(params) if params.curve == EllipticCurve::P256
            );
            let for_signing = jwk
                .common
                .public_key_use
                .as_ref()
                .is_none_or(|usage| *usage == PublicKeyUse::Signature);
            let for_es256 = jwk
                .common
                .key_algorithm
                .is_none_or(|alg| alg == KeyAlgorithm::ES256);
            if !(is_p256 && for_signing && for_es256) {
                return None;
            }
            let kid = jwk.common.key_id.clone().filter(|kid| !kid.is_empty())?;
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            Some((kid, key))
        })
        .collect();

    if keys.is_empty() {
        return Err("the key set holds no ES256 key".to_string());
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use super::*;

    const KEY_A: &str = r#"{"kty":"EC","crv":"P-256","x":"xNYqHxZs5guFNq5tp_QAZDmyzd-rfqKOFuiH_X5XUOc","y":"Kj34xYASE06sFgLfB1lUbvGKeM7Z-p7ktA82965vYdU","kid":"key-a","use":"sig","alg":"ES256"}"#;

    #[test]
    fn given_a_key_set_when_parsed_then_its_es256_keys_are_kept_by_kid() {
        let keys = parse_key_set(&format!(r#"{{"keys":[{KEY_A}]}}"#)).expect("parsed");
        assert_eq!(keys.keys().collect::<Vec<_>>(), ["key-a"]);
    }

    #[test]
    fn given_keys_of_other_shapes_when_parsed_then_only_the_es256_one_is_kept() {
        let body = format!(
            r#"{{"keys":[
                {{"kty":"oct","k":"c2VjcmV0","kid":"symmetric"}},
                {{"kty":"RSA","n":"AQAB","e":"AQAB","kid":"rsa","alg":"RS256"}},
                {{"kty":"EC","crv":"P-384","x":"AA","y":"AA","kid":"p384"}},
                {{"kty":"EC","crv":"P-256","x":"xNYqHxZs5guFNq5tp_QAZDmyzd-rfqKOFuiH_X5XUOc","y":"Kj34xYASE06sFgLfB1lUbvGKeM7Z-p7ktA82965vYdU","kid":"for-encryption","use":"enc"}},
                {{"kty":"EC","crv":"P-256","x":"xNYqHxZs5guFNq5tp_QAZDmyzd-rfqKOFuiH_X5XUOc","y":"Kj34xYASE06sFgLfB1lUbvGKeM7Z-p7ktA82965vYdU"}},
                {{"something":"unknown"}},
                {KEY_A}
            ]}}"#
        );
        let keys = parse_key_set(&body).expect("parsed");
        assert_eq!(keys.keys().collect::<Vec<_>>(), ["key-a"]);
    }

    #[test]
    fn given_a_set_with_no_usable_key_when_parsed_then_it_is_an_error() {
        assert!(parse_key_set(r#"{"keys":[]}"#).is_err());
        assert!(parse_key_set("not json").is_err());
    }

    /// Answers each fetch with the next scripted result, counting fetches.
    struct Scripted {
        answers: StdMutex<Vec<Result<String, String>>>,
        fetches: AtomicUsize,
    }

    impl JwksSource for Arc<Scripted> {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            let answer = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            };
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn given_a_cached_set_when_the_same_key_is_asked_for_then_it_is_not_fetched_again() {
        let source = Arc::new(Scripted {
            answers: StdMutex::new(vec![Ok(format!(r#"{{"keys":[{KEY_A}]}}"#))]),
            fetches: AtomicUsize::new(0),
        });
        let start = Instant::now();
        let elapsed = Arc::new(StdMutex::new(Duration::ZERO));
        let clock = elapsed.clone();
        let cache = JwksCache::new(
            Box::new(source.clone()),
            Arc::new(move || start + *clock.lock().unwrap()),
        );

        assert!(cache.key("key-a").await.is_some());
        *elapsed.lock().unwrap() = Duration::from_secs(120);
        assert!(cache.key("key-a").await.is_some());

        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn given_a_stale_set_when_the_fetch_fails_then_the_known_keys_still_verify() {
        let source = Arc::new(Scripted {
            answers: StdMutex::new(vec![
                Ok(format!(r#"{{"keys":[{KEY_A}]}}"#)),
                Err("connection refused".to_string()),
            ]),
            fetches: AtomicUsize::new(0),
        });
        let start = Instant::now();
        let elapsed = Arc::new(StdMutex::new(Duration::ZERO));
        let clock = elapsed.clone();
        let cache = JwksCache::new(
            Box::new(source.clone()),
            Arc::new(move || start + *clock.lock().unwrap()),
        );
        assert!(cache.key("key-a").await.is_some());

        *elapsed.lock().unwrap() = MAX_AGE;
        assert!(cache.key("key-a").await.is_some());

        assert_eq!(
            source.fetches.load(Ordering::SeqCst),
            2,
            "the stale set was asked for again"
        );
    }

    /// The HTTP source against a one-shot server, so the request it makes and
    /// the way it reads the answer are exercised for real.
    #[tokio::test]
    async fn given_a_server_answering_a_key_set_when_fetched_over_http_then_the_body_is_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = format!(r#"{{"keys":[{KEY_A}]}}"#);
        let served = body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 1024];
            let read = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{served}",
                served.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });

        let source = HttpJwksSource::new(format!("http://{address}/.well-known/jwks.json"));
        let fetched = source.fetch().await.expect("fetched");

        assert_eq!(fetched, body);
        let request = server.await.unwrap();
        assert!(
            request.starts_with("GET /.well-known/jwks.json HTTP/1.1"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn given_a_server_answering_an_error_when_fetched_over_http_then_it_is_a_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let source = HttpJwksSource::new(format!("http://{address}/.well-known/jwks.json"));
        let error = source
            .fetch()
            .await
            .expect_err("an error status is a failure");

        assert!(error.contains("429"), "{error}");
    }
}
