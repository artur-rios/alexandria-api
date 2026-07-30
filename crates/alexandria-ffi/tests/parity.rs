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
    alexandria_free_string, alexandria_index_count_files, alexandria_index_files_json,
    alexandria_index_init, alexandria_index_start, IndexStartResult,
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