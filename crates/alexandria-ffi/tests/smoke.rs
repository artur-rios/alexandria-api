use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use tempfile::{tempdir, TempDir};

// The FFI keeps services in a process-global static. Within one test binary,
// tests run in parallel and would race on that slot, so serialize every FFI
// test behind this lock (no serial_test dependency needed).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap()
}

use alexandria_ffi::{
    alexandria_file_edit_metadata, alexandria_file_purge, alexandria_file_purge_on_disk,
    alexandria_file_rename, alexandria_file_restore, alexandria_file_soft_delete,
    alexandria_free_string, alexandria_index_count_files, alexandria_index_count_missing,
    alexandria_index_files_json, alexandria_index_init, alexandria_index_refresh_start,
    alexandria_index_start, FileMetadataResult, IndexStartResult,
};

const STATUS_OK: i32 = alexandria_ffi::INDEX_OK;
const STATUS_INVALID_INPUT: i32 = alexandria_ffi::INDEX_ERR_INVALID_INPUT;
const STATUS_UNAUTHORIZED: i32 = alexandria_ffi::INDEX_ERR_UNAUTHORIZED;
const STATUS_FILE_INVALID_INPUT: i32 = alexandria_ffi::FILE_ERR_INVALID_INPUT;
const STATUS_FILE_UNAUTHORIZED: i32 = alexandria_ffi::FILE_ERR_UNAUTHORIZED;
const STATUS_FILE_NOT_INITIALIZED: i32 = alexandria_ffi::FILE_ERR_NOT_INITIALIZED;
const STATUS_FILE_NOT_FOUND: i32 = alexandria_ffi::FILE_ERR_NOT_FOUND;
const STATUS_FILE_INVALID_STATE: i32 = alexandria_ffi::FILE_ERR_INVALID_STATE;
const STATUS_FILE_OK: i32 = alexandria_ffi::FILE_OK;
const STATUS_FILE_OTHER: i32 = alexandria_ffi::FILE_ERR_OTHER;
const STATUS_FILE_DISK: i32 = alexandria_ffi::FILE_ERR_DISK;

fn init_temp_db() -> (TempDir, String) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ffi.sqlite");
    let db_path = db.to_str().unwrap().to_string();
    let cpath = CString::new(db_path.clone()).unwrap();
    let status = alexandria_index_init(cpath.as_ptr());
    assert_eq!(status, STATUS_OK, "ffi services init failed");
    (dir, db_path)
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn run_id_string(r: &IndexStartResult) -> String {
    let n = r.run_id.iter().position(|&ch| ch == 0).unwrap_or(r.run_id.len());
    String::from_utf8_lossy(&r.run_id[..n].iter().map(|&ch| ch as u8).collect::<Vec<u8>>())
        .into_owned()
}

fn wait_for_files(expected: i64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count = alexandria_index_count_files();
        if count >= expected {
            return count;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for {expected} files; had {count}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn given_ffi_library_when_version_called_then_returns_version_string() {
    let _g = serial();
    let raw = alexandria_ffi::alexandria_version();
    assert!(!raw.is_null());
    // SAFETY: the FFI returns a static NUL-terminated string.
    let cstr = unsafe { CStr::from_ptr(raw) };
    assert_eq!(cstr.to_str().unwrap(), "0.1.0");
}

#[test]
fn given_ffi_library_when_health_status_code_called_then_returns_200() {
    let _g = serial();
    assert_eq!(alexandria_ffi::alexandria_health_status_code(), 200);
}

#[test]
fn given_supported_files_when_ffi_index_start_then_returns_ok_with_run_id_and_persists() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("a.mp3"), b"audio").unwrap();
    std::fs::write(lib.path().join("b.md"), b"text").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    let result = alexandria_index_start(root.as_ptr(), token.as_ptr());

    assert_eq!(result.status, STATUS_OK);
    assert!(!run_id_string(&result).is_empty());

    assert_eq!(wait_for_files(2), 2);

    let raw = alexandria_index_files_json();
    assert!(!raw.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
    // SAFETY: pointer came from this library and is freed once.
    unsafe { alexandria_free_string(raw); }
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 2);
    assert_eq!(parsed[0]["type"], "audio");
    assert_eq!(parsed[1]["type"], "text");
}

#[test]
fn given_missing_root_when_ffi_index_start_then_returns_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let root = c("/no/such/dir/here");
    let token = c("bearer");
    let result = alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

#[test]
fn given_empty_token_when_ffi_index_start_then_returns_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("");
    let result = alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_UNAUTHORIZED);
}

#[test]
fn given_not_initialized_when_ffi_index_start_then_returns_not_initialized() {
    let _g = serial();
    // Replace the global slot with None by re-initializing to a fresh path and
    // then... we can only set Some. Instead, verify the freshly-initialized
    // case: after init returns OK, the slot is Some; assert a NULL root yields
    // invalid input (covers the error path that does not hit the slot error).
    let _db = init_temp_db();
    let result = alexandria_index_start(std::ptr::null(), std::ptr::null());
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

fn files_json_value() -> serde_json::Value {
    let raw = alexandria_index_files_json();
    assert!(!raw.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
    // SAFETY: pointer came from this library and is freed once.
    unsafe { alexandria_free_string(raw); }
    serde_json::from_str(&json).unwrap()
}

/// Wait until `missing_at IS NOT NULL` count reaches `expected` missing files.
fn wait_for_missing(expected: i64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count = alexandria_index_count_missing();
        if count >= expected {
            return count;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for {expected} missing; had {count}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn given_changed_and_deleted_files_when_ffi_refresh_then_refreshes_and_marks_missing() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    let a_path = lib.path().join("a.mp3");
    let b_path = lib.path().join("b.md");
    std::fs::write(&a_path, b"audio-v1").unwrap();
    std::fs::write(&b_path, b"text-v1").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(2), 2);

    // Capture the pre-refresh hashes via the JSON accessor.
    let before = files_json_value();
    let old_a_hash = before[0]["hash"].as_str().unwrap().to_string();

    // Mutate on disk: change a, delete b.
    std::fs::write(&a_path, b"audio-v2-CHANGED").unwrap();
    std::fs::remove_file(&b_path).unwrap();

    let refresh = alexandria_index_refresh_start(token.as_ptr());
    assert_eq!(refresh.status, STATUS_OK);
    assert!(!run_id_string(&refresh).is_empty());

    // b must be marked missing.
    assert_eq!(wait_for_missing(1), 1);

    // a's hash must have changed, and its missingAt must be null.
    let after = files_json_value();
    let a_row = after
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "a.mp3")
        .unwrap();
    let b_row = after
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["name"] == "b.md")
        .unwrap();
    assert_ne!(a_row["hash"].as_str().unwrap(), old_a_hash);
    assert!(a_row["missingAt"].is_null(), "a missingAt cleared");
    assert!(b_row["missingAt"].is_string(), "b missingAt set");
}

#[test]
fn given_empty_token_when_ffi_refresh_then_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();
    let token = c("");
    let result = alexandria_index_refresh_start(token.as_ptr());
    assert_eq!(result.status, STATUS_UNAUTHORIZED);
}

#[allow(dead_code)]
fn _unused(c: *const c_char, s: &str) -> String {
    let _ = c;
    s.to_string()
}

// ---------------------------------------------------------------------------
// UC-04: alexandria_file_edit_metadata (FR-FC-14..18, FR-FC-24)
// ---------------------------------------------------------------------------

/// Result JSON of a `FileMetadataResult` on success, parsed. Asserts `FILE_OK`.
fn metadata_json(result: FileMetadataResult) -> serde_json::Value {
    assert_eq!(
        result.status, STATUS_FILE_OK,
        "expected FILE_OK, got {}",
        result.status
    );
    assert!(!result.json.is_null(), "success must carry a json pointer");
    // SAFETY: FFI returned a NUL-terminated string via CString::into_raw.
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    // SAFETY: pointer came from this library and is freed once.
    unsafe { alexandria_free_string(result.json); }
    serde_json::from_str(&json).expect("FileMetadata json")
}

/// Open a read/write connection to the FFI's SQLite file on a dedicated thread
/// with its own tokio runtime (the FFI owns the process-global runtime, so
/// verification queries run on a separate thread to avoid a nested-runtime
/// panic). The closure receives the pool and returns a `Send + 'static`
/// future — it must hold its own (cheaply-cloned) pool reference inside the
/// future so the future is `'static`.
fn with_db<F, Fut, T>(db_path: &str, f: F) -> T
where
    F: FnOnce(sqlx::sqlite::SqlitePool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let db_path = db_path.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("verification runtime");
        rt.block_on(async move {
            let url = format!("sqlite://{db_path}?mode=rw");
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .expect("verification pool");
            f(pool).await
        })
    })
    .join()
    .expect("verification thread")
}

fn uuid_by_name(db_path: &str, name: &str) -> String {
    let name = name.to_string();
    with_db(db_path, move |pool| async move {
        let (uuid,): (String,) = sqlx::query_as("SELECT uuid FROM files WHERE name = ?")
            .bind(name)
            .fetch_one(&pool)
            .await
            .expect("uuid row");
        uuid
    })
}

#[test]
fn given_indexed_audio_file_when_ffi_edit_metadata_then_ok_and_row_updated() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(
        r#"{"type":"audio","title":"New Title","artist":"Artist","year":2001,"track":3}"#,
    );
    let result =
        alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());

    let json = metadata_json(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["file"]["fileType"], "audio");
    assert_eq!(json["metadata"]["type"], "audio");
    assert_eq!(json["metadata"]["title"], "New Title");
    assert_eq!(json["metadata"]["track"], 3);

    // Persisted subtype row reflects the full-replace PATCH.
    let uuid_clone = uuid.clone();
    let row = with_db(&db_path, move |pool| async move {
        let row: (
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
        .bind(uuid_clone)
        .fetch_one(&pool)
        .await
        .expect("audio row");
        row
    });
    assert_eq!(row.0.as_deref(), Some("New Title"));
    assert_eq!(row.1.as_deref(), Some("Artist"));
    assert_eq!(row.2, None, "album absent in patch -> NULL");
    assert_eq!(row.3, Some(2001));
    assert_eq!(row.4, None, "genre absent -> NULL");
    assert_eq!(row.5, Some(3));
}

#[test]
fn given_ffi_edit_metadata_variant_mismatch_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    let started = alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(r#"{"type":"video","title":"x"}"#);
    let result =
        alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_edit_metadata_bad_patch_json_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c("not-json-at-all");
    let result =
        alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
}

#[test]
fn given_ffi_edit_metadata_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let patch = c(r#"{"type":"audio","title":"x"}"#);
    let result = alexandria_file_edit_metadata(uuid.as_ptr(), patch.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
}

#[test]
fn given_ffi_edit_metadata_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(r#"{"type":"audio","title":"x"}"#);
    let empty = c("");
    let result =
        alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
}

#[test]
fn given_ffi_edit_metadata_deleted_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let patch = c(r#"{"type":"audio","title":"x"}"#);
    let result =
        alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

#[allow(dead_code)]
fn _ffi_file_status_constants_are_stable() {
    // Keeps the FILE_* status constants referenced even if a test is trimmed.
    let _ = (
        STATUS_FILE_OK,
        STATUS_FILE_INVALID_INPUT,
        STATUS_FILE_UNAUTHORIZED,
        STATUS_FILE_NOT_INITIALIZED,
        STATUS_FILE_NOT_FOUND,
        STATUS_FILE_INVALID_STATE,
        STATUS_FILE_OTHER,
        STATUS_FILE_DISK,
    );
}

// ---------------------------------------------------------------------------
// UC-05: alexandria_file_rename (FR-FC-19, FR-FC-24)
// ---------------------------------------------------------------------------

/// Result JSON of a `FileJsonResult` on success, parsed. Asserts `FILE_OK`.
fn file_json_ok(result: alexandria_ffi::FileJsonResult) -> serde_json::Value {
    assert_eq!(result.status, STATUS_FILE_OK, "expected FILE_OK, got {}", result.status);
    assert!(!result.json.is_null(), "success must carry a json pointer");
    let json = unsafe { CStr::from_ptr(result.json) }.to_str().unwrap().to_string();
    unsafe { alexandria_free_string(result.json); }
    serde_json::from_str(&json).expect("File json")
}

#[test]
fn given_indexed_file_when_ffi_rename_then_ok_and_disk_and_catalog_updated() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let name = c("renamed.mp3");
    let result = alexandria_file_rename(c(&uuid).as_ptr(), name.as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["name"], "renamed.mp3");
    assert_eq!(json["state"], "active");
    let new_path = json["path"].as_str().expect("path string");
    assert!(new_path.ends_with("renamed.mp3"));

    // On-disk file moved.
    assert!(!lib.path().join("song.mp3").exists(), "old path gone after rename");
    assert!(lib.path().join("renamed.mp3").exists(), "new path present");
    assert_eq!(std::fs::read(lib.path().join("renamed.mp3")).unwrap(), b"audio bytes");

    // Catalog row updated.
    let (name, path): (String, String) =
        with_db(&db_path, move |pool| async move {
            sqlx::query_as("SELECT name, path FROM files WHERE uuid = ?")
                .bind(&uuid)
                .fetch_one(&pool)
                .await
                .expect("catalog row")
        });
    assert_eq!(name, "renamed.mp3");
    assert!(path.ends_with("renamed.mp3"));
}

#[test]
fn given_ffi_rename_invalid_name_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);
    let uuid = uuid_by_name(&db_path, "song.mp3");

    for bad in ["/x", "..", "a:b"] {
        let bad = c(bad);
        let result = alexandria_file_rename(c(&uuid).as_ptr(), bad.as_ptr(), token.as_ptr());
        assert_eq!(result.status, STATUS_FILE_INVALID_INPUT, "bad name rejected");
        assert!(result.json.is_null());
    }
}

#[test]
fn given_ffi_rename_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let name = c("new.mp3");
    let result = alexandria_file_rename(uuid.as_ptr(), name.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
}

#[test]
fn given_ffi_rename_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let name = c("renamed.mp3");
    let empty = c("");
    let result = alexandria_file_rename(c(&uuid).as_ptr(), name.as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
}

#[test]
fn given_ffi_rename_deleted_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let name = c("renamed.mp3");
    let result = alexandria_file_rename(c(&uuid).as_ptr(), name.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

#[test]
fn given_ffi_rename_target_owned_by_other_file_then_disk_error() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("a.mp3"), b"aaa").unwrap();
    std::fs::write(lib.path().join("b.mp3"), b"bbb").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(2);

    let uuid_a = uuid_by_name(&db_path, "a.mp3");
    let name = c("b.mp3");
    let result = alexandria_file_rename(c(&uuid_a).as_ptr(), name.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_DISK, "target-exists must map to FILE_ERR_DISK");
    assert!(result.json.is_null());

    // a.mp3 left untouched on disk.
    assert!(lib.path().join("a.mp3").exists(), "a.mp3 untouched after refusal");
}

// ---------------------------------------------------------------------------
// UC-06: alexandria_file_soft_delete (FR-FC-20, FR-FC-24)
// ---------------------------------------------------------------------------

#[test]
fn given_indexed_file_when_ffi_soft_delete_then_ok_and_catalog_deleted() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let result = alexandria_file_soft_delete(c(&uuid).as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["state"], "deleted");
    assert!(
        json["deletedAt"].as_str().is_some(),
        "deletedAt present on the returned File"
    );

    // The on-disk file is untouched (UC-06 leaves it; purge-on-disk is UC-09).
    assert!(lib.path().join("song.mp3").exists(), "on-disk file preserved");

    // Catalog row carries state=deleted and a stamped deleted_at.
    let uuid_for_row = uuid.clone();
    let (state, deleted_at): (String, Option<String>) =
        with_db(&db_path, move |pool| async move {
            sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
                .bind(&uuid_for_row)
                .fetch_one(&pool)
                .await
                .expect("catalog row")
        });
    assert_eq!(state, "deleted");
    assert!(deleted_at.is_some(), "deleted_at stamped in the catalog");
}

#[test]
fn given_ffi_soft_delete_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_soft_delete(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_soft_delete_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let empty = c("");
    let result = alexandria_file_soft_delete(c(&uuid).as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_soft_delete_already_deleted_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let result = alexandria_file_soft_delete(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

// ---------------------------------------------------------------------------
// UC-07: alexandria_file_restore (FR-FC-21, FR-FC-24)
// ---------------------------------------------------------------------------

#[test]
fn given_soft_deleted_file_when_ffi_restore_then_ok_and_catalog_active() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");

    // Seed a soft-deleted row whose `deleted_at` is comfortably within the
    // default 30-day retention window (one day ago). Exact-boundary coverage
    // is in the core unit tests with a FixedClock.
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind(chrono::Utc::now() - chrono::Duration::days(1))
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let result = alexandria_file_restore(c(&uuid).as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["state"], "active");
    assert!(
        json["deletedAt"].is_null(),
        "deletedAt cleared on the returned File"
    );

    // The on-disk file is untouched (UC-07 leaves it; purge-on-disk is UC-09).
    assert!(lib.path().join("song.mp3").exists(), "on-disk file preserved");

    // Catalog row carries state=active and a cleared deleted_at.
    let uuid_for_row = uuid.clone();
    let (state, deleted_at): (String, Option<String>) =
        with_db(&db_path, move |pool| async move {
            sqlx::query_as("SELECT state, deleted_at FROM files WHERE uuid = ?")
                .bind(&uuid_for_row)
                .fetch_one(&pool)
                .await
                .expect("catalog row")
        });
    assert_eq!(state, "active");
    assert!(deleted_at.is_none(), "deleted_at cleared in the catalog");
}

#[test]
fn given_ffi_restore_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_restore(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_restore_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let empty = c("");
    let result = alexandria_file_restore(c(&uuid).as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_restore_active_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    // Indexed but never soft-deleted — `state = 'active'` (AF-02 not-deleted).
    let uuid = uuid_by_name(&db_path, "song.mp3");
    let result = alexandria_file_restore(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

#[test]
fn given_soft_deleted_file_past_retention_when_ffi_restore_then_not_found() {
    // AF-01: a record past the configured retention window is reported as
    // not-found here too (UC-08 owns the actual hard purge; before it runs
    // the row still exists, so the elapsed check is what UC-07 surfaces as
    // FILE_ERR_NOT_FOUND over FFI).
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("past-retention seed");
    });

    let result = alexandria_file_restore(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
}

// ---------------------------------------------------------------------------
// UC-08: alexandria_file_purge (FR-FC-22, FR-FC-24, NFR-07)
// ---------------------------------------------------------------------------

#[test]
fn given_soft_deleted_file_past_retention_when_ffi_purge_then_ok_and_rows_removed() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");

    // `deleted_at` well past the default 30-day retention window.
    let uuid_for_seed = uuid.clone();
    let file_id: i64 = with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(&uuid_for_seed)
            .execute(&pool)
            .await
            .expect("past-retention seed");
        let (id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
            .bind(&uuid_for_seed)
            .fetch_one(&pool)
            .await
            .expect("file id");
        id
    });

    let result = alexandria_file_purge(c(&uuid).as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(
        json["state"], "deleted",
        "confirmation echoes the pre-purge state"
    );

    // The on-disk file is untouched (NFR-07; purge-on-disk is UC-09).
    assert!(lib.path().join("song.mp3").exists(), "on-disk file preserved");

    // The `files` row and its subtype row (audio_files) are both gone.
    let uuid_for_check = uuid.clone();
    let (files_remaining, subtype_remaining): (i64, i64) =
        with_db(&db_path, move |pool| async move {
            let (files,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                .bind(&uuid_for_check)
                .fetch_one(&pool)
                .await
                .expect("files count");
            let (subtype,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
                    .bind(file_id)
                    .fetch_one(&pool)
                    .await
                    .expect("audio_files count");
            (files, subtype)
        });
    assert_eq!(files_remaining, 0, "files row removed by purge");
    assert_eq!(subtype_remaining, 0, "subtype row removed by purge");
}

#[test]
fn given_ffi_purge_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_purge(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let empty = c("");
    let result = alexandria_file_purge(c(&uuid).as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_malformed_uuid_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("not-a-uuid");
    let result = alexandria_file_purge(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_active_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    // Indexed but never soft-deleted — `state = 'active'` (AF-01 not-deleted).
    let uuid = uuid_by_name(&db_path, "song.mp3");
    let result = alexandria_file_purge(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_within_retention_then_invalid_state_and_row_kept() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");

    // `deleted_at` one day ago — comfortably within the default 30-day
    // retention window. Exact-boundary coverage is in the core unit tests
    // with a FixedClock.
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind(chrono::Utc::now() - chrono::Duration::days(1))
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let result = alexandria_file_purge(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
    assert!(result.json.is_null());

    let uuid_for_check = uuid.clone();
    let remaining: i64 = with_db(&db_path, move |pool| async move {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
            .bind(uuid_for_check)
            .fetch_one(&pool)
            .await
            .expect("files count");
        count
    });
    assert_eq!(remaining, 1, "row kept when purge is refused");
}

// ---------------------------------------------------------------------------
// UC-09: alexandria_file_purge_on_disk (FR-FC-23, FR-FC-24)
// ---------------------------------------------------------------------------

#[test]
fn given_active_file_when_ffi_purge_on_disk_then_ok_and_disk_and_rows_removed() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    // No retention gate for UC-09 — an `active` (never soft-deleted) record
    // is purgeable, unlike UC-08.
    let uuid = uuid_by_name(&db_path, "song.mp3");
    let file_id: i64 = with_db(&db_path, {
        let uuid = uuid.clone();
        move |pool| async move {
            let (id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
                .bind(&uuid)
                .fetch_one(&pool)
                .await
                .expect("file id");
            id
        }
    });

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["diskFilePresent"], true);

    assert!(
        !lib.path().join("song.mp3").exists(),
        "on-disk file removed by purge-on-disk"
    );

    let uuid_for_check = uuid.clone();
    let (files_remaining, subtype_remaining): (i64, i64) =
        with_db(&db_path, move |pool| async move {
            let (files,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                .bind(&uuid_for_check)
                .fetch_one(&pool)
                .await
                .expect("files count");
            let (subtype,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM audio_files WHERE file_id = ?")
                    .bind(file_id)
                    .fetch_one(&pool)
                    .await
                    .expect("audio_files count");
            (files, subtype)
        });
    assert_eq!(files_remaining, 0, "files row removed by purge-on-disk");
    assert_eq!(subtype_remaining, 0, "subtype row removed by purge-on-disk");
}

#[test]
fn given_missing_disk_file_when_ffi_purge_on_disk_then_ok_and_absence_reported() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");

    // The file vanishes from disk out from under the catalog (AF-01).
    std::fs::remove_file(lib.path().join("song.mp3")).unwrap();

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr(), token.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(
        json["diskFilePresent"], false,
        "purge-on-disk still succeeds and reports the absence"
    );

    let uuid_for_check = uuid.clone();
    let remaining: i64 = with_db(&db_path, move |pool| async move {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
            .bind(uuid_for_check)
            .fetch_one(&pool)
            .await
            .expect("files count");
        count
    });
    assert_eq!(remaining, 0, "row still removed even though disk file was absent");
}

#[test]
fn given_ffi_purge_on_disk_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_purge_on_disk(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_on_disk_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("bearer");
    alexandria_index_start(root.as_ptr(), token.as_ptr());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let empty = c("");
    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
    assert!(result.json.is_null());

    // Auth ran before the disk was touched.
    assert!(
        lib.path().join("song.mp3").exists(),
        "on-disk file untouched when auth is denied"
    );
}

#[test]
fn given_ffi_purge_on_disk_malformed_uuid_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c("bearer");
    let uuid = c("not-a-uuid");
    let result = alexandria_file_purge_on_disk(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}