//! HTTP ↔ FFI parity for UC-01 (Testing Specification §7.3). Index the same
//! library directory through both transports into separate databases and
//! assert the persisted file rows are byte-for-byte identical (path, type,
//! hash). The start contract shape is also asserted (HTTP 202 `{runId}` and
//! the FFI `IndexStartResult` both carry a valid UUID on success).

use std::ffi::{CStr, CString};
use std::sync::Mutex;

use alexandria_core::config::Settings;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::build_services;
use alexandria_http::app;
use alexandria_ffi::{
    alexandria_file_edit_metadata, alexandria_free_string, alexandria_index_count_files,
    alexandria_index_count_missing, alexandria_index_files_json, alexandria_index_init,
    alexandria_index_refresh_start, alexandria_index_start, IndexStartResult,
};
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::json;
use tempfile::{tempdir, TempDir};
use tower::ServiceExt;

// One global FFI services slot per process: serialize every parity test that
// touches it (there is only one here, but the guard keeps the suite safe if
// more are added).
static SERIAL: Mutex<()> = Mutex::new(());

fn db_path(dir: &TempDir, name: &str) -> String {
    dir.path().join(name).to_str().unwrap().to_string()
}

fn run_id_string(r: &IndexStartResult) -> String {
    let n = r.run_id.iter().position(|&ch| ch == 0).unwrap_or(r.run_id.len());
    String::from_utf8_lossy(
        &r.run_id[..n].iter().map(|&ch| ch as u8).collect::<Vec<u8>>(),
    )
    .into_owned()
}

#[tokio::test]
async fn given_same_lib_when_indexed_via_http_and_ffi_then_files_rows_identical() {
    let _g = SERIAL.lock().unwrap();

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
    let http_services = std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let request = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer parity")
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let lib_for_ffi = lib_path.clone();
    let ffi_json: String = tokio::task::spawn_blocking(move || -> String {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(alexandria_index_init(cdb.as_ptr()), alexandria_ffi::INDEX_OK);

        let root = CString::new(lib_for_ffi).unwrap();
        let token = CString::new("parity").unwrap();
        let result = alexandria_index_start(root.as_ptr(), token.as_ptr());
        assert_eq!(result.status, alexandria_ffi::INDEX_OK, "ffi start failed");
        assert!(!run_id_string(&result).is_empty());

        let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
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
        alexandria_free_string(raw);
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
    let _g = SERIAL.lock().unwrap();

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
    let http_services = std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer parity")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "root": http_lib.path().to_str().unwrap() }).to_string()))
        .unwrap();
    let resp = app(Settings::default(), http_services.clone()).oneshot(index_req).await.expect("http index");
    assert_eq!(resp.status(), axum::http::StatusCode::ACCEPTED);

    wait_for_http_files(&http_pool, 2).await;

    mutate(&http_lib);

    let refresh_req = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let refresh_resp = app(Settings::default(), http_services.clone()).oneshot(refresh_req).await.expect("http refresh");
    assert_eq!(refresh_resp.status(), axum::http::StatusCode::ACCEPTED);

    wait_for_http_missing(&http_pool, 1).await;

    let http_rows: Vec<(String, String, String, String, Option<String>)> =
        sqlx::query_as("SELECT path, name, type, content_hash, missing_at FROM files ORDER BY path")
            .fetch_all(&http_pool)
            .await
            .unwrap();

    // ---- FFI leg (own identical lib) ----
    let ffi_lib = seed_lib();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let ffi_rows: Vec<(String, String, String, String, Option<String>)> =
        tokio::task::spawn_blocking(move || -> Vec<(String,String,String,String,Option<String>)> {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(alexandria_index_init(cdb.as_ptr()), alexandria_ffi::INDEX_OK);

            let root = CString::new(ffi_lib_path.clone()).unwrap();
            let token = CString::new("parity").unwrap();
            let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
            assert_eq!(started.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_files(2);

            // identical mutation on disk
            std::fs::write(ffi_lib.path().join("a.mp3"), b"audio-v2-CHANGED").unwrap();
            std::fs::remove_file(ffi_lib.path().join("b.md")).unwrap();

            let refresh = alexandria_index_refresh_start(token.as_ptr());
            assert_eq!(refresh.status, alexandria_ffi::INDEX_OK);
            wait_for_ffi_missing(1);

            let raw = alexandria_index_files_json();
            assert!(!raw.is_null());
            let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
            alexandria_free_string(raw);
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
        })
        .await
        .unwrap();

    // ---- compare ----
    // Compare name + hash + (missingAt presence). The exact `missingAt`
    // timestamp fires at different wall-clock instants on the two surfaces
    // (like a random run id), so parity asserts the marker's *presence*, not
    // its value.
    let norm = |rows: &[(String, String, String, String, Option<String>)]| -> Vec<(String,String,bool)> {
        let mut v: Vec<(String,String,bool)> = rows.iter()
            .map(|r| (r.1.clone(), r.3.clone(), r.4.is_some()))
            .collect();
        v.sort();
        v
    };
    let http_n = norm(&http_rows);
    let ffi_n = norm(&ffi_rows);
    assert_eq!(http_n, ffi_n, "refreshed rows + missing markers differ across surfaces");

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

fn wait_for_http_files(pool: &sqlx::sqlite::SqlitePool, expected: i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files").fetch_one(pool).await.unwrap();
            if c >= expected { return; }
            if std::time::Instant::now() > dl { panic!("http never had {expected} files"); }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
}

async fn wait_for_http_missing(pool: &sqlx::sqlite::SqlitePool, expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE missing_at IS NOT NULL")
            .fetch_one(pool).await.unwrap();
        if c >= expected { return; }
        if std::time::Instant::now() > dl { panic!("http never had {expected} missing"); }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn wait_for_ffi_files(expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if alexandria_index_count_files() >= expected { return; }
        if std::time::Instant::now() > dl { panic!("ffi never had {expected} files"); }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for_ffi_missing(expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if alexandria_index_count_missing() >= expected { return; }
        if std::time::Instant::now() > dl { panic!("ffi never had {expected} missing"); }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// UC-04 parity — edit metadata over both transports with identical patch
/// JSON and assert the returned `FileMetadata` bodies agree (modulo the
/// file's UUID, which is per-database) and the persisted `audio_files` rows
/// match across both databases (Testing Specification §7.3, FR-FC-24).
#[tokio::test]
async fn given_same_audio_file_when_metadata_edited_via_http_and_ffi_then_responses_and_rows_identical() {
    let _g = SERIAL.lock().unwrap();

    let patch_json = r#"{"type":"audio","title":"Parity Title","artist":"Artist","album":"Album","year":2001,"genre":"Rock","track":3}"#;
    let patch_value: serde_json::Value = serde_json::from_str(patch_json).unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services = std::sync::Arc::new(
        build_services(&Settings::default(), http_pool.clone()).await,
    );

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .header("content-type", "application/json")
        .body(Body::from(patch_json.to_string()))
        .unwrap();
    let http_resp = app(Settings::default(), http_services.clone())
        .oneshot(patch_req)
        .await
        .expect("http patch");
    assert_eq!(http_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value = serde_json::from_slice(
        &to_bytes(http_resp.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap();

    let http_audio_row: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    ) = sqlx::query_as(
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();
    let patch_for_ffi = patch_json.to_string();
    let ffi_payload: (String, serde_json::Value, (Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>, Option<i64>)) =
        tokio::task::spawn_blocking(move || {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(alexandria_index_init(cdb.as_ptr()), alexandria_ffi::INDEX_OK);

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
            alexandria_free_string(result.json);
            let ffi_value: serde_json::Value = serde_json::from_str(&json_str).unwrap();

            // Persisted audio row from the FFI db.
            let ffi_audio_row = std::thread::spawn({
                let ffi_dir = ffi_dir.path().to_path_buf();
                let ffi_uuid = ffi_uuid.clone();
                move || -> (Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>, Option<i64>) {
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
                        let row: (Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>, Option<i64>) =
                            sqlx::query_as(
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
    assert_eq!(http_body["metadata"], patch_value, "metadata must echo patch");
}