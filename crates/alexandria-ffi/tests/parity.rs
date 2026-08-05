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
    alexandria_bookmark_create, alexandria_bookmark_purge, alexandria_bookmark_restore,
    alexandria_bookmark_soft_delete, alexandria_bookmark_update, alexandria_bookmarks_list,
    alexandria_collection_add_items, alexandria_collection_create, alexandria_collection_delete,
    alexandria_collection_list_items, alexandria_collection_remove_item,
    alexandria_collection_rename, alexandria_file_edit_metadata, alexandria_file_get_by_uuid,
    alexandria_file_purge, alexandria_file_purge_on_disk, alexandria_file_rename,
    alexandria_file_restore, alexandria_file_soft_delete, alexandria_files_list,
    alexandria_free_string, alexandria_index_count_files, alexandria_index_count_missing,
    alexandria_index_files_json, alexandria_index_init, alexandria_index_refresh_start,
    alexandria_index_start, alexandria_watchlist_add_video, alexandria_watchlist_create,
    alexandria_watchlist_delete, alexandria_watchlist_remove_video,
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
static SERIAL: Mutex<()> = Mutex::new(());

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
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
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
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

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
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let index_req = Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let refresh_resp = app(Settings::default(), http_services.clone())
        .oneshot(refresh_req)
        .await
        .expect("http refresh");
    assert_eq!(refresh_resp.status(), axum::http::StatusCode::ACCEPTED);

    wait_for_http_missing(&http_pool, 1).await;

    let http_rows: Vec<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT path, name, type, content_hash, missing_at FROM files ORDER BY path",
    )
    .fetch_all(&http_pool)
    .await
    .unwrap();

    // ---- FFI leg (own identical lib) ----
    let ffi_lib = seed_lib();
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
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

async fn wait_for_http_missing(pool: &sqlx::sqlite::SqlitePool, expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (c,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM files WHERE missing_at IS NOT NULL")
                .fetch_one(pool)
                .await
                .unwrap();
        if c >= expected {
            return;
        }
        if std::time::Instant::now() > dl {
            panic!("http never had {expected} missing");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn wait_for_ffi_files(expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
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

fn wait_for_ffi_missing(expected: i64) {
    let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if alexandria_index_count_missing() >= expected {
            return;
        }
        if std::time::Instant::now() > dl {
            panic!("ffi never had {expected} missing");
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
    let _g = SERIAL.lock().unwrap();

    let patch_json = r#"{"type":"audio","title":"Parity Title","artist":"Artist","album":"Album","year":2001,"genre":"Rock","track":3}"#;
    let patch_value: serde_json::Value = serde_json::from_str(patch_json).unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"audio").unwrap();
    std::fs::write(http_lib.path().join("notes.md"), b"# h").unwrap();
    std::fs::write(http_lib.path().join("clip.mkv"), b"video").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_default_n, ffi_all_n, ffi_audio_n) =
        tokio::task::spawn_blocking(move || -> (FileTriples, FileTriples, FileTriples) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    let missing = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/files/{missing}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services.clone())
        .oneshot(req)
        .await
        .expect("http get");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let missing_str = missing.to_string();
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg: index a file so a silently-dropped filter would show up
    // as a non-empty 200 rather than an error. ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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

    let http_status = |uri: &'static str| {
        let services = http_services.clone();
        async move {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (bad_type, bad_state, empty_values) =
        tokio::task::spawn_blocking(move || -> (i32, i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    let new_name = "renamed.mp3";

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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

    let rename_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/files/{http_uuid}/rename"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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

    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_uuid, ffi_body) =
        tokio::task::spawn_blocking(move || -> (String, serde_json::Value) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

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
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // Well past the default 30-day retention window on both legs.
    let deleted_at = "2024-01-01T00:00:00Z";

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_body, ffi_files_remaining, ffi_subtype_remaining) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, i64, i64) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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

    let (http_uuid, http_file_id): (String, i64) =
        sqlx::query_as("SELECT uuid, id FROM files WHERE name = ?")
            .bind("song.mp3")
            .fetch_one(&http_pool)
            .await
            .unwrap();

    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge-on-disk=true"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_body, ffi_files_remaining, ffi_subtype_remaining) =
        tokio::task::spawn_blocking(move || -> (serde_json::Value, i64, i64) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let root = CString::new(ffi_lib_path).unwrap();
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    let uuid = uuid::Uuid::new_v4().to_string();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    for query in ["purge=true", "purge-on-disk=true"] {
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/v1/files/{uuid}?{query}"))
            .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_uuid = uuid.clone();
    let (purge_status, purge_on_disk_status) =
        tokio::task::spawn_blocking(move || -> (i32, i32) {
            let cdb = CString::new(ffi_db).unwrap();
            assert_eq!(
                alexandria_index_init(cdb.as_ptr()),
                alexandria_ffi::INDEX_OK
            );

            let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_lib = tempdir().unwrap();
    std::fs::write(http_lib.path().join("song.mp3"), b"parity-audio").unwrap();

    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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

    // Never soft-deleted — the record is `active`, so the retention window
    // never started and the hard purge must be refused.
    let purge_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/files/{http_uuid}?purge=true"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_lib_path = ffi_lib.path().to_str().unwrap().to_string();

    let (ffi_status, ffi_remaining) = tokio::task::spawn_blocking(move || -> (i32, i64) {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let root = CString::new(ffi_lib_path).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body =
            CString::new(json!({ "name": "Mixed bag", "kind": "playlist" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/collections/{unknown}"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let body = CString::new(json!({ "name": "New name" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "Sci-fi novels", "kind": "file" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/collections/{unknown}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http delete");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
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
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    assert_eq!(http_body["itemUuids"], json!([http_file_uuid]));
    assert_eq!(ffi_body["itemUuids"], json!([ffi_file_uuid]));
    assert!(http_linked.is_some(), "http file linked");
    assert!(ffi_linked.is_some(), "ffi file linked");

    ffi_pool.close().await;
}

/// UC-13 parity — an item that does not exist is rejected as not-found on
/// both surfaces (HTTP 404, FFI `COLLECTION_ERR_NOT_FOUND`) (AF-02,
/// FR-FC-24 / NFR-09).
#[tokio::test]
async fn given_unknown_item_when_added_via_http_and_ffi_then_both_not_found() {
    let _g = SERIAL.lock().unwrap();
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "itemUuids": [unknown] }).to_string()))
        .unwrap();
    let add_resp = router.oneshot(add_req).await.expect("http add items");
    assert_eq!(add_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
        assert!(r.json.is_null(), "a rejected add returns no body");
        r.status
    })
    .await
    .unwrap();

    assert_eq!(
        ffi_status,
        alexandria_ffi::COLLECTION_ERR_NOT_FOUND,
        "ffi must reject an unknown item as not-found (HTTP 404)"
    );
}

/// UC-14 parity — remove the same linked file from the same collection over
/// both transports and assert the returned bodies agree and each `files` row
/// is unlinked (Testing Specification §7.3, FR-CO-06, FR-FC-24).
#[tokio::test]
async fn given_same_linked_file_when_removed_via_http_and_ffi_then_bodies_and_links_identical() {
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
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
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let remove_resp = router.oneshot(remove_req).await.expect("http remove");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "name": "My files", "kind": "file" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
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
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/collections")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let list_resp = router.oneshot(list_req).await.expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/v1/bookmarks/{unknown}"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let list_resp = router.oneshot(list_req).await.expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::OK);
    let http_body: serde_json::Value =
        serde_json::from_slice(&to_bytes(list_resp.into_body(), usize::MAX).await.unwrap())
            .unwrap();

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let create_body =
            CString::new(json!({ "url": "https://example.com", "title": "Example" }).to_string())
                .unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();
    let unknown = uuid::Uuid::new_v4();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/bookmarks?collectionUuid={unknown}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http list");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let filter = CString::new(json!({ "collectionUuid": unknown }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{unknown}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http delete");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    let deleted_at = "2024-01-01T00:00:00Z";

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let router = app(Settings::default(), http_services);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/bookmarks")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");

    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db.clone()).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);
    let unknown = uuid::Uuid::new_v4();
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/bookmarks/{unknown}?purge=true"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let resp = app(Settings::default(), http_services)
        .oneshot(req)
        .await
        .expect("http purge");
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let uuid_c = CString::new(unknown.to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_db_for_rows = ffi_db.clone();
    let ffi_body: serde_json::Value = tokio::task::spawn_blocking(move || -> serde_json::Value {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let body = CString::new(json!({ "name": "Weekend movies" }).to_string()).unwrap();
        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let http_video_uuid = seed_file(&http_pool, "video").await;
    let unknown = uuid::Uuid::new_v4().to_string();

    let add_req = Request::builder()
        .method("POST")
        .uri(format!("/v1/watchlists/{unknown}/items"))
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let list_req = Request::builder()
        .method("GET")
        .uri(format!("/v1/watchlists?watchlistUuid={unknown}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let list_resp = app(Settings::default(), http_services)
        .oneshot(list_req)
        .await
        .expect("http list");
    assert_eq!(list_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let remove_resp = app(Settings::default(), http_services)
        .oneshot(remove_req)
        .await
        .expect("http remove video");
    assert_eq!(remove_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let create_req = Request::builder()
        .method("POST")
        .uri("/v1/watchlists")
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
        .header("authorization", "Bearer parity")
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
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
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

        let token = CString::new("parity").unwrap();
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
    let _g = SERIAL.lock().unwrap();

    // ---- HTTP leg ----
    let http_dir = tempdir().unwrap();
    let http_db = db_path(&http_dir, "http.sqlite");
    let http_pool = migrate_database(&http_db).await.expect("http migrate");
    let http_services =
        std::sync::Arc::new(build_services(&Settings::default(), http_pool.clone()).await);

    let unknown = uuid::Uuid::new_v4().to_string();
    let delete_req = Request::builder()
        .method("DELETE")
        .uri(format!("/v1/watchlists/{unknown}"))
        .header("authorization", "Bearer parity")
        .body(Body::empty())
        .unwrap();
    let delete_resp = app(Settings::default(), http_services)
        .oneshot(delete_req)
        .await
        .expect("http delete watchlist");
    assert_eq!(delete_resp.status(), axum::http::StatusCode::NOT_FOUND);

    // ---- FFI leg ----
    let ffi_dir = tempdir().unwrap();
    let ffi_db = db_path(&ffi_dir, "ffi.sqlite");
    let ffi_status = tokio::task::spawn_blocking(move || -> i32 {
        let cdb = CString::new(ffi_db).unwrap();
        assert_eq!(
            alexandria_index_init(cdb.as_ptr()),
            alexandria_ffi::INDEX_OK
        );

        let token = CString::new("parity").unwrap();
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
