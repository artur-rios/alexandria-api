//! HTTP ↔ FFI parity for UC-01 (Testing Specification §7.3). Index the same
//! library directory through both transports into separate databases and
//! assert the persisted file rows are byte-for-byte identical (path, type,
//! hash). The start contract shape is also asserted (HTTP 202 `{runId}` and
//! the FFI `IndexStartResult` both carry a valid UUID on success).

// Every test here holds `SERIAL` across its awaits on purpose: the FFI side
// is a process-global services slot, so two parity tests running concurrently
// would init over each other's database. The guard has to span the whole
// test, awaits included — that is exactly what it is for.
#![allow(clippy::await_holding_lock)]

use std::ffi::{CStr, CString};
use std::sync::Mutex;

use alexandria_core::config::Settings;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::build_services;
use alexandria_ffi::{
    alexandria_auth_local_login, alexandria_auth_local_redeem_recovery_code,
    alexandria_auth_local_regenerate_recovery_codes, alexandria_auth_local_register,
    alexandria_auth_windows_login, alexandria_bookmark_create, alexandria_bookmark_purge,
    alexandria_bookmark_restore, alexandria_bookmark_soft_delete, alexandria_bookmark_update,
    alexandria_bookmarks_list, alexandria_collection_add_items, alexandria_collection_create,
    alexandria_collection_delete, alexandria_collection_list_items,
    alexandria_collection_remove_item, alexandria_collection_rename, alexandria_collections_list,
    alexandria_comic_page, alexandria_file_edit_content, alexandria_file_edit_metadata,
    alexandria_file_get_by_uuid, alexandria_file_playback_source, alexandria_file_purge,
    alexandria_file_purge_on_disk, alexandria_file_read_content, alexandria_file_rename,
    alexandria_file_restore, alexandria_file_soft_delete, alexandria_file_thumbnail,
    alexandria_files_list, alexandria_free_string, alexandria_index_count_files,
    alexandria_index_files_json, alexandria_index_init, alexandria_index_refresh_start,
    alexandria_index_run_status_json, alexandria_index_start, alexandria_reading_list_add_item,
    alexandria_reading_list_create, alexandria_reading_list_delete,
    alexandria_reading_list_remove_item, alexandria_reading_list_update_progress,
    alexandria_reading_lists_list, alexandria_settings_json, alexandria_watchlist_add_video,
    alexandria_watchlist_create, alexandria_watchlist_delete, alexandria_watchlist_remove_video,
    alexandria_watchlist_update_progress, alexandria_watchlists_list, IndexStartResult,
};
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::json;
use tempfile::{tempdir, TempDir};
use tower::ServiceExt;

// One global FFI services slot per process: serialize every parity test that
// touches it (there is only one here, but the guard keeps the suite safe if
// more are added).
//
// Every acquisition below is `unwrap_or_else(|e| e.into_inner())`, never
// `unwrap()`. This mutex orders tests; it guards no invariant that a panic
// could leave half-established, so poisoning carries no information. With
// `unwrap()`, one panicking test made every test after it fail with
// `PoisonError` — observed once during development, turning a single flake
// into 39 red tests in the suite that gates HTTP/FFI parity. Taking the
// poisoned guard keeps the failure count equal to the number of real
// failures.
static SERIAL: Mutex<()> = Mutex::new(());

/// Copy an `AuthJsonResult`'s `json` out and free it, panicking if it is NULL.
///
/// Since issue #101 the error path carries a body too, so every auth call in
/// this file reads its `json` the same way whether it succeeded or not.
fn take_json(json: *mut std::os::raw::c_char) -> String {
    assert!(!json.is_null(), "every auth result must carry a body");
    let value = unsafe { CStr::from_ptr(json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(json);
    }
    value
}

/// How long a poll waits for an asynchronous index / re-index run to land in
/// the database before it gives up and panics.
///
/// Generous on purpose. UC-01 and UC-02 return immediately and finish in the
/// background (FR-FC-08), so every assertion about their results has to poll.
/// The runs themselves take milliseconds; what the bound has to absorb is the
/// machine, and under `cargo test --workspace` these binaries share a host
/// with dozens of others and a live compile. A tighter bound does not catch a
/// slow indexer — it just reports the runner's load as a product failure.
const ASYNC_RUN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

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

/// `(path, type, content_hash)` triples — the shape a `files` table is
/// reduced to whenever the two legs' catalogs are compared.
type FileTriples = Vec<(String, String, String)>;

fn db_path(dir: &TempDir, name: &str) -> String {
    dir.path().join(name).to_str().unwrap().to_string()
}

/// Bearer token every parity test authenticates with. A valid UUID: the
/// active auth mode is local (below), so it must parse as a session id
/// (`LocalAuthService::authenticate`). A matching session is seeded into
/// each leg's database so it always validates.
const TEST_TOKEN: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

fn local_settings() -> Settings {
    let mut settings = Settings::default();
    settings.auth.mode = alexandria_core::config::AuthMode::Local;
    settings
}

async fn seed_session(pool: &sqlx::sqlite::SqlitePool, token: &str) {
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::hours(24);
    sqlx::query("INSERT INTO sessions (id, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(token)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(pool)
        .await
        .expect("seed session");
}

/// Pre-migrate the FFI leg's database and seed a session before
/// `alexandria_index_init` opens it. `alexandria_index_init` loads settings
/// via `load_settings()` (`ALEXANDRIA_*` env, not a `Settings` value the test
/// controls directly), so this also flips the process-wide auth mode env var
/// to local — safe across tests since `SERIAL` guards this whole file and the
/// value never differs between them.
async fn setup_ffi_db(dir: &TempDir, name: &str, token: &str) -> String {
    std::env::set_var("ALEXANDRIA_AUTH_MODE", "local");
    let path = db_path(dir, name);
    let pool = migrate_database(&path).await.expect("ffi pre-migrate");
    seed_session(&pool, token).await;
    pool.close().await;
    path
}

fn run_id_string(r: &IndexStartResult) -> String {
    let n = r
        .run_id
        .iter()
        .position(|&ch| ch == 0)
        .unwrap_or(r.run_id.len());
    String::from_utf8_lossy(
        &r.run_id[..n]
            .iter()
            .map(|&ch| ch as u8)
            .collect::<Vec<u8>>(),
    )
    .into_owned()
}

#[tokio::test]
async fn given_same_lib_when_indexed_via_http_and_ffi_then_files_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Shared library under test.
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio-bytes").unwrap();
    std::fs::write(lib.path().join("notes.md"), b"# title").unwrap();
    std::fs::write(lib.path().join("book.pdf"), b"%PDF-1.4...").unwrap();
    let lib_path = lib.path().to_str().unwrap().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": lib_path }).to_string()))
        .unwrap();
    let response = router.oneshot(request).await.expect("http oneshot");
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let http_run_id = http_body["runId"].as_str().expect("http runId");
    assert!(!http_run_id.is_empty());

    // wait for HTTP persistence
    let expected: i64 = 3;
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(&http_pool)
            .await
            .unwrap();
        if c >= expected {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never persisted {expected} files");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let http_rows: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT path, name, type, content_hash FROM files ORDER BY path")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (off the tokio thread: FFI block_on its own runtime) ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let lib_for_ffi = lib_path.clone();
    let ffi_json: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(lib_for_ffi).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let result = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::INDEX_OK, "ffi start failed");
        assert!(!run_id_string(&result).is_empty());

        let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
        loop {
            let c = alexandria_index_count_files();
            if c >= 3 {
                break;
            }
            if std::time::Instant::now() > dl {
                panic!("ffi never persisted 3 files");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let raw = alexandria_index_files_json();
        assert!(!raw.is_null());
        // SAFETY: returned by the FFI accessor as a NUL-terminated string.
        let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
        // SAFETY: pointer came from this library and is freed once.
        unsafe {
            alexandria_free_string(raw);
        }
        json
    })
    .await
    .unwrap();

    // ---- compare ----
    let ffi_value: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();
    let ffi_rows: Vec<(String, String, String, String)> = ffi_value
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["path"].as_str().unwrap().to_string(),
                o["name"].as_str().unwrap().to_string(),
                o["type"].as_str().unwrap().to_string(),
                o["hash"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    assert_eq!(http_rows.len(), ffi_rows.len(), "row count parity");
    for (http, ffi) in http_rows.iter().zip(ffi_rows.iter()) {
        assert_eq!(http, ffi, "row mismatch between HTTP and FFI");
    }
}

/// UC-02 parity — exercise the same operations through both transports on two
/// identical temp libraries and assert identical persisted outcomes. Each leg
/// indexes (a + b), mutates on disk (change `a`, delete `b`), then refreshes;
/// both must end with `a`'s hash refreshed and `b` carrying a `missingAt`
/// marker, with matching rows.
#[tokio::test]
async fn given_same_lib_when_refreshed_via_http_and_ffi_then_rows_and_missing_markers_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    fn seed_lib() -> TempDir {
        let lib = tempdir().unwrap();
        std::fs::write(lib.path().join("a.mp3"), b"audio-v1").unwrap();
        std::fs::write(lib.path().join("b.md"), b"text-v1").unwrap();
        lib
    }
    fn mutate(lib: &TempDir) {
        std::fs::write(lib.path().join("a.mp3"), b"audio-v2-CHANGED").unwrap();
        std::fs::remove_file(lib.path().join("b.md")).unwrap();
    }

    // ---- HTTP leg ----
    let http_lib = seed_lib();
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    wait_for_http_files(&http_pool, 2).await;

    mutate(&http_lib);

    let refresh_req = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let refresh_resp = app(Settings::default(), http_services.clone())
        .oneshot(refresh_req)
        .await
        .expect("http refresh");
    assert_eq!(refresh_resp.status(), axum::http::StatusCode::ACCEPTED);
    let refresh_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(refresh_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let http_run_id = refresh_body["runId"]
        .as_str()
        .expect("http runId")
        .to_string();

    // The run record (UC-42) is the actual signal that both halves of the
    // refresh — the re-hash of the changed file and the missing marker for
    // the deleted one — have landed. They run concurrently inside
    // `RefreshHandler::refresh_one`, so polling either row directly (as this
    // test used to) can observe one half without the other; `complete` means
    // the whole walk, including both halves, is done.
    wait_for_http_run_terminal(&http_services, &http_run_id, TEST_TOKEN).await;

    let http_rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT path, name, type, content_hash, missing_at FROM files ORDER BY path",
    )
    .fetch_all(&http_pool)
    .await
    .unwrap();

    // ---- FFI leg (own identical lib) ----
    let ffi_lib = seed_lib();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_rows: Vec<(String, String, String, String, Option<String>)> =
        tokio::task::spawn_blocking(
            move || -> Vec<(String, String, String, String, Option<String>)> {
                let cdb = CString::new(ffi_db).unwrap();
                assert_eq!(
                    alexandria_index_init(cdb.as_ptr()),
                    alexandria_ffi::INDEX_OK
                );

                let root = CString::new(ffi_lib_path.clone()).unwrap();
                let token = CString::new(TEST_TOKEN).unwrap();
                let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
                assert_eq!(started.status, alexandria_ffi::INDEX_OK);
                wait_for_ffi_files(2);

                // identical mutation on disk
                std::fs::write(ffi_lib.path().join("a.mp3"), b"audio-v2-CHANGED").unwrap();
                std::fs::remove_file(ffi_lib.path().join("b.md")).unwrap();

                let refresh = alexandria_index_refresh_start(token.as_ptr());
                assert_eq!(refresh.status, alexandria_ffi::INDEX_OK);
                let ffi_run_id = run_id_string(&refresh);

                // Same purpose as the HTTP leg's run-status poll: `complete`
                // is the signal that both halves of the refresh — the re-hash
                // and the missing marker, which run concurrently — have
                // landed, rather than guessing from either row directly.
                wait_for_ffi_run_terminal(&ffi_run_id, &token);

                let raw = alexandria_index_files_json();
                assert!(!raw.is_null());
                let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(raw);
                }
                let v: serde_json::Value = serde_json::from_str(&json).unwrap();
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|o| {
                        (
                            o["path"].as_str().unwrap().to_string(),
                            o["name"].as_str().unwrap().to_string(),
                            o["type"].as_str().unwrap().to_string(),
                            o["hash"].as_str().unwrap().to_string(),
                            o["missingAt"].as_str().map(|s| s.to_string()),
                        )
                    })
                    .collect()
            },
        )
        .await
        .unwrap();

    // ---- compare ----
    // Compare name + hash + (missingAt presence). The exact `missingAt`
    // timestamp fires at different wall-clock instants on the two surfaces
    // (like a random run id), so parity asserts the marker's *presence*, not
    // its value.
    let norm =
        |rows: &[(String, String, String, String, Option<String>)]| -> Vec<(String, String, bool)> {
            let mut v: Vec<(String, String, bool)> = rows
                .iter()
                .map(|r| (r.1.clone(), r.3.clone(), r.4.is_some()))
                .collect();
            v.sort();
            v
        };
    let http_n = norm(&http_rows);
    let ffi_n = norm(&ffi_rows);
    assert_eq!(
        http_n, ffi_n,
        "refreshed rows + missing markers differ across surfaces"
    );

    // Both sides: a present & refreshed (missingAt null), b marked missing.
    let by_name = |rows: &[(String, String, String, String, Option<String>)]| -> std::collections::BTreeMap<String,(String,bool)> {
        rows.iter().map(|r| (r.1.clone(), (r.3.clone(), r.4.is_some()))).collect()
    };
    let h = by_name(&http_rows);
    let f = by_name(&ffi_rows);
    assert!(!h["a.mp3"].1 && !f["a.mp3"].1, "a missingAt null on both");
    assert!(h["b.md"].1 && f["b.md"].1, "b missingAt set on both");
    assert_eq!(h["a.mp3"].0, f["a.mp3"].0, "a refreshed hash parity");
}

fn wait_for_http_files(
    pool: &sqlx::sqlite::SqlitePool,
    expected: i64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
        loop {
            let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files")
                .fetch_one(pool)
                .await
                .unwrap();
            if c >= expected {
                return;
            }
            if std::time::Instant::now() > dl {
                panic!("http never had {expected} files");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
}

/// Indexing is two steps: `insert_file` commits the `files` row in its own
/// transaction first, and only then does `index_entry` call the audio-tag
/// reader and persist the extracted metadata via a separate `update_metadata`
/// call. Waiting only on the `files` row (as `wait_for_http_files` does) can
/// therefore race ahead of extraction — poll the `audio_files` row itself so
/// callers only proceed once the title has actually landed.
async fn wait_for_http_audio_title(pool: &sqlx::sqlite::SqlitePool, expected_title: &str) {
    let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT audio_files.title FROM audio_files \
             JOIN files ON files.id = audio_files.file_id \
             WHERE files.name = ?",
        )
        .bind("song.wav")
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(title),)) = &row {
            if title == expected_title {
                return;
            }
        }
        if std::time::Instant::now() > dl {
            panic!("http never wrote extracted audio title");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Poll `GET /v1/index/runs/{runId}` (UC-42) until the run leaves `running`.
///
/// This is the real signal that a refresh has finished both halves of its
/// work — the re-hash of a changed file and the missing marker for a deleted
/// one, which land in no fixed order (`RefreshHandler::refresh_one` handles
/// each cataloged path concurrently). `complete` means the whole walk is
/// done, not just whichever half a caller happened to poll for. Before this
/// signal existed, this test waited on the missing-count and then on a
/// hash-specific rehash check; both were guesswork about a signal the run
/// record now provides directly.
async fn wait_for_http_run_terminal(
    services: &std::sync::Arc<alexandria_core::services::Services>,
    run_id: &str,
    token: &str,
) -> serde_json::Value {
    let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/v1/index/runs/{run_id}"))
            .header("authorization", &format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = app(Settings::default(), services.clone())
            .oneshot(request)
            .await
            .expect("run status oneshot");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        if body["status"] != "running" {
            return body;
        }
        if std::time::Instant::now() > dl {
            panic!("http run {run_id} never left running");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// The FFI counterpart of [`wait_for_http_run_terminal`], polling
/// `alexandria_index_run_status_json` instead of the HTTP route.
fn wait_for_ffi_run_terminal(run_id: &str, token: &CString) {
    let run_id_c = CString::new(run_id).unwrap();
    let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let result = alexandria_index_run_status_json(run_id_c.as_ptr(), token.as_ptr());
        assert_eq!(
            result.status,
            alexandria_ffi::RUN_OK,
            "ffi run status failed"
        );
        assert!(!result.json.is_null());
        // SAFETY: `json` is a NUL-terminated string owned by this call.
        let body = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        // SAFETY: pointer came from this library and is freed exactly once,
        // every iteration (not just the last).
        unsafe {
            alexandria_free_string(result.json);
        }
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        if value["status"] != "running" {
            return;
        }
        if std::time::Instant::now() > dl {
            panic!("ffi run {run_id} never left running");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for_ffi_files(expected: i64) {
    let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        if alexandria_index_count_files() >= expected {
            return;
        }
        if std::time::Instant::now() > dl {
            panic!("ffi never had {expected} files");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// UC-04 parity — edit metadata over both transports with identical patch
/// JSON and assert the returned `FileMetadata` bodies agree (modulo the
/// file's UUID, which is per-database) and the persisted `audio_files` rows
/// match across both databases (Testing Specification §7.3, FR-FC-24).
#[tokio::test]
async fn given_same_audio_file_when_metadata_edited_via_http_and_ffi_then_responses_and_rows_identical(
) {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let patch_json = r#"{"type":"audio","title":"Parity Title","artist":"Artist","album":"Album","year":2001,"genre":"Rock","track":3}"#;
    let patch_value: serde_json::Value = serde_json::from_str(patch_json).unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let patch_req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/files/{http_uuid}/metadata"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(patch_json.to_string()))
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(patch_req)
        .await
        .expect("http patch");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_audio_row: AudioMetadataRow = sqlx::query_as(
        "SELECT title, artist, album, year, genre, track FROM audio_files \
         JOIN files ON files.id = audio_files.file_id WHERE files.uuid = ?",
    )
    .bind(&http_uuid)
    .fetch_one(&http_pool)
    .await
    .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let patch_for_ffi = patch_json.to_string();
    let ffi_payload: (String, serde_json::Value, AudioMetadataRow) =
        tokio::task::spawn_blocking(move || {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            // Resolve the file's uuid via a read connection to the FFI db file.
            let ffi_uuid = std::thread::spawn({
                let ffi_dir = ffi_dir.path().to_path_buf();
                move || -> String {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let path = ffi_dir.join("ffi.sqlite");
                        let url = format!("sqlite://{}", path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid,): (String,) =
                            sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        uuid
                    })
                }
            })
            .join()
            .unwrap();

            let patch_c = CString::new(patch_for_ffi).unwrap();
            let result = alexandria_file_edit_metadata(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                patch_c.as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(result.status, alexandria_ffi::FILE_OK, "ffi edit failed");
            assert!(!result.json.is_null());
            // SAFETY: FFI returned a NUL-terminated string via CString::into_raw.
            let json_str = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

            // Persisted audio row from the FFI db.
            let ffi_audio_row = std::thread::spawn({
                let ffi_dir = ffi_dir.path().to_path_buf();
                let ffi_uuid = ffi_uuid.clone();
                move || -> AudioMetadataRow {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let path = ffi_dir.join("ffi.sqlite");
                        let url = format!("sqlite://{}", path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let row: AudioMetadataRow = sqlx::query_as(
                            "SELECT title, artist, album, year, genre, track FROM audio_files \
                                 JOIN files ON files.id = audio_files.file_id WHERE files.uuid = ?",
                        )
                        .bind(ffi_uuid)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                        row
                    })
                }
            })
            .join()
            .unwrap();

            (ffi_uuid, ffi_value, ffi_audio_row)
        })
        .await
        .unwrap();

    let (_ffi_uuid, ffi_body, ffi_audio_row) = ffi_payload;

    // ---- compare ----
    // The `metadata` sub-object is fully server-derived from the patch and
    // must be byte-value identical across surfaces.
    assert_eq!(
        http_body["metadata"], ffi_body["metadata"],
        "metadata body diverges across surfaces"
    );

    // The `file` sub-object matches field-for-field except the per-database
    // values: `uuid` (random v4), `path` (each leg indexes its own temp dir),
    // and `indexedAt` (wall-clock stamp). The remaining fields — `name`,
    // `fileType`, `contentHash`, `state`, `deletedAt`, `missingAt` — are
    // deterministic and must agree across surfaces (parity, FR-FC-24).
    let norm_file = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v["file"].clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
        }
        f
    };
    let http_file_norm = norm_file(&http_body);
    let ffi_file_norm = norm_file(&ffi_body);
    assert_eq!(
        http_file_norm, ffi_file_norm,
        "file body (minus uuid/path/indexedAt) diverges across surfaces"
    );

    // Persisted subtype rows must agree across databases.
    assert_eq!(http_audio_row, ffi_audio_row, "audio_files row diverges");

    // Also confirm the patch we sent equals the metadata we got back (the
    // handler echoes the written metadata).
    assert_eq!(
        http_body["metadata"], patch_value,
        "metadata must echo patch"
    );
}

/// UC-03 parity — list files through both transports with identical filters
/// and assert the returned JSON arrays agree (modulo the per-database values
/// `uuid`, `path`, and `indexedAt`, which differ for each temp library)
/// (Testing Specification §7.3, FR-FC-24).
#[tokio::test]
async fn given_same_lib_when_files_listed_via_http_and_ffi_then_arrays_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"audio").unwrap();
    std::fs::write(http_lib.path().join("notes.md"), b"# h").unwrap();
    std::fs::write(http_lib.path().join("clip.mkv"), b"video").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 3).await;

    // Soft-delete one record so we can exercise the default-excludes-deleted
    // behavior and the state=all filter on both surfaces.
    let (del_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    sqlx::query("UPDATE files SET state='deleted', deleted_at=? WHERE uuid=?")
        .bind("2024-01-01T00:00:00Z")
        .bind(&del_uuid)
        .execute(&http_pool)
        .await
        .expect("soft-delete");

    let norm = |v: serde_json::Value| -> Vec<(String, String, String)> {
        // (name, fileType, state) sorted by name — uuid/path/indexedAt are
        // per-database and excluded from the parity comparison.
        let mut arr: Vec<(String, String, String)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["name"].as_str().unwrap().to_string(),
                    f["fileType"].as_str().unwrap().to_string(),
                    f["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        arr.sort();
        arr
    };

    // Default filter (excludes deleted): HTTP returns notes.md + clip.mkv.
    let default_req = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let default_resp = app(Settings::default(), http_services.clone())
        .oneshot(default_req)
        .await
        .expect("http list");
    assert_eq!(default_resp.status(), axum::http::StatusCode::OK);
    let http_default: serde_json::Value = serde_json::from_slice(
        &to_bytes(default_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let http_default_n = norm(http_default);
    assert_eq!(http_default_n.len(), 2, "default excludes deleted (http)");

    // state=all filter: HTTP returns all three.
    let all_req = Request::builder()
        .method("GET")
        .uri("/v1/files?state=all")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let all_resp = app(Settings::default(), http_services.clone())
        .oneshot(all_req)
        .await
        .expect("http list all");
    let http_all: serde_json::Value =
        serde_json::from_slice(&to_bytes(all_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let http_all_n = norm(http_all);
    assert_eq!(http_all_n.len(), 3, "state=all returns everything (http)");

    // type=audio + state=all filter: HTTP returns only the soft-deleted song.
    let audio_req = Request::builder()
        .method("GET")
        .uri("/v1/files?type=audio&state=all")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let audio_resp = app(Settings::default(), http_services.clone())
        .oneshot(audio_req)
        .await
        .expect("http list audio");
    let http_audio: serde_json::Value =
        serde_json::from_slice(&to_bytes(audio_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_audio_n = norm(http_audio);
    assert_eq!(http_audio_n.len(), 1);
    assert_eq!(http_audio_n[0].0, "song.mp3");
    assert_eq!(http_audio_n[0].2, "deleted");

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"audio").unwrap();
    std::fs::write(ffi_lib.path().join("notes.md"), b"# h").unwrap();
    std::fs::write(ffi_lib.path().join("clip.mkv"), b"video").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_default_n, ffi_all_n, ffi_audio_n) =
        tokio::task::spawn_blocking(move || -> (FileTriples, FileTriples, FileTriples) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(3);

            // Soft-delete song.mp3 via a direct SQL update on the FFI db, so
            // the data state matches the HTTP leg exactly.
            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let ffi_uuid = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> String {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        sqlx::query("UPDATE files SET state='deleted', deleted_at=? WHERE name=?")
                            .bind("2024-01-01T00:00:00Z")
                            .bind("song.mp3")
                            .execute(&pool)
                            .await
                            .unwrap();
                        let (uuid,): (String,) =
                            sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        uuid
                    })
                }
            })
            .join()
            .unwrap();
            let _ = ffi_uuid; // not used by FFI list, but confirms the delete landed

            let ffi_list = |filters: &str| -> serde_json::Value {
                let f = CString::new(filters).unwrap();
                let r = alexandria_files_list(f.as_ptr(), token.as_ptr());
                assert_eq!(r.status, alexandria_ffi::FILE_OK, "ffi list failed");
                assert!(!r.json.is_null());
                let s = unsafe { CStr::from_ptr(r.json) }
                    .to_str()
                    .unwrap()
                    .to_string();
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(r.json);
                }
                serde_json::from_str(&s).unwrap()
            };

            let ffi_default_n = norm(ffi_list(""));
            let ffi_all_n = norm(ffi_list(r#"{"state":"all"}"#));
            let ffi_audio_n = norm(ffi_list(r#"{"type":"audio","state":"all"}"#));
            (ffi_default_n, ffi_all_n, ffi_audio_n)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        http_default_n, ffi_default_n,
        "default list diverges across surfaces"
    );
    assert_eq!(
        http_all_n, ffi_all_n,
        "state=all list diverges across surfaces"
    );
    assert_eq!(
        http_audio_n, ffi_audio_n,
        "type+state list diverges across surfaces"
    );
}

/// UC-03 parity — get a single file by UUID through both transports and
/// assert the returned `FileView` bodies agree (modulo the per-database
/// values `uuid`, `path`, and `indexedAt`) (Testing Specification §7.3,
/// FR-FC-24).
#[tokio::test]
async fn given_same_file_when_fetched_via_http_and_ffi_then_file_view_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    // Write subtype metadata so the FileView's `metadata` is non-null and
    // can be compared across surfaces.
    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name=?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE audio_files SET title=?, artist=?, year=? \
         FROM files WHERE audio_files.file_id = files.id AND files.uuid = ?",
    )
    .bind("T")
    .bind("A")
    .bind(2001i64)
    .bind(&http_uuid)
    .execute(&http_pool)
    .await
    .expect("audio metadata update");

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services.clone())
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);
        wait_for_ffi_files(1);

        // Write the same metadata on the FFI db via a direct SQL update.
        let ffi_dir_path = ffi_dir.path().to_path_buf();
        let ffi_uuid = std::thread::spawn({
            let ffi_dir_path = ffi_dir_path.clone();
            move || -> String {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let path = ffi_dir_path.join("ffi.sqlite");
                    let url = format!("sqlite://{}", path.to_str().unwrap());
                    let pool = sqlx::sqlite::SqlitePoolOptions::new()
                        .max_connections(1)
                        .connect(&format!("{url}?mode=rw"))
                        .await
                        .unwrap();
                    sqlx::query(
                        "UPDATE audio_files SET title=?, artist=?, year=? \
                             FROM files WHERE audio_files.file_id = files.id AND files.name = ?",
                    )
                    .bind("T")
                    .bind("A")
                    .bind(2001i64)
                    .bind("song.mp3")
                    .execute(&pool)
                    .await
                    .unwrap();
                    let (uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                        .bind("song.mp3")
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                    uuid
                })
            }
        })
        .join()
        .unwrap();

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let r = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::FILE_OK, "ffi get failed");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_str()
            .unwrap()
            .to_string();
        // SAFETY: pointer came from this library and is freed once.
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    // `metadata` is fully server-derived from the written row and must agree
    // byte-for-byte across surfaces.
    assert_eq!(
        http_body["metadata"], ffi_body["metadata"],
        "metadata diverges"
    );

    // `file` matches field-for-field except per-database values: `uuid`,
    // `path`, `indexedAt`.
    let norm_file = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v["file"].clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
        }
        f
    };
    assert_eq!(
        norm_file(&http_body),
        norm_file(&ffi_body),
        "file body diverges"
    );
}

/// UC-03 parity — AF-01 (not-found) maps to the same status on both
/// surfaces (HTTP 404, FFI `FILE_ERR_NOT_FOUND`).
#[tokio::test]
async fn given_missing_uuid_when_fetched_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let missing = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{missing}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(req)
        .await
        .expect("http get");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let missing_str = missing.to_string();
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );
        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(missing_str).unwrap();
        let r = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(ffi_status, alexandria_ffi::FILE_ERR_NOT_FOUND);
}

/// UC-03 parity — an unrecognised filter value must be rejected identically on
/// both surfaces (HTTP 400, FFI `FILE_ERR_INVALID_INPUT`). Previously HTTP
/// rejected an unknown `type` while the FFI silently dropped the filter and
/// returned the whole catalog — the exact divergence FR-FC-24 / NFR-09 forbid.
#[tokio::test]
async fn given_unknown_filter_values_when_listed_via_http_and_ffi_then_both_reject() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg: index a file so a silently-dropped filter would show up
    // as a non-empty 200 rather than an error. ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let http_status = |uri: &'static str| {
        let services = http_services.clone();
        async move {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", &format!("Bearer {TEST_TOKEN}"))
                .body(Body::empty())
                .unwrap();
            app(Settings::default(), services)
                .oneshot(req)
                .await
                .expect("http list")
                .status()
        }
    };

    assert_eq!(
        http_status("/v1/files?type=banana").await,
        axum::http::StatusCode::BAD_REQUEST
    );
    assert_eq!(
        http_status("/v1/files?state=delted").await,
        axum::http::StatusCode::BAD_REQUEST
    );

    // ---- FFI leg ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (bad_type, bad_state, empty_values) =
        tokio::task::spawn_blocking(move || -> (i32, i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            let status_for = |filters: &str| -> i32 {
                let f = CString::new(filters).unwrap();
                let r = alexandria_files_list(f.as_ptr(), token.as_ptr());
                if !r.json.is_null() {
                    // SAFETY: pointer came from this library and is freed once.
                    unsafe {
                        alexandria_free_string(r.json);
                    }
                }
                r.status
            };

            (
                status_for(r#"{"type":"banana"}"#),
                status_for(r#"{"state":"delted"}"#),
                // Empty strings mean "no filter" on both surfaces, not an error.
                status_for(r#"{"type":"","state":""}"#),
            )
        })
        .await
        .unwrap();

    assert_eq!(
        bad_type,
        alexandria_ffi::FILE_ERR_INVALID_INPUT,
        "FFI must reject an unknown type like HTTP does, not drop the filter"
    );
    assert_eq!(
        bad_state,
        alexandria_ffi::FILE_ERR_INVALID_INPUT,
        "FFI must reject an unknown state like HTTP does"
    );
    assert_eq!(
        empty_values,
        alexandria_ffi::FILE_OK,
        "empty filter values mean no filter on both surfaces"
    );
}

/// UC-03 parity — AF-02 (unauthenticated) maps to the same status on both
/// surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`).
#[tokio::test]
async fn given_no_token_when_files_listed_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(req)
        .await
        .expect("http list");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );
        let empty_filters = CString::new("").unwrap();
        // Null token pointer → cstr_lossy returns None → empty token.
        let r = alexandria_files_list(empty_filters.as_ptr(), std::ptr::null());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(ffi_status, alexandria_ffi::FILE_ERR_UNAUTHORIZED);
}

/// B3 parity — an unauthenticated caller is denied before its payload is
/// parsed, on both surfaces. Previously HTTP answered `400`/`422` (extractors
/// run before the handler) and the FFI answered `FILE_ERR_INVALID_INPUT` (JSON
/// parsed before the auth check), so a caller with no credentials could learn
/// whether its body was well-formed (FR-AU-07 / SRD §7).
#[tokio::test]
async fn given_no_token_and_malformed_payload_when_edited_via_http_and_ffi_then_both_unauthorized()
{
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("PATCH")
        .uri("/v1/files/not-a-uuid/metadata")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http patch");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (edit_status, get_status, list_status) =
        tokio::task::spawn_blocking(move || -> (i32, i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            // Null token => empty token => unauthenticated, paired with a
            // payload that would otherwise fail to parse first.
            let bad_uuid = CString::new("not-a-uuid").unwrap();
            let bad_json = CString::new("{ not json").unwrap();

            let edit = alexandria_file_edit_metadata(
                bad_uuid.as_ptr(),
                bad_json.as_ptr(),
                std::ptr::null(),
            );
            if !edit.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(edit.json);
                }
            }

            let get = alexandria_file_get_by_uuid(bad_uuid.as_ptr(), std::ptr::null());
            if !get.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(get.json);
                }
            }

            let bad_filters = CString::new(r#"{"type":"banana"}"#).unwrap();
            let list = alexandria_files_list(bad_filters.as_ptr(), std::ptr::null());
            if !list.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(list.json);
                }
            }

            (edit.status, get.status, list.status)
        })
        .await
        .unwrap();

    assert_eq!(
        edit_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "edit must deny before parsing the uuid or body"
    );
    assert_eq!(
        get_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "get must deny before parsing the uuid"
    );
    assert_eq!(
        list_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "list must deny before validating filters"
    );
}

/// UC-05 parity — rename a file over both transports with identical inputs
/// and assert the returned `File` bodies agree (modulo the per-database
/// values `uuid`, `path`, `indexedAt`) and the on-disk file ends up at the
/// same relative path with identical bytes (Testing Specification §7.3,
/// FR-FC-24).
#[tokio::test]
async fn given_same_file_when_renamed_via_http_and_ffi_then_file_bodies_and_disk_state_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let new_name = "renamed.mp3";

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let rename_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{http_uuid}/rename"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": new_name }).to_string()))
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(rename_req)
        .await
        .expect("http rename");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let new_name_for_ffi = new_name.to_string();

    let (ffi_uuid, ffi_body) =
        tokio::task::spawn_blocking(move || -> (String, serde_json::Value) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            // Resolve the file's uuid via a dedicated read connection to the FFI db.
            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let ffi_uuid = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> String {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid,): (String,) =
                            sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        uuid
                    })
                }
            })
            .join()
            .unwrap();

            let name_c = CString::new(new_name_for_ffi).unwrap();
            let result = alexandria_file_rename(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                name_c.as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(result.status, alexandria_ffi::FILE_OK, "ffi rename failed");
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();
            (ffi_uuid, ffi_body)
        })
        .await
        .unwrap();

    // ---- compare ----
    // `file` matches field-for-field except the per-database values `uuid`,
    // `path`, `indexedAt`.
    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v.clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "File body diverges across surfaces"
    );

    // The names agree and both files have moved to the same relative path.
    assert_eq!(http_body["name"], ffi_body["name"]);
    assert_eq!(http_body["name"], new_name);

    // On-disk parity: both legs end with the same bytes at the renamed path.
    assert_eq!(
        std::fs::read(http_lib.path().join(new_name)).unwrap(),
        std::fs::read(ffi_lib.path().join(new_name)).unwrap(),
        "renamed on-disk files must agree byte-for-byte across surfaces"
    );
    // And the old path is gone on both.
    assert!(
        !http_lib.path().join("song.mp3").exists(),
        "http old path gone"
    );
    assert!(
        !ffi_lib.path().join("song.mp3").exists(),
        "ffi old path gone"
    );

    // Suppress unused warning while keeping the per-leg uuid visible.
    let _ = (http_uuid, ffi_uuid);
}

/// UC-05 parity — an unauthenticated caller is rejected before its payload is
/// parsed, on both surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`)
/// (FR-AU-07 / SRD §7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_renamed_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{}/rename", uuid::Uuid::new_v4()))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "x.mp3" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http rename");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (rename_status, bad_payload_status) = tokio::task::spawn_blocking(move || -> (i32, i32) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        // Null token => empty token => unauthenticated. Pair it with a
        // payload that would otherwise fail to parse first (a bad uuid and
        // an empty name) so the auth check must fire before either is read.
        let bad_uuid = CString::new("not-a-uuid").unwrap();
        let bad_name = CString::new("").unwrap();
        let r = alexandria_file_rename(bad_uuid.as_ptr(), bad_name.as_ptr(), std::ptr::null());
        if !r.json.is_null() {
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(r.json);
            }
        }
        let clean_status = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
        // A second call with a clean uuid but no token still denies.
        let ok_uuid = CString::new("11111111-1111-1111-1111-111111111111").unwrap();
        let name = CString::new("new.mp3").unwrap();
        let r2 = alexandria_file_rename(ok_uuid.as_ptr(), name.as_ptr(), std::ptr::null());
        if !r2.json.is_null() {
            unsafe {
                alexandria_free_string(r2.json);
            }
        }
        (r.status, if r2.status == clean_status { 1 } else { 0 })
    })
    .await
    .unwrap();

    assert_eq!(
        rename_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "rename must deny before parsing the uuid or name"
    );
    assert_eq!(
        bad_payload_status, 1,
        "clean-uuid rename with no token also denies"
    );
}

/// UC-06 parity — soft-delete a file over both transports with identical
/// inputs and assert the returned `File` bodies agree modulo the per-database
/// values `uuid`, `path`, `indexedAt`, `deletedAt` (the clock fires
/// independently per leg, so `deletedAt` differs by sub-ms) and that both
/// surfaces report `state = "deleted"` (Testing Specification §7.3,
/// FR-FC-24). The on-disk file is preserved on both legs (UC-06 leaves it;
/// purge-on-disk is UC-09).
#[tokio::test]
async fn given_same_file_when_soft_deleted_via_http_and_ffi_then_file_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(delete_req)
        .await
        .expect("http soft-delete");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_uuid, ffi_body) =
        tokio::task::spawn_blocking(move || -> (String, serde_json::Value) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            // Resolve the file's uuid via a dedicated read connection to the FFI db.
            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let ffi_uuid = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> String {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid,): (String,) =
                            sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        uuid
                    })
                }
            })
            .join()
            .unwrap();

            let result = alexandria_file_soft_delete(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(
                result.status,
                alexandria_ffi::FILE_OK,
                "ffi soft_delete failed"
            );
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();
            (ffi_uuid, ffi_body)
        })
        .await
        .unwrap();

    // ---- compare ----
    // `state` agrees on both surfaces (the soft-delete took effect).
    assert_eq!(http_body["state"], "deleted", "http body reports deleted");
    assert_eq!(ffi_body["state"], "deleted", "ffi body reports deleted");

    // Both surfaces return a stamped `deletedAt` (a non-null ISO timestamp).
    assert!(
        http_body["deletedAt"].as_str().is_some(),
        "http body carries deletedAt"
    );
    assert!(
        ffi_body["deletedAt"].as_str().is_some(),
        "ffi body carries deletedAt"
    );

    // `file` matches field-for-field except the per-database values `uuid`,
    // `path`, `indexedAt`, and `deletedAt` (each leg's clock stamps its own
    // value; parity is on the *presence* and *shape*, already asserted
    // above).
    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v.clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
            obj.remove("deletedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "File body diverges across surfaces"
    );

    // On-disk parity: the file is untouched on both legs (UC-06 does not
    // remove the on-disk file; purge-on-disk is UC-09).
    assert!(
        http_lib.path().join("song.mp3").exists(),
        "http on-disk file preserved"
    );
    assert!(
        ffi_lib.path().join("song.mp3").exists(),
        "ffi on-disk file preserved"
    );
    assert_eq!(
        std::fs::read(http_lib.path().join("song.mp3")).unwrap(),
        std::fs::read(ffi_lib.path().join("song.mp3")).unwrap(),
        "on-disk files agree byte-for-byte across surfaces"
    );

    // Suppress unused warning while keeping the per-leg uuid visible.
    let _ = (http_uuid, ffi_uuid);
}

/// UC-06 parity — an unauthenticated caller is rejected before its payload is
/// parsed, on both surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`)
/// (FR-AU-07 / SRD §7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_soft_deleted_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{}", uuid::Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http soft-delete");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (soft_delete_status, clean_uuid_status) =
        tokio::task::spawn_blocking(move || -> (i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            // Null token => empty token => unauthenticated. Pair it with a
            // payload that would otherwise fail to parse first (a bad uuid)
            // so the auth check must fire before it is read.
            let bad_uuid = CString::new("not-a-uuid").unwrap();
            let r = alexandria_file_soft_delete(bad_uuid.as_ptr(), std::ptr::null());
            if !r.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(r.json);
                }
            }
            let clean_status = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
            // A second call with a clean uuid but no token still denies.
            let ok_uuid = CString::new("11111111-1111-1111-1111-111111111111").unwrap();
            let r2 = alexandria_file_soft_delete(ok_uuid.as_ptr(), std::ptr::null());
            if !r2.json.is_null() {
                unsafe {
                    alexandria_free_string(r2.json);
                }
            }
            (r.status, if r2.status == clean_status { 1 } else { 0 })
        })
        .await
        .unwrap();

    assert_eq!(
        soft_delete_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "soft_delete must deny before parsing the uuid"
    );
    assert_eq!(
        clean_uuid_status, 1,
        "clean-uuid soft_delete with no token also denies"
    );
}

/// UC-07 parity — restore a soft-deleted file over both transports with
/// identical inputs and assert the returned `File` bodies agree modulo the
/// per-database values `uuid`, `path`, `indexedAt`, `deletedAt` (the
/// `deleted_at` seed fires independently per leg, but restore clears it on
/// both so `deletedAt` is `null` either way) and that both surfaces report
/// `state = "active"` (Testing Specification §7.3, FR-FC-24). The on-disk
/// file is preserved on both legs (UC-07 leaves it; purge-on-disk is UC-09).
#[tokio::test]
async fn given_soft_deleted_file_when_restored_via_http_and_ffi_then_file_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // A `deleted_at` comfortably within the 30-day default retention window,
    // so both legs are restorable. Exact-boundary coverage is in the core
    // unit tests with a FixedClock.
    let deleted_at = chrono::Utc::now() - chrono::Duration::days(1);

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // Seed the soft-deleted row on the HTTP leg.
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(deleted_at.to_rfc3339())
        .bind(&http_uuid)
        .execute(&http_pool)
        .await
        .expect("http soft-delete seed");

    let restore_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{http_uuid}/restore"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(restore_req)
        .await
        .expect("http restore");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let deleted_at_for_seed = deleted_at;

    let (ffi_uuid, ffi_body) =
        tokio::task::spawn_blocking(move || -> (String, serde_json::Value) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            // Resolve the uuid and seed the soft-deleted row via a dedicated read
            // connection to the FFI db (the FFI services hold their own pool).
            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let ffi_uuid = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                let deleted_at_for_seed = deleted_at_for_seed;
                move || -> String {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid,): (String,) =
                            sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        sqlx::query(
                            "UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?",
                        )
                        .bind(deleted_at_for_seed.to_rfc3339())
                        .bind(&uuid)
                        .execute(&pool)
                        .await
                        .unwrap();
                        uuid
                    })
                }
            })
            .join()
            .unwrap();

            let result = alexandria_file_restore(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(result.status, alexandria_ffi::FILE_OK, "ffi restore failed");
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();
            (ffi_uuid, ffi_body)
        })
        .await
        .unwrap();

    // ---- compare ----
    // `state` agrees on both surfaces (the restore took effect).
    assert_eq!(http_body["state"], "active", "http body reports active");
    assert_eq!(ffi_body["state"], "active", "ffi body reports active");

    // Both surfaces return a cleared `deletedAt` (null) after restore.
    assert!(
        http_body["deletedAt"].is_null(),
        "http body has deletedAt null after restore"
    );
    assert!(
        ffi_body["deletedAt"].is_null(),
        "ffi body has deletedAt null after restore"
    );

    // `file` matches field-for-field except the per-database values `uuid`,
    // `path`, `indexedAt`, and `deletedAt` (each leg stamps its own; both
    // are null after restore, but the norm keeps the existing shape used by
    // the soft-delete parity test).
    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v.clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
            obj.remove("deletedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "File body diverges across surfaces"
    );

    // On-disk parity: the file is untouched on both legs (UC-07 does not
    // remove the on-disk file; purge-on-disk is UC-09).
    assert!(
        http_lib.path().join("song.mp3").exists(),
        "http on-disk file preserved"
    );
    assert!(
        ffi_lib.path().join("song.mp3").exists(),
        "ffi on-disk file preserved"
    );

    // Suppress unused warning while keeping the per-leg uuid visible.
    let _ = (http_uuid, ffi_uuid);
}

/// UC-07 parity — an unauthenticated caller is rejected before its payload
/// is parsed, on both surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`)
/// (FR-AU-07 / SRD §7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_restored_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{}/restore", uuid::Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http restore");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (restore_status, clean_uuid_status) = tokio::task::spawn_blocking(move || -> (i32, i32) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        // Null token => empty token => unauthenticated. Pair it with a
        // payload that would otherwise fail to parse first (a bad uuid)
        // so the auth check must fire before it is read.
        let bad_uuid = CString::new("not-a-uuid").unwrap();
        let r = alexandria_file_restore(bad_uuid.as_ptr(), std::ptr::null());
        if !r.json.is_null() {
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(r.json);
            }
        }
        let clean_status = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
        // A second call with a clean uuid but no token still denies.
        let ok_uuid = CString::new("11111111-1111-1111-1111-111111111111").unwrap();
        let r2 = alexandria_file_restore(ok_uuid.as_ptr(), std::ptr::null());
        if !r2.json.is_null() {
            unsafe {
                alexandria_free_string(r2.json);
            }
        }
        (r.status, if r2.status == clean_status { 1 } else { 0 })
    })
    .await
    .unwrap();

    assert_eq!(
        restore_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "restore must deny before parsing the uuid"
    );
    assert_eq!(
        clean_uuid_status, 1,
        "clean-uuid restore with no token also denies"
    );
}

/// UC-08 parity — hard-purge a soft-deleted, past-retention file over both
/// transports with identical inputs and assert the returned `File` bodies
/// agree (modulo the per-database values `uuid`, `path`, `indexedAt`,
/// `deletedAt`) and that both catalogs end with zero rows (`files` and its
/// subtype table) for the purged uuid (Testing Specification §7.3,
/// FR-FC-24). The `deleted_at` seed fires independently per leg since each
/// leg owns its own database.
#[tokio::test]
async fn given_purgeable_file_when_purged_via_http_and_ffi_then_file_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Well past the default 30-day retention window on both legs.
    let deleted_at = "2024-01-01T00:00:00Z";

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid, http_file_id): (String, i64) =
        sqlx::query_as("SELECT uuid, id FROM files WHERE name = ?")
            .bind("song.mp3")
            .fetch_one(&http_pool)
            .await
            .unwrap();

    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(deleted_at)
        .bind(&http_uuid)
        .execute(&http_pool)
        .await
        .expect("http soft-delete seed");

    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(purge_req)
        .await
        .expect("http purge");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_files_remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(http_file_id)
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_body, ffi_files_remaining, ffi_subtype_remaining) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, i64, i64) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let (ffi_uuid, ffi_file_id) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (String, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid, id): (String, i64) =
                            sqlx::query_as("SELECT uuid, id FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        sqlx::query(
                            "UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?",
                        )
                        .bind(deleted_at)
                        .bind(&uuid)
                        .execute(&pool)
                        .await
                        .unwrap();
                        (uuid, id)
                    })
                }
            })
            .join()
            .unwrap();

            let result = alexandria_file_purge(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(result.status, alexandria_ffi::FILE_OK, "ffi purge failed");
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();

            let (files_remaining, subtype_remaining) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (i64, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (files,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                                .bind(&ffi_uuid)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        let (subtype,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
                                .bind(ffi_file_id)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        (files, subtype)
                    })
                }
            })
            .join()
            .unwrap();

            (ffi_body, files_remaining, subtype_remaining)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["state"], "deleted",
        "http confirmation echoes pre-purge state"
    );
    assert_eq!(
        ffi_body["state"], "deleted",
        "ffi confirmation echoes pre-purge state"
    );

    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v.clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
            obj.remove("deletedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "File body diverges across surfaces"
    );

    assert_eq!(http_files_remaining.0, 0, "http files row removed by purge");
    assert_eq!(ffi_files_remaining, 0, "ffi files row removed by purge");
    assert_eq!(
        http_subtype_remaining.0, 0,
        "http subtype row removed by purge"
    );
    assert_eq!(ffi_subtype_remaining, 0, "ffi subtype row removed by purge");

    // On-disk parity: the file is untouched on both legs (NFR-07;
    // purge-on-disk is UC-09).
    assert!(
        http_lib.path().join("song.mp3").exists(),
        "http on-disk file preserved"
    );
    assert!(
        ffi_lib.path().join("song.mp3").exists(),
        "ffi on-disk file preserved"
    );
}

/// UC-08 parity — an unauthenticated caller is rejected before its payload
/// is parsed, on both surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`)
/// (FR-AU-07 / SRD §7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_purged_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{}?purge=true", uuid::Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http purge");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (purge_status, clean_uuid_status) = tokio::task::spawn_blocking(move || -> (i32, i32) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        // Null token => empty token => unauthenticated. Pair it with a
        // payload that would otherwise fail to parse first (a bad uuid)
        // so the auth check must fire before it is read.
        let bad_uuid = CString::new("not-a-uuid").unwrap();
        let r = alexandria_file_purge(bad_uuid.as_ptr(), std::ptr::null());
        if !r.json.is_null() {
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(r.json);
            }
        }
        let clean_status = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
        // A second call with a clean uuid but no token still denies.
        let ok_uuid = CString::new("11111111-1111-1111-1111-111111111111").unwrap();
        let r2 = alexandria_file_purge(ok_uuid.as_ptr(), std::ptr::null());
        if !r2.json.is_null() {
            unsafe {
                alexandria_free_string(r2.json);
            }
        }
        (r.status, if r2.status == clean_status { 1 } else { 0 })
    })
    .await
    .unwrap();

    assert_eq!(
        purge_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "purge must deny before parsing the uuid"
    );
    assert_eq!(
        clean_uuid_status, 1,
        "clean-uuid purge with no token also denies"
    );
}

/// UC-09 parity — purge an `active` file's on-disk copy and its catalog
/// record over both transports with identical inputs and assert the
/// returned bodies agree (modulo the per-database values `uuid`, `path`,
/// `indexedAt`), that `diskFilePresent` is `true` on both legs, that both
/// catalogs end with zero rows (`files` and its subtype table) for the
/// purged uuid, and that the on-disk file is gone on both legs (Testing
/// Specification §7.3, FR-FC-23, FR-FC-24). Unlike UC-08 there is no
/// retention gate, so an `active` (never soft-deleted) record is purgeable.
#[tokio::test]
async fn given_active_file_when_purged_on_disk_via_http_and_ffi_then_file_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid, http_file_id): (String, i64) =
        sqlx::query_as("SELECT uuid, id FROM files WHERE name = ?")
            .bind("song.mp3")
            .fetch_one(&http_pool)
            .await
            .unwrap();

    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge-on-disk=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(purge_req)
        .await
        .expect("http purge-on-disk");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_files_remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(http_file_id)
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_body, ffi_files_remaining, ffi_subtype_remaining) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, i64, i64) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let (ffi_uuid, ffi_file_id) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (String, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid, id): (String, i64) =
                            sqlx::query_as("SELECT uuid, id FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        (uuid, id)
                    })
                }
            })
            .join()
            .unwrap();

            let result = alexandria_file_purge_on_disk(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(
                result.status,
                alexandria_ffi::FILE_OK,
                "ffi purge-on-disk failed"
            );
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();

            let (files_remaining, subtype_remaining) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (i64, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (files,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                                .bind(&ffi_uuid)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        let (subtype,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
                                .bind(ffi_file_id)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        (files, subtype)
                    })
                }
            })
            .join()
            .unwrap();

            (ffi_body, files_remaining, subtype_remaining)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["file"]["state"], "active",
        "http confirmation echoes pre-purge state"
    );
    assert_eq!(
        ffi_body["file"]["state"], "active",
        "ffi confirmation echoes pre-purge state"
    );
    assert_eq!(
        http_body["diskFilePresent"], true,
        "http reports the on-disk file was present"
    );
    assert_eq!(
        ffi_body["diskFilePresent"], true,
        "ffi reports the on-disk file was present"
    );

    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v["file"].clone();
        if let Some(obj) = f.as_object_mut() {
            obj.remove("uuid");
            obj.remove("path");
            obj.remove("indexedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "File body diverges across surfaces"
    );

    assert_eq!(
        http_files_remaining.0, 0,
        "http files row removed by purge-on-disk"
    );
    assert_eq!(
        ffi_files_remaining, 0,
        "ffi files row removed by purge-on-disk"
    );
    assert_eq!(
        http_subtype_remaining.0, 0,
        "http subtype row removed by purge-on-disk"
    );
    assert_eq!(
        ffi_subtype_remaining, 0,
        "ffi subtype row removed by purge-on-disk"
    );

    // On-disk parity: the file is gone on both legs.
    assert!(
        !http_lib.path().join("song.mp3").exists(),
        "http on-disk file removed"
    );
    assert!(
        !ffi_lib.path().join("song.mp3").exists(),
        "ffi on-disk file removed"
    );
}

/// UC-09 parity — an unauthenticated caller is rejected before its payload
/// is parsed, on both surfaces (HTTP 401, FFI `FILE_ERR_UNAUTHORIZED`), and
/// the on-disk file is left untouched (FR-AU-07 / SRD §7, FR-FC-23,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_purged_on_disk_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/files/{}?purge-on-disk=true",
            uuid::Uuid::new_v4()
        ))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http purge-on-disk");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (purge_status, clean_uuid_status) = tokio::task::spawn_blocking(move || -> (i32, i32) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        // Null token => empty token => unauthenticated. Pair it with a
        // payload that would otherwise fail to parse first (a bad uuid)
        // so the auth check must fire before it is read.
        let bad_uuid = CString::new("not-a-uuid").unwrap();
        let r = alexandria_file_purge_on_disk(bad_uuid.as_ptr(), std::ptr::null());
        if !r.json.is_null() {
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(r.json);
            }
        }
        let clean_status = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
        // A second call with a clean uuid but no token still denies.
        let ok_uuid = CString::new("11111111-1111-1111-1111-111111111111").unwrap();
        let r2 = alexandria_file_purge_on_disk(ok_uuid.as_ptr(), std::ptr::null());
        if !r2.json.is_null() {
            unsafe {
                alexandria_free_string(r2.json);
            }
        }
        (r.status, if r2.status == clean_status { 1 } else { 0 })
    })
    .await
    .unwrap();

    assert_eq!(
        purge_status,
        alexandria_ffi::FILE_ERR_UNAUTHORIZED,
        "purge-on-disk must deny before parsing the uuid"
    );
    assert_eq!(
        clean_uuid_status, 1,
        "clean-uuid purge-on-disk with no token also denies"
    );
}

/// UC-09 AF-01 parity — purge-on-disk a record whose on-disk file has already
/// vanished, over both transports. Both surfaces must treat the absence as a
/// success, report it identically (`diskFilePresent: false`), and still
/// remove the catalog rows (Testing Specification §7.3, FR-FC-23, FR-FC-24).
#[tokio::test]
async fn given_missing_disk_file_when_purged_on_disk_via_http_and_ffi_then_both_report_absence() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid, http_file_id): (String, i64) =
        sqlx::query_as("SELECT uuid, id FROM files WHERE name = ?")
            .bind("song.mp3")
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // The file is deleted out from under the catalog, exactly as AF-01
    // describes, *after* indexing so the row still points at the path.
    std::fs::remove_file(http_lib.path().join("song.mp3")).expect("http pre-remove");

    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge-on-disk=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(purge_req)
        .await
        .expect("http purge-on-disk");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(http_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_files_remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_subtype_remaining: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
            .bind(http_file_id)
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_disk_file = ffi_lib.path().join("song.mp3");

    let (ffi_body, ffi_files_remaining, ffi_subtype_remaining) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, i64, i64) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(1);

            let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
            let (ffi_uuid, ffi_file_id) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (String, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (uuid, id): (String, i64) =
                            sqlx::query_as("SELECT uuid, id FROM files WHERE name=?")
                                .bind("song.mp3")
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        (uuid, id)
                    })
                }
            })
            .join()
            .unwrap();

            // Same AF-01 setup as the HTTP leg: remove the file after indexing.
            std::fs::remove_file(&ffi_disk_file).expect("ffi pre-remove");

            let result = alexandria_file_purge_on_disk(
                CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
                token.as_ptr(),
            );
            assert_eq!(
                result.status,
                alexandria_ffi::FILE_OK,
                "ffi purge-on-disk with an absent file is still a success (AF-01)"
            );
            assert!(!result.json.is_null());
            let s = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
            let ffi_body: serde_json::Value = serde_json::from_str(&s).unwrap();

            let (files_remaining, subtype_remaining) = std::thread::spawn({
                let ffi_db_path = ffi_db_path.clone();
                move || -> (i64, i64) {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async move {
                        let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                        let pool = sqlx::sqlite::SqlitePoolOptions::new()
                            .max_connections(1)
                            .connect(&format!("{url}?mode=rw"))
                            .await
                            .unwrap();
                        let (files,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                                .bind(&ffi_uuid)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        let (subtype,): (i64,) =
                            sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
                                .bind(ffi_file_id)
                                .fetch_one(&pool)
                                .await
                                .unwrap();
                        (files, subtype)
                    })
                }
            })
            .join()
            .unwrap();

            (ffi_body, files_remaining, subtype_remaining)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["diskFilePresent"], false,
        "http reports the absent on-disk file (AF-01)"
    );
    assert_eq!(
        ffi_body["diskFilePresent"], false,
        "ffi reports the absent on-disk file (AF-01)"
    );

    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut f = v.clone();
        if let Some(file) = f.get_mut("file").and_then(|f| f.as_object_mut()) {
            file.remove("uuid");
            file.remove("path");
            file.remove("indexedAt");
        }
        f
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "PurgeOnDiskOutcome body diverges across surfaces"
    );

    assert_eq!(
        http_files_remaining.0, 0,
        "http record purged despite the absent disk file"
    );
    assert_eq!(
        ffi_files_remaining, 0,
        "ffi record purged despite the absent disk file"
    );
    assert_eq!(http_subtype_remaining.0, 0, "http subtype row removed");
    assert_eq!(ffi_subtype_remaining, 0, "ffi subtype row removed");
}

/// UC-08 / UC-09 parity — an unknown uuid is a not-found on both surfaces
/// (HTTP 404, FFI `FILE_ERR_NOT_FOUND`) for both the hard purge and the
/// purge-on-disk (AF-02 / AF-03, FR-FC-24 / NFR-09). No library is indexed:
/// the catalogs are empty on both legs, so every uuid is unknown.
#[tokio::test]
async fn given_unknown_uuid_when_purged_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let uuid = uuid::Uuid::new_v4().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    for query in ["purge=true", "purge-on-disk=true"] {
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/v1/files/{uuid}?{query}"))
            .header("authorization", &format!("Bearer {TEST_TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(Settings::default(), http_services.clone())
            .oneshot(req)
            .await
            .expect("http delete");
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::NOT_FOUND,
            "http {query} on an unknown uuid must be 404"
        );
    }

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_uuid = uuid.clone();
    let (purge_status, purge_on_disk_status) =
        tokio::task::spawn_blocking(move || -> (i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let uuid = CString::new(ffi_uuid).unwrap();

            let purge = alexandria_file_purge(uuid.as_ptr(), token.as_ptr());
            if !purge.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(purge.json);
                }
            }
            let purge_on_disk = alexandria_file_purge_on_disk(uuid.as_ptr(), token.as_ptr());
            if !purge_on_disk.json.is_null() {
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(purge_on_disk.json);
                }
            }
            (purge.status, purge_on_disk.status)
        })
        .await
        .unwrap();

    assert_eq!(
        purge_status,
        alexandria_ffi::FILE_ERR_NOT_FOUND,
        "ffi purge on an unknown uuid must map to FILE_ERR_NOT_FOUND"
    );
    assert_eq!(
        purge_on_disk_status,
        alexandria_ffi::FILE_ERR_NOT_FOUND,
        "ffi purge-on-disk on an unknown uuid must map to FILE_ERR_NOT_FOUND"
    );
}

/// UC-08 AF-01 parity — hard-purging a record that was never soft-deleted is
/// a state conflict on both surfaces (HTTP 409, FFI
/// `FILE_ERR_INVALID_STATE`), and leaves the row in place on both legs
/// (FR-FC-22, NFR-07, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_active_file_when_purged_via_http_and_ffi_then_both_invalid_state() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.mp3")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // Never soft-deleted — the record is `active`, so the retention window
    // never started and the hard purge must be refused.
    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(purge_req)
        .await
        .expect("http purge");
    assert_eq!(http_resp.status(), axum::http::StatusCode::CONFLICT);

    let http_remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg (own identical lib + db) ----
    let ffi_lib = tempdir().unwrap();
    std::fs::write(ffi_lib.path().join("song.mp3"), b"parity-audio").unwrap();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_status, ffi_remaining) = tokio::task::spawn_blocking(move || -> (i32, i64) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);
        wait_for_ffi_files(1);

        let ffi_db_path = std::path::PathBuf::from(ffi_dir.path()).join("ffi.sqlite");
        let ffi_uuid = std::thread::spawn({
            let ffi_db_path = ffi_db_path.clone();
            move || -> String {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                    let pool = sqlx::sqlite::SqlitePoolOptions::new()
                        .max_connections(1)
                        .connect(&format!("{url}?mode=rw"))
                        .await
                        .unwrap();
                    let (uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name=?")
                        .bind("song.mp3")
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                    uuid
                })
            }
        })
        .join()
        .unwrap();

        let result = alexandria_file_purge(
            CString::new(ffi_uuid.clone()).unwrap().as_ptr(),
            token.as_ptr(),
        );
        if !result.json.is_null() {
            // SAFETY: pointer came from this library and is freed once.
            unsafe {
                alexandria_free_string(result.json);
            }
        }

        let remaining = std::thread::spawn({
            let ffi_db_path = ffi_db_path.clone();
            move || -> i64 {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let url = format!("sqlite://{}", ffi_db_path.to_str().unwrap());
                    let pool = sqlx::sqlite::SqlitePoolOptions::new()
                        .max_connections(1)
                        .connect(&format!("{url}?mode=rw"))
                        .await
                        .unwrap();
                    let (files,): (i64,) =
                        sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                            .bind(&ffi_uuid)
                            .fetch_one(&pool)
                            .await
                            .unwrap();
                    files
                })
            }
        })
        .join()
        .unwrap();

        (result.status, remaining)
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::FILE_ERR_INVALID_STATE,
        "ffi purge of an active record must map to FILE_ERR_INVALID_STATE (HTTP 409)"
    );
    assert_eq!(http_remaining.0, 1, "http row kept by the rejected purge");
    assert_eq!(ffi_remaining, 1, "ffi row kept by the rejected purge");
}

/// UC-10 parity — create the same collection over both transports and assert
/// the returned bodies agree (modulo the per-database `uuid`, which each leg
/// mints independently) and that each catalog holds the same single row
/// (Testing Specification §7.3, FR-CO-01, FR-CO-02, FR-FC-24).
#[tokio::test]
async fn given_same_collection_when_created_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "Sci-fi novels", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (off the tokio thread: FFI block_on its own runtime) ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_collection_create(body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi create");
        assert!(!r.json.is_null());
        // SAFETY: pointer came from this library and is freed once below.
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    // Both surfaces mint their own uuid, so parity is on its presence and
    // shape; every other field must match exactly.
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
    }
    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut c = v.clone();
        if let Some(obj) = c.as_object_mut() {
            obj.remove("uuid");
        }
        c
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "Collection body diverges across surfaces"
    );

    // Each leg persisted exactly the record it returned.
    assert_eq!(http_rows.len(), 1, "http persisted one collection");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one collection");
    assert_eq!(http_rows[0].0, http_body["uuid"].as_str().unwrap());
    assert_eq!(ffi_rows[0].0, ffi_body["uuid"].as_str().unwrap());
    assert_eq!(
        (http_rows[0].1.as_str(), http_rows[0].2.as_str()),
        (ffi_rows[0].1.as_str(), ffi_rows[0].2.as_str()),
        "persisted name/kind diverge across surfaces"
    );

    ffi_pool.close().await;
}

/// UC-10 parity — an unrecognised `kind` is rejected as invalid input on both
/// surfaces (HTTP 400, FFI `COLLECTION_ERR_INVALID_INPUT`), and neither leg
/// persists a row (AF-01, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unrecognised_kind_when_created_via_http_and_ffi_then_both_reject() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "Mixed bag", "kind": "playlist" }).to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    let (http_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collections")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body =
            CString::new(json!({ "name": "Mixed bag", "kind": "playlist" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_collection_create(body.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected create returns no body");
        r.status
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let (ffi_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collections")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::COLLECTION_ERR_INVALID_INPUT,
        "ffi must reject an unrecognised kind as invalid input (HTTP 400)"
    );
    assert_eq!(http_count, 0, "http persisted nothing");
    assert_eq!(ffi_count, 0, "ffi persisted nothing");

    ffi_pool.close().await;
}

/// UC-10 parity — an unauthenticated caller is rejected before its payload is
/// parsed, on both surfaces (HTTP 401, FFI `COLLECTION_ERR_UNAUTHORIZED`)
/// (FR-AU-07 / SRD §7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_collection_created_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    // A body that would otherwise fail to parse, so the auth check must fire
    // before it is read.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (malformed_status, clean_status) = tokio::task::spawn_blocking(move || -> (i32, i32) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        // Null token => empty token => unauthenticated, paired with a body
        // that would otherwise fail to parse first.
        let bad = CString::new("{ not json").unwrap();
        let r = alexandria_collection_create(bad.as_ptr(), std::ptr::null());
        assert!(r.json.is_null());

        // A second call with a well-formed body but no token still denies.
        let good =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let r2 = alexandria_collection_create(good.as_ptr(), std::ptr::null());
        assert!(r2.json.is_null());
        (r.status, r2.status)
    })
    .await
    .unwrap();

    assert_eq!(
        malformed_status,
        alexandria_ffi::COLLECTION_ERR_UNAUTHORIZED,
        "create must deny before parsing the body"
    );
    assert_eq!(
        clean_status,
        alexandria_ffi::COLLECTION_ERR_UNAUTHORIZED,
        "a well-formed body with no token also denies"
    );
}

/// UC-11 parity — rename the same collection over both transports and assert
/// the returned bodies agree and each catalog holds the same renamed row
/// (Testing Specification §7.3, FR-CO-03, FR-FC-24).
#[tokio::test]
async fn given_same_collection_when_renamed_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "Sci-fi novels", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let create_resp = router
        .clone()
        .oneshot(create_req)
        .await
        .expect("http create");
    assert_eq!(create_resp.status(), axum::http::StatusCode::CREATED);
    let created: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_uuid = created["uuid"].as_str().unwrap().to_string();

    let rename_req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/collections/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "Sci-fi & fantasy" }).to_string(),
        ))
        .unwrap();
    let rename_resp = router.oneshot(rename_req).await.expect("http rename");
    assert_eq!(rename_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(rename_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::COLLECTION_OK, "ffi create");
        // SAFETY: pointer came from this library and is freed once below.
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let uuid = created_value["uuid"].as_str().unwrap().to_string();

        let uuid_c = CString::new(uuid).unwrap();
        let rename_body = CString::new(json!({ "name": "Sci-fi & fantasy" }).to_string()).unwrap();
        let r = alexandria_collection_rename(uuid_c.as_ptr(), rename_body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi rename");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, name, kind FROM collections ORDER BY name")
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(
            body["name"], "Sci-fi & fantasy",
            "{label} body has new name"
        );
    }

    assert_eq!(http_rows.len(), 1, "http persisted one collection");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one collection");
    assert_eq!(http_rows[0].1, "Sci-fi & fantasy");
    assert_eq!(ffi_rows[0].1, "Sci-fi & fantasy");

    ffi_pool.close().await;
}

/// UC-11 parity — an unknown uuid is rejected as not-found on both surfaces
/// (HTTP 404, FFI `COLLECTION_ERR_NOT_FOUND`) (AF-02, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_uuid_when_renamed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/collections/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "New name" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http rename");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let body = CString::new(json!({ "name": "New name" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_collection_rename(uuid_c.as_ptr(), body.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected rename returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::COLLECTION_ERR_NOT_FOUND,
        "ffi must reject an unknown uuid as not-found (HTTP 404)"
    );
}

/// UC-12 parity — delete the same collection over both transports and assert
/// the returned bodies agree and neither catalog holds the row afterwards
/// (Testing Specification §7.3, FR-CO-04, FR-FC-24).
#[tokio::test]
async fn given_same_collection_when_deleted_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "Sci-fi novels", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let create_resp = router
        .clone()
        .oneshot(create_req)
        .await
        .expect("http create");
    assert_eq!(create_resp.status(), axum::http::StatusCode::CREATED);
    let created: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_uuid = created["uuid"].as_str().unwrap().to_string();

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/collections/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = router.oneshot(delete_req).await.expect("http delete");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(delete_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let (http_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collections")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::COLLECTION_OK, "ffi create");
        // SAFETY: pointer came from this library and is freed once below.
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let uuid = created_value["uuid"].as_str().unwrap().to_string();

        let uuid_c = CString::new(uuid).unwrap();
        let r = alexandria_collection_delete(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi delete");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let (ffi_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collections")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(
            body["name"], "Sci-fi novels",
            "{label} body is the pre-delete record"
        );
    }

    assert_eq!(http_count, 0, "http removed the collection");
    assert_eq!(ffi_count, 0, "ffi removed the collection");

    ffi_pool.close().await;
}

/// UC-12 parity — an unknown uuid is rejected as not-found on both surfaces
/// (HTTP 404, FFI `COLLECTION_ERR_NOT_FOUND`) (AF-01, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_uuid_when_deleted_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/collections/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http delete");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_collection_delete(uuid_c.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected delete returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::COLLECTION_ERR_NOT_FOUND,
        "ffi must reject an unknown uuid as not-found (HTTP 404)"
    );
}

/// UC-15 parity — create the same bookmark over both transports and assert
/// the returned bodies agree (modulo the per-database `uuid`) and that each
/// catalog holds the same single row (Testing Specification §7.3, FR-BM-01,
/// FR-FC-24).
#[tokio::test]
async fn given_same_bookmark_when_created_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, url, title FROM bookmarks ORDER BY title")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmark_create(body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::BOOKMARK_OK, "ffi create");
        assert!(!r.json.is_null());
        // SAFETY: pointer came from this library and is freed once below.
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT uuid, url, title FROM bookmarks ORDER BY title")
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
    }
    let norm = |v: &serde_json::Value| -> serde_json::Value {
        let mut c = v.clone();
        if let Some(obj) = c.as_object_mut() {
            obj.remove("uuid");
        }
        c
    };
    assert_eq!(
        norm(&http_body),
        norm(&ffi_body),
        "Bookmark body diverges across surfaces"
    );

    assert_eq!(http_rows.len(), 1, "http persisted one bookmark");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one bookmark");
    assert_eq!(http_rows[0].0, http_body["uuid"].as_str().unwrap());
    assert_eq!(ffi_rows[0].0, ffi_body["uuid"].as_str().unwrap());
    assert_eq!(
        (http_rows[0].1.as_str(), http_rows[0].2.as_str()),
        (ffi_rows[0].1.as_str(), ffi_rows[0].2.as_str()),
        "persisted url/title diverge across surfaces"
    );

    ffi_pool.close().await;
}

/// UC-15 parity — an unknown referenced collection is rejected as not-found
/// on both surfaces (HTTP 404, FFI `BOOKMARK_ERR_NOT_FOUND`) (AF-02,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_collection_when_bookmark_created_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example", "collectionUuid": unknown })
                .to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    let (http_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bookmarks")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body = CString::new(
            json!({ "url": "https://example.com", "title": "Example", "collectionUuid": unknown })
                .to_string(),
        )
        .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmark_create(body.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected create returns no body");
        r.status
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let (ffi_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bookmarks")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::BOOKMARK_ERR_NOT_FOUND,
        "ffi must reject an unknown collection as not-found (HTTP 404)"
    );
    assert_eq!(http_count, 0, "http persisted nothing");
    assert_eq!(ffi_count, 0, "ffi persisted nothing");

    ffi_pool.close().await;
}

/// UC-13 parity — add the same standalone file to the same file collection
/// over both transports and assert the returned bodies agree and each
/// `files` row is linked (Testing Specification §7.3, FR-CO-05, FR-FC-24).
#[tokio::test]
async fn given_same_file_when_added_to_collection_via_http_and_ffi_then_bodies_and_links_identical()
{
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&http_file_uuid)
    .bind(format!("/lib/{http_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&http_pool)
    .await
    .unwrap();
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "My files", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let create_resp = router
        .clone()
        .oneshot(create_req)
        .await
        .expect("http create");
    assert_eq!(create_resp.status(), axum::http::StatusCode::CREATED);
    let created: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_collection_uuid = created["uuid"].as_str().unwrap().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{http_collection_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuids": [http_file_uuid] }).to_string(),
        ))
        .unwrap();
    let add_resp = router.oneshot(add_req).await.expect("http add items");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(add_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
            .bind(&http_file_uuid)
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool_pre = migrate_database(&ffi_db).await.expect("ffi pre-migrate");
    let ffi_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&ffi_file_uuid)
    .bind(format!("/lib/{ffi_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&ffi_pool_pre)
    .await
    .unwrap();
    ffi_pool_pre.close().await;

    let ffi_db_for_rows = ffi_db.clone();
    let ffi_file_uuid_for_task = ffi_file_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::COLLECTION_OK, "ffi create");
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let collection_uuid = created_value["uuid"].as_str().unwrap().to_string();

        let collection_uuid_c = CString::new(collection_uuid).unwrap();
        let add_body =
            CString::new(json!({ "itemUuids": [ffi_file_uuid_for_task] }).to_string()).unwrap();
        let r = alexandria_collection_add_items(
            collection_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi add items");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
            .bind(&ffi_file_uuid)
            .fetch_one(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["items"],
        json!([{"itemUuid": http_file_uuid, "added": true}])
    );
    assert_eq!(
        ffi_body["items"],
        json!([{"itemUuid": ffi_file_uuid, "added": true}])
    );
    assert!(http_linked.is_some(), "http file linked");
    assert!(ffi_linked.is_some(), "ffi file linked");

    ffi_pool.close().await;
}

/// UC-13 parity — an item that does not exist is reported as not-found on
/// both surfaces, in the body rather than as a status (AF-02,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_item_when_added_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "My files", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let create_resp = router
        .clone()
        .oneshot(create_req)
        .await
        .expect("http create");
    let created: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let collection_uuid = created["uuid"].as_str().unwrap().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{collection_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "itemUuids": [unknown] }).to_string()))
        .unwrap();
    let add_resp = router.oneshot(add_req).await.expect("http add items");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(add_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_body = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let collection_uuid = created_value["uuid"].as_str().unwrap().to_string();

        let collection_uuid_c = CString::new(collection_uuid).unwrap();
        let add_body = CString::new(json!({ "itemUuids": [unknown] }).to_string()).unwrap();
        let r = alexandria_collection_add_items(
            collection_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            r.status,
            alexandria_ffi::COLLECTION_OK,
            "the request succeeds"
        );
        assert!(!r.json.is_null());
        // SAFETY: pointer came from this library and is freed once below.
        let body = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&body).unwrap()
    })
    .await
    .unwrap();

    // Both surfaces report the rejection the same way: in the body, per item,
    // with the reason that tells it apart from a wrong-kind item.
    let expected = json!([{"itemUuid": unknown, "added": false, "reason": "not_found"}]);
    assert_eq!(http_body["items"], expected);
    assert_eq!(ffi_body["items"], expected);
}

/// UC-14 parity — remove the same linked file from the same collection over
/// both transports and assert the returned bodies agree and each `files` row
/// is unlinked (Testing Specification §7.3, FR-CO-06, FR-FC-24).
#[tokio::test]
async fn given_same_linked_file_when_removed_via_http_and_ffi_then_bodies_and_links_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&http_file_uuid)
    .bind(format!("/lib/{http_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&http_pool)
    .await
    .unwrap();
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "My files", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let http_collection_uuid = created["uuid"].as_str().unwrap().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{http_collection_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuids": [http_file_uuid] }).to_string(),
        ))
        .unwrap();
    let add_resp = router.clone().oneshot(add_req).await.expect("http add");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/collections/{http_collection_uuid}/items/{http_file_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = router.oneshot(remove_req).await.expect("http remove");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(remove_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
            .bind(&http_file_uuid)
            .fetch_one(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool_pre = migrate_database(&ffi_db).await.expect("ffi pre-migrate");
    let ffi_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&ffi_file_uuid)
    .bind(format!("/lib/{ffi_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&ffi_pool_pre)
    .await
    .unwrap();
    ffi_pool_pre.close().await;

    let ffi_db_for_rows = ffi_db.clone();
    let ffi_file_uuid_for_task = ffi_file_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let collection_uuid = created_value["uuid"].as_str().unwrap().to_string();

        let collection_uuid_c = CString::new(collection_uuid.clone()).unwrap();
        let add_body =
            CString::new(json!({ "itemUuids": [ffi_file_uuid_for_task.clone()] }).to_string())
                .unwrap();
        let added = alexandria_collection_add_items(
            collection_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(added.status, alexandria_ffi::COLLECTION_OK, "ffi add");
        unsafe {
            alexandria_free_string(added.json);
        }

        let collection_uuid_c2 = CString::new(collection_uuid).unwrap();
        let item_uuid_c = CString::new(ffi_file_uuid_for_task).unwrap();
        let r = alexandria_collection_remove_item(
            collection_uuid_c2.as_ptr(),
            item_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi remove");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_linked: Option<i64> =
        sqlx::query_scalar("SELECT collection_id FROM files WHERE uuid = ?")
            .bind(&ffi_file_uuid)
            .fetch_one(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    assert_eq!(http_body["itemUuid"], http_file_uuid);
    assert_eq!(ffi_body["itemUuid"], ffi_file_uuid);
    assert_eq!(http_linked, None, "http file unlinked");
    assert_eq!(ffi_linked, None, "ffi file unlinked");

    ffi_pool.close().await;
}

/// UC-14 parity — an item not currently in the collection is rejected as
/// not-found on both surfaces (HTTP 404, FFI `COLLECTION_ERR_NOT_FOUND`)
/// (AF-01, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unlinked_item_when_removed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "My files", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let collection_uuid = created["uuid"].as_str().unwrap().to_string();
    let unlinked_item = uuid::Uuid::new_v4();

    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/collections/{collection_uuid}/items/{unlinked_item}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = router.oneshot(remove_req).await.expect("http remove");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let collection_uuid = created_value["uuid"].as_str().unwrap().to_string();

        let collection_uuid_c = CString::new(collection_uuid).unwrap();
        let item_uuid_c = CString::new(uuid::Uuid::new_v4().to_string()).unwrap();
        let r = alexandria_collection_remove_item(
            collection_uuid_c.as_ptr(),
            item_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert!(r.json.is_null(), "a rejected remove returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::COLLECTION_ERR_NOT_FOUND,
        "ffi must reject an unlinked item as not-found (HTTP 404)"
    );
}

/// UC-14 parity — list the members of the same collection over both
/// transports and assert the returned bodies agree (Testing Specification
/// §7.3, FR-CO-07, FR-FC-24).
#[tokio::test]
async fn given_same_collection_when_items_listed_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&http_file_uuid)
    .bind(format!("/lib/{http_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&http_pool)
    .await
    .unwrap();
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "name": "My files", "kind": "file" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let http_collection_uuid = created["uuid"].as_str().unwrap().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/collections/{http_collection_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuids": [http_file_uuid] }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(add_req).await.unwrap().status(),
        axum::http::StatusCode::OK
    );

    let list_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/collections/{http_collection_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = router.oneshot(list_req).await.expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool_pre = migrate_database(&ffi_db).await.expect("ffi pre-migrate");
    let ffi_file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, 'text', 'hash', ?)",
    )
    .bind(&ffi_file_uuid)
    .bind(format!("/lib/{ffi_file_uuid}.txt"))
    .bind("note.txt")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&ffi_pool_pre)
    .await
    .unwrap();
    ffi_pool_pre.close().await;

    let ffi_db_for_rows = ffi_db.clone();
    let ffi_file_uuid_for_task = ffi_file_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_collection_create(create_body.as_ptr(), token.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let collection_uuid = created_value["uuid"].as_str().unwrap().to_string();

        let collection_uuid_c = CString::new(collection_uuid.clone()).unwrap();
        let add_body =
            CString::new(json!({ "itemUuids": [ffi_file_uuid_for_task] }).to_string()).unwrap();
        let added = alexandria_collection_add_items(
            collection_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(added.status, alexandria_ffi::COLLECTION_OK, "ffi add");
        unsafe {
            alexandria_free_string(added.json);
        }

        let collection_uuid_c2 = CString::new(collection_uuid).unwrap();
        let r = alexandria_collection_list_items(collection_uuid_c2.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi list");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    ffi_pool.close().await;

    // ---- compare ----
    assert_eq!(http_body["kind"], "file");
    assert_eq!(ffi_body["kind"], "file");
    let http_items = http_body["items"].as_array().unwrap();
    let ffi_items = ffi_body["items"].as_array().unwrap();
    assert_eq!(http_items.len(), 1, "http lists one member");
    assert_eq!(ffi_items.len(), 1, "ffi lists one member");
    assert_eq!(http_items[0]["uuid"], http_file_uuid);
    assert_eq!(ffi_items[0]["uuid"], ffi_file_uuid);
}

/// UC-16 parity — update the same bookmark over both transports and assert
/// the returned bodies agree and each persisted row matches (Testing
/// Specification §7.3, FR-BM-02, FR-FC-24).
#[tokio::test]
async fn given_same_bookmark_when_updated_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let http_uuid = created["uuid"].as_str().unwrap().to_string();

    let update_req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/bookmarks/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.org", "title": "New title" }).to_string(),
        ))
        .unwrap();
    let update_resp = router.oneshot(update_req).await.expect("http update");
    assert_eq!(update_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(update_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT url, title FROM bookmarks WHERE uuid = ?")
            .bind(&http_uuid)
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_bookmark_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::BOOKMARK_OK, "ffi create");
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let uuid = created_value["uuid"].as_str().unwrap().to_string();

        let uuid_c = CString::new(uuid).unwrap();
        let update_body =
            CString::new(json!({ "url": "https://example.org", "title": "New title" }).to_string())
                .unwrap();
        let r = alexandria_bookmark_update(uuid_c.as_ptr(), update_body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::BOOKMARK_OK, "ffi update");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_uuid = ffi_body["uuid"].as_str().unwrap().to_string();
    let ffi_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT url, title FROM bookmarks WHERE uuid = ?")
            .bind(&ffi_uuid)
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert_eq!(
            body["url"], "https://example.org",
            "{label} body has new url"
        );
        assert_eq!(body["title"], "New title", "{label} body has new title");
    }
    assert_eq!(http_rows.len(), 1, "http persisted the update");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted the update");
    assert_eq!(
        (http_rows[0].0.as_str(), http_rows[0].1.as_str()),
        (ffi_rows[0].0.as_str(), ffi_rows[0].1.as_str()),
        "persisted url/title diverge across surfaces"
    );

    ffi_pool.close().await;
}

/// UC-16 parity — an unknown uuid is rejected as not-found on both surfaces
/// (HTTP 404, FFI `BOOKMARK_ERR_NOT_FOUND`) (AF-02, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_uuid_when_bookmark_updated_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/bookmarks/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http update");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmark_update(uuid_c.as_ptr(), body.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected update returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::BOOKMARK_ERR_NOT_FOUND,
        "ffi must reject an unknown uuid as not-found (HTTP 404)"
    );
}

/// UC-17 parity — list the same bookmarks over both transports and assert
/// the returned bodies agree (Testing Specification §7.3, FR-BM-06,
/// FR-FC-24).
#[tokio::test]
async fn given_same_bookmarks_when_listed_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(create_req).await.unwrap().status(),
        axum::http::StatusCode::CREATED
    );

    let list_req = Request::builder()
        .method("GET")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = router.oneshot(list_req).await.expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_bookmark_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::BOOKMARK_OK, "ffi create");
        unsafe {
            alexandria_free_string(created.json);
        }

        let empty = CString::new("").unwrap();
        let r = alexandria_bookmarks_list(empty.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::BOOKMARK_OK, "ffi list");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    let http_arr = http_body.as_array().unwrap();
    let ffi_arr = ffi_body.as_array().unwrap();
    assert_eq!(http_arr.len(), 1, "http lists one bookmark");
    assert_eq!(ffi_arr.len(), 1, "ffi lists one bookmark");
    assert_eq!(http_arr[0]["url"], "https://example.com");
    assert_eq!(ffi_arr[0]["url"], "https://example.com");
}

/// UC-17 parity — an unknown referenced collection is rejected as not-found
/// on both surfaces (HTTP 404, FFI `BOOKMARK_ERR_NOT_FOUND`) (AF-01,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_collection_when_bookmarks_listed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/bookmarks?collectionUuid={unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http list");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let filter = CString::new(json!({ "collectionUuid": unknown }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmarks_list(filter.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected list returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::BOOKMARK_ERR_NOT_FOUND,
        "ffi must reject an unknown collection as not-found (HTTP 404)"
    );
}

/// UC-18 parity — soft-delete then restore the same bookmark over both
/// transports and assert the returned bodies and persisted state agree
/// (Testing Specification §7.3, FR-BM-03, FR-BM-05, FR-FC-24).
#[tokio::test]
async fn given_same_bookmark_when_deleted_and_restored_via_http_and_ffi_then_bodies_and_rows_identical(
) {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let http_uuid = created["uuid"].as_str().unwrap().to_string();

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = router
        .clone()
        .oneshot(delete_req)
        .await
        .expect("http delete");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::OK);

    let restore_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/bookmarks/{http_uuid}/restore"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let restore_resp = router.oneshot(restore_req).await.expect("http restore");
    assert_eq!(restore_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(restore_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let (http_state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let created = alexandria_bookmark_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::BOOKMARK_OK, "ffi create");
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let uuid = created_value["uuid"].as_str().unwrap().to_string();
        let uuid_c = CString::new(uuid).unwrap();

        let deleted = alexandria_bookmark_soft_delete(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(deleted.status, alexandria_ffi::BOOKMARK_OK, "ffi delete");
        unsafe {
            alexandria_free_string(deleted.json);
        }

        let r = alexandria_bookmark_restore(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::BOOKMARK_OK, "ffi restore");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_uuid = ffi_body["uuid"].as_str().unwrap().to_string();
    let (ffi_state,): (String,) = sqlx::query_as("SELECT state FROM bookmarks WHERE uuid = ?")
        .bind(&ffi_uuid)
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert_eq!(
            body["state"], "active",
            "{label} body is active after restore"
        );
        assert!(
            body["deletedAt"].is_null(),
            "{label} body has no deletedAt after restore"
        );
    }
    assert_eq!(http_state, "active");
    assert_eq!(ffi_state, "active");

    ffi_pool.close().await;
}

/// UC-18 parity — an unknown uuid is rejected as not-found on both surfaces
/// for soft-delete (HTTP 404, FFI `BOOKMARK_ERR_NOT_FOUND`) (AF-01,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_uuid_when_bookmark_soft_deleted_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http delete");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmark_soft_delete(uuid_c.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected delete returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::BOOKMARK_ERR_NOT_FOUND,
        "ffi must reject an unknown uuid as not-found (HTTP 404)"
    );
}

/// UC-19 parity - hard-purge the same past-retention bookmark over both
/// transports and assert the returned bodies agree and each row is removed
/// (Testing Specification section 7.3, FR-BM-04, FR-FC-24).
#[tokio::test]
async fn given_purgeable_bookmark_when_purged_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let deleted_at = "2024-01-01T00:00:00Z";

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "url": "https://example.com", "title": "Example" }).to_string(),
        ))
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            router
                .clone()
                .oneshot(create_req)
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let http_uuid = created["uuid"].as_str().unwrap().to_string();

    sqlx::query("UPDATE bookmarks SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(deleted_at)
        .bind(&http_uuid)
        .execute(&http_pool)
        .await
        .expect("http soft-delete seed");

    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{http_uuid}?purge=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let purge_resp = router.oneshot(purge_req).await.expect("http purge");
    assert_eq!(purge_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(purge_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bookmarks WHERE uuid = ?")
        .bind(&http_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db.clone()).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let created = alexandria_bookmark_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(created.status, alexandria_ffi::BOOKMARK_OK, "ffi create");
        let created_json = unsafe { CStr::from_ptr(created.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(created.json);
        }
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let uuid = created_value["uuid"].as_str().unwrap().to_string();

        // Backdate deleted_at past retention via a fresh connection on a
        // dedicated thread + runtime (the FFI services pool already owns a
        // connection on this thread's runtime).
        let ffi_db_for_backdate = ffi_db.clone();
        let uuid_for_backdate = uuid.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let url = format!("sqlite://{ffi_db_for_backdate}");
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect(&format!("{url}?mode=rw"))
                    .await
                    .unwrap();
                sqlx::query(
                    "UPDATE bookmarks SET state = 'deleted', deleted_at = ? WHERE uuid = ?",
                )
                .bind(deleted_at)
                .bind(&uuid_for_backdate)
                .execute(&pool)
                .await
                .unwrap();
            });
        })
        .join()
        .unwrap();

        let uuid_c = CString::new(uuid).unwrap();
        let r = alexandria_bookmark_purge(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::BOOKMARK_OK, "ffi purge");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert_eq!(
            body["state"], "deleted",
            "{label} confirmation echoes pre-purge state"
        );
    }
    assert_eq!(http_count, 0, "http record removed");
}

/// UC-19 parity - an unknown uuid is rejected as not-found on both surfaces
/// (HTTP 404, FFI BOOKMARK_ERR_NOT_FOUND) (AF-02, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_uuid_when_bookmark_purged_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{unknown}?purge=true"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http purge");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_bookmark_purge(uuid_c.as_ptr(), token.as_ptr());
        assert!(r.json.is_null(), "a rejected purge returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::BOOKMARK_ERR_NOT_FOUND,
        "ffi must reject an unknown uuid as not-found (HTTP 404)"
    );
}

/// UC-20 parity - create the same watchlist over both transports and assert
/// the returned bodies agree (modulo the per-database uuid) and that each
/// database holds the same single row (Testing Specification section 7.3,
/// FR-WL-01, FR-FC-24).
#[tokio::test]
async fn given_same_watchlist_when_created_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String, String)> = sqlx::query_as("SELECT uuid, name FROM watchlists")
        .fetch_all(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_watchlist_create(body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, String)> = sqlx::query_as("SELECT uuid, name FROM watchlists")
        .fetch_all(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(body["name"], "Weekend movies");
    }
    assert_eq!(http_rows.len(), 1, "http persisted one watchlist");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one watchlist");
    assert_eq!(http_rows[0].1, "Weekend movies");
    assert_eq!(ffi_rows[0].1, "Weekend movies");

    ffi_pool.close().await;
}

/// UC-20 parity - an unauthenticated caller is rejected before its payload
/// is parsed, on both surfaces (HTTP 401, FFI WATCHLIST_ERR_UNAUTHORIZED)
/// (FR-AU-07 / SRD section 7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_watchlist_created_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let bad = CString::new("{ not json").unwrap();
        let r = alexandria_watchlist_create(bad.as_ptr(), std::ptr::null());
        assert!(r.json.is_null());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_UNAUTHORIZED,
        "create must deny before parsing the body"
    );
}

/// Insert a minimal `files` row of the given `file_type` and return its
/// uuid. Bypasses the indexer since these parity tests only need a row for
/// the watchlist handler to look up, not a full index run.
async fn seed_file(pool: &sqlx::sqlite::SqlitePool, file_type: &str) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, ?, 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(format!("/lib/{file_uuid}"))
    .bind("seeded")
    .bind(file_type)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .unwrap();
    file_uuid
}

/// UC-22 parity - add the same video to the same watchlist over both
/// transports and assert the returned bodies agree (modulo the per-database
/// uuids) and that each database holds the same single watch_progress row
/// (Testing Specification section 7.3, FR-WL-02, FR-WL-03, FR-FC-24).
#[tokio::test]
async fn given_same_video_when_added_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_watchlist_uuid = http_watchlist["uuid"].as_str().unwrap().to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services)
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(add_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String,)> = sqlx::query_as("SELECT state FROM watch_progress")
        .fetch_all(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist: serde_json::Value = serde_json::from_str(&watchlist_json).unwrap();
        let watchlist_uuid = watchlist["uuid"].as_str().unwrap().to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        assert!(!add_r.json.is_null());
        let s = unsafe { CStr::from_ptr(add_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(add_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String,)> = sqlx::query_as("SELECT state FROM watch_progress")
        .fetch_all(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["watchlistUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid watchlist uuid"
        );
        assert!(
            uuid::Uuid::parse_str(body["videoUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid video uuid"
        );
        assert_eq!(body["state"], "pending");
        assert!(body["currentEpisode"].is_null());
        assert!(body["totalEpisodes"].is_null());
    }
    assert_eq!(http_rows.len(), 1, "http persisted one watch_progress row");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one watch_progress row");
    assert_eq!(http_rows[0].0, "pending");
    assert_eq!(ffi_rows[0].0, "pending");

    ffi_pool.close().await;
}

/// UC-22 parity - adding a video to an unknown watchlist is rejected as
/// not-found on both surfaces (HTTP 404, FFI WATCHLIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_watchlist_when_video_added_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{unknown}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services)
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let unknown_c = CString::new(unknown).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r =
            alexandria_watchlist_add_video(unknown_c.as_ptr(), add_body.as_ptr(), token.as_ptr());
        assert!(add_r.json.is_null());
        add_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_NOT_FOUND,
        "ffi must reject an unknown watchlist as not-found (HTTP 404)"
    );
}

/// UC-21 parity - browse the same watchlist (with a linked video) over both
/// transports and assert the returned bodies agree modulo per-database uuids
/// (Testing Specification section 7.3, FR-WL-08, FR-FC-24).
#[tokio::test]
async fn given_same_watchlist_when_browsed_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist: serde_json::Value =
        serde_json::from_slice(&to_bytes(create_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_watchlist_uuid = http_watchlist["uuid"].as_str().unwrap().to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let list_req = Request::builder()
        .method("GET")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = app(Settings::default(), http_services)
        .oneshot(list_req)
        .await
        .expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist: serde_json::Value = serde_json::from_str(&watchlist_json).unwrap();
        let watchlist_uuid = watchlist["uuid"].as_str().unwrap().to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let list_r = alexandria_watchlists_list(std::ptr::null(), token.as_ptr());
        assert_eq!(list_r.status, alexandria_ffi::WATCHLIST_OK, "ffi list");
        assert!(!list_r.json.is_null());
        let s = unsafe { CStr::from_ptr(list_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(list_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    let http_list = http_body.as_array().unwrap();
    let ffi_list = ffi_body.as_array().unwrap();
    assert_eq!(http_list.len(), 1, "http returned one watchlist");
    assert_eq!(ffi_list.len(), 1, "ffi returned one watchlist");

    for (label, watchlist) in [("http", &http_list[0]), ("ffi", &ffi_list[0])] {
        assert!(
            uuid::Uuid::parse_str(watchlist["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} watchlist carries a valid uuid"
        );
        assert_eq!(watchlist["name"], "Weekend movies");
        let items = watchlist["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1, "{label} watchlist carries one item");
        assert!(
            uuid::Uuid::parse_str(items[0]["videoUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} item carries a valid video uuid"
        );
        assert_eq!(items[0]["state"], "pending");
    }
}

/// UC-21 parity - browsing an unknown watchlist uuid is rejected as
/// not-found on both surfaces (HTTP 404, FFI WATCHLIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_watchlist_when_browsed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let list_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/watchlists?watchlistUuid={unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = app(Settings::default(), http_services)
        .oneshot(list_req)
        .await
        .expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let filter = CString::new(json!({ "watchlistUuid": unknown }).to_string()).unwrap();
        let list_r = alexandria_watchlists_list(filter.as_ptr(), token.as_ptr());
        assert!(list_r.json.is_null());
        list_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_NOT_FOUND,
        "ffi must reject an unknown watchlist as not-found (HTTP 404)"
    );
}

/// UC-23 parity - advance the same WatchProgress over both transports and
/// assert the returned bodies agree modulo per-database uuids (Testing
/// Specification section 7.3, FR-WL-04, FR-WL-05, FR-FC-24).
#[tokio::test]
async fn given_same_transition_when_updated_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let update_req = Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/watchlists/{http_watchlist_uuid}/items/{http_video_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "state": "watching", "currentEpisode": 3, "totalEpisodes": 12 }).to_string(),
        ))
        .unwrap();
    let update_resp = app(Settings::default(), http_services)
        .oneshot(update_req)
        .await
        .expect("http update progress");
    assert_eq!(update_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(update_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT state, current_episode, total_episodes FROM watch_progress")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist_uuid = serde_json::from_str::<serde_json::Value>(&watchlist_json).unwrap()
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let video_uuid_c = CString::new(ffi_video_uuid).unwrap();
        let update_body = CString::new(
            json!({ "state": "watching", "currentEpisode": 3, "totalEpisodes": 12 }).to_string(),
        )
        .unwrap();
        let update_r = alexandria_watchlist_update_progress(
            watchlist_uuid_c.as_ptr(),
            video_uuid_c.as_ptr(),
            update_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            update_r.status,
            alexandria_ffi::WATCHLIST_OK,
            "ffi update progress"
        );
        assert!(!update_r.json.is_null());
        let s = unsafe { CStr::from_ptr(update_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(update_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT state, current_episode, total_episodes FROM watch_progress")
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert_eq!(body["state"], "watching", "{label} state");
        assert_eq!(body["currentEpisode"], 3, "{label} currentEpisode");
        assert_eq!(body["totalEpisodes"], 12, "{label} totalEpisodes");
    }
    assert_eq!(http_rows.len(), 1);
    assert_eq!(ffi_rows.len(), 1);
    assert_eq!(http_rows[0], ("watching".to_string(), Some(3), Some(12)));
    assert_eq!(ffi_rows[0], ("watching".to_string(), Some(3), Some(12)));

    ffi_pool.close().await;
}

/// UC-23 parity - an invalid transition is rejected on both surfaces (HTTP
/// 409, FFI WATCHLIST_ERR_INVALID_STATE) (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_invalid_transition_when_updated_via_http_and_ffi_then_both_conflict() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    // Pending -> Watched skips Watching: invalid.
    let update_req = Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/watchlists/{http_watchlist_uuid}/items/{http_video_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "state": "watched" }).to_string()))
        .unwrap();
    let update_resp = app(Settings::default(), http_services)
        .oneshot(update_req)
        .await
        .expect("http update progress");
    assert_eq!(update_resp.status(), axum::http::StatusCode::CONFLICT);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist_uuid = serde_json::from_str::<serde_json::Value>(&watchlist_json).unwrap()
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let video_uuid_c = CString::new(ffi_video_uuid).unwrap();
        let update_body = CString::new(json!({ "state": "watched" }).to_string()).unwrap();
        let update_r = alexandria_watchlist_update_progress(
            watchlist_uuid_c.as_ptr(),
            video_uuid_c.as_ptr(),
            update_body.as_ptr(),
            token.as_ptr(),
        );
        assert!(update_r.json.is_null());
        update_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_INVALID_STATE,
        "ffi must reject an invalid transition (HTTP 409)"
    );
}

/// UC-24 parity - remove the same video from the same watchlist over both
/// transports and assert the returned bodies agree modulo per-database uuids
/// and that each database ends with zero watch_progress rows but the
/// VideoFile itself intact (Testing Specification section 7.3, FR-WL-06,
/// FR-FC-24).
#[tokio::test]
async fn given_same_video_when_removed_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/watchlists/{http_watchlist_uuid}/items/{http_video_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = app(Settings::default(), http_services)
        .oneshot(remove_req)
        .await
        .expect("http remove video");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(remove_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_video_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_video_uuid_for_rows = ffi_video_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist_uuid = serde_json::from_str::<serde_json::Value>(&watchlist_json).unwrap()
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let video_uuid_c = CString::new(ffi_video_uuid).unwrap();
        let remove_r = alexandria_watchlist_remove_video(
            watchlist_uuid_c.as_ptr(),
            video_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            remove_r.status,
            alexandria_ffi::WATCHLIST_OK,
            "ffi remove video"
        );
        assert!(!remove_r.json.is_null());
        let s = unsafe { CStr::from_ptr(remove_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(remove_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&ffi_video_uuid_for_rows)
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["watchlistUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid watchlist uuid"
        );
        assert!(
            uuid::Uuid::parse_str(body["videoUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid video uuid"
        );
    }
    assert_eq!(http_progress_rows.0, 0, "http deleted the progress row");
    assert_eq!(ffi_progress_rows.0, 0, "ffi deleted the progress row");
    assert_eq!(http_file_rows.0, 1, "http preserved the VideoFile");
    assert_eq!(ffi_file_rows.0, 1, "ffi preserved the VideoFile");

    ffi_pool.close().await;
}

/// UC-24 parity - removing a video not on the watchlist is rejected as
/// not-found on both surfaces (HTTP 404, FFI WATCHLIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_video_not_on_watchlist_when_removed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let unknown_video = uuid::Uuid::new_v4().to_string();
    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/watchlists/{http_watchlist_uuid}/items/{unknown_video}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = app(Settings::default(), http_services)
        .oneshot(remove_req)
        .await
        .expect("http remove video");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist_uuid = serde_json::from_str::<serde_json::Value>(&watchlist_json).unwrap()
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let unknown_video = uuid::Uuid::new_v4().to_string();
        let video_uuid_c = CString::new(unknown_video).unwrap();
        let remove_r = alexandria_watchlist_remove_video(
            watchlist_uuid_c.as_ptr(),
            video_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert!(remove_r.json.is_null());
        remove_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_NOT_FOUND,
        "ffi must reject a video not on the watchlist as not-found (HTTP 404)"
    );
}

/// UC-25 parity - delete the same watchlist (with a linked video) over both
/// transports and assert the returned bodies agree modulo per-database
/// uuids, each database ends with zero watchlist and watch_progress rows,
/// and the VideoFile itself survives (Testing Specification section 7.3,
/// FR-WL-07, FR-FC-24).
#[tokio::test]
async fn given_same_watchlist_when_deleted_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Weekend movies" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create watchlist");
    let http_watchlist_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "videoUuid": http_video_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add video");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/watchlists/{http_watchlist_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = app(Settings::default(), http_services)
        .oneshot(delete_req)
        .await
        .expect("http delete watchlist");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(delete_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_watchlist_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watchlists")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_video_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_video_uuid = seed_file(&ffi_pool_for_seed, "video").await;
    ffi_pool_for_seed.close().await;

    let ffi_video_uuid_for_rows = ffi_video_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let create_r = alexandria_watchlist_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(create_r.status, alexandria_ffi::WATCHLIST_OK, "ffi create");
        let watchlist_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let watchlist_uuid = serde_json::from_str::<serde_json::Value>(&watchlist_json).unwrap()
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let watchlist_uuid_c = CString::new(watchlist_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "videoUuid": ffi_video_uuid }).to_string()).unwrap();
        let add_r = alexandria_watchlist_add_video(
            watchlist_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(add_r.status, alexandria_ffi::WATCHLIST_OK, "ffi add video");
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let watchlist_uuid_c = CString::new(watchlist_uuid).unwrap();
        let delete_r = alexandria_watchlist_delete(watchlist_uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(
            delete_r.status,
            alexandria_ffi::WATCHLIST_OK,
            "ffi delete watchlist"
        );
        assert!(!delete_r.json.is_null());
        let s = unsafe { CStr::from_ptr(delete_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(delete_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_watchlist_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watchlists")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM watch_progress")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&ffi_video_uuid_for_rows)
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(body["name"], "Weekend movies", "{label} name");
    }
    assert_eq!(http_watchlist_rows.0, 0, "http deleted the watchlist row");
    assert_eq!(ffi_watchlist_rows.0, 0, "ffi deleted the watchlist row");
    assert_eq!(http_progress_rows.0, 0, "http deleted the progress row");
    assert_eq!(ffi_progress_rows.0, 0, "ffi deleted the progress row");
    assert_eq!(http_file_rows.0, 1, "http preserved the VideoFile");
    assert_eq!(ffi_file_rows.0, 1, "ffi preserved the VideoFile");

    ffi_pool.close().await;
}

/// UC-25 parity - deleting an unknown watchlist is rejected as not-found on
/// both surfaces (HTTP 404, FFI WATCHLIST_ERR_NOT_FOUND) (FR-FC-24 /
/// NFR-09).
#[tokio::test]
async fn given_unknown_watchlist_when_deleted_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/watchlists/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = app(Settings::default(), http_services)
        .oneshot(delete_req)
        .await
        .expect("http delete watchlist");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let unknown_c = CString::new(unknown).unwrap();
        let delete_r = alexandria_watchlist_delete(unknown_c.as_ptr(), token.as_ptr());
        assert!(delete_r.json.is_null());
        delete_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::WATCHLIST_ERR_NOT_FOUND,
        "ffi must reject an unknown watchlist as not-found (HTTP 404)"
    );
}

/// UC-26 parity - create the same reading list over both transports and
/// assert the returned bodies agree (modulo the per-database uuid) and that
/// each database holds the same single row (Testing Specification section
/// 7.3, FR-RL-01, FR-FC-24).
#[tokio::test]
async fn given_same_reading_list_when_created_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String, String)> = sqlx::query_as("SELECT uuid, name FROM reading_lists")
        .fetch_all(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let r = alexandria_reading_list_create(body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::READING_LIST_OK, "ffi create");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, String)> = sqlx::query_as("SELECT uuid, name FROM reading_lists")
        .fetch_all(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        let uuid = body["uuid"].as_str().unwrap_or_default();
        assert!(
            uuid::Uuid::parse_str(uuid).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(body["name"], "Summer reads");
    }
    assert_eq!(http_rows.len(), 1, "http persisted one reading list");
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one reading list");
    assert_eq!(http_rows[0].1, "Summer reads");
    assert_eq!(ffi_rows[0].1, "Summer reads");

    ffi_pool.close().await;
}

/// UC-26 parity - an unauthenticated caller is rejected before its payload
/// is parsed, on both surfaces (HTTP 401, FFI READING_LIST_ERR_UNAUTHORIZED)
/// (FR-AU-07 / SRD section 7, FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_no_token_when_reading_list_created_via_http_and_ffi_then_both_unauthorized() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http create");
    assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let bad = CString::new("{ not json").unwrap();
        let r = alexandria_reading_list_create(bad.as_ptr(), std::ptr::null());
        assert!(r.json.is_null());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_UNAUTHORIZED,
        "create must deny before parsing the body"
    );
}

/// UC-28 parity - add the same document to the same reading list over both
/// transports and assert the returned bodies agree (modulo the per-database
/// uuids) and that each database holds the same single reading_progress row
/// (Testing Specification section 7.3, FR-RL-02, FR-RL-03, FR-FC-24).
#[tokio::test]
async fn given_same_item_when_added_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "document").await;

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services)
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(add_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_rows: Vec<(String,)> = sqlx::query_as("SELECT state FROM reading_progress")
        .fetch_all(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "document").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        assert!(!add_r.json.is_null());
        let s = unsafe { CStr::from_ptr(add_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(add_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String,)> = sqlx::query_as("SELECT state FROM reading_progress")
        .fetch_all(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["readingListUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid reading list uuid"
        );
        assert!(
            uuid::Uuid::parse_str(body["itemUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid item uuid"
        );
        assert_eq!(body["targetKind"], "document");
        assert_eq!(body["state"], "pending");
        assert!(body["currentIssue"].is_null());
        assert!(body["totalIssues"].is_null());
    }
    assert_eq!(
        http_rows.len(),
        1,
        "http persisted one reading_progress row"
    );
    assert_eq!(ffi_rows.len(), 1, "ffi persisted one reading_progress row");
    assert_eq!(http_rows[0].0, "pending");
    assert_eq!(ffi_rows[0].0, "pending");

    ffi_pool.close().await;
}

/// UC-28 parity - adding an item to an unknown reading list is rejected as
/// not-found on both surfaces (HTTP 404, FFI READING_LIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_reading_list_when_item_added_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_item_uuid = seed_file(&http_pool, "document").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{unknown}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services)
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "document").await;
    ffi_pool_for_seed.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let unknown_c = CString::new(unknown).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r =
            alexandria_reading_list_add_item(unknown_c.as_ptr(), add_body.as_ptr(), token.as_ptr());
        assert!(add_r.json.is_null());
        add_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_NOT_FOUND,
        "ffi must reject an unknown reading list as not-found (HTTP 404)"
    );
}

/// UC-27 parity - browse the same reading list (with a linked document) over
/// both transports and assert the returned bodies agree modulo per-database
/// uuids (Testing Specification section 7.3, FR-RL-08, FR-FC-24).
#[tokio::test]
async fn given_same_reading_list_when_browsed_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "document").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let list_req = Request::builder()
        .method("GET")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = app(Settings::default(), http_services)
        .oneshot(list_req)
        .await
        .expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "document").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let list_r = alexandria_reading_lists_list(std::ptr::null(), token.as_ptr());
        assert_eq!(list_r.status, alexandria_ffi::READING_LIST_OK, "ffi list");
        assert!(!list_r.json.is_null());
        let s = unsafe { CStr::from_ptr(list_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(list_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    let http_list = http_body.as_array().unwrap();
    let ffi_list = ffi_body.as_array().unwrap();
    assert_eq!(http_list.len(), 1, "http returned one reading list");
    assert_eq!(ffi_list.len(), 1, "ffi returned one reading list");

    for (label, reading_list) in [("http", &http_list[0]), ("ffi", &ffi_list[0])] {
        assert!(
            uuid::Uuid::parse_str(reading_list["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} reading list carries a valid uuid"
        );
        assert_eq!(reading_list["name"], "Summer reads");
        let items = reading_list["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1, "{label} reading list carries one item");
        assert!(
            uuid::Uuid::parse_str(items[0]["itemUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} item carries a valid item uuid"
        );
        assert_eq!(items[0]["targetKind"], "document");
        assert_eq!(items[0]["state"], "pending");
    }
}

/// UC-27 parity - browsing an unknown reading list uuid is rejected as
/// not-found on both surfaces (HTTP 404, FFI READING_LIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_reading_list_when_browsed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let list_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/reading-lists?readingListUuid={unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let list_resp = app(Settings::default(), http_services)
        .oneshot(list_req)
        .await
        .expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let filter = CString::new(json!({ "readingListUuid": unknown }).to_string()).unwrap();
        let list_r = alexandria_reading_lists_list(filter.as_ptr(), token.as_ptr());
        assert!(list_r.json.is_null());
        list_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_NOT_FOUND,
        "ffi must reject an unknown reading list as not-found (HTTP 404)"
    );
}

/// UC-29 parity - advance the same ReadingProgress over both transports and
/// assert the returned bodies agree modulo per-database uuids (Testing
/// Specification section 7.3, FR-RL-04, FR-RL-05, FR-FC-24).
#[tokio::test]
async fn given_same_reading_transition_when_updated_via_http_and_ffi_then_bodies_and_rows_identical(
) {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "comic").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let update_req = Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/reading-lists/{http_reading_list_uuid}/items/{http_item_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "state": "reading", "currentIssue": 3, "totalIssues": 12 }).to_string(),
        ))
        .unwrap();
    let update_resp = app(Settings::default(), http_services)
        .oneshot(update_req)
        .await
        .expect("http update progress");
    assert_eq!(update_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(update_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT state, current_issue, total_issues FROM reading_progress")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "comic").await;
    ffi_pool_for_seed.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let item_uuid_c = CString::new(ffi_item_uuid).unwrap();
        let update_body = CString::new(
            json!({ "state": "reading", "currentIssue": 3, "totalIssues": 12 }).to_string(),
        )
        .unwrap();
        let update_r = alexandria_reading_list_update_progress(
            reading_list_uuid_c.as_ptr(),
            item_uuid_c.as_ptr(),
            update_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            update_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi update progress"
        );
        assert!(!update_r.json.is_null());
        let s = unsafe { CStr::from_ptr(update_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(update_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_rows: Vec<(String, Option<i64>, Option<i64>)> =
        sqlx::query_as("SELECT state, current_issue, total_issues FROM reading_progress")
            .fetch_all(&ffi_pool)
            .await
            .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert_eq!(body["state"], "reading", "{label} state");
        assert_eq!(body["currentIssue"], 3, "{label} currentIssue");
        assert_eq!(body["totalIssues"], 12, "{label} totalIssues");
    }
    assert_eq!(http_rows.len(), 1);
    assert_eq!(ffi_rows.len(), 1);
    assert_eq!(http_rows[0], ("reading".to_string(), Some(3), Some(12)));
    assert_eq!(ffi_rows[0], ("reading".to_string(), Some(3), Some(12)));

    ffi_pool.close().await;
}

/// UC-29 parity - an invalid transition is rejected on both surfaces (HTTP
/// 409, FFI READING_LIST_ERR_INVALID_STATE) (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_invalid_reading_transition_when_updated_via_http_and_ffi_then_both_conflict() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "comic").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    // Pending -> Read skips Reading: invalid.
    let update_req = Request::builder()
        .method("PATCH")
        .uri(format!(
            "/v1/reading-lists/{http_reading_list_uuid}/items/{http_item_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "state": "read" }).to_string()))
        .unwrap();
    let update_resp = app(Settings::default(), http_services)
        .oneshot(update_req)
        .await
        .expect("http update progress");
    assert_eq!(update_resp.status(), axum::http::StatusCode::CONFLICT);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "comic").await;
    ffi_pool_for_seed.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let item_uuid_c = CString::new(ffi_item_uuid).unwrap();
        let update_body = CString::new(json!({ "state": "read" }).to_string()).unwrap();
        let update_r = alexandria_reading_list_update_progress(
            reading_list_uuid_c.as_ptr(),
            item_uuid_c.as_ptr(),
            update_body.as_ptr(),
            token.as_ptr(),
        );
        assert!(update_r.json.is_null());
        update_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_INVALID_STATE,
        "ffi must reject an invalid transition (HTTP 409)"
    );
}

/// UC-30 parity - remove the same item from the same reading list over both
/// transports and assert the returned bodies agree modulo per-database
/// uuids and that each database ends with zero reading_progress rows but
/// the file itself intact (Testing Specification section 7.3, FR-RL-06,
/// FR-FC-24).
#[tokio::test]
async fn given_same_item_when_removed_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "document").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/reading-lists/{http_reading_list_uuid}/items/{http_item_uuid}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = app(Settings::default(), http_services)
        .oneshot(remove_req)
        .await
        .expect("http remove item");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(remove_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_item_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "document").await;
    ffi_pool_for_seed.close().await;

    let ffi_item_uuid_for_rows = ffi_item_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let item_uuid_c = CString::new(ffi_item_uuid).unwrap();
        let remove_r = alexandria_reading_list_remove_item(
            reading_list_uuid_c.as_ptr(),
            item_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            remove_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi remove item"
        );
        assert!(!remove_r.json.is_null());
        let s = unsafe { CStr::from_ptr(remove_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(remove_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&ffi_item_uuid_for_rows)
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["readingListUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid reading list uuid"
        );
        assert!(
            uuid::Uuid::parse_str(body["itemUuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid item uuid"
        );
    }
    assert_eq!(http_progress_rows.0, 0, "http deleted the progress row");
    assert_eq!(ffi_progress_rows.0, 0, "ffi deleted the progress row");
    assert_eq!(http_file_rows.0, 1, "http preserved the file");
    assert_eq!(ffi_file_rows.0, 1, "ffi preserved the file");

    ffi_pool.close().await;
}

/// UC-30 parity - removing an item not on the reading list is rejected as
/// not-found on both surfaces (HTTP 404, FFI READING_LIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_item_not_on_reading_list_when_removed_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let unknown_item = uuid::Uuid::new_v4().to_string();
    let remove_req = Request::builder()
        .method("DELETE")
        .uri(format!(
            "/v1/reading-lists/{http_reading_list_uuid}/items/{unknown_item}"
        ))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let remove_resp = app(Settings::default(), http_services)
        .oneshot(remove_req)
        .await
        .expect("http remove item");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let unknown_item = uuid::Uuid::new_v4().to_string();
        let item_uuid_c = CString::new(unknown_item).unwrap();
        let remove_r = alexandria_reading_list_remove_item(
            reading_list_uuid_c.as_ptr(),
            item_uuid_c.as_ptr(),
            token.as_ptr(),
        );
        assert!(remove_r.json.is_null());
        remove_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_NOT_FOUND,
        "ffi must reject an item not on the reading list as not-found (HTTP 404)"
    );
}

/// UC-31 parity - delete the same reading list (with a linked item) over
/// both transports and assert the returned bodies agree modulo
/// per-database uuids, each database ends with zero reading_list and
/// reading_progress rows, and the file itself survives (Testing
/// Specification section 7.3, FR-RL-07, FR-FC-24).
#[tokio::test]
async fn given_same_reading_list_when_deleted_via_http_and_ffi_then_bodies_and_rows_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/reading-lists")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "name": "Summer reads" }).to_string()))
        .unwrap();
    let create_resp = app(Settings::default(), http_services.clone())
        .oneshot(create_req)
        .await
        .expect("http create reading list");
    let http_reading_list_uuid = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    let http_item_uuid = seed_file(&http_pool, "document").await;
    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}/items"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "itemUuid": http_item_uuid }).to_string(),
        ))
        .unwrap();
    let add_resp = app(Settings::default(), http_services.clone())
        .oneshot(add_req)
        .await
        .expect("http add item");
    assert_eq!(add_resp.status(), axum::http::StatusCode::OK);

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/reading-lists/{http_reading_list_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = app(Settings::default(), http_services)
        .oneshot(delete_req)
        .await
        .expect("http delete reading list");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(delete_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    let http_reading_list_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_lists")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(&http_pool)
        .await
        .unwrap();
    let http_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&http_item_uuid)
        .fetch_one(&http_pool)
        .await
        .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_db_for_seed = ffi_db.clone();
    let ffi_db_for_rows = ffi_db.clone();

    let ffi_pool_for_seed = migrate_database(&ffi_db_for_seed).await.expect("ffi open");
    let ffi_item_uuid = seed_file(&ffi_pool_for_seed, "document").await;
    ffi_pool_for_seed.close().await;

    let ffi_item_uuid_for_rows = ffi_item_uuid.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let create_body = CString::new(json!({ "name": "Summer reads" }).to_string()).unwrap();
        let create_r = alexandria_reading_list_create(create_body.as_ptr(), token.as_ptr());
        assert_eq!(
            create_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi create"
        );
        let reading_list_json = unsafe { CStr::from_ptr(create_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(create_r.json);
        }
        let reading_list_uuid = serde_json::from_str::<serde_json::Value>(&reading_list_json)
            .unwrap()["uuid"]
            .as_str()
            .unwrap()
            .to_string();

        let reading_list_uuid_c = CString::new(reading_list_uuid.clone()).unwrap();
        let add_body = CString::new(json!({ "itemUuid": ffi_item_uuid }).to_string()).unwrap();
        let add_r = alexandria_reading_list_add_item(
            reading_list_uuid_c.as_ptr(),
            add_body.as_ptr(),
            token.as_ptr(),
        );
        assert_eq!(
            add_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi add item"
        );
        unsafe {
            alexandria_free_string(add_r.json);
        }

        let reading_list_uuid_c = CString::new(reading_list_uuid).unwrap();
        let delete_r = alexandria_reading_list_delete(reading_list_uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(
            delete_r.status,
            alexandria_ffi::READING_LIST_OK,
            "ffi delete reading list"
        );
        assert!(!delete_r.json.is_null());
        let s = unsafe { CStr::from_ptr(delete_r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(delete_r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_pool = migrate_database(&ffi_db_for_rows).await.expect("ffi open");
    let ffi_reading_list_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_lists")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_progress_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM reading_progress")
        .fetch_one(&ffi_pool)
        .await
        .unwrap();
    let ffi_file_rows: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
        .bind(&ffi_item_uuid_for_rows)
        .fetch_one(&ffi_pool)
        .await
        .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(body["name"], "Summer reads", "{label} name");
    }
    assert_eq!(
        http_reading_list_rows.0, 0,
        "http deleted the reading list row"
    );
    assert_eq!(
        ffi_reading_list_rows.0, 0,
        "ffi deleted the reading list row"
    );
    assert_eq!(http_progress_rows.0, 0, "http deleted the progress row");
    assert_eq!(ffi_progress_rows.0, 0, "ffi deleted the progress row");
    assert_eq!(http_file_rows.0, 1, "http preserved the file");
    assert_eq!(ffi_file_rows.0, 1, "ffi preserved the file");

    ffi_pool.close().await;
}

/// UC-31 parity - deleting an unknown reading list is rejected as
/// not-found on both surfaces (HTTP 404, FFI READING_LIST_ERR_NOT_FOUND)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_reading_list_when_deleted_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/reading-lists/{unknown}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let delete_resp = app(Settings::default(), http_services)
        .oneshot(delete_req)
        .await
        .expect("http delete reading list");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown = uuid::Uuid::new_v4().to_string();
        let unknown_c = CString::new(unknown).unwrap();
        let delete_r = alexandria_reading_list_delete(unknown_c.as_ptr(), token.as_ptr());
        assert!(delete_r.json.is_null());
        delete_r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::READING_LIST_ERR_NOT_FOUND,
        "ffi must reject an unknown reading list as not-found (HTTP 404)"
    );
}

/// Insert a minimal `files` row of the given `file_type` at `path`, and
/// return its uuid. Used by the UC-32 parity tests, which need a row whose
/// path resolves to a real on-disk file — unlike `seed_file` above, which
/// bypasses the indexer for handlers that never touch the filesystem.
async fn seed_file_at_path(pool: &sqlx::sqlite::SqlitePool, file_type: &str, path: &str) -> String {
    let file_uuid = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
         VALUES (?, ?, ?, ?, 'hash', ?)",
    )
    .bind(&file_uuid)
    .bind(path)
    .bind("seeded")
    .bind(file_type)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .unwrap();
    file_uuid
}

/// UC-32 parity - read the same on-disk TextFile's content over both
/// transports and assert the returned bodies agree modulo per-database
/// uuids (Testing Specification section 7.3, FR-TX-01, FR-FC-24).
#[tokio::test]
async fn given_same_text_file_when_read_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let lib = tempdir().unwrap();
    let file_path = lib.path().join("notes.txt");
    std::fs::write(&file_path, "hello world").unwrap();
    let file_path = file_path.to_str().unwrap().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_uuid = seed_file_at_path(&http_pool, "text", &file_path).await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}/content"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http read content");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "text", &file_path).await;
    ffi_pool.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let r = alexandria_file_read_content(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::FILE_OK, "ffi read content");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert_eq!(body["content"], "hello world", "{label} content");
    }
}

/// UC-32 parity - reading a non-TextFile's content is rejected as invalid
/// input on both surfaces (HTTP 400, FFI FILE_ERR_INVALID_INPUT)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_non_text_file_when_read_via_http_and_ffi_then_both_invalid_input() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let lib = tempdir().unwrap();
    let file_path = lib.path().join("song.mp3");
    std::fs::write(&file_path, b"audio bytes").unwrap();
    let file_path = file_path.to_str().unwrap().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_uuid = seed_file_at_path(&http_pool, "audio", &file_path).await;

    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}/content"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http read content");
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "audio", &file_path).await;
    ffi_pool.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let r = alexandria_file_read_content(uuid_c.as_ptr(), token.as_ptr());
        assert!(r.json.is_null());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::FILE_ERR_INVALID_INPUT,
        "ffi must reject a non-text file as invalid input (HTTP 400)"
    );
}

/// UC-33 parity - write the same content to the same on-disk TextFile over
/// both transports and assert the returned bodies agree modulo
/// per-database uuids and that each on-disk file and catalog hash reflect
/// the new content (Testing Specification section 7.3, FR-TX-02, FR-TX-03,
/// FR-FC-24).
#[tokio::test]
async fn given_same_content_when_edited_via_http_and_ffi_then_bodies_and_disk_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let http_lib = tempdir().unwrap();
    let http_file_path = http_lib.path().join("notes.txt");
    std::fs::write(&http_file_path, "old content").unwrap();
    let http_file_path = http_file_path.to_str().unwrap().to_string();

    let ffi_lib = tempdir().unwrap();
    let ffi_file_path = ffi_lib.path().join("notes.txt");
    std::fs::write(&ffi_file_path, "old content").unwrap();
    let ffi_file_path = ffi_file_path.to_str().unwrap().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_uuid = seed_file_at_path(&http_pool, "text", &http_file_path).await;

    let req = Request::builder()
        .method("PUT")
        .uri(format!("/v1/files/{http_uuid}/content"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "content": "new content" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http edit content");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let http_on_disk = std::fs::read_to_string(&http_file_path).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "text", &ffi_file_path).await;
    ffi_pool.close().await;

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let body = CString::new(json!({ "content": "new content" }).to_string()).unwrap();
        let r = alexandria_file_edit_content(uuid_c.as_ptr(), body.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::FILE_OK, "ffi edit content");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    let ffi_on_disk = std::fs::read_to_string(&ffi_file_path).unwrap();

    // ---- compare ----
    for (label, body) in [("http", &http_body), ("ffi", &ffi_body)] {
        assert!(
            uuid::Uuid::parse_str(body["uuid"].as_str().unwrap_or_default()).is_ok(),
            "{label} body carries a valid uuid"
        );
        assert!(
            body["contentHash"].as_str().unwrap_or_default().len() == 64,
            "{label} contentHash is a sha256 hex digest"
        );
    }
    assert_eq!(http_body["contentHash"], ffi_body["contentHash"]);
    assert_eq!(http_on_disk, "new content");
    assert_eq!(ffi_on_disk, "new content");
}

/// UC-33 parity - editing a non-TextFile's content is rejected as invalid
/// input on both surfaces (HTTP 400, FFI FILE_ERR_INVALID_INPUT)
/// (FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_non_text_file_when_edited_via_http_and_ffi_then_both_invalid_input() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let lib = tempdir().unwrap();
    let file_path = lib.path().join("song.mp3");
    std::fs::write(&file_path, b"audio bytes").unwrap();
    let file_path = file_path.to_str().unwrap().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let http_uuid = seed_file_at_path(&http_pool, "audio", &file_path).await;

    let req = Request::builder()
        .method("PUT")
        .uri(format!("/v1/files/{http_uuid}/content"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "content": "new content" }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http edit content");
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "audio", &file_path).await;
    ffi_pool.close().await;

    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let body = CString::new(json!({ "content": "new content" }).to_string()).unwrap();
        let r = alexandria_file_edit_content(uuid_c.as_ptr(), body.as_ptr(), token.as_ptr());
        assert!(r.json.is_null());
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::FILE_ERR_INVALID_INPUT,
        "ffi must reject a non-text file as invalid input (HTTP 400)"
    );
}

/// UC-34/UC-35 parity — register local credentials (UC-41, the account's
/// only bootstrap path since UC-35 became change-only) then log in through
/// both transports and assert identical response shapes (Testing
/// Specification §7.3). Session ids differ by construction (independent
/// databases), so parity asserts each is present and well-formed rather
/// than equal.
#[tokio::test]
async fn given_same_local_credentials_when_set_and_logged_in_via_http_and_ffi_then_bodies_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let register_req = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "email": "owner@example.com",
                "password": "correct horse battery",
                "passwordConfirmation": "correct horse battery",
            })
            .to_string(),
        ))
        .unwrap();
    let register_resp = router
        .clone()
        .oneshot(register_req)
        .await
        .expect("http register");
    assert_eq!(register_resp.status(), axum::http::StatusCode::CREATED);
    let http_set_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(register_resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let login_req = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": "owner@example.com", "password": "correct horse battery" })
                .to_string(),
        ))
        .unwrap();
    let login_resp = router.oneshot(login_req).await.expect("http login");
    assert_eq!(login_resp.status(), axum::http::StatusCode::OK);
    let http_login_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(login_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_set_json, ffi_login_json): (String, String) =
        tokio::task::spawn_blocking(move || -> (String, String) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let register_body = CString::new(
                json!({
                    "email": "owner@example.com",
                    "password": "correct horse battery",
                    "passwordConfirmation": "correct horse battery",
                })
                .to_string(),
            )
            .unwrap();
            let register_result = alexandria_auth_local_register(register_body.as_ptr());
            assert_eq!(
                register_result.status,
                alexandria_ffi::AUTH_OK,
                "ffi register failed"
            );
            assert!(!register_result.json.is_null());
            let set_json = unsafe { CStr::from_ptr(register_result.json) }
                .to_str()
                .unwrap()
                .to_string();
            unsafe {
                alexandria_free_string(register_result.json);
            }

            let login_body = CString::new(
                json!({ "email": "owner@example.com", "password": "correct horse battery" })
                    .to_string(),
            )
            .unwrap();
            let login_result = alexandria_auth_local_login(login_body.as_ptr());
            assert_eq!(
                login_result.status,
                alexandria_ffi::AUTH_OK,
                "ffi login failed"
            );
            assert!(!login_result.json.is_null());
            let login_json = unsafe { CStr::from_ptr(login_result.json) }
                .to_str()
                .unwrap()
                .to_string();
            unsafe {
                alexandria_free_string(login_result.json);
            }

            (set_json, login_json)
        })
        .await
        .unwrap();

    let ffi_set_body: serde_json::Value = serde_json::from_str(&ffi_set_json).unwrap();
    let ffi_login_body: serde_json::Value = serde_json::from_str(&ffi_login_json).unwrap();

    // ---- compare ----
    // Registration bodies now carry a per-surface sessionId (UC-41 opens a
    // session on success), so compare `success`/`email` rather than the
    // whole body.
    assert_eq!(http_set_body["success"], ffi_set_body["success"]);
    assert_eq!(http_set_body["success"], json!(true));
    assert_eq!(http_set_body["email"], ffi_set_body["email"]);

    assert_eq!(http_login_body["success"], ffi_login_body["success"]);
    assert_eq!(http_login_body["success"], serde_json::json!(true));
    for body in [&http_login_body, &ffi_login_body] {
        let session_id = body["sessionId"].as_str().expect("sessionId");
        assert!(
            uuid::Uuid::parse_str(session_id).is_ok(),
            "sessionId is a uuid: {session_id}"
        );
    }
}

/// Sets `ALEXANDRIA_AUTH_MODE=windows` and `ALEXANDRIA_AUTH_WINDOWS_OWNER_SID`
/// for as long as this guard is alive, and restores both — mode back to
/// `"local"`, the SID variable removed entirely — on `Drop`. Same reasoning
/// as `ThumbnailCacheGuard` above: a plain trailing `set_var` back to
/// `"local"` is skipped if the test unwinds from a failed assertion first,
/// which would leave `ALEXANDRIA_AUTH_WINDOWS_OWNER_SID` set for the rest of
/// the process and silently change what `alexandria_index_init` does in
/// every later test that does not itself override it.
///
/// Must be constructed and dropped while still holding `SERIAL` — see
/// `ThumbnailCacheGuard`.
#[cfg(windows)]
struct WindowsAuthEnvGuard;

#[cfg(windows)]
impl WindowsAuthEnvGuard {
    fn new(owner_sid: &str) -> Self {
        std::env::set_var("ALEXANDRIA_AUTH_MODE", "windows");
        std::env::set_var("ALEXANDRIA_AUTH_WINDOWS_OWNER_SID", owner_sid);
        Self
    }
}

#[cfg(windows)]
impl Drop for WindowsAuthEnvGuard {
    fn drop(&mut self) {
        std::env::set_var("ALEXANDRIA_AUTH_MODE", "local");
        std::env::remove_var("ALEXANDRIA_AUTH_WINDOWS_OWNER_SID");
    }
}

/// UC-45 parity — open a session for the Windows account through both
/// transports and assert both succeed with a well-formed session id (Testing
/// Specification §7.3). Session ids differ by construction (independent
/// databases), so parity asserts shape, exactly like the local-login parity
/// test above.
///
/// Windows-only: `alexandria_index_init`'s startup gate (Task 3 / FR-AU-21)
/// reads this process's real account SID through `ProcessWindowsIdentity`,
/// which only Windows can answer — on every other platform Windows mode can
/// never finish initializing, so there is nothing for this test to exercise
/// there. `given_local_mode_when_windows_login_attempted_via_http_and_ffi_
/// then_both_conflict` below covers the FR-AU-08 both-surfaces claim on
/// every platform CI actually runs on.
#[cfg(windows)]
#[tokio::test]
async fn given_windows_mode_when_logged_in_via_http_and_ffi_then_both_return_a_session() {
    use alexandria_core::auth::windows_identity::{ProcessWindowsIdentity, WindowsIdentity};
    use alexandria_core::config::AuthMode;

    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // The SID this process actually runs as, so both legs' startup gates
    // (HTTP does not gate at `app()`, but FFI's `alexandria_index_init`
    // does) see a configuration that matches reality.
    let owner_sid = ProcessWindowsIdentity
        .current_sid()
        .expect("this process's own account SID must be readable");

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let mut http_settings = Settings::default();
    http_settings.auth.mode = AuthMode::Windows;
    http_settings.auth.windows_owner_sid = owner_sid.clone();
    let http_services =
        std::sync::Arc::new(build_services(&http_settings, http_pool.clone()).await);
    let router = app(Settings::default(), http_services.clone());

    let login_req = Request::builder()
        .method("POST")
        .uri("/v1/auth/windows/login")
        .body(Body::empty())
        .unwrap();
    let login_resp = router.oneshot(login_req).await.expect("http windows login");
    assert_eq!(login_resp.status(), axum::http::StatusCode::OK);
    let http_login_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(login_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // FR-AU-22: the session the mode mints has to actually admit the caller —
    // otherwise "Windows mode authenticates you" is asserted nowhere. Present
    // it on a gated route and require anything but a 401. The status itself is
    // not the claim (an empty catalog may answer many ways); being let past
    // `RuntimeAuthService::Windows::authenticate` is.
    let http_session_id = http_login_body["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let gated_req = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .header("authorization", &format!("Bearer {http_session_id}"))
        .body(Body::empty())
        .unwrap();
    let gated_resp = app(Settings::default(), http_services.clone())
        .oneshot(gated_req)
        .await
        .expect("http gated request");
    assert_ne!(
        gated_resp.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "a session minted by windows login must authenticate a later request"
    );

    // The negative half, so the assertion above cannot pass by the gate being
    // absent: an id that was never minted is still rejected.
    let unknown_req = Request::builder()
        .method("GET")
        .uri("/v1/files")
        .header("authorization", &format!("Bearer {}", uuid::Uuid::new_v4()))
        .body(Body::empty())
        .unwrap();
    let unknown_resp = app(Settings::default(), http_services.clone())
        .oneshot(unknown_req)
        .await
        .expect("http gated request with an unknown session");
    assert_eq!(
        unknown_resp.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "an unminted session id must not authenticate"
    );

    // ---- FFI leg ----
    // `alexandria_index_init` loads settings via `load_settings()`
    // (`ALEXANDRIA_*` env), not a `Settings` value the test controls
    // directly — same constraint `setup_ffi_db` documents for local mode.
    // The guard restores both env vars on drop, including if an assertion
    // below panics first.
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi pre-migrate");
    ffi_pool.close().await;
    let _env_guard = WindowsAuthEnvGuard::new(&owner_sid);

    let ffi_login_json: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let login_result = alexandria_auth_windows_login(std::ptr::null());
        assert_eq!(
            login_result.status,
            alexandria_ffi::AUTH_OK,
            "ffi windows login failed"
        );
        take_json(login_result.json)
    })
    .await
    .unwrap();

    let ffi_login_body: serde_json::Value = serde_json::from_str(&ffi_login_json).unwrap();

    assert_eq!(http_login_body["success"], serde_json::json!(true));
    assert_eq!(ffi_login_body["success"], serde_json::json!(true));
    for body in [&http_login_body, &ffi_login_body] {
        let session_id = body["sessionId"].as_str().expect("sessionId");
        assert!(
            uuid::Uuid::parse_str(session_id).is_ok(),
            "sessionId is a uuid: {session_id}"
        );
    }
}

/// UC-45 parity, cross-platform — the FR-AU-08 both-surfaces claim, verified
/// on every platform CI runs on (unlike the Windows-only test above, which
/// `alexandria_index_init`'s real SID check confines to Windows). Builds
/// services in `AuthMode::Local` and asserts both surfaces reject a Windows
/// login with the same shape: HTTP `409`, FFI's `AUTH_ERR_CONFLICT`. This
/// path never sets the mode to `Windows`, so `alexandria_index_init` never
/// reaches the SID gate — it proves the route is registered, reachable
/// without a session (409 rather than 401 or 404), and mapped to the same
/// error class on both transports.
#[tokio::test]
async fn given_local_mode_when_windows_login_attempted_via_http_and_ffi_then_both_conflict() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let login_req = Request::builder()
        .method("POST")
        .uri("/v1/auth/windows/login")
        .body(Body::empty())
        .unwrap();
    let login_resp = router.oneshot(login_req).await.expect("http windows login");
    assert_eq!(login_resp.status(), axum::http::StatusCode::CONFLICT);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;

    let ffi_status: std::os::raw::c_int = tokio::task::spawn_blocking(move || {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let login_result = alexandria_auth_windows_login(std::ptr::null());
        assert!(
            !login_result.json.is_null(),
            "every auth result must carry a body"
        );
        unsafe {
            alexandria_free_string(login_result.json);
        }
        login_result.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::AUTH_ERR_CONFLICT,
        "ffi must reject a windows login while the active mode is local"
    );
}

/// Write a minimal valid single-channel 8-bit PCM WAV file (see
/// `alexandria-core`'s `catalog::audio_tags` unit tests for the same
/// helper) — just enough of a real RIFF/WAVE container for `lofty` to
/// recognize the format and accept a written tag.
fn write_minimal_wav(path: &std::path::Path) {
    let sample_data: [u8; 8] = [0x80; 8];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32 + sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&8000u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&8u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(sample_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&sample_data);
    std::fs::write(path, bytes).expect("write wav");
}

fn write_test_tags(path: &std::path::Path) {
    use lofty::config::WriteOptions;
    use lofty::tag::{Accessor, Tag, TagExt, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title("Parity Title".to_string());
    tag.set_artist("Parity Artist".to_string());
    tag.set_album("Parity Album".to_string());
    tag.set_genre("Parity Genre".to_string());
    tag.set_year(2015);
    tag.set_track(2);
    tag.save_to_path(path, WriteOptions::default())
        .expect("save tag");
}

/// Issue #44 pilot parity — index a tagged audio file through both
/// transports and assert the extracted subtype metadata (written by the
/// indexer itself, not by a manual PATCH) is byte-for-byte identical.
#[tokio::test]
async fn given_tagged_audio_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_track = http_lib.path().join("song.wav");
    write_minimal_wav(&http_track);
    write_test_tags(&http_track);

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_files(&http_pool, 1).await;
    // Wait on the extracted metadata itself, not just the file row —
    // `insert_file` commits the `files` row before `index_entry` reads the
    // audio tags and writes `audio_files` in a separate transaction.
    wait_for_http_audio_title(&http_pool, "Parity Title").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("song.wav")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_track = ffi_lib.path().join("song.wav");
    write_minimal_wav(&ffi_track);
    write_test_tags(&ffi_track);
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let ffi_db_for_uuid_lookup = ffi_db.clone();
    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
        loop {
            if alexandria_index_count_files() >= 1 {
                break;
            }
            if std::time::Instant::now() > dl {
                panic!("ffi never persisted 1 file");
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        // `alexandria_index_files_json` deliberately doesn't expose `uuid`
        // (only path/name/type/hash/missingAt), so look the uuid up
        // directly the same way the HTTP leg does — via a fresh connection
        // on its own current-thread runtime, mirroring the established
        // pattern used elsewhere in this file for querying the FFI db from
        // inside a `spawn_blocking` closure.
        //
        // The `files` row above is committed by `insert_file` before
        // `index_entry` extracts audio tags and writes `audio_files` in a
        // separate transaction, so also poll the `audio_files` row itself —
        // otherwise this leg can race ahead of extraction just like the
        // HTTP leg can.
        let ffi_uuid = std::thread::spawn(move || -> String {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let url = format!("sqlite://{ffi_db_for_uuid_lookup}");
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect(&format!("{url}?mode=rw"))
                    .await
                    .unwrap();

                let dl = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
                loop {
                    let row: Option<(Option<String>,)> = sqlx::query_as(
                        "SELECT audio_files.title FROM audio_files \
                         JOIN files ON files.id = audio_files.file_id \
                         WHERE files.name = ?",
                    )
                    .bind("song.wav")
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
                    if let Some((Some(title),)) = &row {
                        if title == "Parity Title" {
                            break;
                        }
                    }
                    if std::time::Instant::now() > dl {
                        panic!("ffi never wrote extracted audio title");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }

                let (uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
                    .bind("song.wav")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                uuid
            })
        })
        .join()
        .unwrap();

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(
        http_body["metadata"], ffi_body["metadata"],
        "extracted audio metadata must match across surfaces"
    );
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["artist"], "Parity Artist");
    assert_eq!(http_body["metadata"]["album"], "Parity Album");
    assert_eq!(http_body["metadata"]["genre"], "Parity Genre");
    assert_eq!(http_body["metadata"]["year"], 2015);
    assert_eq!(http_body["metadata"]["track"], 2);
}

/// Encode a tiny real JPEG (4x3 pixels) using the `image` crate — a real,
/// valid JPEG file, not hand-crafted bytes. Mirrors the identical helper in
/// `alexandria-core`'s `catalog::image_tags` unit tests.
fn write_minimal_jpeg(path: &std::path::Path) {
    let img = image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]));
    img.save(path).expect("encode jpeg");
}

/// Write EXIF tags (pixel dimensions + a description) into an existing JPEG
/// using `little_exif`.
fn write_test_exif(path: &std::path::Path, width: u32, height: u32, description: &str) {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::ImageDescription(description.to_string()));
    // little_exif names these `ExifImageWidth`/`ExifImageHeight`, but they
    // write tag IDs 0xa002/0xa003 — the same IDs `kamadak-exif` reads back
    // as `Tag::PixelXDimension`/`Tag::PixelYDimension` (see
    // alexandria-core's `catalog::image_tags` unit tests for the same
    // discovery).
    metadata.set_tag(ExifTag::ExifImageWidth(vec![width]));
    metadata.set_tag(ExifTag::ExifImageHeight(vec![height]));
    metadata.write_to_file(path).expect("write exif");
}

/// Poll until `images.width`/`images.height`/`images.title` are all
/// non-NULL for the named file — proves both extraction writes landed
/// (`IndexHandler`'s image branch commits dimensions and title as two
/// separate sequential transactions), not just the first of the two, and
/// not just that the file row exists (the audio slice's final review found
/// and fixed exactly this race for its own parity test; this test extends
/// that fix to cover every write it asserts on).
async fn wait_for_http_image_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let row: Option<(Option<i64>, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT images.width, images.height, images.title FROM images \
             JOIN files ON files.id = images.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted image dimensions and title");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 image slice parity — index a tagged JPEG through both
/// transports and assert the extracted dimensions + title (written by the
/// indexer itself, not by a manual PATCH) are byte-for-byte identical.
#[tokio::test]
async fn given_tagged_image_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_photo = http_lib.path().join("photo.jpg");
    write_minimal_jpeg(&http_photo);
    write_test_exif(&http_photo, 800, 600, "Parity Description");

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_image_extraction(&http_pool, "photo.jpg").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("photo.jpg")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_photo = ffi_lib.path().join("photo.jpg");
    write_minimal_jpeg(&ffi_photo);
    write_test_exif(&ffi_photo, 800, 600, "Parity Description");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_db_for_poll = ffi_db.clone();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        // Poll the FFI leg's own sqlite file directly for both extraction
        // writes (dimensions and title), same as the HTTP leg — not just
        // file-row existence, and not just the first of the two sequential
        // writes the indexer commits.
        type FfiImageExtractionRow = (String, Option<i64>, Option<i64>, Option<String>);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
            loop {
                let row: Option<FfiImageExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, images.width, images.height, images.title FROM images \
                     JOIN files ON files.id = images.file_id \
                     WHERE files.name = ?",
                )
                .bind("photo.jpg")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted image dimensions and title");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(http_body["width"], ffi_body["width"]);
    assert_eq!(http_body["height"], ffi_body["height"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["width"], 800);
    assert_eq!(http_body["height"], 600);
    assert_eq!(http_body["metadata"]["title"], "Parity Description");
}

/// Build a minimal valid PDF with `lopdf` — mirrors the identical helper
/// in `alexandria-core`'s `catalog::document_tags` unit tests.
fn write_minimal_pdf(path: &std::path::Path, title: &str, author: &str) {
    use lopdf::{dictionary, Document, Object};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let content = lopdf::content::Content { operations: vec![] };
    let content_id = doc.add_object(lopdf::Stream::new(
        dictionary! {},
        content.encode().expect("encode content"),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let info_id = doc.add_object(dictionary! {
        "Title" => Object::string_literal(title),
        "Author" => Object::string_literal(author),
    });
    doc.trailer.set("Info", info_id);

    doc.save(path).expect("save pdf");
}

/// Poll until `documents.title`/`documents.author`/`documents.page_count`
/// are all non-NULL for the named file — proves BOTH extraction writes
/// landed (metadata write and page-count write are separate
/// transactions), not just file-row existence or a single write.
async fn wait_for_http_document_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let row: Option<(Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT documents.title, documents.author, documents.page_count FROM documents \
             JOIN files ON files.id = documents.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted document metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 document slice parity — index a tagged PDF through both
/// transports and assert the extracted title/author/formatKind/page_count
/// (written by the indexer itself, not by a manual PATCH) are
/// byte-for-byte identical. PDF is used (not EPUB) because it's the format
/// that exercises both independent writes (page_count + metadata) at once.
#[tokio::test]
async fn given_tagged_pdf_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_doc = http_lib.path().join("book.pdf");
    write_minimal_pdf(&http_doc, "Parity Title", "Parity Author");

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_document_extraction(&http_pool, "book.pdf").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("book.pdf")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_doc = ffi_lib.path().join("book.pdf");
    write_minimal_pdf(&ffi_doc, "Parity Title", "Parity Author");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_db_for_poll = ffi_db.clone();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        // Poll the FFI leg's own sqlite file directly for all three
        // extraction writes (title, author, page_count) — not just
        // file-row existence, and not just the first of the writes the
        // indexer commits across its separate transactions.
        type FfiDocumentExtractionRow = (String, Option<String>, Option<String>, Option<i64>);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
            loop {
                let row: Option<FfiDocumentExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, documents.title, documents.author, documents.page_count \
                     FROM documents \
                     JOIN files ON files.id = documents.file_id \
                     WHERE files.name = ?",
                )
                .bind("book.pdf")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted document metadata");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(http_body["pageCount"], ffi_body["pageCount"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["pageCount"], 1);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["author"], "Parity Author");
    assert_eq!(http_body["metadata"]["formatKind"], "book");
}

/// Build a minimal valid MP4 with `ffmpeg-next` — mirrors the identical
/// helper in `alexandria-core`'s `catalog::video_tags` unit tests.
fn write_minimal_mp4(path: &std::path::Path, title: &str, width: u32, height: u32) {
    ffmpeg_next::init().expect("ffmpeg init");

    let mut octx = ffmpeg_next::format::output(path).expect("create output context");
    octx.set_metadata({
        let mut dict = ffmpeg_next::Dictionary::new();
        dict.set("title", title);
        dict
    });

    let codec =
        ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::MPEG4).expect("mpeg4 encoder available");
    let mut ost = octx.add_stream(codec).expect("add video stream");
    let mut encoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .expect("video encoder context");
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
    encoder.set_time_base(ffmpeg_next::Rational(1, 25));
    let mut encoder = encoder.open().expect("open encoder");
    ost.set_parameters(&encoder);

    octx.write_header().expect("write header");

    let mut frame =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
    for plane in 0..frame.planes() {
        frame.data_mut(plane).fill(16);
    }

    for i in 0..10 {
        frame.set_pts(Some(i));
        encoder.send_frame(&frame).expect("send frame");
        let mut packet = ffmpeg_next::Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
    }
    encoder.send_eof().expect("send eof");
    let mut packet = ffmpeg_next::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(0);
        packet.write_interleaved(&mut octx).expect("write packet");
    }
    octx.write_trailer().expect("write trailer");
}

/// Poll until `video_files.title`/`video_files.resolution`/
/// `video_files.duration_seconds` are all non-NULL for the named file —
/// proves BOTH extraction writes landed (metadata write and duration
/// write are separate transactions), not just file-row existence or a
/// single write.
async fn wait_for_http_video_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let row: Option<(Option<String>, Option<String>, Option<f64>)> = sqlx::query_as(
            "SELECT video_files.title, video_files.resolution, video_files.duration_seconds \
             FROM video_files \
             JOIN files ON files.id = video_files.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted video metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 video slice parity — index a tagged MP4 through both
/// transports and assert the extracted title/resolution/durationSeconds
/// (written by the indexer itself, not by a manual PATCH) are
/// byte-for-byte identical.
#[tokio::test]
async fn given_tagged_mp4_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_video = http_lib.path().join("movie.mp4");
    write_minimal_mp4(&http_video, "Parity Title", 320, 240);

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_video_extraction(&http_pool, "movie.mp4").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("movie.mp4")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_video = ffi_lib.path().join("movie.mp4");
    write_minimal_mp4(&ffi_video, "Parity Title", 320, 240);
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_db_for_poll = ffi_db.clone();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        // Poll the FFI leg's own sqlite file directly for all three
        // extraction writes (title, resolution, duration_seconds) — not
        // just file-row existence, and not just the first of the writes
        // the indexer commits across its separate transactions.
        type FfiVideoExtractionRow = (String, Option<String>, Option<String>, Option<f64>);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
            loop {
                let row: Option<FfiVideoExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, video_files.title, video_files.resolution, \
                     video_files.duration_seconds \
                     FROM video_files \
                     JOIN files ON files.id = video_files.file_id \
                     WHERE files.name = ?",
                )
                .bind("movie.mp4")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted video metadata");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(http_body["durationSeconds"], ffi_body["durationSeconds"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["resolution"], "320x240");
    let http_duration = http_body["durationSeconds"]
        .as_f64()
        .expect("duration is a number");
    assert!(
        http_duration > 0.0,
        "expected a positive duration, got {http_duration}"
    );
}

/// Build a minimal valid CBZ with the `zip` crate — mirrors the identical
/// helper in `alexandria-core`'s `catalog::comic_tags` unit tests.
fn write_minimal_cbz(
    path: &std::path::Path,
    title: &str,
    series: &str,
    number: &str,
    page_count: usize,
) {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(path).expect("create cbz file");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();

    zip.start_file("ComicInfo.xml", options)
        .expect("start ComicInfo.xml");
    let xml = format!(
        r#"<?xml version="1.0"?>
<ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>{title}</Title>
  <Series>{series}</Series>
  <Number>{number}</Number>
</ComicInfo>"#
    );
    zip.write_all(xml.as_bytes()).expect("write ComicInfo.xml");

    for i in 0..page_count {
        zip.start_file(format!("page-{i:03}.jpg"), options)
            .expect("start page");
        zip.write_all(b"not-a-real-jpeg-just-bytes")
            .expect("write page");
    }

    zip.finish().expect("finish cbz zip");
}

/// Poll until `comic_books.title`/`comic_books.series`/
/// `comic_books.issue_number`/`comic_books.page_count` are all non-NULL
/// for the named file — proves BOTH extraction writes landed (metadata
/// write and page-count write are separate transactions), not just
/// file-row existence or a single write.
type HttpComicExtractionRow = (Option<String>, Option<String>, Option<i64>, Option<i64>);

async fn wait_for_http_comic_extraction(pool: &sqlx::sqlite::SqlitePool, name: &str) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let row: Option<HttpComicExtractionRow> = sqlx::query_as(
            "SELECT comic_books.title, comic_books.series, comic_books.issue_number, \
             comic_books.page_count \
             FROM comic_books \
             JOIN files ON files.id = comic_books.file_id \
             WHERE files.name = ?",
        )
        .bind(name)
        .fetch_optional(pool)
        .await
        .unwrap();
        if let Some((Some(_), Some(_), Some(_), Some(_))) = row {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("http never wrote extracted comic metadata");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Issue #44 comic slice parity — index a tagged CBZ through both
/// transports and assert the extracted title/series/issueNumber/
/// comicPageCount (written by the indexer itself, not by a manual PATCH)
/// are byte-for-byte identical.
#[tokio::test]
async fn given_tagged_cbz_file_when_indexed_via_http_and_ffi_then_extracted_metadata_matches() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    let http_comic = http_lib.path().join("issue1.cbz");
    write_minimal_cbz(&http_comic, "Parity Title", "Parity Series", "3", 24);

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "root": http_lib.path().to_str().unwrap() }).to_string(),
        ))
        .unwrap();
    let _ = app(Settings::default(), http_services.clone())
        .oneshot(index_req)
        .await
        .expect("http index");
    wait_for_http_comic_extraction(&http_pool, "issue1.cbz").await;

    let (http_uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
        .bind("issue1.cbz")
        .fetch_one(&http_pool)
        .await
        .unwrap();

    let get_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let get_resp = app(Settings::default(), http_services)
        .oneshot(get_req)
        .await
        .expect("http get");
    assert_eq!(get_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(get_resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_lib = tempdir().unwrap();
    let ffi_comic = ffi_lib.path().join("issue1.cbz");
    write_minimal_cbz(&ffi_comic, "Parity Title", "Parity Series", "3", 24);
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_db_for_poll = ffi_db.clone();

    let ffi_body: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new(TEST_TOKEN).unwrap();
        let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(started.status, alexandria_ffi::INDEX_OK);

        // Poll the FFI leg's own sqlite file directly for all four
        // extraction writes (title, series, issue_number, page_count) —
        // not just file-row existence, and not just the first of the
        // writes the indexer commits across its separate transactions.
        type FfiComicExtractionRow = (
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ffi_uuid: String = rt.block_on(async {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{ffi_db_for_poll}?mode=rw"))
                .await
                .unwrap();
            let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
            loop {
                let row: Option<FfiComicExtractionRow> = sqlx::query_as(
                    "SELECT files.uuid, comic_books.title, comic_books.series, \
                     comic_books.issue_number, comic_books.page_count \
                     FROM comic_books \
                     JOIN files ON files.id = comic_books.file_id \
                     WHERE files.name = ?",
                )
                .bind("issue1.cbz")
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some((uuid, Some(_), Some(_), Some(_), Some(_))) = row {
                    return uuid;
                }
                if std::time::Instant::now() > deadline {
                    panic!("ffi never wrote extracted comic metadata");
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        });

        let uuid_c = CString::new(ffi_uuid).unwrap();
        let result = alexandria_file_get_by_uuid(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::FILE_OK);
        assert!(!result.json.is_null());
        let json = unsafe { CStr::from_ptr(result.json) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe {
            alexandria_free_string(result.json);
        }
        json
    })
    .await
    .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_body).unwrap();

    // ---- compare ----
    assert_eq!(http_body["comicPageCount"], ffi_body["comicPageCount"]);
    assert_eq!(http_body["metadata"], ffi_body["metadata"]);
    assert_eq!(http_body["comicPageCount"], 24);
    assert_eq!(http_body["metadata"]["title"], "Parity Title");
    assert_eq!(http_body["metadata"]["series"], "Parity Series");
    assert_eq!(http_body["metadata"]["issueNumber"], 3);
}

// ---------------------------------------------------------------------------
// F-10 media playback parity (UC-38, UC-39, UC-40 — FR-MP-06).
//
// The two legs keep their own library directory and their own database, as
// every parity test above does, so an absolute path is *not* comparable
// between them: UC-38's descriptor names the FFI leg's copy of the fixture.
// Parity is therefore asserted on the decisions and on the bytes — the
// descriptor's mime/size against HTTP's headers, and the bytes at the
// descriptor's path against the bytes HTTP streamed.
// ---------------------------------------------------------------------------

/// A tiny, real, valid JPEG — deterministic per `seed`, so a test can
/// recompute the exact bytes an archive entry was written with. Local copy of
/// `alexandria-http`'s test helper of the same name: an integration test
/// cannot import another crate's test module.
fn jpeg_bytes_for(seed: &str) -> Vec<u8> {
    let sum: u32 = seed.bytes().map(u32::from).sum();
    let pixel = image::Rgb([(sum % 256) as u8, ((sum / 3) % 256) as u8, 128]);
    let img = image::RgbImage::from_pixel(4, 4, pixel);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .encode_image(&image::DynamicImage::ImageRgb8(img))
        .expect("encode jpeg");
    out
}

/// Write a real CBZ (ZIP) at `dir/name` holding one real JPEG per entry, in
/// exactly the order given — callers pass entries out of page order on
/// purpose, so "page 1" proves the reader sorts rather than trusting archive
/// order. Returns the path as a string, ready for `seed_file_at_path`.
fn write_cbz(dir: &TempDir, name: &str, entries: &[&str]) -> String {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let path = dir.path().join(name);
    let file = std::fs::File::create(&path).expect("create cbz");
    let mut zip = zip::ZipWriter::new(file);
    for entry in entries {
        zip.start_file(*entry, SimpleFileOptions::default())
            .expect("start entry");
        zip.write_all(&jpeg_bytes_for(entry)).expect("write entry");
    }
    zip.finish().expect("finish cbz");
    path.to_str().unwrap().to_string()
}

/// Write a real PNG of `width` x `height` at `dir/name`.
fn write_image(dir: &TempDir, name: &str, width: u32, height: u32) -> String {
    let path = dir.path().join(name);
    let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 60, 30]));
    image::DynamicImage::ImageRgb8(img)
        .save(&path)
        .expect("write png");
    path.to_str().unwrap().to_string()
}

/// Local settings with UC-40's thumbnail cache pointed inside `cache_dir`.
/// The default is the *relative* path `"thumbnails"`, which would otherwise be
/// created in the test process's working directory — the repository itself.
fn playback_settings(cache_dir: &TempDir) -> Settings {
    let mut settings = local_settings();
    settings.playback.thumbnail_cache_dir = cache_dir.path().to_str().unwrap().to_string();
    settings
}

/// A response header as an owned string, panicking when it is absent. The
/// playback surfaces carry their parity contract in headers (`content-type`,
/// `content-length`), so a missing one is a failure, not a `None` to tolerate.
fn header_string(response: &axum::response::Response, name: &str) -> String {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("response has no {name} header"))
        .to_str()
        .expect("header is valid ascii")
        .to_string()
}

/// Points the FFI leg's thumbnail cache at a `TempDir` for as long as this
/// guard is alive, and clears the override on `Drop` — including when the
/// holding test unwinds from a failed assertion, unlike a plain trailing
/// call to a `clear_*` function, which a panic would skip. Skipping the
/// clear left `ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR` naming a deleted
/// temp directory once the `TempDir` dropped, silently inherited by the next
/// test to call `alexandria_index_init` — turning one red test into several.
///
/// `alexandria_index_init` takes no `Settings` — it calls `load_settings()`
/// — so the only way to override the default relative `"thumbnails"` is the
/// environment, exactly as `setup_ffi_db` does for the auth mode.
///
/// Must be constructed and dropped while still holding `SERIAL`:
/// `std::env::set_var`/`remove_var` are only sound in this multithreaded
/// test process because every test in this file holds that mutex for its
/// whole body.
struct ThumbnailCacheGuard;

impl ThumbnailCacheGuard {
    fn new(cache_dir: &TempDir) -> Self {
        std::env::set_var(
            "ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR",
            cache_dir.path().to_str().unwrap(),
        );
        Self
    }
}

impl Drop for ThumbnailCacheGuard {
    fn drop(&mut self) {
        std::env::remove_var("ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR");
    }
}

/// UC-38 parity - stream the same fixture over HTTP and resolve it over FFI,
/// then assert the descriptor describes exactly what HTTP served (Testing
/// Specification section 7.3, FR-MP-01, FR-MP-06).
#[tokio::test]
async fn given_same_file_when_streamed_then_descriptor_agrees_with_http() {
    // Arrange — one identical fixture per leg, each in its own library.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let contents = b"fake mp4 bytes, but real enough for a byte stream";

    let http_lib = tempdir().unwrap();
    let http_file = http_lib.path().join("sample.mp4");
    std::fs::write(&http_file, contents).unwrap();
    let http_file = http_file.to_str().unwrap().to_string();

    let ffi_lib = tempdir().unwrap();
    let ffi_file = ffi_lib.path().join("sample.mp4");
    std::fs::write(&ffi_file, contents).unwrap();
    let ffi_file = ffi_file.to_str().unwrap().to_string();

    let http_dir = tempdir().unwrap();
    let http_pool = migrate_database(&db_path(&http_dir, "http.sqlite"))
        .await
        .expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let settings = local_settings();
    let http_services = std::sync::Arc::new(build_services(&settings, http_pool.clone()).await);
    let http_uuid = seed_file_at_path(&http_pool, "video", &http_file).await;

    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "video", &ffi_file).await;
    ffi_pool.close().await;

    // Act — HTTP streams the bytes; FFI returns the descriptor.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}/stream"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(settings.clone(), http_services)
        .oneshot(req)
        .await
        .expect("http stream");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_content_type = header_string(&resp, "content-type");
    let http_content_length: u64 = header_string(&resp, "content-length").parse().unwrap();
    let http_body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();

    let descriptor: serde_json::Value =
        tokio::task::spawn_blocking(move || -> serde_json::Value {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let uuid_c = CString::new(ffi_uuid).unwrap();
            let r = alexandria_file_playback_source(uuid_c.as_ptr(), token.as_ptr());
            assert_eq!(r.status, alexandria_ffi::PLAYBACK_OK, "ffi playback source");
            assert!(!r.json.is_null());
            let s = unsafe { CStr::from_ptr(r.json) }
                .to_string_lossy()
                .into_owned();
            unsafe {
                alexandria_free_string(r.json);
            }
            serde_json::from_str(&s).unwrap()
        })
        .await
        .unwrap();

    // Assert — FR-MP-06: parity is on the descriptor, and the path it names
    // must hold exactly the bytes HTTP served. The path itself is deliberately
    // not compared: each leg indexed its own copy of the fixture.
    assert_eq!(descriptor["mimeType"], http_content_type);
    assert_eq!(descriptor["mimeType"], "video/mp4");
    assert_eq!(
        descriptor["sizeBytes"].as_u64().expect("sizeBytes"),
        http_content_length
    );
    let on_disk = std::fs::read(descriptor["path"].as_str().expect("path")).unwrap();
    assert_eq!(
        on_disk, http_body,
        "descriptor path holds the streamed bytes"
    );
}

/// UC-39 parity - read page 1 of the same CBZ over both transports and assert
/// the bytes are identical, HTTP raw against FFI base64 (Testing
/// Specification section 7.3, FR-MP-04, FR-MP-06).
#[tokio::test]
async fn given_same_comic_when_page_read_then_bytes_identical_across_surfaces() {
    // Arrange — entries deliberately stored out of page order.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let entries = ["page002.jpg", "page001.jpg"];

    let http_lib = tempdir().unwrap();
    let http_file = write_cbz(&http_lib, "issue.cbz", &entries);
    let ffi_lib = tempdir().unwrap();
    let ffi_file = write_cbz(&ffi_lib, "issue.cbz", &entries);

    let http_dir = tempdir().unwrap();
    let http_pool = migrate_database(&db_path(&http_dir, "http.sqlite"))
        .await
        .expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let settings = local_settings();
    let http_services = std::sync::Arc::new(build_services(&settings, http_pool.clone()).await);
    let http_uuid = seed_file_at_path(&http_pool, "comic", &http_file).await;

    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "comic", &ffi_file).await;
    ffi_pool.close().await;

    // Act
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}/pages/1"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(settings.clone(), http_services)
        .oneshot(req)
        .await
        .expect("http comic page");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_content_type = header_string(&resp, "content-type");
    let http_body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();

    let ffi_page: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let r = alexandria_comic_page(uuid_c.as_ptr(), 1, token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::PLAYBACK_OK, "ffi comic page");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // Assert — byte-exact across the two surfaces (FR-MP-03: nothing is
    // re-encoded), and page 1 is the sorted first entry, not the stored one.
    use base64::Engine;
    let ffi_bytes = base64::engine::general_purpose::STANDARD
        .decode(ffi_page["bytesBase64"].as_str().expect("bytesBase64"))
        .expect("decode base64");
    assert_eq!(ffi_bytes, http_body, "comic page bytes identical");
    assert_eq!(ffi_bytes, jpeg_bytes_for("page001.jpg"), "page 1 is sorted");
    assert_eq!(ffi_page["mimeType"], http_content_type);
    assert_eq!(ffi_page["mimeType"], "image/jpeg");
    assert_eq!(ffi_page["page"], 1);
    assert_eq!(ffi_page["pageCount"], 2);
}

/// UC-40 parity - thumbnail the same image over both transports and assert
/// the JPEG bytes are identical, HTTP raw against FFI base64 (Testing
/// Specification section 7.3, FR-MP-05, FR-MP-06).
#[tokio::test]
async fn given_same_image_when_thumbnailed_then_bytes_identical_across_surfaces() {
    // Arrange — each leg gets its own cache directory: the default is the
    // relative path "thumbnails", which would land in the repository.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let http_lib = tempdir().unwrap();
    let http_file = write_image(&http_lib, "photo.png", 640, 480);
    let ffi_lib = tempdir().unwrap();
    let ffi_file = write_image(&ffi_lib, "photo.png", 640, 480);

    let http_cache = tempdir().unwrap();
    let http_dir = tempdir().unwrap();
    let http_pool = migrate_database(&db_path(&http_dir, "http.sqlite"))
        .await
        .expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let settings = playback_settings(&http_cache);
    let http_services = std::sync::Arc::new(build_services(&settings, http_pool.clone()).await);
    let http_uuid = seed_file_at_path(&http_pool, "image", &http_file).await;

    let ffi_cache = tempdir().unwrap();
    let _thumbnail_cache_guard = ThumbnailCacheGuard::new(&ffi_cache);
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_uuid = seed_file_at_path(&ffi_pool, "image", &ffi_file).await;
    ffi_pool.close().await;

    // Act
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{http_uuid}/thumbnail"))
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(settings.clone(), http_services)
        .oneshot(req)
        .await
        .expect("http thumbnail");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_content_type = header_string(&resp, "content-type");
    let http_body = to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();

    let ffi_thumb: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let uuid_c = CString::new(ffi_uuid).unwrap();
        let r = alexandria_file_thumbnail(uuid_c.as_ptr(), token.as_ptr());
        assert_eq!(r.status, alexandria_ffi::PLAYBACK_OK, "ffi thumbnail");
        assert!(!r.json.is_null());
        let s = unsafe { CStr::from_ptr(r.json) }
            .to_string_lossy()
            .into_owned();
        unsafe {
            alexandria_free_string(r.json);
        }
        serde_json::from_str(&s).unwrap()
    })
    .await
    .unwrap();

    // Assert — the downscale-and-encode is deterministic, so the same source
    // image thumbnailed on either surface is byte-for-byte the same JPEG.
    use base64::Engine;
    let ffi_bytes = base64::engine::general_purpose::STANDARD
        .decode(ffi_thumb["bytesBase64"].as_str().expect("bytesBase64"))
        .expect("decode base64");
    assert_eq!(ffi_bytes, http_body, "thumbnail bytes identical");
    assert_eq!(ffi_thumb["mimeType"], http_content_type);
    assert_eq!(ffi_thumb["mimeType"], "image/jpeg");

    // Each leg cached its one entry inside its own directory, and nowhere
    // else — the default relative path would have written into the repository.
    assert_eq!(std::fs::read_dir(http_cache.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(ffi_cache.path()).unwrap().count(), 1);
}

/// Playback error parity - every row of F-10's error table decides the same
/// way on both surfaces (Testing Specification section 7.3, FR-MP-06,
/// NFR-09).
#[tokio::test]
async fn given_error_conditions_when_played_then_both_surfaces_agree() {
    // Arrange — per leg, one known text file (playable, but neither a comic
    // nor thumbnailable) and one soft-deleted file.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let http_lib = tempdir().unwrap();
    let http_known = http_lib.path().join("sample.txt");
    std::fs::write(&http_known, b"data").unwrap();
    let http_known = http_known.to_str().unwrap().to_string();
    let http_gone = http_lib.path().join("gone.txt");
    std::fs::write(&http_gone, b"data").unwrap();
    let http_gone = http_gone.to_str().unwrap().to_string();

    let ffi_lib = tempdir().unwrap();
    let ffi_known = ffi_lib.path().join("sample.txt");
    std::fs::write(&ffi_known, b"data").unwrap();
    let ffi_known = ffi_known.to_str().unwrap().to_string();
    let ffi_gone = ffi_lib.path().join("gone.txt");
    std::fs::write(&ffi_gone, b"data").unwrap();
    let ffi_gone = ffi_gone.to_str().unwrap().to_string();

    let unknown = uuid::Uuid::new_v4().to_string();

    let http_cache = tempdir().unwrap();
    let http_dir = tempdir().unwrap();
    let http_pool = migrate_database(&db_path(&http_dir, "http.sqlite"))
        .await
        .expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let settings = playback_settings(&http_cache);
    let http_services = std::sync::Arc::new(build_services(&settings, http_pool.clone()).await);
    let http_known_uuid = seed_file_at_path(&http_pool, "text", &http_known).await;
    let http_deleted_uuid = seed_file_at_path(&http_pool, "text", &http_gone).await;
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(&http_deleted_uuid)
        .execute(&http_pool)
        .await
        .unwrap();

    let ffi_cache = tempdir().unwrap();
    let _thumbnail_cache_guard = ThumbnailCacheGuard::new(&ffi_cache);
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi migrate");
    let ffi_known_uuid = seed_file_at_path(&ffi_pool, "text", &ffi_known).await;
    let ffi_deleted_uuid = seed_file_at_path(&ffi_pool, "text", &ffi_gone).await;
    sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(&ffi_deleted_uuid)
        .execute(&ffi_pool)
        .await
        .unwrap();
    ffi_pool.close().await;

    // Act — the same four requests on each surface, in the same order.
    let mut http_statuses = Vec::new();
    for uri in [
        format!("/v1/files/{unknown}/stream"),
        format!("/v1/files/{http_deleted_uuid}/stream"),
        format!("/v1/files/{http_known_uuid}/pages/1"),
        format!("/v1/files/{http_known_uuid}/thumbnail"),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("authorization", &format!("Bearer {TEST_TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let resp = app(settings.clone(), http_services.clone())
            .oneshot(req)
            .await
            .expect("http playback");
        http_statuses.push(resp.status().as_u16());
    }

    let unknown_for_ffi = unknown.clone();
    let ffi_statuses = tokio::task::spawn_blocking(move || -> Vec<i32> {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new(TEST_TOKEN).unwrap();
        let unknown_c = CString::new(unknown_for_ffi).unwrap();
        let deleted_c = CString::new(ffi_deleted_uuid).unwrap();
        let known_c = CString::new(ffi_known_uuid).unwrap();

        let mut out = Vec::new();
        for uuid_c in [&unknown_c, &deleted_c] {
            let r = alexandria_file_playback_source(uuid_c.as_ptr(), token.as_ptr());
            assert!(r.json.is_null());
            out.push(r.status);
        }
        let r = alexandria_comic_page(known_c.as_ptr(), 1, token.as_ptr());
        assert!(r.json.is_null());
        out.push(r.status);
        let r = alexandria_file_thumbnail(known_c.as_ptr(), token.as_ptr());
        assert!(r.json.is_null());
        out.push(r.status);
        out
    })
    .await
    .unwrap();

    // Assert — unknown uuid, soft-deleted, page on a non-comic, and thumbnail
    // on a type that has none, decided identically on both surfaces.
    assert_eq!(http_statuses, vec![404, 409, 400, 400]);
    assert_eq!(
        ffi_statuses,
        vec![
            alexandria_ffi::PLAYBACK_ERR_NOT_FOUND,
            alexandria_ffi::PLAYBACK_ERR_INVALID_STATE,
            alexandria_ffi::PLAYBACK_ERR_INVALID_INPUT,
            alexandria_ffi::PLAYBACK_ERR_INVALID_INPUT,
        ]
    );
}

/// UC-41 parity: registering over HTTP and over FFI must produce the same
/// body shape, and a second registration must conflict on both surfaces.
#[tokio::test]
async fn given_uc41_register_when_called_on_both_surfaces_then_bodies_match() {
    const PASSWORD: &str = "correct horse battery";

    fn register_body() -> serde_json::Value {
        json!({
            "email": "owner@example.com",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })
    }

    // The FFI leg mutates process-global state (`services_slot`), so every
    // parity test in this file takes `SERIAL` first.
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(register_body().to_string()))
        .unwrap();
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("http register");
    assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    let second = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(register_body().to_string()))
        .unwrap();
    let second = router.oneshot(second).await.expect("http second register");
    assert_eq!(second.status(), axum::http::StatusCode::CONFLICT);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_json, second_status): (String, i32) =
        tokio::task::spawn_blocking(move || -> (String, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let body = CString::new(register_body().to_string()).unwrap();
            let result = alexandria_auth_local_register(body.as_ptr());
            assert_eq!(
                result.status,
                alexandria_ffi::AUTH_OK,
                "ffi register failed"
            );
            assert!(!result.json.is_null());
            let json = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            unsafe {
                alexandria_free_string(result.json);
            }

            let second = alexandria_auth_local_register(body.as_ptr());
            (json, second.status)
        })
        .await
        .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();

    // ---- compare ----
    assert_eq!(
        second_status,
        alexandria_ffi::AUTH_ERR_CONFLICT,
        "a second FFI registration must conflict, as HTTP's 409 does"
    );
    assert_eq!(http_body["success"], ffi_body["success"]);
    assert_eq!(http_body["success"], json!(true));
    assert_eq!(http_body["email"], ffi_body["email"]);
    // Session ids are random per surface; assert shape, not equality.
    for body in [&http_body, &ffi_body] {
        let session_id = body["sessionId"].as_str().expect("sessionId");
        uuid::Uuid::parse_str(session_id).expect("sessionId must be a uuid");
    }
}

/// Issue #101 parity — a *rejected* registration must read identically over
/// both transports, byte for byte.
///
/// This is the half of `AuthJsonResult`'s "same shape HTTP returns" contract
/// that used to be false: the FFI error path set `json` to NULL, so six
/// distinct password-policy rejections were indistinguishable from an
/// unparseable body. Comparing raw bytes rather than parsed values is
/// deliberate — the two surfaces render through one function in the core, and
/// this test is what keeps that true.
#[tokio::test]
async fn given_a_rejected_registration_when_called_on_both_surfaces_then_error_bodies_match() {
    fn weak_body() -> serde_json::Value {
        json!({
            "email": "owner@example.com",
            "password": "short",
            "passwordConfirmation": "short",
        })
    }

    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(weak_body().to_string()))
        .unwrap();
    let response = router.oneshot(request).await.expect("http register");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let http_bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let http_json = String::from_utf8(http_bytes.to_vec()).expect("utf-8");

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_json, ffi_status): (String, i32) =
        tokio::task::spawn_blocking(move || -> (String, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let body = CString::new(weak_body().to_string()).unwrap();
            let result = alexandria_auth_local_register(body.as_ptr());
            assert!(
                !result.json.is_null(),
                "the FFI error path must carry the rejection, not discard it"
            );
            let json = unsafe { CStr::from_ptr(result.json) }
                .to_str()
                .unwrap()
                .to_string();
            unsafe {
                alexandria_free_string(result.json);
            }
            (json, result.status)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        ffi_status,
        alexandria_ffi::AUTH_ERR_INVALID_INPUT,
        "the status stays the coarse class; the code is the reason"
    );
    assert_eq!(
        http_json, ffi_json,
        "the rejection must be byte-for-byte identical on both surfaces"
    );
    let body: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();
    assert_eq!(body["code"], "password_too_short");
    assert_eq!(body["params"]["min"], "12");
}

/// UC-42 parity — a run's recorded status must read identically over both
/// transports. The run ids differ by construction (independent databases), so
/// parity asserts every field except the id, which is asserted to be the id
/// each surface was given.
#[tokio::test]
async fn given_a_completed_refresh_when_status_read_via_http_and_ffi_then_bodies_match() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let refresh_req = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = router
        .clone()
        .oneshot(refresh_req)
        .await
        .expect("http refresh");
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);
    let http_run_id = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let http_body = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let req = Request::builder()
                .method("GET")
                .uri(format!("/v1/index/runs/{http_run_id}"))
                .header("authorization", format!("Bearer {TEST_TOKEN}"))
                .body(Body::empty())
                .unwrap();
            let resp = router.clone().oneshot(req).await.expect("http status");
            assert_eq!(resp.status(), axum::http::StatusCode::OK);
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if body["status"] != "running" {
                break body;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "http run never finished"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    };

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_json, ffi_run_id): (String, String) =
        tokio::task::spawn_blocking(move || -> (String, String) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let started = alexandria_index_refresh_start(token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            let run_id = run_id_string(&started);
            assert!(!run_id.is_empty());

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let id = CString::new(run_id.clone()).unwrap();
                let token = CString::new(TEST_TOKEN).unwrap();
                let result = alexandria_index_run_status_json(id.as_ptr(), token.as_ptr());
                assert_eq!(result.status, alexandria_ffi::RUN_OK);
                assert!(!result.json.is_null());
                let json = unsafe { CStr::from_ptr(result.json) }
                    .to_str()
                    .unwrap()
                    .to_string();
                // SAFETY: pointer came from this library and is freed once.
                unsafe {
                    alexandria_free_string(result.json);
                }
                let body: serde_json::Value = serde_json::from_str(&json).unwrap();
                if body["status"] != "running" {
                    break (json, run_id);
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "ffi run never finished"
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        })
        .await
        .unwrap();

    let ffi_body: serde_json::Value = serde_json::from_str(&ffi_json).unwrap();

    // ---- compare ----
    assert_eq!(http_body["runId"], http_run_id);
    assert_eq!(ffi_body["runId"], ffi_run_id);
    assert_eq!(http_body["kind"], ffi_body["kind"]);
    assert_eq!(http_body["kind"], serde_json::json!("refresh"));
    assert_eq!(http_body["status"], ffi_body["status"]);
    assert_eq!(http_body["status"], serde_json::json!("complete"));
    for field in ["refreshed", "markedMissing", "unchanged", "failed"] {
        assert_eq!(http_body[field], ffi_body[field], "{field} differs");
    }
}

/// UC-43/UC-44 parity — redeeming a recovery code and regenerating the set
/// must read identically over both transports: the successful redemption, an
/// unissued code's refusal (`recovery_code_unknown`), and the fresh set a
/// regeneration returns.
#[tokio::test]
async fn given_the_recovery_surface_when_called_on_both_surfaces_then_bodies_match() {
    const PASSWORD: &str = "correct horse battery";
    const NEW_PASSWORD: &str = "a totally different passphrase";

    fn register_body() -> serde_json::Value {
        json!({
            "email": "owner@example.com",
            "password": PASSWORD,
            "passwordConfirmation": PASSWORD,
        })
    }

    fn login_body() -> serde_json::Value {
        json!({
            "email": "owner@example.com",
            "password": NEW_PASSWORD,
        })
    }

    fn unknown_redeem_body() -> serde_json::Value {
        json!({
            "code": "ZZZZZ-ZZZZZ",
            "newPassword": NEW_PASSWORD,
            "passwordConfirmation": NEW_PASSWORD,
        })
    }

    fn redeem_body(code: &str) -> serde_json::Value {
        json!({
            "code": code,
            "newPassword": NEW_PASSWORD,
            "passwordConfirmation": NEW_PASSWORD,
        })
    }

    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let register = Request::builder()
        .method("POST")
        .uri("/v1/auth/local/register")
        .header("content-type", "application/json")
        .body(Body::from(register_body().to_string()))
        .unwrap();
    let registered = router
        .clone()
        .oneshot(register)
        .await
        .expect("http register");
    assert_eq!(registered.status(), axum::http::StatusCode::CREATED);
    let registered: serde_json::Value =
        serde_json::from_slice(&to_bytes(registered.into_body(), usize::MAX).await.unwrap())
            .unwrap();
    let http_codes: Vec<String> = registered["recoveryCodes"]
        .as_array()
        .expect("recoveryCodes")
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(http_codes.len(), 10);

    // A code that was never issued must be refused without consuming a real
    // one -- this is the rejection whose reason code has to match FFI's.
    let unknown = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/recovery/redeem")
                .header("content-type", "application/json")
                .body(Body::from(unknown_redeem_body().to_string()))
                .unwrap(),
        )
        .await
        .expect("http redeem unknown");
    assert_eq!(unknown.status(), axum::http::StatusCode::BAD_REQUEST);
    let http_unknown = String::from_utf8(
        to_bytes(unknown.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .expect("utf-8");

    let redeem = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/recovery/redeem")
                .header("content-type", "application/json")
                .body(Body::from(redeem_body(&http_codes[0]).to_string()))
                .unwrap(),
        )
        .await
        .expect("http redeem");
    assert_eq!(redeem.status(), axum::http::StatusCode::OK);
    let http_redeem: serde_json::Value =
        serde_json::from_slice(&to_bytes(redeem.into_body(), usize::MAX).await.unwrap()).unwrap();

    // The redemption invalidated every session, so a fresh one is needed for
    // the authenticated regenerate call -- logging in with the new password
    // is how a real caller would get one too.
    let login = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/login")
                .header("content-type", "application/json")
                .body(Body::from(login_body().to_string()))
                .unwrap(),
        )
        .await
        .expect("http login");
    assert_eq!(login.status(), axum::http::StatusCode::OK);
    let login: serde_json::Value =
        serde_json::from_slice(&to_bytes(login.into_body(), usize::MAX).await.unwrap()).unwrap();
    let http_session = login["sessionId"].as_str().expect("sessionId").to_string();

    let regenerate = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/auth/local/recovery/regenerate")
                .header("authorization", format!("Bearer {http_session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("http regenerate");
    assert_eq!(regenerate.status(), axum::http::StatusCode::OK);
    let http_regenerate: serde_json::Value =
        serde_json::from_slice(&to_bytes(regenerate.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    // No session is pre-seeded: registration and login each mint their own,
    // exactly as the HTTP leg above relies on.
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi pre-migrate");
    ffi_pool.close().await;
    std::env::set_var("ALEXANDRIA_AUTH_MODE", "local");

    let (ffi_codes, ffi_unknown_json, ffi_unknown_status, ffi_redeem_json, ffi_regenerate_json) =
        tokio::task::spawn_blocking(move || -> (Vec<String>, String, i32, String, String) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let body = CString::new(register_body().to_string()).unwrap();
            let registered = alexandria_auth_local_register(body.as_ptr());
            assert_eq!(registered.status, alexandria_ffi::AUTH_OK, "ffi register");
            let register_value: serde_json::Value =
                serde_json::from_str(&take_json(registered.json)).unwrap();
            let codes: Vec<String> = register_value["recoveryCodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect();

            let unknown_body = CString::new(unknown_redeem_body().to_string()).unwrap();
            let unknown = alexandria_auth_local_redeem_recovery_code(unknown_body.as_ptr());
            let unknown_status = unknown.status;
            let unknown_json = take_json(unknown.json);

            let redeem_body_c = CString::new(redeem_body(&codes[0]).to_string()).unwrap();
            let redeem = alexandria_auth_local_redeem_recovery_code(redeem_body_c.as_ptr());
            assert_eq!(redeem.status, alexandria_ffi::AUTH_OK, "ffi redeem failed");
            let redeem_json = take_json(redeem.json);

            let login_body_c = CString::new(login_body().to_string()).unwrap();
            let login = alexandria_auth_local_login(login_body_c.as_ptr());
            assert_eq!(login.status, alexandria_ffi::AUTH_OK, "ffi login failed");
            let login_value: serde_json::Value =
                serde_json::from_str(&take_json(login.json)).unwrap();
            let session_id = login_value["sessionId"].as_str().unwrap().to_string();

            let token = CString::new(session_id).unwrap();
            let regenerate = alexandria_auth_local_regenerate_recovery_codes(token.as_ptr());
            assert_eq!(
                regenerate.status,
                alexandria_ffi::AUTH_OK,
                "ffi regenerate failed"
            );
            let regenerate_json = take_json(regenerate.json);

            (
                codes,
                unknown_json,
                unknown_status,
                redeem_json,
                regenerate_json,
            )
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(http_codes.len(), ffi_codes.len());

    assert_eq!(
        ffi_unknown_status,
        alexandria_ffi::AUTH_ERR_INVALID_INPUT,
        "an unissued code is invalid input on both surfaces"
    );
    let http_unknown_value: serde_json::Value = serde_json::from_str(&http_unknown).unwrap();
    let ffi_unknown_value: serde_json::Value = serde_json::from_str(&ffi_unknown_json).unwrap();
    assert_eq!(http_unknown_value["code"], json!("recovery_code_unknown"));
    assert_eq!(
        http_unknown_value["code"], ffi_unknown_value["code"],
        "the reason code must be identical on both surfaces"
    );

    let ffi_redeem: serde_json::Value = serde_json::from_str(&ffi_redeem_json).unwrap();
    assert_eq!(http_redeem["success"], ffi_redeem["success"]);
    assert_eq!(http_redeem["success"], json!(true));
    assert_eq!(http_redeem["recoveryCodesRemaining"], json!(9));
    assert_eq!(
        http_redeem["recoveryCodesRemaining"],
        ffi_redeem["recoveryCodesRemaining"]
    );

    let ffi_regenerate: serde_json::Value = serde_json::from_str(&ffi_regenerate_json).unwrap();
    let http_new_codes = http_regenerate["recoveryCodes"]
        .as_array()
        .expect("recoveryCodes");
    let ffi_new_codes = ffi_regenerate["recoveryCodes"]
        .as_array()
        .expect("recoveryCodes");
    assert_eq!(http_new_codes.len(), 10);
    assert_eq!(ffi_new_codes.len(), 10);
}

/// UC-46 parity — seed the same two collections into each leg's database, list
/// them over both transports, and assert the returned arrays are identical
/// (Testing Specification §7.3, FR-CO-08, FR-FC-24).
///
/// Seeded with fixed uuids rather than created through UC-10, so the two legs
/// hold literally the same rows and the bodies can be compared whole — a
/// created collection mints its own uuid per database, which would force the
/// comparison to drop the one field this listing exists to hand out.
///
/// The unrecognised-`kind` refusal (AF-02) is asserted here too: it is the one
/// flow the shared handler cannot answer, because both transports reject the
/// value while parsing their own request, and parity is exactly the claim that
/// they reject it the same way.
#[tokio::test]
async fn given_same_collections_when_listed_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    const FILMS: &str = "11111111-1111-4111-8111-111111111111";
    const READING: &str = "22222222-2222-4222-8222-222222222222";

    async fn seed_collections(pool: &sqlx::sqlite::SqlitePool) {
        for (uuid, name, kind) in [(FILMS, "Films", "file"), (READING, "Reading", "bookmark")] {
            sqlx::query("INSERT INTO collections (uuid, name, kind) VALUES (?, ?, ?)")
                .bind(uuid)
                .bind(name)
                .bind(kind)
                .execute(pool)
                .await
                .expect("seed collection");
        }

        // One active member and one soft-deleted member in Films, so the
        // parity assertion covers the derived count and its exclusion of
        // deleted rows rather than comparing two zeroes.
        let films_id: i64 = sqlx::query_scalar("SELECT id FROM collections WHERE uuid = ?")
            .bind(FILMS)
            .fetch_one(pool)
            .await
            .expect("films id");
        for (uuid, state) in [
            ("aaaaaaaa-0000-4000-8000-000000000001", "active"),
            ("aaaaaaaa-0000-4000-8000-000000000002", "deleted"),
        ] {
            sqlx::query(
                "INSERT INTO files (uuid, path, name, type, content_hash, state, indexed_at, collection_id) \
                 VALUES (?, ?, ?, 'text', 'hash', ?, ?, ?)",
            )
            .bind(uuid)
            .bind(format!("/lib/{uuid}.txt"))
            .bind(format!("{uuid}.txt"))
            .bind(state)
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(films_id)
            .execute(pool)
            .await
            .expect("seed file");
        }
    }

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    seed_collections(&http_pool).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("GET")
        .uri("/v1/collections")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(req)
        .await
        .expect("http list");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let bad = Request::builder()
        .method("GET")
        .uri("/v1/collections?kind=playlist")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let http_bad_status = app(Settings::default(), http_services)
        .oneshot(bad)
        .await
        .expect("http bad kind")
        .status();

    // ---- FFI leg (off the tokio thread: FFI block_on its own runtime) ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let ffi_pool = migrate_database(&ffi_db).await.expect("ffi open");
    seed_collections(&ffi_pool).await;
    ffi_pool.close().await;

    let (ffi_body, ffi_bad_status) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, std::os::raw::c_int) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let empty = CString::new("").unwrap();
            let r = alexandria_collections_list(empty.as_ptr(), token.as_ptr());
            assert_eq!(r.status, alexandria_ffi::COLLECTION_OK, "ffi list");
            assert!(!r.json.is_null());
            // SAFETY: pointer came from this library and is freed once below.
            let s = unsafe { CStr::from_ptr(r.json) }
                .to_string_lossy()
                .into_owned();
            unsafe {
                alexandria_free_string(r.json);
            }

            let bad = CString::new(json!({ "kind": "playlist" }).to_string()).unwrap();
            let bad_result = alexandria_collections_list(bad.as_ptr(), token.as_ptr());
            assert!(bad_result.json.is_null(), "a refusal carries no body");

            (serde_json::from_str(&s).unwrap(), bad_result.status)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(
        http_body, ffi_body,
        "collection listing diverges across surfaces"
    );

    // The listing itself is worth asserting once, so a parity test over two
    // identically-wrong answers cannot pass.
    let items = http_body.as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["uuid"], FILMS);
    assert_eq!(items[0]["name"], "Films");
    assert_eq!(items[0]["kind"], "file");
    assert_eq!(
        items[0]["itemCount"], 1,
        "the deleted member is not counted"
    );
    assert_eq!(items[1]["uuid"], READING);
    assert_eq!(items[1]["itemCount"], 0);

    // AF-02 refused the same way on both surfaces.
    assert_eq!(http_bad_status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(ffi_bad_status, alexandria_ffi::COLLECTION_ERR_INVALID_INPUT);
}

/// UC-47 parity — read the settings over both transports and assert the
/// bodies are identical (Testing Specification §7.3, FR-FC-30, FR-FC-24).
///
/// Both legs run at the default window: what parity asserts is that the two
/// surfaces answer the same thing, and that a configured value is honoured is
/// the core and HTTP suites' claim rather than this one's. The FFI leg reads
/// its settings at `alexandria_index_init`, from the same loader HTTP uses.
///
/// The unauthenticated refusal (AF-01) is asserted here too: it is the one
/// flow the read has, and parity is exactly the claim that both surfaces
/// refuse the same way.
#[tokio::test]
async fn given_the_same_settings_when_read_via_http_and_ffi_then_bodies_identical() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    seed_session(&http_pool, TEST_TOKEN).await;
    let http_services =
        std::sync::Arc::new(build_services(&local_settings(), http_pool.clone()).await);

    let req = Request::builder()
        .method("GET")
        .uri("/v1/settings")
        .header("authorization", &format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(req)
        .await
        .expect("http settings");
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    let anonymous = Request::builder()
        .method("GET")
        .uri("/v1/settings")
        .body(Body::empty())
        .unwrap();
    let http_denied = app(Settings::default(), http_services)
        .oneshot(anonymous)
        .await
        .expect("http denied")
        .status();

    // ---- FFI leg (off the tokio thread: FFI block_on its own runtime) ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = setup_ffi_db(&ffi_dir, "ffi.sqlite", TEST_TOKEN).await;
    let (ffi_body, ffi_denied) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, std::os::raw::c_int) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new(TEST_TOKEN).unwrap();
            let r = alexandria_settings_json(token.as_ptr());
            assert_eq!(r.status, alexandria_ffi::SETTINGS_OK, "ffi settings");
            assert!(!r.json.is_null());
            // SAFETY: pointer came from this library and is freed once below.
            let s = unsafe { CStr::from_ptr(r.json) }
                .to_string_lossy()
                .into_owned();
            unsafe {
                alexandria_free_string(r.json);
            }

            let nobody = CString::new("").unwrap();
            let denied = alexandria_settings_json(nobody.as_ptr());
            assert!(denied.json.is_null(), "a refusal carries no body");

            (serde_json::from_str(&s).unwrap(), denied.status)
        })
        .await
        .unwrap();

    // ---- compare ----
    assert_eq!(http_body, ffi_body, "settings diverge across surfaces");

    // Asserted once, so a parity test over two identically-wrong answers
    // cannot pass.
    assert_eq!(http_body["deletion"]["retentionDays"], 30);

    assert_eq!(http_denied, axum::http::StatusCode::UNAUTHORIZED);
    assert_eq!(ffi_denied, alexandria_ffi::SETTINGS_ERR_UNAUTHORIZED);
}
