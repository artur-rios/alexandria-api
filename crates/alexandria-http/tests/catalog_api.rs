mod common;

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tempfile::tempdir;
use tower::ServiceExt;

use crate::common::{
    file_rows, file_rows_with_missing, file_rows_with_uuid, test_app, wait_for_files,
};

/// The editable columns of an `audio_files` row, in the order every
/// assertion here selects them: title, artist, album, year, genre, track.
type AudioMetadataRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
);

fn index_request(root: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": root }).to_string()))
        .unwrap()
}

fn refresh_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_supported_files_when_index_posted_then_returns_202_with_run_id() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");
    common::write_file(&lib, "notes.md", b"# title\n");

    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    let run_id = json["runId"].as_str().expect("runId string");
    assert!(!run_id.is_empty());

    wait_for_files(&test.pool, 2).await;
    let rows = file_rows(&test.pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "notes.md");
    assert_eq!(rows[0].2, "text");
    assert_eq!(rows[1].1, "song.mp3");
    assert_eq!(rows[1].2, "audio");
    assert!(!rows[0].3.is_empty(), "content hash is stored");
}

#[tokio::test]
async fn given_indexed_path_when_reindex_posted_then_existing_path_skipped_no_duplicate() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "track.mp3", b"audio");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let first = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    wait_for_files(&test.pool, 1).await;

    // A second index run over the same root must not duplicate the path.
    let router2 = app(Settings::default(), test.services);
    let second = router2
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
        .fetch_one(&test.pool)
        .await
        .expect("count");
    assert_eq!(count.0, 1, "no duplicate path row created");
}

#[tokio::test]
async fn given_missing_root_when_index_posted_then_returns_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(index_request("/this/does/not/exist"))
        .await
        .expect("one-shot");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_no_bearer_when_index_posted_then_returns_401() {
    let lib = tempdir().unwrap();

    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();

    let response = router.oneshot(request).await.expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_subtype_rows_when_indexed_then_each_file_has_subtype_row() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "a.mp3", b"x");
    common::write_file(&lib, "v.mp4", b"y");
    common::write_file(&lib, "p.pdf", b"z");

    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("one-shot");

    wait_for_files(&test.pool, 3).await;

    let audio: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audio_files")
        .fetch_one(&test.pool)
        .await
        .expect("audio");
    let video: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM video_files")
        .fetch_one(&test.pool)
        .await
        .expect("video");
    let document: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM documents")
        .fetch_one(&test.pool)
        .await
        .expect("document");
    assert_eq!(audio.0, 1);
    assert_eq!(video.0, 1);
    assert_eq!(document.0, 1);
}

#[tokio::test]
async fn given_changed_and_deleted_files_when_refresh_posted_then_refreshes_and_marks_missing() {
    let lib = tempdir().unwrap();
    let a = common::write_file(&lib, "a.mp3", b"audio-v1");
    let _b = common::write_file(&lib, "b.md", b"text-v1");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 2).await;

    let before = file_rows(&test.pool).await;
    assert_eq!(before.len(), 2);
    let old_a_hash = before
        .iter()
        .find(|r| r.1 == "a.mp3")
        .map(|r| r.3.clone())
        .expect("a indexed");
    let old_b_hash = before
        .iter()
        .find(|r| r.1 == "b.md")
        .map(|r| r.3.clone())
        .expect("b indexed");

    // Mutate: change a's bytes on disk, delete b from disk.
    std::fs::write(&a, b"audio-v2-CHANGED").unwrap();
    std::fs::remove_file(lib.path().join("b.md")).unwrap();

    let router2 = app(Settings::default(), test.services.clone());
    let response = router2
        .oneshot(refresh_request())
        .await
        .expect("refresh one-shot");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(!body["runId"].as_str().unwrap().is_empty());

    // Wait for the refresh to settle: a's hash must differ from the old one,
    // and b must have a missing_at set.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let rows = file_rows_with_missing(&test.pool).await;
        let a_row = rows.iter().find(|r| r.1 == "a.mp3").expect("a");
        let b_row = rows.iter().find(|r| r.1 == "b.md").expect("b");
        let a_refreshed = a_row.3 != old_a_hash;
        let b_marked = b_row.4.is_some();
        if a_refreshed && b_marked {
            assert_ne!(a_row.3, old_a_hash, "a hash refreshed");
            assert_eq!(a_row.4, None, "a missing marker cleared");
            assert_eq!(b_row.3, old_b_hash, "b hash untouched when missing");
            assert!(b_row.4.is_some(), "b marked missing");
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("refresh never settled; a refreshed={a_refreshed}, b marked={b_marked}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn given_no_bearer_when_refresh_posted_then_returns_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// UC-04: PATCH /v1/files/{uuid}/metadata (FR-FC-14..18, FR-FC-24)
// ---------------------------------------------------------------------------

fn patch_metadata(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(format!("/v1/files/{uuid}/metadata"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Resolve the public UUID of the single cataloged file named `name`.
async fn uuid_for_name(pool: &sqlx::sqlite::SqlitePool, name: &str) -> String {
    let rows = file_rows_with_uuid(pool).await;
    rows.iter()
        .find(|r| r.2 == name)
        .map(|r| r.0.clone())
        .unwrap_or_else(|| panic!("no cataloged file named {name}"))
}

#[tokio::test]
async fn given_indexed_audio_file_when_patch_audio_metadata_then_200_and_row_updated() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 1).await;

    let uuid = uuid_for_name(&test.pool, "song.mp3").await;
    let body = json!({
        "type": "audio",
        "title": "New Title",
        "artist": "New Artist",
        "album": "New Album",
        "year": 2001,
        "genre": "Rock",
        "track": 3
    });

    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(&uuid, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["file"]["fileType"], "audio");
    assert_eq!(json["file"]["state"], "active");
    assert_eq!(json["metadata"]["type"], "audio");
    assert_eq!(json["metadata"]["title"], "New Title");
    assert_eq!(json["metadata"]["track"], 3);

    // Persisted subtype row reflects the full-replace PATCH.
    let row: AudioMetadataRow = sqlx::query_as(
        "SELECT title, artist, album, year, genre, track FROM audio_files \
             JOIN files ON files.id = audio_files.file_id WHERE files.uuid = ?",
    )
    .bind(&uuid)
    .fetch_one(&test.pool)
    .await
    .expect("audio row");
    assert_eq!(row.0.as_deref(), Some("New Title"));
    assert_eq!(row.1.as_deref(), Some("New Artist"));
    assert_eq!(row.2.as_deref(), Some("New Album"));
    assert_eq!(row.3, Some(2001));
    assert_eq!(row.4.as_deref(), Some("Rock"));
    assert_eq!(row.5, Some(3));
}

#[tokio::test]
async fn given_indexed_video_file_when_patch_video_metadata_then_200_and_row_updated() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "clip.mkv", b"video bytes");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 1).await;

    let uuid = uuid_for_name(&test.pool, "clip.mkv").await;
    let body = json!({
        "type": "video",
        "title": "A Film",
        "year": 1999,
        "resolution": "1080p",
        "mediaKind": "movie"
    });

    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(&uuid, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(json["metadata"]["mediaKind"], "movie");

    let row: (Option<String>, Option<i64>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT title, year, resolution, media_kind FROM video_files \
         JOIN files ON files.id = video_files.file_id WHERE files.uuid = ?",
    )
    .bind(&uuid)
    .fetch_one(&test.pool)
    .await
    .expect("video row");
    assert_eq!(row.0.as_deref(), Some("A Film"));
    assert_eq!(row.1, Some(1999));
    assert_eq!(row.2.as_deref(), Some("1080p"));
    assert_eq!(row.3.as_deref(), Some("movie"));
}

#[tokio::test]
async fn given_indexed_text_file_when_patch_any_metadata_then_400() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "notes.md", b"# title");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 1).await;

    let uuid = uuid_for_name(&test.pool, "notes.md").await;
    // Text has no editable subtype metadata; any PATCH body's variant
    // mismatches the file's `text` type → AF-01 invalid input.
    let body = json!({ "type": "audio", "title": "x" });

    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(&uuid, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_indexed_audio_file_when_patch_with_video_body_then_400() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 1).await;

    let uuid = uuid_for_name(&test.pool, "song.mp3").await;
    let body = json!({ "type": "video", "title": "x" });

    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(&uuid, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_missing_uuid_when_patch_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let missing = uuid::Uuid::new_v4().to_string();
    let body = json!({ "type": "audio", "title": "x" });

    let response = router
        .oneshot(patch_metadata(&missing, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_patch_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);
    let uuid = uuid::Uuid::new_v4().to_string();
    let body = json!({ "type": "audio", "title": "x" });

    let request = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/files/{uuid}/metadata"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// UC-03: GET /v1/files and GET /v1/files/{uuid} (FR-FC-12, FR-FC-13, FR-FC-24)
// ---------------------------------------------------------------------------

/// Index a temp library with `names.len()` files into the given pool and
/// wait for persistence. Builds a one-shot services instance against `pool`
/// so the spawned index task persists to the same database the test queries.
async fn index_library(
    lib: &tempfile::TempDir,
    pool: &sqlx::sqlite::SqlitePool,
    names: &[(&str, &[u8])],
) {
    for (name, bytes) in names {
        common::write_file(lib, name, bytes);
    }
    let services = std::sync::Arc::new(
        alexandria_core::services::build_services(&Settings::default(), pool.clone()).await,
    );
    let _ = app(Settings::default(), services)
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(pool, names.len() as i64).await;
}

fn get_files(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

fn get_files_no_auth(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_indexed_files_when_get_files_then_200_array_excluding_deleted_by_default() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("a.mp3", b"x"), ("b.md", b"y")]).await;

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files"))
        .await
        .expect("list one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    // Ordered by path; both are active.
    assert!(arr.iter().all(|f| f["state"] == "active"));
}

#[tokio::test]
async fn given_indexed_files_when_get_files_with_type_filter_then_only_matching_type_returned() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("a.mp3", b"x"), ("b.mp4", b"y")]).await;

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?type=audio"))
        .await
        .expect("list one-shot");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let arr = body.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["fileType"], "audio");
}

#[tokio::test]
async fn given_unknown_type_when_get_files_then_400() {
    let test = test_app().await;
    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?type=banana"))
        .await
        .expect("list one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_unknown_state_when_get_files_then_400() {
    let test = test_app().await;
    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?state=delted"))
        .await
        .expect("list one-shot");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an unknown state must be rejected, not silently coerced to active"
    );
}

#[tokio::test]
async fn given_empty_type_and_state_when_get_files_then_treated_as_no_filter() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?type=&state="))
        .await
        .expect("list one-shot");
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn given_deleted_file_when_get_files_default_then_excluded() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;

    let uuid = uuid_for_name(&test.pool, "song.mp3").await;
    // Seed a soft-delete directly.
    sqlx::query("UPDATE files SET state='deleted', deleted_at=? WHERE uuid=?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete");

    let response = app(Settings::default(), test.services.clone())
        .oneshot(get_files("/v1/files"))
        .await
        .expect("list one-shot");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body.as_array().unwrap().len(),
        0,
        "deleted excluded by default"
    );
}

#[tokio::test]
async fn given_deleted_file_when_get_files_state_deleted_then_only_deleted_returned() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("a.mp3", b"x"), ("b.md", b"y")]).await;
    let uuid_a = uuid_for_name(&test.pool, "a.mp3").await;
    sqlx::query("UPDATE files SET state='deleted', deleted_at=? WHERE uuid=?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid_a)
        .execute(&test.pool)
        .await
        .expect("soft-delete");

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?state=deleted"))
        .await
        .expect("list one-shot");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["state"], "deleted");
    assert_eq!(arr[0]["name"], "a.mp3");
}

#[tokio::test]
async fn given_deleted_file_when_get_files_state_all_then_both_returned() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("a.mp3", b"x"), ("b.md", b"y")]).await;
    let uuid_a = uuid_for_name(&test.pool, "a.mp3").await;
    sqlx::query("UPDATE files SET state='deleted', deleted_at=? WHERE uuid=?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid_a)
        .execute(&test.pool)
        .await
        .expect("soft-delete");

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files?state=all"))
        .await
        .expect("list one-shot");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn given_no_bearer_when_get_files_then_401() {
    let test = test_app().await;
    let response = app(Settings::default(), test.services)
        .oneshot(get_files_no_auth("/v1/files"))
        .await
        .expect("list one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_indexed_file_when_get_file_by_uuid_then_200_with_file_view() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let response = app(Settings::default(), test.services)
        .oneshot(get_files(&format!("/v1/files/{uuid}")))
        .await
        .expect("get one-shot");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["file"]["uuid"], uuid);
    assert_eq!(body["file"]["fileType"], "audio");
    assert_eq!(body["file"]["state"], "active");
    // No metadata written yet → null.
    assert!(body["metadata"].is_null());
}

#[tokio::test]
async fn given_indexed_file_with_written_metadata_when_get_file_by_uuid_then_metadata_echoed() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;
    sqlx::query(
        "UPDATE audio_files SET title='T', artist='A', album=NULL, year=2001, genre=NULL, \
         track=NULL FROM files WHERE audio_files.file_id = files.id AND files.uuid = ?",
    )
    .bind(&uuid)
    .execute(&test.pool)
    .await
    .expect("audio metadata");

    let response = app(Settings::default(), test.services)
        .oneshot(get_files(&format!("/v1/files/{uuid}")))
        .await
        .expect("get one-shot");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["metadata"]["type"], "audio");
    assert_eq!(body["metadata"]["title"], "T");
    assert_eq!(body["metadata"]["artist"], "A");
    assert_eq!(body["metadata"]["year"], 2001);
}

#[tokio::test]
async fn given_missing_uuid_when_get_file_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(get_files(&format!("/v1/files/{missing}")))
        .await
        .expect("get one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_get_file_by_uuid_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(get_files_no_auth(&format!("/v1/files/{uuid}")))
        .await
        .expect("get one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_indexed_text_file_when_get_file_by_uuid_then_metadata_is_null() {
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("notes.md", b"# h")]).await;
    let uuid = uuid_for_name(&test.pool, "notes.md").await;

    let response = app(Settings::default(), test.services)
        .oneshot(get_files(&format!("/v1/files/{uuid}")))
        .await
        .expect("get one-shot");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["file"]["fileType"], "text");
    assert!(body["metadata"].is_null());
}

#[tokio::test]
async fn given_soft_deleted_file_when_patch_then_409() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"audio bytes");

    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let _ = router
        .oneshot(index_request(lib.path().to_str().unwrap()))
        .await
        .expect("index one-shot");
    wait_for_files(&test.pool, 1).await;

    let uuid = uuid_for_name(&test.pool, "song.mp3").await;
    // Seed a soft-delete directly (UC-06 is not implemented yet) so the
    // record is in `deleted` state with `deleted_at` set.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete seed");

    let body = json!({ "type": "audio", "title": "x" });
    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(&uuid, body))
        .await
        .expect("patch one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
// ---------------------------------------------------------------------------
// Auth ordering (B3): authentication is decided before any request payload is
// parsed, so an unauthenticated caller always sees 401 — never a 400/422 that
// leaks whether the body or path happened to be well-formed.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_no_bearer_and_malformed_body_when_patch_then_401_not_422() {
    let test = test_app().await;
    let request = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/files/{}/metadata", uuid::Uuid::new_v4()))
        .header("content-type", "application/json")
        .body(Body::from("{ not json at all"))
        .unwrap();

    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("patch one-shot");

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "auth must be decided before the body is parsed"
    );
}

#[tokio::test]
async fn given_no_bearer_and_invalid_uuid_when_patch_then_401_not_400() {
    let test = test_app().await;
    let request = Request::builder()
        .method("PATCH")
        .uri("/v1/files/not-a-uuid/metadata")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "type": "audio" }).to_string()))
        .unwrap();

    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("patch one-shot");

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "auth must be decided before the path is parsed"
    );
}

#[tokio::test]
async fn given_authenticated_and_malformed_body_when_patch_then_400_with_error_envelope() {
    let test = test_app().await;
    let request = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/files/{}/metadata", uuid::Uuid::new_v4()))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from("{ not json at all"))
        .unwrap();

    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("patch one-shot");

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a malformed body is invalid input, not axum's default 422"
    );
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
            .expect("error responses are JSON");
    assert!(
        body["error"].is_string(),
        "must use the {{\"error\": …}} envelope like every other failure"
    );
}

#[tokio::test]
async fn given_no_bearer_when_health_requested_then_still_reachable() {
    let test = test_app().await;
    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("health one-shot");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "health must stay open — it is not a /v1 catalog operation"
    );
}

// ---------------------------------------------------------------------------
// Data-integrity failures are surfaced, not coerced into plausible answers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn given_missing_subtype_row_when_patch_then_error_not_silent_success() {
    // B5: the UPDATE matches zero rows. The caller must not be told the write
    // succeeded and handed back metadata the database never stored.
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    sqlx::query("DELETE FROM audio_files WHERE file_id = (SELECT id FROM files WHERE uuid = ?)")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("drop subtype row");

    let response = app(Settings::default(), test.services)
        .oneshot(patch_metadata(
            &uuid,
            json!({ "type": "audio", "title": "T" }),
        ))
        .await
        .expect("patch one-shot");

    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a write that touched no rows must not report success"
    );
}

#[tokio::test]
async fn given_corrupt_uuid_row_when_get_files_then_error_not_nil_uuid() {
    // B6: an unparseable stored uuid used to be coerced to the nil UUID and
    // served as though it were real.
    let lib = tempdir().unwrap();
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio")]).await;

    sqlx::query("UPDATE files SET uuid = 'definitely-not-a-uuid' WHERE name = 'song.mp3'")
        .execute(&test.pool)
        .await
        .expect("corrupt the uuid");

    let response = app(Settings::default(), test.services)
        .oneshot(get_files("/v1/files"))
        .await
        .expect("list one-shot");

    assert_eq!(
        response.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a corrupt row must surface as an error, not a nil-uuid record"
    );
}

// ---------------------------------------------------------------------------
// UC-05: POST /v1/files/{uuid}/rename (FR-FC-19, FR-FC-24)
// ---------------------------------------------------------------------------

fn rename_request(uuid: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{uuid}/rename"))
        .header("authorization", "Bearer test-token")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn given_indexed_file_when_post_rename_then_200_and_on_disk_and_catalog_updated() {
    let lib = tempdir().unwrap();
    let a_path = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;

    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let response = app(Settings::default(), test.services)
        .oneshot(rename_request(&uuid, json!({ "name": "renamed.mp3" })))
        .await
        .expect("rename one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["name"], "renamed.mp3");
    assert_eq!(body["state"], "active");
    let new_path = body["path"].as_str().expect("path string");
    assert!(new_path.ends_with("renamed.mp3"), "path {new_path}");

    // On-disk file moved (old path gone, new path present with same bytes).
    assert!(!a_path.exists(), "old on-disk path no longer exists");
    let renamed_path = a_path.with_file_name("renamed.mp3");
    assert!(renamed_path.exists(), "renamed on-disk file exists");
    assert_eq!(std::fs::read(&renamed_path).unwrap(), b"audio bytes");

    // Catalog row carries the new name + path.
    let row: (String, String) = sqlx::query_as("SELECT name, path FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("catalog row");
    assert_eq!(row.0, "renamed.mp3");
    assert!(row.1.ends_with("renamed.mp3"));
}

#[tokio::test]
async fn given_invalid_name_when_post_rename_then_400() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    for bad in ["/slash", "..", "a:b", "name."] {
        let response = app(Settings::default(), test.services.clone())
            .oneshot(rename_request(&uuid, json!({ "name": bad })))
            .await
            .expect("rename one-shot");
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "name {bad:?} rejected as invalid input"
        );
    }
}

#[tokio::test]
async fn given_missing_body_when_post_rename_then_400() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{uuid}/rename"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_missing_uuid_when_post_rename_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(rename_request(&missing, json!({ "name": "x.mp3" })))
        .await
        .expect("rename one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_post_rename_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{uuid}/rename"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "x.mp3" }).to_string()))
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_soft_deleted_file_when_post_rename_then_409() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete seed");

    let response = app(Settings::default(), test.services)
        .oneshot(rename_request(&uuid, json!({ "name": "renamed.mp3" })))
        .await
        .expect("rename one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_target_path_owned_by_other_file_when_post_rename_then_500_disk_error() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "a.mp3", b"aaa");
    common::write_file(&lib, "b.mp3", b"bbb");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("a.mp3", b"aaa"), ("b.mp3", b"bbb")]).await;

    let uuid_a = uuid_for_name(&test.pool, "a.mp3").await;

    let response = app(Settings::default(), test.services)
        .oneshot(rename_request(&uuid_a, json!({ "name": "b.mp3" })))
        .await
        .expect("rename one-shot");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"], "disk error");

    let a_path = lib.path().join("a.mp3");
    assert!(a_path.exists(), "a.mp3 left untouched on disk");
}

#[tokio::test]
async fn given_no_bearer_and_malformed_body_when_post_rename_then_401_not_400() {
    let test = test_app().await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{}/rename", uuid::Uuid::new_v4()))
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "auth must be decided before the path or body is parsed"
    );
}

// ---------------------------------------------------------------------------
// UC-06: DELETE /v1/files/{uuid} (FR-FC-20, FR-FC-24)
// ---------------------------------------------------------------------------

fn delete_file_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_indexed_file_when_delete_file_then_200_and_state_deleted_in_catalog() {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let response = app(Settings::default(), test.services)
        .oneshot(delete_file_request(&uuid))
        .await
        .expect("delete one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["state"], "deleted");
    assert!(
        body["deletedAt"].as_str().is_some(),
        "deletedAt is present on the response"
    );

    // Catalog row carries state=deleted and a stamped deleted_at.
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&test.pool)
            .await
            .expect("catalog row");
    assert_eq!(row.0, "deleted");
    assert!(row.1.is_some(), "deleted_at stamped");

    // The on-disk file is untouched (UC-06 leaves it; purge-on-disk is UC-09).
    assert!(on_disk.exists(), "on-disk file preserved by soft-delete");
}

#[tokio::test]
async fn given_missing_uuid_when_delete_file_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(delete_file_request(&missing))
        .await
        .expect("delete one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_delete_file_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}"))
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_no_bearer_and_malformed_uuid_when_delete_file_then_401_not_400() {
    // Auth must be decided before the path is parsed (FR-AU-07 / SRD §7): a
    // malformed uuid alone cannot turn a 401 into a 400.
    let test = test_app().await;
    let request = Request::builder()
        .method("DELETE")
        .uri("/v1/files/not-a-uuid")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_soft_deleted_file_when_delete_file_then_409() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete seed");

    let response = app(Settings::default(), test.services)
        .oneshot(delete_file_request(&uuid))
        .await
        .expect("delete one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// UC-07: POST /v1/files/{uuid}/restore (FR-FC-21, FR-FC-24)
// ---------------------------------------------------------------------------

fn restore_file_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{uuid}/restore"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_soft_deleted_file_when_restore_file_then_200_and_state_active_in_catalog() {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // Seed a soft-deleted row whose `deleted_at` is comfortably within the
    // default 30-day retention window (one day ago). The handler must accept
    // it; unit tests cover the exact-boundary case with a FixedClock.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(chrono::Utc::now() - chrono::Duration::days(1))
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete seed");

    let response = app(Settings::default(), test.services)
        .oneshot(restore_file_request(&uuid))
        .await
        .expect("restore one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["uuid"], uuid);
    assert_eq!(body["state"], "active");
    assert!(
        body["deletedAt"].is_null(),
        "deletedAt is cleared on the response"
    );

    // Catalog row carries state=active and cleared deleted_at.
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&test.pool)
            .await
            .expect("catalog row");
    assert_eq!(row.0, "active");
    assert!(row.1.is_none(), "deleted_at cleared in the catalog");

    // The on-disk file is untouched (UC-07 leaves it; purge-on-disk is UC-09).
    assert!(on_disk.exists(), "on-disk file preserved by restore");
}

#[tokio::test]
async fn given_missing_uuid_when_restore_file_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(restore_file_request(&missing))
        .await
        .expect("restore one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_restore_file_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{uuid}/restore"))
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_no_bearer_and_malformed_uuid_when_restore_file_then_401_not_400() {
    // Auth must be decided before the path is parsed (FR-AU-07 / SRD §7): a
    // malformed uuid alone cannot turn a 401 into a 400.
    let test = test_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/files/not-a-uuid/restore")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_active_file_when_restore_file_then_409() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // Indexed but never soft-deleted — `state = 'active'` (AF-02 not-deleted).
    let response = app(Settings::default(), test.services)
        .oneshot(restore_file_request(&uuid))
        .await
        .expect("restore one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_soft_deleted_file_past_retention_when_restore_file_then_404() {
    // AF-01: a record past the configured retention window is reported as
    // not-found (UC-08 owns the actual hard purge; before it runs the row
    // still exists, so the elapsed check is what UC-07 surfaces as 404).
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // `deleted_at` well before the 30-day default retention window.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("past-retention seed");

    let response = app(Settings::default(), test.services)
        .oneshot(restore_file_request(&uuid))
        .await
        .expect("restore one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The catalog row is unchanged: still deleted, still past retention.
    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&test.pool)
            .await
            .expect("catalog row");
    assert_eq!(row.0, "deleted");
    assert!(
        row.1.is_some(),
        "deleted_at unchanged by past-retention restore"
    );
}

// ---------------------------------------------------------------------------
// UC-08: DELETE /v1/files/{uuid}?purge=true (FR-FC-22, FR-FC-24)
// ---------------------------------------------------------------------------

fn purge_file_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge=true"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_soft_deleted_file_past_retention_when_purged_then_200_and_rows_removed_and_disk_preserved(
) {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // `deleted_at` well past the default 30-day retention window.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("past-retention seed");

    let file_id: (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("file id");

    let response = app(Settings::default(), test.services)
        .oneshot(purge_file_request(&uuid))
        .await
        .expect("purge one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["uuid"], uuid);
    assert_eq!(
        body["state"], "deleted",
        "confirmation echoes the pre-purge state"
    );

    // The `files` row is gone.
    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("files count");
    assert_eq!(remaining.0, 0, "files row removed by purge");

    // The subtype row (audio_files) is also gone.
    let subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(file_id.0)
            .fetch_one(&test.pool)
            .await
            .expect("audio_files count");
    assert_eq!(subtype_remaining.0, 0, "subtype row removed by purge");

    // The on-disk file is untouched (NFR-07; purge-on-disk is UC-09).
    assert!(on_disk.exists(), "on-disk file preserved by purge");
}

#[tokio::test]
async fn given_soft_deleted_file_within_retention_when_purged_then_409_and_row_kept() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // `deleted_at` comfortably within the default 30-day retention window.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(chrono::Utc::now() - chrono::Duration::days(1))
        .bind(&uuid)
        .execute(&test.pool)
        .await
        .expect("soft-delete seed");

    let response = app(Settings::default(), test.services)
        .oneshot(purge_file_request(&uuid))
        .await
        .expect("purge one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&test.pool)
            .await
            .expect("catalog row");
    assert_eq!(row.0, "deleted");
    assert!(
        row.1.is_some(),
        "row kept, deleted_at unchanged by rejected purge"
    );
}

#[tokio::test]
async fn given_active_file_when_purged_then_409() {
    let lib = tempdir().unwrap();
    common::write_file(&lib, "song.mp3", b"x");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"x")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // Indexed but never soft-deleted — `state = 'active'` (AF-01: never
    // started a retention window).
    let response = app(Settings::default(), test.services)
        .oneshot(purge_file_request(&uuid))
        .await
        .expect("purge one-shot");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_missing_uuid_when_purged_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(purge_file_request(&missing))
        .await
        .expect("purge one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_purged_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge=true"))
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_no_bearer_and_malformed_uuid_when_purged_then_401_not_400() {
    // Auth must be decided before the path is parsed (FR-AU-07 / SRD §7): a
    // malformed uuid alone cannot turn a 401 into a 400.
    let test = test_app().await;
    let request = Request::builder()
        .method("DELETE")
        .uri("/v1/files/not-a-uuid?purge=true")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_non_boolean_purge_query_when_delete_then_400_with_error_envelope() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge=notabool"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["error"].as_str().is_some(), "error envelope present");
}

#[tokio::test]
async fn given_no_purge_param_when_delete_then_still_soft_deletes() {
    // Regression guard for UC-06: the bare `DELETE /v1/files/{uuid}` form
    // (no `purge` query param) must still soft-delete, not hard-purge.
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let response = app(Settings::default(), test.services)
        .oneshot(delete_file_request(&uuid))
        .await
        .expect("delete one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&test.pool)
            .await
            .expect("catalog row");
    assert_eq!(
        row.0, "deleted",
        "no-param delete still soft-deletes (UC-06)"
    );
    assert!(row.1.is_some(), "deleted_at stamped by no-param delete");
    assert!(on_disk.exists(), "on-disk file preserved");
}

// ---------------------------------------------------------------------------
// UC-09: DELETE /v1/files/{uuid}?purge-on-disk=true (FR-FC-23, FR-FC-24)
// ---------------------------------------------------------------------------

fn purge_on_disk_request(uuid: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge-on-disk=true"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn given_active_file_when_purge_on_disk_then_200_and_rows_and_disk_file_removed() {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // Never soft-deleted (`state = 'active'`) — UC-09 has no retention gate,
    // unlike UC-08.
    let file_id: (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("file id");

    let response = app(Settings::default(), test.services)
        .oneshot(purge_on_disk_request(&uuid))
        .await
        .expect("purge-on-disk one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["file"]["uuid"], uuid);
    assert_eq!(
        body["file"]["state"], "active",
        "confirmation echoes the pre-purge state"
    );
    assert_eq!(body["diskFilePresent"], true);

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("files count");
    assert_eq!(remaining.0, 0, "files row removed by purge-on-disk");

    let subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(file_id.0)
            .fetch_one(&test.pool)
            .await
            .expect("audio_files count");
    assert_eq!(
        subtype_remaining.0, 0,
        "subtype row removed by purge-on-disk"
    );

    assert!(!on_disk.exists(), "on-disk file deleted by purge-on-disk");
}

#[tokio::test]
async fn given_missing_disk_file_when_purge_on_disk_then_200_disk_file_present_false_and_row_removed(
) {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    // The file was deleted out from under the catalog (AF-01).
    std::fs::remove_file(&on_disk).expect("pre-remove on-disk file");

    let response = app(Settings::default(), test.services)
        .oneshot(purge_on_disk_request(&uuid))
        .await
        .expect("purge-on-disk one-shot");
    assert_eq!(response.status(), StatusCode::OK);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        body["diskFilePresent"], false,
        "AF-01: no on-disk file to delete"
    );

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("files count");
    assert_eq!(
        remaining.0, 0,
        "record still purged despite absent disk file"
    );
}

#[tokio::test]
async fn given_disk_delete_failure_when_purge_on_disk_then_500_disk_error_and_row_kept() {
    let lib = tempdir().unwrap();
    let on_disk = common::write_file(&lib, "song.mp3", b"audio bytes");
    let test = test_app().await;
    index_library(&lib, &test.pool, &[("song.mp3", b"audio bytes")]).await;
    let uuid = uuid_for_name(&test.pool, "song.mp3").await;

    let file_id: (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("file id");

    // Replace the indexed file with a directory at the same path so
    // `std::fs::remove_file` fails with something other than `NotFound`
    // (AF-02), on both Windows and Unix.
    std::fs::remove_file(&on_disk).expect("pre-remove indexed file");
    std::fs::create_dir(&on_disk).expect("create directory in place of indexed file");

    let response = app(Settings::default(), test.services)
        .oneshot(purge_on_disk_request(&uuid))
        .await
        .expect("purge-on-disk one-shot");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"], "disk error");

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&uuid)
        .fetch_one(&test.pool)
        .await
        .expect("files count");
    assert_eq!(
        remaining.0, 1,
        "AF-02: record kept when the disk delete fails"
    );

    let subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(file_id.0)
            .fetch_one(&test.pool)
            .await
            .expect("audio_files count");
    assert_eq!(
        subtype_remaining.0, 1,
        "AF-02: subtype row kept when the disk delete fails"
    );
}

#[tokio::test]
async fn given_missing_uuid_when_purge_on_disk_then_404() {
    let test = test_app().await;
    let missing = uuid::Uuid::new_v4().to_string();
    let response = app(Settings::default(), test.services)
        .oneshot(purge_on_disk_request(&missing))
        .await
        .expect("purge-on-disk one-shot");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_bearer_when_purge_on_disk_then_401() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge-on-disk=true"))
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_purge_and_purge_on_disk_both_true_when_delete_then_400_with_error_envelope() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge=true&purge-on-disk=true"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["error"].as_str().is_some(), "error envelope present");
}

#[tokio::test]
async fn given_non_boolean_purge_on_disk_query_when_delete_then_400_with_error_envelope() {
    let test = test_app().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{uuid}?purge-on-disk=notabool"))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let response = app(Settings::default(), test.services)
        .oneshot(request)
        .await
        .expect("one-shot");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["error"].as_str().is_some(), "error envelope present");
}
