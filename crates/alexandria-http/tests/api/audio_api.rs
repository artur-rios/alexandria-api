//! Audio over HTTP: a library's music and recordings are catalogued, listed
//! by type, streamed and thumbnailed like any other file Alexandria reads
//! (libraries-only design, section 5). No music manager — no playlists, no
//! lookups — only the files.

use crate::common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// A one-second, untagged PCM WAV: 8 kHz, mono, 8-bit. Its header is all a
/// reader needs for the duration, and it carries no tags and no picture.
fn one_second_wav() -> Vec<u8> {
    let samples: u32 = 8000;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
    wav.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&1u16.to_le_bytes()); // block align
    wav.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&samples.to_le_bytes());
    wav.extend(std::iter::repeat_n(128u8, samples as usize));
    wav
}

fn authed(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

async fn json_of(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

/// Add `files` as a library and wait for it to be indexed; answers the
/// router.
async fn library_of(harness: &common::TestApp, files: &[(&str, Vec<u8>)]) -> axum::Router {
    let router = app(Settings::default(), harness.services.clone());
    let root = harness.folder("/music");
    for (name, bytes) in files {
        std::fs::write(format!("{root}/{name}"), bytes).unwrap();
    }
    let response = router
        .clone()
        .oneshot(authed(
            "POST",
            "/v1/libraries",
            Some(json!({ "name": "Music", "rootPath": root })),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_of(response).await;
    common::wait_for_run_terminal(
        &harness.services,
        body["runId"].as_str().unwrap(),
        common::TEST_TOKEN,
    )
    .await;
    router
}

async fn audio_listing(router: &axum::Router) -> Value {
    let response = router
        .clone()
        .oneshot(authed("GET", "/v1/files?type=audio", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json_of(response).await
}

#[tokio::test]
async fn given_an_audio_file_with_no_tags_when_indexed_then_it_is_catalogued_by_name_with_its_duration(
) {
    let harness = common::test_app().await;
    let router = library_of(&harness, &[("tone.wav", one_second_wav())]).await;

    let listed = audio_listing(&router).await;

    assert_eq!(listed.as_array().map(|a| a.len()), Some(1), "{listed}");
    assert_eq!(listed[0]["file"]["name"], "tone.wav");
    assert_eq!(listed[0]["file"]["fileType"], "audio");
    let duration = listed[0]["durationSeconds"].as_f64().expect("a duration");
    assert!((duration - 1.0).abs() < 0.05, "{duration}");
}

#[tokio::test]
async fn given_audio_as_a_type_filter_when_listed_then_only_audio() {
    let harness = common::test_app().await;
    let router = library_of(
        &harness,
        &[
            ("tone.wav", one_second_wav()),
            ("notes.md", b"# notes".to_vec()),
        ],
    )
    .await;

    let listed = audio_listing(&router).await;

    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|view| view["file"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["tone.wav"]);
}

#[tokio::test]
async fn given_an_audio_file_with_no_cover_when_its_thumbnail_is_asked_then_no_thumbnail() {
    let harness = common::test_app().await;
    let router = library_of(&harness, &[("tone.wav", one_second_wav())]).await;
    let uuid = audio_listing(&router).await[0]["file"]["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let response = router
        .clone()
        .oneshot(authed("GET", &format!("/v1/files/{uuid}/thumbnail"), None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        json_of(response).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cover art"),
        "the answer says why there is no picture"
    );
}

#[tokio::test]
async fn given_an_audio_file_when_streamed_with_a_range_then_206_and_an_audio_mime_type() {
    let harness = common::test_app().await;
    let router = library_of(
        &harness,
        &[("song.mp3", vec![0u8; 64]), ("song.flac", vec![0u8; 64])],
    )
    .await;
    let listed = audio_listing(&router).await;

    for (name, mime) in [("song.mp3", "audio/mpeg"), ("song.flac", "audio/flac")] {
        let uuid = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["file"]["name"] == name)
            .unwrap_or_else(|| panic!("{name} was not catalogued: {listed}"))["file"]["uuid"]
            .as_str()
            .unwrap()
            .to_string();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/v1/files/{uuid}/stream"))
                    .header(
                        "authorization",
                        format!("Bearer {}", common::TEST_TOKEN).as_str(),
                    )
                    .header("range", "bytes=0-9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{name}");
        assert_eq!(response.headers()["content-type"], mime, "{name}");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            10
        );
    }
}

#[tokio::test]
async fn given_an_audio_file_when_fetched_alone_then_it_says_what_the_listing_says() {
    let harness = common::test_app().await;
    let router = library_of(&harness, &[("tone.wav", one_second_wav())]).await;
    let listed = audio_listing(&router).await;
    let uuid = listed[0]["file"]["uuid"].as_str().unwrap().to_string();

    let response = router
        .clone()
        .oneshot(authed("GET", &format!("/v1/files/{uuid}"), None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let alone = json_of(response).await;
    assert!(alone["durationSeconds"].as_f64().is_some(), "{alone}");
    assert_eq!(alone, listed[0]);
}

#[tokio::test]
async fn given_an_audio_file_when_its_metadata_is_edited_then_the_listing_answers_the_edit() {
    let harness = common::test_app().await;
    let router = library_of(&harness, &[("tone.wav", one_second_wav())]).await;
    let uuid = audio_listing(&router).await[0]["file"]["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let response = router
        .clone()
        .oneshot(authed(
            "PATCH",
            &format!("/v1/files/{uuid}/metadata"),
            Some(json!({
                "type": "audio",
                "title": "Tone",
                "artist": "Oscillator",
                "albumArtist": "Oscillator",
                "track": 1
            })),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let listed = audio_listing(&router).await;
    assert_eq!(listed[0]["metadata"]["title"], "Tone");
    assert_eq!(listed[0]["metadata"]["artist"], "Oscillator");
    assert_eq!(listed[0]["metadata"]["albumArtist"], "Oscillator");
    assert_eq!(listed[0]["metadata"]["track"], 1);
    assert!(
        listed[0]["durationSeconds"].as_f64().is_some(),
        "the edit leaves the read duration alone"
    );
}
