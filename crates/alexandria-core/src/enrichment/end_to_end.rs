//! One successful lookup, all the way through, against local stubs.
//!
//! The gap this closes: every other test of this feature replaces the three
//! service clients with fakes, so the path where a *real* client parses a
//! *real* HTTP response and the result is stored and read back had never run
//! anywhere. Each piece was covered; their composition was not.
//!
//! The services are stubbed rather than called. Calling them for real would
//! be slow, flaky, and rude to MusicBrainz, which rate-limits to one request
//! per second — and it would prove nothing this does not, since what is
//! being checked is that these four parties agree on names, shapes and
//! order, not that the internet exists.

use std::sync::Arc;
use std::sync::Mutex;

use axum::extract::RawQuery;
use axum::routing::get;

use crate::auth::{AuthService, Principal};
use crate::catalog::clock::SystemClock;
use crate::catalog::model::{FileType, NewFile, SubtypeMetadata};
use crate::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use crate::config::{AuthMode, MetadataSettings};
use crate::enrichment::commands::{EnrichHandler, FsArtistImageStore};
use crate::enrichment::model::{EnrichmentOutcome, EnrichmentScope};
use crate::enrichment::providers::commons::CommonsImageClient;
use crate::enrichment::providers::lrclib::LrclibClient;
use crate::enrichment::providers::musicbrainz::MusicBrainzClient;
use crate::enrichment::queries::ReadEnrichmentHandler;
use crate::enrichment::repos::SqliteEnrichmentRepository;
use crate::errors::DomainError;
use crate::migrate::migrate_database;

/// Authenticates anyone; this file is about the wire, not about auth.
#[derive(Clone, Copy)]
struct AllowAll;

impl AuthService for AllowAll {
    async fn authenticate(&self, _token: &str) -> Result<Principal, DomainError> {
        Ok(Principal {
            user_id: "owner".to_string(),
        })
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}

/// A 1x1 PNG — enough to be real bytes that land on disk with a length.
const PIXEL: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1, 13, 10,
    45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Every path the three clients reach, served locally.
async fn services() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));

    let mb = seen.clone();
    let wd = seen.clone();
    let lr = seen.clone();

    let app = axum::Router::new()
        .route(
            "/ws/2/artist",
            get(move || {
                mb.lock().unwrap().push("mb:artist".to_string());
                async {
                    axum::Json(serde_json::json!({
                        "artists": [{"id": "mb-artist", "name": "Miles Davis", "score": 100}]
                    }))
                }
            }),
        )
        .route(
            "/ws/2/recording",
            get(|| async {
                axum::Json(serde_json::json!({
                    "recordings": [{"id": "mb-recording", "score": 100}]
                }))
            }),
        )
        // Wikidata answers two different questions on one path, told apart by
        // `action` — the search that finds the entity carrying the
        // MusicBrainz id, then the claim that names the Commons file.
        .route(
            "/w/api.php",
            get(move |RawQuery(query): RawQuery| {
                let query = query.unwrap_or_default();
                wd.lock().unwrap().push(format!("wd:{query}"));
                async move {
                    if query.contains("wbgetclaims") {
                        axum::Json(serde_json::json!({
                            "claims": {"P18": [{"mainsnak": {"datavalue":
                                {"value": "Miles Davis 1955.jpg"}}}]}
                        }))
                    } else {
                        axum::Json(serde_json::json!({
                            "query": {"search": [{"title": "Q101"}]}
                        }))
                    }
                }
            }),
        )
        .route(
            "/wiki/Special:FilePath/{file}",
            get(|| async { ([(axum::http::header::CONTENT_TYPE, "image/png")], PIXEL) }),
        )
        .route(
            "/api/get",
            get(move || {
                lr.lock().unwrap().push("lrclib".to_string());
                async {
                    axum::Json(serde_json::json!({
                        "plainLyrics": "first line\nsecond line",
                        "syncedLyrics": "[00:01.00] first line"
                    }))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (base, seen)
}

#[tokio::test]
async fn given_the_services_answer_when_a_track_is_enriched_then_it_is_stored_and_readable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");
    let catalog = SqliteCatalogRepository::new(pool.clone());

    let file_uuid = uuid::Uuid::new_v4();
    catalog
        .insert_file(NewFile {
            uuid: file_uuid,
            path: "/library/so-what.flac".to_string(),
            name: "so-what.flac".to_string(),
            file_type: FileType::Audio,
            content_hash: Some("0".repeat(64)),
            size_bytes: None,
            mtime: None,
            indexed_at: chrono::Utc::now(),
        })
        .await
        .expect("insert");
    catalog
        .update_metadata(
            file_uuid,
            &SubtypeMetadata::Audio {
                title: Some("So What".to_string()),
                artist: Some("Miles Davis".to_string()),
                album: Some("Kind of Blue".to_string()),
                year: Some(1959),
                genre: None,
                track: Some(1),
                album_artist: Some("Miles Davis".to_string()),
            },
        )
        .await
        .expect("metadata");

    let (base, seen) = services().await;
    let images = dir.path().join("artist-images");

    let handler = EnrichHandler::new(
        AllowAll,
        SqliteEnrichmentRepository::new(pool.clone()),
        MusicBrainzClient::against("owner@example.com", &format!("{base}/ws/2")).unwrap(),
        CommonsImageClient::against(
            "owner@example.com",
            &format!("{base}/w/api.php"),
            &format!("{base}/wiki/Special:FilePath"),
        )
        .unwrap(),
        LrclibClient::against("owner@example.com", &format!("{base}/api/get")).unwrap(),
        FsArtistImageStore::new(&images),
        SystemClock,
        MetadataSettings {
            enabled: true,
            contact: "owner@example.com".to_string(),
            image_cache_dir: images.to_string_lossy().into_owned(),
        },
    );

    let report = handler
        .enrich(EnrichmentScope::pending(), "token")
        .await
        .expect("the run failed");

    // One artist image and one track's lyrics.
    assert_eq!(report.found, 2, "report: {report:?}");
    assert_eq!(report.failed, 0);

    // The bytes actually landed on disk, under the artist's MusicBrainz id.
    let stored_image = images.join("mb-artist.jpg");
    assert!(stored_image.exists(), "no image was written");
    assert_eq!(
        std::fs::read(&stored_image).expect("read").len(),
        PIXEL.len()
    );

    // And the whole thing reads back through the query a client calls,
    // with the path resolved to something openable.
    let view = ReadEnrichmentHandler::new(
        AllowAll,
        SqliteEnrichmentRepository::new(pool.clone()),
        &images,
    )
    .read(file_uuid, Some("Miles Davis"), "token")
    .await
    .expect("read");

    let image = view.artist_image.expect("an image");
    assert_eq!(image.outcome, EnrichmentOutcome::Found);
    assert_eq!(image.mbid.as_deref(), Some("mb-artist"));
    assert_eq!(image.image_path.as_deref(), stored_image.to_str());
    assert!(
        image.source_url.expect("a credit").contains("Miles_Davis"),
        "the attribution was lost"
    );

    let lyrics = view.lyrics.expect("lyrics");
    assert_eq!(lyrics.plain.as_deref(), Some("first line\nsecond line"));
    assert_eq!(lyrics.synced.as_deref(), Some("[00:01.00] first line"));
    assert_eq!(lyrics.source.as_deref(), Some("lrclib"));
    // The recording is identified only because lyrics were found.
    assert_eq!(lyrics.mbid.as_deref(), Some("mb-recording"));

    // Every service was actually reached: a stub nothing calls proves
    // nothing at all.
    let calls = seen.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c == "mb:artist"), "{calls:?}");
    assert!(calls.iter().any(|c| c.starts_with("wd:")), "{calls:?}");
    assert!(calls.iter().any(|c| c == "lrclib"), "{calls:?}");
}

#[tokio::test]
async fn given_a_second_run_when_nothing_changed_then_the_services_are_not_re_asked() {
    // Resumability, proved through the real clients rather than a fake: a
    // second sweep over an already-enriched library must spend no requests.
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");
    let catalog = SqliteCatalogRepository::new(pool.clone());

    let file_uuid = uuid::Uuid::new_v4();
    catalog
        .insert_file(NewFile {
            uuid: file_uuid,
            path: "/library/so-what.flac".to_string(),
            name: "so-what.flac".to_string(),
            file_type: FileType::Audio,
            content_hash: Some("0".repeat(64)),
            size_bytes: None,
            mtime: None,
            indexed_at: chrono::Utc::now(),
        })
        .await
        .expect("insert");
    catalog
        .update_metadata(
            file_uuid,
            &SubtypeMetadata::Audio {
                title: Some("So What".to_string()),
                artist: Some("Miles Davis".to_string()),
                album: None,
                year: None,
                genre: None,
                track: None,
                album_artist: Some("Miles Davis".to_string()),
            },
        )
        .await
        .expect("metadata");

    let (base, seen) = services().await;
    let images = dir.path().join("artist-images");

    let build = || {
        EnrichHandler::new(
            AllowAll,
            SqliteEnrichmentRepository::new(pool.clone()),
            MusicBrainzClient::against("owner@example.com", &format!("{base}/ws/2")).unwrap(),
            CommonsImageClient::against(
                "owner@example.com",
                &format!("{base}/w/api.php"),
                &format!("{base}/wiki/Special:FilePath"),
            )
            .unwrap(),
            LrclibClient::against("owner@example.com", &format!("{base}/api/get")).unwrap(),
            FsArtistImageStore::new(&images),
            SystemClock,
            MetadataSettings {
                enabled: true,
                contact: "owner@example.com".to_string(),
                image_cache_dir: images.to_string_lossy().into_owned(),
            },
        )
    };

    build()
        .enrich(EnrichmentScope::pending(), "token")
        .await
        .expect("first run");
    let after_first = seen.lock().unwrap().len();

    let report = build()
        .enrich(EnrichmentScope::pending(), "token")
        .await
        .expect("second run");

    assert_eq!(
        seen.lock().unwrap().len(),
        after_first,
        "the second run asked the services again"
    );
    assert_eq!(report.considered, 0, "the file was still pending");
}

#[tokio::test]
async fn given_an_artist_name_when_its_image_is_asked_for_then_it_is_stored_under_that_name() {
    // The defect this closes, end to end. A run over a *file* looks the
    // artist up under whatever that file is tagged with, and a client's
    // artists list is grouped by a name it worked out across every track of
    // the record — so a track tagged `Miles Davis Quintet` with no album
    // artist stored a picture the list, showing `Miles Davis`, would never
    // find. Fetched, paid for, and never shown.
    //
    // Nothing here inserts a file at all, which is the point: the name is the
    // whole input, and the picture has to come back under it.
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");

    let (base, seen) = services().await;
    let images = dir.path().join("artist-images");

    let handler = EnrichHandler::new(
        AllowAll,
        SqliteEnrichmentRepository::new(pool.clone()),
        MusicBrainzClient::against("owner@example.com", &format!("{base}/ws/2")).unwrap(),
        CommonsImageClient::against(
            "owner@example.com",
            &format!("{base}/w/api.php"),
            &format!("{base}/wiki/Special:FilePath"),
        )
        .unwrap(),
        LrclibClient::against("owner@example.com", &format!("{base}/api/get")).unwrap(),
        FsArtistImageStore::new(&images),
        SystemClock,
        MetadataSettings {
            enabled: true,
            contact: "owner@example.com".to_string(),
            image_cache_dir: images.to_string_lossy().into_owned(),
        },
    );

    let fetched = handler
        .artist_image("Miles Davis", "token")
        .await
        .expect("the lookup failed");

    assert_eq!(fetched.outcome, EnrichmentOutcome::Found);
    assert_eq!(fetched.artist_name, "Miles Davis");
    assert!(
        images.join("mb-artist.jpg").exists(),
        "no image was written"
    );

    // And it reads back by the same name, with the path resolved to
    // something a client can open — which is the half the artists list uses.
    let read = ReadEnrichmentHandler::new(
        AllowAll,
        SqliteEnrichmentRepository::new(pool.clone()),
        &images,
    )
    .artist_image("Miles Davis", "token")
    .await
    .expect("read")
    .expect("an image");

    assert_eq!(
        read.image_path.as_deref(),
        images.join("mb-artist.jpg").to_str()
    );

    // Asked again, nothing is re-fetched: a settled row is the answer, which
    // is what keeps a library of five hundred artists from being five
    // hundred requests every session.
    let before = seen.lock().expect("not poisoned").len();
    let again = handler
        .artist_image("Miles Davis", "token")
        .await
        .expect("the second lookup failed");

    assert_eq!(again.outcome, EnrichmentOutcome::Found);
    assert_eq!(
        seen.lock().expect("not poisoned").len(),
        before,
        "the services were asked again for a picture already in hand"
    );
}

#[tokio::test]
async fn given_no_name_when_an_image_is_asked_for_then_it_is_refused() {
    // Searching on nothing returns whatever is most popular under the empty
    // string, which is how a library ends up with a confidently wrong face.
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");

    let (base, _) = services().await;
    let images = dir.path().join("artist-images");
    let handler = EnrichHandler::new(
        AllowAll,
        SqliteEnrichmentRepository::new(pool.clone()),
        MusicBrainzClient::against("owner@example.com", &format!("{base}/ws/2")).unwrap(),
        CommonsImageClient::against(
            "owner@example.com",
            &format!("{base}/w/api.php"),
            &format!("{base}/wiki/Special:FilePath"),
        )
        .unwrap(),
        LrclibClient::against("owner@example.com", &format!("{base}/api/get")).unwrap(),
        FsArtistImageStore::new(&images),
        SystemClock,
        MetadataSettings {
            enabled: true,
            contact: "owner@example.com".to_string(),
            image_cache_dir: images.to_string_lossy().into_owned(),
        },
    );

    let refused = handler.artist_image("   ", "token").await;

    assert!(matches!(refused, Err(DomainError::InvalidInput(_))));
}

/// What the image client refuses, against a stub that answers badly.
///
/// The two directions the download can go wrong, and both were found by
/// review rather than by a failure: a response with no declared length was
/// read into memory in full before the cap could judge it, and a picture in a
/// format nothing can draw was stored and settled as `Found` — leaving the
/// artist showing a placeholder for good, with no retry and nothing anywhere
/// saying why.
mod image_refusals {
    use super::*;
    use crate::enrichment::providers::{ArtistImageProvider, ProviderError};

    /// The stub, with the P18 file name and the body under the caller's
    /// control. `chunked` answers a body with no `Content-Length` at all,
    /// which is what a real Commons rendering often is and what made the
    /// declared-length check the only one that ran before the read.
    async fn commons(file_name: &'static str, body_bytes: usize, chunked: bool) -> String {
        let app = axum::Router::new()
            .route(
                "/w/api.php",
                get(move |RawQuery(query): RawQuery| {
                    let query: String = query.unwrap_or_default();
                    async move {
                        if query.contains("wbgetclaims") {
                            axum::Json(serde_json::json!({
                                "claims": {"P18": [{"mainsnak": {"datavalue":
                                    {"value": file_name}}}]}
                            }))
                        } else {
                            axum::Json(serde_json::json!({
                                "query": {"search": [{"title": "Q101"}]}
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wiki/Special:FilePath/{file}",
                get(move || async move {
                    let bytes = vec![0u8; body_bytes];
                    if chunked {
                        // A stream body carries no Content-Length, so the
                        // cap has nothing to check before the read begins.
                        axum::body::Body::from_stream(futures_util::stream::once(async move {
                            Ok::<_, std::io::Error>(bytes)
                        }))
                    } else {
                        axum::body::Body::from(bytes)
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    fn client(base: &str) -> CommonsImageClient {
        CommonsImageClient::against(
            "owner@example.com",
            &format!("{base}/w/api.php"),
            &format!("{base}/wiki/Special:FilePath"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn given_a_vector_picture_when_fetched_then_it_is_answered_as_none() {
        // `Ok(None)`, not an error: "there is a picture nothing can draw" is,
        // to every caller, the same fact as "there is no picture" — and
        // recording it as nothing found is what settles the artist honestly
        // instead of settling them against a file that renders as a blank.
        let base = commons("A band logo.svg", 32, false).await;

        let answer = client(&base).image_for("mb-artist").await;

        assert!(matches!(answer, Ok(None)), "{answer:?}");
    }

    #[tokio::test]
    async fn given_a_raster_picture_when_fetched_then_it_is_taken() {
        // The guard on the guard: refusing formats must not refuse the ones
        // the whole feature is for.
        let base = commons("Miles Davis 1955.jpg", 32, false).await;

        let asset = client(&base)
            .image_for("mb-artist")
            .await
            .expect("the lookup failed")
            .expect("no asset");

        assert_eq!(asset.extension, "jpg");
        assert_eq!(asset.bytes.len(), 32);
    }

    #[tokio::test]
    async fn given_an_oversized_body_with_no_declared_length_when_fetched_then_it_is_refused() {
        // The cap has to bound what is *read*, not what is kept. Nine
        // megabytes against an eight-megabyte cap, with nothing in the
        // headers to refuse it on.
        let base = commons("Huge.jpg", 9 * 1024 * 1024, true).await;

        let answer = client(&base).image_for("mb-artist").await;

        assert!(
            matches!(answer, Err(ProviderError::Unusable(_))),
            "an unbounded body was accepted: {answer:?}"
        );
    }
}
