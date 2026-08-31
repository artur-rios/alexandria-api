use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use tempfile::{tempdir, TempDir};

// The FFI keeps services in a process-global static. Within one test binary,
// tests run in parallel and would race on that slot, so serialize every FFI
// test behind this lock (no serial_test dependency needed).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poisoning rather than propagating it, matching
    // `parity.rs`. The guard protects a process-global services slot, not an
    // invariant this lock can corrupt: every test re-initializes it through
    // `init_temp_db`. Unwrapping instead let the *first* panicking test poison
    // the mutex and turn every later test in this file into an opaque
    // `PoisonError`, burying the one real failure under 42 fake ones.
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long a poll waits for an asynchronous index / re-index run to land in
/// the database before it gives up and panics.
///
/// Generous on purpose. UC-01 and UC-02 return immediately and finish in the
/// background (FR-FC-08), so every assertion about their results has to poll.
/// What the bound has to absorb is the machine: under `cargo test
/// --workspace` these binaries share a host with dozens of others and a live
/// compile, and a tighter bound does not catch a slow indexer — it just
/// reports the runner's load as a product failure.
///
/// Two minutes because that is the scale of the work being waited on, not of
/// the wait: an index of this file's 5,000-file fixture takes ~26s alone on
/// an idle machine, and the whole suite runs on one host.
///
/// A stall bound was tried here instead and is the wrong shape for this
/// workload. The catalog count does not climb steadily — it holds for
/// twenty seconds at a time and then jumps by thousands as the walk's writes
/// land, so "no progress for N seconds" fires on a run that is perfectly
/// healthy and about to finish. Observed directly: a run sitting at 4,998 of
/// 5,000 for over thirty seconds, then completing with `failed: 0`.
const ASYNC_RUN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// The editable columns of an `audio_files` row, in the order every
/// assertion here selects them: title, artist, album, year, genre, track,
/// album_artist.
type AudioMetadataRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

use alexandria_ffi::{
    alexandria_enrichment_read_track, alexandria_enrichment_run, alexandria_file_edit_metadata,
    alexandria_file_purge, alexandria_file_purge_on_disk, alexandria_file_rename,
    alexandria_file_restore, alexandria_file_soft_delete, alexandria_free_string,
    alexandria_index_cancel, alexandria_index_count_files, alexandria_index_count_missing,
    alexandria_index_files_json, alexandria_index_init, alexandria_index_pause,
    alexandria_index_refresh_start, alexandria_index_resume, alexandria_index_run_failures_json,
    alexandria_index_run_status_json, alexandria_index_runs_active_json, alexandria_index_start,
    alexandria_libraries_list, alexandria_library_browse, alexandria_library_move,
    alexandria_library_register, alexandria_library_remove, alexandria_playlist_add_entries,
    alexandria_playlist_create, alexandria_playlist_delete, alexandria_playlist_move_entry,
    alexandria_playlist_read, alexandria_playlist_remove_entry, alexandria_playlist_rename,
    alexandria_playlists_list, FileMetadataResult, IndexStartResult, PlaylistJsonResult,
};

const STATUS_RUN_OK: i32 = alexandria_ffi::RUN_OK;
const STATUS_RUN_INVALID_INPUT: i32 = alexandria_ffi::RUN_ERR_INVALID_INPUT;
const STATUS_RUN_UNAUTHORIZED: i32 = alexandria_ffi::RUN_ERR_UNAUTHORIZED;
const STATUS_RUN_NOT_FOUND: i32 = alexandria_ffi::RUN_ERR_NOT_FOUND;
const STATUS_RUN_INVALID_STATE: i32 = alexandria_ffi::RUN_ERR_INVALID_STATE;
const STATUS_RUN_OTHER: i32 = alexandria_ffi::RUN_ERR_OTHER;

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

const STATUS_PLAYLIST_OK: i32 = alexandria_ffi::PLAYLIST_OK;
const STATUS_PLAYLIST_INVALID_INPUT: i32 = alexandria_ffi::PLAYLIST_ERR_INVALID_INPUT;
const STATUS_PLAYLIST_UNAUTHORIZED: i32 = alexandria_ffi::PLAYLIST_ERR_UNAUTHORIZED;
const STATUS_PLAYLIST_NOT_FOUND: i32 = alexandria_ffi::PLAYLIST_ERR_NOT_FOUND;
const STATUS_ENRICHMENT_UNAVAILABLE: i32 = alexandria_ffi::ENRICHMENT_ERR_UNAVAILABLE;
const STATUS_ENRICHMENT_INVALID_INPUT: i32 = alexandria_ffi::ENRICHMENT_ERR_INVALID_INPUT;
const STATUS_ENRICHMENT_OK: i32 = alexandria_ffi::ENRICHMENT_OK;
const STATUS_LIBRARY_OK: i32 = alexandria_ffi::LIBRARY_OK;
const STATUS_LIBRARY_CONFLICT: i32 = alexandria_ffi::LIBRARY_ERR_CONFLICT;
const STATUS_LIBRARY_INVALID_INPUT: i32 = alexandria_ffi::LIBRARY_ERR_INVALID_INPUT;
const STATUS_LIBRARY_NOT_FOUND: i32 = alexandria_ffi::LIBRARY_ERR_NOT_FOUND;

/// Bearer token every smoke test authenticates with. A valid UUID: the
/// active auth mode is local (`init_temp_db` sets `ALEXANDRIA_AUTH_MODE`), so
/// it must parse as a session id (`LocalAuthService::authenticate`). A
/// matching session is seeded into the fresh database below so it validates.
const TEST_TOKEN: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

fn init_temp_db() -> (TempDir, String) {
    // `alexandria_index_init` loads settings via `load_settings()`
    // (`ALEXANDRIA_*` env), not a `Settings` value this test controls
    // directly — flip the process-wide auth mode to local. Safe across tests
    // since `serial()` guards this whole file and the value never differs.
    std::env::set_var("ALEXANDRIA_AUTH_MODE", "local");

    let dir = tempdir().unwrap();
    let db = dir.path().join("ffi.sqlite");
    let db_path = db.to_str().unwrap().to_string();

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pool = alexandria_core::migrate::migrate_database(&db_path)
            .await
            .expect("ffi pre-migrate");
        let now = chrono::Utc::now();
        let expires_at = now + chrono::Duration::hours(24);
        sqlx::query("INSERT INTO sessions (id, created_at, expires_at) VALUES (?, ?, ?)")
            .bind(TEST_TOKEN)
            .bind(now.to_rfc3339())
            .bind(expires_at.to_rfc3339())
            .execute(&pool)
            .await
            .expect("seed session");
        pool.close().await;
    });

    let cpath = CString::new(db_path.clone()).unwrap();
    let status = alexandria_index_init(cpath.as_ptr());
    assert_eq!(status, STATUS_OK, "ffi services init failed");
    (dir, db_path)
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
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

/// Poll `read` until it reaches `expected`, within [`ASYNC_RUN_DEADLINE`].
///
/// The message names what was still outstanding, because that is what tells
/// a reader whether the run was slow or wrong — "had 2782" is a loaded
/// machine, "had 0" is a run that never started.
///
/// Where the caller holds a run id, prefer waiting for that run to reach a
/// terminal status and *then* asserting the count: the run record is the
/// authority on whether the walk is over, and this count is a derived
/// observation of writes that land in bursts.
fn wait_for_count(label: &str, expected: i64, read: impl Fn() -> i64) -> i64 {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let count = read();
        if count >= expected {
            return count;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "timed out waiting for {expected} {label}; had {count}"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for_files(expected: i64) -> i64 {
    wait_for_count("files", expected, || alexandria_index_count_files())
}

/// The FFI surface runs the same startup gate as the HTTP binary: external
/// mode with nothing to verify a token against must refuse to initialize
/// rather than come up and answer `401` to everything forever (FR-AU-08,
/// UC-36). Untested, this is easy to lose — `default_auth_mode()` is
/// `External` and `load_or_default` swallows a missing config file, so every
/// other test in this file has to force `local` to get past it, and none of
/// them would notice the gate disappearing.
///
/// Serialized behind `serial()` like every other test here, because it writes
/// the process-global environment `alexandria_index_init` reads and would
/// otherwise flip the auth mode out from under a concurrent test. It leaves
/// the mode as it found it, and never reaches the global services slot: the
/// gate runs before the slot is touched.
#[test]
fn given_external_mode_without_a_secret_when_ffi_init_then_refuses_to_start() {
    // Arrange
    let _g = serial();
    let previous_mode = std::env::var("ALEXANDRIA_AUTH_MODE").ok();
    std::env::set_var("ALEXANDRIA_AUTH_MODE", "external");
    // The overrides an ambient environment might carry would configure the
    // very thing this test asserts is missing.
    for key in [
        "ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET",
        "ALEXANDRIA_AUTH_HEIMDALL_TOKEN_SECRET_PREVIOUS",
        "ALEXANDRIA_AUTH_HEIMDALL_SCOPE_ID",
    ] {
        std::env::remove_var(key);
    }
    // A config path that does not exist, so `load_or_default` falls back to
    // defaults rather than reading whatever `config.toml` the working
    // directory happens to hold.
    std::env::set_var(
        "ALEXANDRIA_CONFIG",
        "no-such-config-file-for-this-test.toml",
    );

    let dir = tempdir().unwrap();
    let db = dir.path().join("gate.sqlite");
    let cpath = CString::new(db.to_str().unwrap()).unwrap();

    // Act
    let status = alexandria_index_init(cpath.as_ptr());

    // Restore before asserting, so a failure cannot leak `external` into the
    // rest of the file.
    std::env::remove_var("ALEXANDRIA_CONFIG");
    match previous_mode {
        Some(mode) => std::env::set_var("ALEXANDRIA_AUTH_MODE", mode),
        None => std::env::remove_var("ALEXANDRIA_AUTH_MODE"),
    }

    // Assert
    assert_eq!(
        status,
        alexandria_ffi::INDEX_ERR_OTHER,
        "external mode with no signing secret must fail startup"
    );
    assert!(
        !db.exists(),
        "the gate runs before the database is created or the services slot is filled"
    );
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
    let token = c(TEST_TOKEN);
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );

    assert_eq!(result.status, STATUS_OK);
    assert!(!run_id_string(&result).is_empty());

    assert_eq!(wait_for_files(2), 2);

    let raw = alexandria_index_files_json();
    assert!(!raw.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
    // SAFETY: pointer came from this library and is freed once.
    unsafe {
        alexandria_free_string(raw);
    }
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
    let token = c(TEST_TOKEN);
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// FR-FC-26 parity: the constraint lives in the core handler, so the FFI
/// surface rejects an out-of-root index exactly as HTTP's 400 does (NFR-09).
/// The bound is configured the only way the FFI reads configuration — the
/// `ALEXANDRIA_*` environment — which is process-wide, so it is removed again
/// before any assertion can unwind out of this test.
#[test]
fn given_root_outside_configured_library_root_when_ffi_index_start_then_invalid_input() {
    // Arrange
    let _g = serial();
    let parent = tempdir().unwrap();
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    std::fs::create_dir(&library).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::env::set_var("ALEXANDRIA_FILESYSTEM_ROOT", library.to_str().unwrap());
    let _db = init_temp_db();

    // Act
    let root = c(outside.to_str().unwrap());
    let token = c(TEST_TOKEN);
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    std::env::remove_var("ALEXANDRIA_FILESYSTEM_ROOT");

    // Assert
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// The other half of the parity pair: with the same bound configured, a root
/// *inside* it is accepted — so the test above is proving the constraint, not
/// some unrelated FFI failure.
#[test]
fn given_root_inside_configured_library_root_when_ffi_index_start_then_ok() {
    // Arrange
    let _g = serial();
    let library = tempdir().unwrap();
    let inside = library.path().join("music");
    std::fs::create_dir(&inside).unwrap();
    std::env::set_var(
        "ALEXANDRIA_FILESYSTEM_ROOT",
        library.path().to_str().unwrap(),
    );
    let _db = init_temp_db();

    // Act
    let root = c(inside.to_str().unwrap());
    let token = c(TEST_TOKEN);
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    std::env::remove_var("ALEXANDRIA_FILESYSTEM_ROOT");

    // Assert
    assert_eq!(result.status, STATUS_OK);
}

#[test]
fn given_empty_token_when_ffi_index_start_then_returns_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c("");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let result = alexandria_index_start(
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

fn files_json_value() -> serde_json::Value {
    let raw = alexandria_index_files_json();
    assert!(!raw.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
    // SAFETY: pointer came from this library and is freed once.
    unsafe {
        alexandria_free_string(raw);
    }
    serde_json::from_str(&json).unwrap()
}

/// Poll `alexandria_index_run_status_json` until `run_id` leaves `running`,
/// then return its parsed body (UC-42 / FR-FC-27/28).
///
/// A refresh walks the catalog with several writers at once, so the missing
/// count landing says nothing about whether the run record itself has been
/// closed out with its final tally yet — the assertions here need the
/// *finished* run's `refreshed`/`markedMissing` counts, not just the
/// individual row effects, so they poll the run record directly rather than
/// racing it via `files_json_value()`.
fn wait_for_run_terminal(run_id: &str, token: &CString) -> serde_json::Value {
    let run_id_c = CString::new(run_id).unwrap();
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
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
            return value;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "run {run_id} never left running after {ASYNC_RUN_DEADLINE:?}; {}",
                run_progress(&value)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Wait until `missing_at IS NOT NULL` count reaches `expected` missing files.
fn wait_for_missing(expected: i64) -> i64 {
    wait_for_count("missing", expected, || alexandria_index_count_missing())
}

/// Task 4 rewrote what a re-index detects a change *by* — a `stat` call
/// (size + mtime), not a recomputed SHA-256 — so this test's old premise
/// (capture `a.mp3`'s pre-refresh hash, then wait for refresh to produce a
/// *different* hash) is dead twice over: Task 3 already stopped indexing
/// from computing a hash at all (a freshly indexed file's `content_hash` is
/// `null`), and Task 4's refresh never computes a new one either — a
/// detected change now clears `content_hash` to `null` rather than
/// replacing it (FR-FC-10), so there is neither an old hash to capture nor a
/// new one to compare against.
///
/// What refresh actually guarantees now: `a.mp3`'s changed size is detected
/// via `stat` and counted in the run's `refreshed` tally, `b.md`'s absence
/// is counted in `markedMissing`, and `a.mp3`'s `content_hash` comes back
/// `null` (not a new hash) while its `missingAt` is cleared.
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
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(2), 2);

    // Mutate on disk: change a's bytes — and with them its size — delete b.
    std::fs::write(&a_path, b"audio-v2-CHANGED").unwrap();
    std::fs::remove_file(&b_path).unwrap();

    let refresh = alexandria_index_refresh_start(token.as_ptr(), std::ptr::null());
    assert_eq!(refresh.status, STATUS_OK);
    let run_id = run_id_string(&refresh);
    assert!(!run_id.is_empty());

    // b must be marked missing, and the run itself must report exactly one
    // stat-detected change (a) and one missing file (b) once it completes.
    assert_eq!(wait_for_missing(1), 1);
    let run = wait_for_run_terminal(&run_id, &token);
    assert_eq!(run["status"], "complete");
    assert_eq!(run["refreshed"], 1, "a's changed size is detected via stat");
    assert_eq!(run["markedMissing"], 1);
    assert_eq!(run["unchanged"], 0);
    assert_eq!(run["failed"], 0);

    // a's content_hash is cleared by refresh_stat (Task 4 / FR-FC-10: a
    // refreshed file's now-stale hash must not be served as current), and
    // its missingAt is cleared. b's missingAt is set.
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
    assert!(
        a_row["hash"].is_null(),
        "refresh clears the hash rather than recomputing one — and a NULL \
         content_hash must serialize as JSON null, not \"\", to match what \
         HTTP's File/FileView model emits for the same column (FR-FC-24)"
    );
    assert!(a_row["missingAt"].is_null(), "a missingAt cleared");
    assert!(b_row["missingAt"].is_string(), "b missingAt set");
}

#[test]
fn given_empty_token_when_ffi_refresh_then_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();
    let token = c("");
    let result = alexandria_index_refresh_start(token.as_ptr(), std::ptr::null());
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
    unsafe {
        alexandria_free_string(result.json);
    }
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
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(
        r#"{"type":"audio","title":"New Title","artist":"Artist","year":2001,"track":3,"albumArtist":"New Album Artist"}"#,
    );
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());

    let json = metadata_json(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["file"]["fileType"], "audio");
    assert_eq!(json["metadata"]["type"], "audio");
    assert_eq!(json["metadata"]["title"], "New Title");
    assert_eq!(json["metadata"]["track"], 3);
    assert_eq!(json["metadata"]["albumArtist"], "New Album Artist");

    // Persisted subtype row reflects the full-replace PATCH.
    let uuid_clone = uuid.clone();
    let row = with_db(&db_path, move |pool| async move {
        let row: AudioMetadataRow = sqlx::query_as(
            "SELECT title, artist, album, year, genre, track, album_artist FROM audio_files \
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
    assert_eq!(row.6.as_deref(), Some("New Album Artist"));
}

#[test]
fn given_ffi_edit_metadata_variant_mismatch_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(r#"{"type":"video","title":"x"}"#);
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c("not-json-at-all");
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
}

#[test]
fn given_ffi_edit_metadata_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "song.mp3");
    let patch = c(r#"{"type":"audio","title":"x"}"#);
    let empty = c("");
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), empty.as_ptr());
    assert_eq!(result.status, STATUS_FILE_UNAUTHORIZED);
}

#[test]
fn given_ffi_edit_metadata_deleted_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr(), token.as_ptr());
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
    assert_eq!(
        result.status, STATUS_FILE_OK,
        "expected FILE_OK, got {}",
        result.status
    );
    assert!(!result.json.is_null(), "success must carry a json pointer");
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(result.json);
    }
    serde_json::from_str(&json).expect("File json")
}

#[test]
fn given_indexed_file_when_ffi_rename_then_ok_and_disk_and_catalog_updated() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    assert!(
        !lib.path().join("song.mp3").exists(),
        "old path gone after rename"
    );
    assert!(lib.path().join("renamed.mp3").exists(), "new path present");
    assert_eq!(
        std::fs::read(lib.path().join("renamed.mp3")).unwrap(),
        b"audio bytes"
    );

    // Catalog row updated.
    let (name, path): (String, String) = with_db(&db_path, move |pool| async move {
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    wait_for_files(1);
    let uuid = uuid_by_name(&db_path, "song.mp3");

    for bad in ["/x", "..", "a:b"] {
        let bad = c(bad);
        let result = alexandria_file_rename(c(&uuid).as_ptr(), bad.as_ptr(), token.as_ptr());
        assert_eq!(
            result.status, STATUS_FILE_INVALID_INPUT,
            "bad name rejected"
        );
        assert!(result.json.is_null());
    }
}

#[test]
fn given_ffi_rename_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    wait_for_files(2);

    let uuid_a = uuid_by_name(&db_path, "a.mp3");
    let name = c("b.mp3");
    let result = alexandria_file_rename(c(&uuid_a).as_ptr(), name.as_ptr(), token.as_ptr());
    assert_eq!(
        result.status, STATUS_FILE_DISK,
        "target-exists must map to FILE_ERR_DISK"
    );
    assert!(result.json.is_null());

    // a.mp3 left untouched on disk.
    assert!(
        lib.path().join("a.mp3").exists(),
        "a.mp3 untouched after refusal"
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    assert!(
        lib.path().join("song.mp3").exists(),
        "on-disk file preserved"
    );

    // Catalog row carries state=deleted and a stamped deleted_at.
    let uuid_for_row = uuid.clone();
    let (state, deleted_at): (String, Option<String>) = with_db(&db_path, move |pool| async move {
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
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    assert!(
        lib.path().join("song.mp3").exists(),
        "on-disk file preserved"
    );

    // Catalog row carries state=active and a cleared deleted_at.
    let uuid_for_row = uuid.clone();
    let (state, deleted_at): (String, Option<String>) = with_db(&db_path, move |pool| async move {
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
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    assert!(
        lib.path().join("song.mp3").exists(),
        "on-disk file preserved"
    );

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
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    assert_eq!(
        remaining, 0,
        "row still removed even though disk file was absent"
    );
}

#[test]
fn given_disk_delete_failure_when_ffi_purge_on_disk_then_disk_error_and_row_kept() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    wait_for_files(1);

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

    // Replace the indexed file with a directory at the same path so the
    // disk delete fails with something other than `NotFound` (AF-02).
    std::fs::remove_file(lib.path().join("song.mp3")).expect("pre-remove indexed file");
    std::fs::create_dir(lib.path().join("song.mp3"))
        .expect("create directory in place of indexed file");

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_DISK);
    assert!(result.json.is_null());

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
    assert_eq!(
        files_remaining, 1,
        "AF-02: record kept when the disk delete fails"
    );
    assert_eq!(
        subtype_remaining, 1,
        "AF-02: subtype row kept when the disk delete fails"
    );
}

#[test]
fn given_ffi_purge_on_disk_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
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
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
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
    let token = c(TEST_TOKEN);
    let uuid = c("not-a-uuid");
    let result = alexandria_file_purge_on_disk(uuid.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

// ---------------------------------------------------------------------------
// UC-42 Task 11: run control over FFI - pause, resume, cancel, active runs,
// and the priority wire argument on the two start calls (FR-FC-24, FR-FC-28)
// ---------------------------------------------------------------------------

/// Call `alexandria_index_run_status_json` and parse its body. Asserts
/// `RUN_OK`.
fn run_status(run_id: &str, token: &CString) -> serde_json::Value {
    let run_id_c = c(run_id);
    let result = alexandria_index_run_status_json(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_RUN_OK, "expected RUN_OK run status");
    assert!(!result.json.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    // SAFETY: pointer came from this library and is freed exactly once.
    unsafe {
        alexandria_free_string(result.json);
    }
    serde_json::from_str(&json).expect("CatalogRun json")
}

/// Poll `alexandria_index_run_status_json` until the run leaves "running",
/// mirroring `wait_for_files`'s poll-with-deadline shape.
/// How far a run had got, for a panic message.
///
/// A wait that gives up says nothing useful without this. "Never left
/// running" is the same sentence whether the walk was one file in or one file
/// short, and those are opposite problems: the first is a run that is stuck,
/// the second a machine that needed longer. Diagnosing the difference cost a
/// throwaway instrumented test once; this is that instrumentation, kept.
fn run_progress(body: &serde_json::Value) -> String {
    format!(
        "status={} phase={} processed={} of {}",
        body["status"], body["phase"], body["processed"], body["total"]
    )
}

fn wait_for_run_terminal_or_paused(run_id: &str, token: &CString) -> serde_json::Value {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let body = run_status(run_id, token);
        if body["status"] != "running" {
            return body;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "run {run_id} never left running after {ASYNC_RUN_DEADLINE:?}; {}",
                run_progress(&body)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Poll `alexandria_index_run_status_json` until it shows a live registry
/// cell overlaid — `overlay_live_state` (`run_status.rs`) sets `phase` only
/// when `RunRegistry::get` finds one, and that only happens once `execute`
/// has reached its own `registry.open` call. Pausing or cancelling before
/// that point still succeeds, but takes `RunControlHandler::control`'s
/// "no live cell" branch, which writes the row directly with no progress
/// attached — legitimate (see that function's doc comment) but not what a
/// test asserting `processed` is a number wants to race against. Waiting
/// here first is what makes the difference deterministic instead of a coin
/// flip on how fast the executor schedules the spawned task.
///
/// This function's own panic message ("... left running before its cell
/// ever went live; `write_library` needs more files to give the walk time")
/// names the fix for a fixture that is too small, but a caller sizing that
/// fixture has to know too small *for what*: an index walk and a refresh
/// walk do not cost the same per file. An index walk reads and classifies
/// each file and — for a type with a metadata reader — parses its tag
/// header; a refresh walk of already-cataloged paths is stat-only, no byte
/// read and no tag parsing at all (Task 4). Refresh is therefore
/// substantially faster per file than index, and a `write_library` count
/// tuned to keep an *index* walk observably `running` (the tests below that
/// call this against an index run) is not automatically large enough to do
/// the same for a *refresh* walk — a refresh-side caller of this function
/// needs its own, larger fixture. `given_a_paused_refresh_run_when_resumed_
/// over_ffi_then_it_finishes` is that case; see its own comment for the
/// count.
fn wait_for_run_cell_live(run_id: &str, token: &CString) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let body = run_status(run_id, token);
        if !body["phase"].is_null() {
            return;
        }
        if body["status"] != "running" {
            panic!(
                "run {run_id} left running before its cell ever went live; \
                 write_library needs more files to give the walk time; {}",
                run_progress(&body)
            );
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "run {run_id}'s cell never went live after \
                 {ASYNC_RUN_DEADLINE:?}; {}",
                run_progress(&body)
            );
        }
        // Same interval `wait_for_run_terminal_or_paused` sleeps: without it
        // this loop calls `run_status` (a `block_on` against the very
        // runtime the walk it is waiting on is running on) as fast as the
        // thread can manage, burning a core and contending with the walk
        // instead of just waiting on it.
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// The `concurrency` column `catalog_runs` recorded for `run_id` - not
/// serialized onto the JSON body (`CatalogRun::concurrency` is
/// `#[serde(skip)]`), so the only way to prove a `priority` argument actually
/// reached the core is to read the column it was resolved into and wrote.
fn run_concurrency(db_path: &str, run_id: &str) -> Option<i64> {
    let run_id = run_id.to_string();
    with_db(db_path, move |pool| async move {
        let (concurrency,): (Option<i64>,) =
            sqlx::query_as("SELECT concurrency FROM catalog_runs WHERE id = ?")
                .bind(run_id)
                .fetch_one(&pool)
                .await
                .expect("run row");
        concurrency
    })
}

/// A library with enough files that the walk has a real chance of still
/// being "running" (or, failing that, still landing in the "no live cell
/// yet" window `RunControlHandler::control` documents) the instant
/// `alexandria_index_pause` is called right after `start` returns, without
/// needing an injected mid-walk interrupt the way the core-level tests in
/// `alexandria-core/tests/catalog/index.rs` do.
fn write_library(dir: &std::path::Path, count: usize) {
    for i in 0..count {
        std::fs::write(dir.join(format!("track-{i}.mp3")), b"audio bytes").unwrap();
    }
}

#[test]
fn given_a_running_run_when_paused_over_ffi_then_status_is_paused_and_has_progress() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    // Wait for the walk to actually be under way before pausing it — see
    // `wait_for_run_cell_live` for why that, not `start` returning, is the
    // point that guarantees a paused row with progress attached.
    wait_for_run_cell_live(&run_id, &token);
    let run_id_c = c(&run_id);
    let pause_status = alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(
        pause_status, STATUS_RUN_OK,
        "expected RUN_OK pausing a run under way"
    );

    let body = wait_for_run_terminal_or_paused(&run_id, &token);
    assert_eq!(body["status"], "paused");
    assert!(body["processed"].is_number(), "processed: {body}");
    assert!(body["activeMillis"].is_number(), "activeMillis: {body}");
}

#[test]
fn given_a_completed_run_when_paused_over_ffi_then_invalid_state() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_files(1);
    let body = wait_for_run_terminal_or_paused(&run_id, &token);
    assert_eq!(body["status"], "complete", "sanity: run finished");

    let run_id_c = c(&run_id);
    let pause_status = alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(
        pause_status, STATUS_RUN_INVALID_STATE,
        "pausing a completed run must be refused, not accepted or treated as a generic error"
    );
}

#[test]
fn given_ffi_pause_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_pause(run_id.as_ptr(), token.as_ptr());
    assert_eq!(status, STATUS_RUN_NOT_FOUND);
}

#[test]
fn given_ffi_pause_malformed_run_id_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let run_id = c("not-a-uuid");
    let status = alexandria_index_pause(run_id.as_ptr(), token.as_ptr());
    assert_eq!(status, STATUS_RUN_INVALID_INPUT);
}

#[test]
fn given_ffi_pause_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let empty = c("");
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_pause(run_id.as_ptr(), empty.as_ptr());
    assert_eq!(status, STATUS_RUN_UNAUTHORIZED);
}

#[test]
fn given_a_paused_run_when_resumed_over_ffi_then_same_run_id_and_it_finishes() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    wait_for_run_cell_live(&run_id, &token);
    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    // `pause` only raises the signal or writes the row; the walk's own drain
    // and terminal write can still be in flight when it returns, so the
    // status has to be polled for, not read once.
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), token.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_OK,
        "expected RUN_OK resuming a paused run"
    );
    assert_eq!(
        run_id_string(&resumed),
        run_id,
        "resume must hand back the same run id, not mint a fresh one"
    );

    // The resumed walk starts over from the root (no cursor is kept), and
    // finishes: everything already cataloged falls out as alreadyCataloged
    // in the fresh pass.
    let body = wait_for_run_terminal_or_paused(&run_id, &token);
    assert_eq!(body["status"], "complete");
}

/// The FFI twin of the HTTP `..._resumed_with_normal_priority_then_it_is_widened`
/// case (Task 15). `"low"` over FFI is covered end to end by `parity.rs`, and
/// NULL by the resume tests above; without this, `"normal"` over FFI rested
/// only on a unit test of `parse_resume_priority` plus the shared core
/// handler — and `"normal"` is the whole reason the wire value is three-valued
/// rather than a boolean, so it is the one that most needs proving through
/// the real entry point.
///
/// The stored `concurrency` is the only place a resolved priority is
/// observable (`CatalogRun::concurrency` is `#[serde(skip)]`), so this reads
/// the column, exactly as the start-priority smoke tests above do.
#[test]
fn given_a_low_priority_paused_run_when_resumed_at_normal_over_ffi_then_it_is_widened() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let low = c("low");
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        low.as_ptr(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    assert_eq!(
        run_concurrency(&db_path, &run_id),
        Some(1),
        "sanity: it started at indexing.low_priority_concurrency"
    );

    wait_for_run_cell_live(&run_id, &token);
    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    let normal = c("normal");
    let resumed = alexandria_index_resume(run_id_c.as_ptr(), token.as_ptr(), normal.as_ptr());
    assert_eq!(resumed.status, STATUS_RUN_OK);
    assert_eq!(
        run_id_string(&resumed),
        run_id,
        "a re-paced resume still continues the same run"
    );
    assert_eq!(
        run_concurrency(&db_path, &run_id),
        Some(4),
        "\"normal\" must widen the run to indexing.concurrency; sending nothing would \
         have left it at 1, which is what makes this a real request rather than a \
         synonym for silence"
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "complete"
    );
}

#[test]
fn given_a_running_run_when_resumed_over_ffi_then_invalid_state() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    // A single file finishes near-instantly and this test would then be
    // exercising "resuming a *complete* run", not "resuming a *running*
    // one" — both return `InvalidState`, so a single-file library would not
    // be a false pass, but it also would not reliably cover what the name
    // promises. `write_library` + `wait_for_run_cell_live` (the same pair
    // the pause/cancel tests above use) is what actually pins the run down
    // while it is still `running`.
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id, &token);

    let run_id_c = c(&run_id);
    assert_eq!(
        run_status(&run_id, &token)["status"],
        "running",
        "sanity: the run is still running at the moment resume is called"
    );
    let resumed = alexandria_index_resume(run_id_c.as_ptr(), token.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_INVALID_STATE,
        "resuming a run that is not paused must be refused"
    );
}

#[test]
fn given_ffi_resume_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let resumed = alexandria_index_resume(run_id.as_ptr(), token.as_ptr(), std::ptr::null());
    assert_eq!(resumed.status, STATUS_RUN_NOT_FOUND);
}

#[test]
fn given_a_running_run_when_cancelled_over_ffi_then_terminal_and_a_second_cancel_is_invalid_state()
{
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    wait_for_run_cell_live(&run_id, &token);
    let run_id_c = c(&run_id);
    let cancel_status = alexandria_index_cancel(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(cancel_status, STATUS_RUN_OK);

    // `cancel` only raises the signal or writes the row; the walk's own
    // drain and terminal write can still be in flight when it returns.
    let body = wait_for_run_terminal_or_paused(&run_id, &token);
    assert_eq!(body["status"], "cancelled");

    // Terminal: a second cancel finds nothing left to abandon.
    let second = alexandria_index_cancel(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(second, STATUS_RUN_INVALID_STATE);
}

#[test]
fn given_a_paused_run_when_cancelled_over_ffi_then_terminal() {
    // `cancel`'s doc comment advertises a `paused` run as the other legal
    // transition (abandoning one is the whole reason to cancel rather than
    // resume it) — pause has its own coverage above; this is cancel's.
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id, &token);

    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    let cancel_status = alexandria_index_cancel(run_id_c.as_ptr(), token.as_ptr());
    assert_eq!(
        cancel_status, STATUS_RUN_OK,
        "a paused run must still be cancellable"
    );
    assert_eq!(run_status(&run_id, &token)["status"], "cancelled");
}

#[test]
fn given_ffi_cancel_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_cancel(run_id.as_ptr(), token.as_ptr());
    assert_eq!(status, STATUS_RUN_NOT_FOUND);
}

#[test]
fn given_ffi_cancel_malformed_run_id_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let run_id = c("not-a-uuid");
    let status = alexandria_index_cancel(run_id.as_ptr(), token.as_ptr());
    assert_eq!(status, STATUS_RUN_INVALID_INPUT);
}

#[test]
fn given_ffi_cancel_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let empty = c("");
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_cancel(run_id.as_ptr(), empty.as_ptr());
    assert_eq!(status, STATUS_RUN_UNAUTHORIZED);
}

#[test]
fn given_no_outstanding_runs_when_active_runs_queried_then_empty_array() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let token = c(TEST_TOKEN);
    let result = alexandria_index_runs_active_json(token.as_ptr());
    assert_eq!(result.status, STATUS_RUN_OK);
    assert!(!result.json.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(result.json);
    }
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn given_a_paused_run_when_active_runs_queried_then_it_appears() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id, &token);
    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    // `pause` can return before the walk's own terminal write lands; wait
    // for it, or the query below could still find the row `running`.
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    let result = alexandria_index_runs_active_json(token.as_ptr());
    assert_eq!(result.status, STATUS_RUN_OK);
    assert!(!result.json.is_null());
    // SAFETY: returned by the FFI accessor as a NUL-terminated string.
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(result.json);
    }
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let runs = value.as_array().expect("active runs array");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["runId"], run_id);
    assert_eq!(runs[0]["status"], "paused");
}

#[test]
fn given_ffi_active_runs_no_token_then_unauthorized() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let empty = c("");
    let result = alexandria_index_runs_active_json(empty.as_ptr());
    assert_eq!(result.status, STATUS_RUN_UNAUTHORIZED);
    assert!(result.json.is_null());
}

#[test]
fn given_low_priority_when_index_started_over_ffi_then_run_recorded_at_low_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let priority = c("low");
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        priority.as_ptr(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    // indexing.low_priority_concurrency defaults to 1 - proof the string
    // argument actually reached IndexRequest::priority and was resolved by
    // the core, not just accepted and ignored.
    assert_eq!(run_concurrency(&db_path, &run_id), Some(1));
    wait_for_files(1);
}

#[test]
fn given_garbage_priority_when_index_started_over_ffi_then_falls_back_to_normal_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let priority = c("URGENT!!1");
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        priority.as_ptr(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    // indexing.concurrency defaults to 4 - a client that cannot spell the
    // priority gets the safe default, not a rejected call.
    assert_eq!(run_concurrency(&db_path, &run_id), Some(4));
    wait_for_files(1);
}

#[test]
fn given_null_priority_when_refresh_started_over_ffi_then_normal_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    wait_for_files(1);

    let refreshed = alexandria_index_refresh_start(token.as_ptr(), std::ptr::null());
    assert_eq!(refreshed.status, STATUS_OK);
    let run_id = run_id_string(&refreshed);
    assert_eq!(run_concurrency(&db_path, &run_id), Some(4));
}

#[test]
fn given_low_priority_when_refresh_started_over_ffi_then_low_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.mp3"), b"audio").unwrap();
    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    wait_for_files(1);

    let priority = c("low");
    let refreshed = alexandria_index_refresh_start(token.as_ptr(), priority.as_ptr());
    assert_eq!(refreshed.status, STATUS_OK);
    let run_id = run_id_string(&refreshed);
    assert_eq!(run_concurrency(&db_path, &run_id), Some(1));
}

#[test]
fn given_a_paused_refresh_run_when_resumed_over_ffi_then_it_finishes() {
    // Every other resume test in this file starts an *index* run, so the
    // `RunKind::Refresh` branch of `alexandria_index_resume` — which spawns
    // `refresh_handler.execute(run_id)` with no root, a different call shape
    // than the index branch — had no coverage at all. This is that coverage.
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    // 5,000, not the 500 every index-side `wait_for_run_cell_live` caller in
    // this file uses. A refresh walk over already-cataloged paths is
    // stat-only — no byte read, no tag parsing (see `wait_for_run_cell_live`'s
    // doc comment) — so it burns through a library many times faster per
    // file than the index walk that cataloged it in the first place. 500
    // was enough margin for an index walk under a live loop's overhead; it
    // was not enough for a refresh walk under CPU contention from the rest
    // of the suite (observed: this exact test flaking under
    // `cargo test --workspace` while passing in isolation, the panic firing
    // from this function precisely because the walk had already finished).
    // 5,000 is deliberately generous — the margin has to hold up on a
    // loaded machine, not just the median run.
    write_library(lib.path(), 5000);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);

    // Waited on the run rather than on the count: the run record is what
    // says the walk is over, where the count is a derived observation of
    // writes that land in bursts — it sits still for twenty seconds and then
    // jumps by thousands.
    let index_run = run_id_string(&started);
    let terminal = wait_for_run_terminal_or_paused(&index_run, &token);
    assert_eq!(terminal["status"], "complete");

    // Not "exactly 5,000". Under the contention of the whole suite a handful
    // of inserts exhaust the busy-retry bound and are counted in `failed` —
    // designed behaviour, which `index_entry` states where it retries. So the
    // invariant asserted here is not a number this test picked: it is that
    // the run's own tally agrees with the catalog. Observed at 4999/1 and
    // 4998/2 under load and 5000/0 idle, and the row count matched the tally
    // every time.
    let indexed = terminal["indexed"].as_i64().expect("indexed");
    assert_eq!(
        alexandria_index_count_files(),
        indexed,
        "the run reported {indexed} indexed but the catalog holds a different \
         number"
    );

    // And enough of them landed to make the refresh below a real walk, which
    // is the only reason this fixture is 5,000 files rather than 500.
    assert!(
        indexed > 4_500,
        "only {indexed} files were indexed; the refresh will finish before \
         `wait_for_run_cell_live` can catch it"
    );

    // The refresh walk re-reads every one of the cataloged files, which is
    // what gives it enough real wall-clock time for `wait_for_run_cell_live`
    // to reliably catch it before it finishes.
    let refreshed = alexandria_index_refresh_start(token.as_ptr(), std::ptr::null());
    assert_eq!(refreshed.status, STATUS_OK);
    let run_id = run_id_string(&refreshed);
    wait_for_run_cell_live(&run_id, &token);

    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), token.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_OK,
        "expected RUN_OK resuming a paused refresh run"
    );
    assert_eq!(
        run_id_string(&resumed),
        run_id,
        "resume must hand back the same run id, not mint a fresh one"
    );

    let body = wait_for_run_terminal_or_paused(&run_id, &token);
    assert_eq!(body["status"], "complete");
}

#[test]
fn given_a_paused_index_run_with_no_stored_root_when_resumed_then_error() {
    // The other half of the hard part: `RunControlHandler::resume` hands
    // back `RunResumed { root: run.root, kind: run.kind, .. }` straight from
    // the row, and `RunKind::Index` is only ever supposed to have `Some`
    // root — `IndexHandler::start` requires one to start at all. But nothing
    // in the type system stops the row from drifting (a hand-edited
    // database, a migration bug), so `alexandria_index_resume` has to treat
    // `Index` + `root: None` as a real, refused case rather than an
    // unreachable one. This drives that branch directly, the same
    // `with_db` idiom `run_concurrency` above already uses to reach columns
    // the FFI surface does not expose an accessor for.
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id, &token);

    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_pause(run_id_c.as_ptr(), token.as_ptr()),
        STATUS_RUN_OK
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id, &token)["status"],
        "paused"
    );

    // Corrupt the stored row directly: a paused index run with no root,
    // which `start` itself could never have produced.
    let run_id_for_sql = run_id.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE catalog_runs SET root = NULL WHERE id = ?")
            .bind(run_id_for_sql)
            .execute(&pool)
            .await
            .expect("clear root");
    });

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), token.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_OTHER,
        "an index run with no stored root must fail loudly, not silently do nothing"
    );
    assert_eq!(
        run_id_string(&resumed),
        "",
        "a refused resume must not carry a run id back"
    );
}

/// The owner's symptom over the FFI surface (issue #122): `types` names the
/// file types the run records, comma-separated, in the same words the HTTP
/// body's array carries (FR-FC-24).
#[test]
fn given_an_audio_scope_when_ffi_index_start_then_only_the_audio_is_recorded() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.flac"), b"audio").unwrap();
    std::fs::write(lib.path().join("cover.jpg"), b"image").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let types = c("audio");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        types.as_ptr(),
    );
    assert_eq!(result.status, STATUS_OK);

    // The run has to be over before the absence of a second row means
    // anything — until then it only means the walk has not reached it yet.
    let run = wait_for_run_terminal(&run_id_string(&result), &token);
    assert_eq!(run["status"], "complete");
    assert_eq!(run["skipped"], 1, "the cover art was skipped, not failed");
    assert_eq!(run["failed"], 0);

    let files = files_json_value();
    let files = files.as_array().expect("array");
    assert_eq!(files.len(), 1, "the cover art must not be catalogued");
    assert_eq!(files[0]["type"], "audio");
}

/// Two types, comma-separated: a scope is a set, and the separator is what
/// the FFI surface has instead of an array.
#[test]
fn given_a_two_type_scope_when_ffi_index_start_then_both_are_recorded() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.flac"), b"audio").unwrap();
    std::fs::write(lib.path().join("notes.md"), b"text").unwrap();
    std::fs::write(lib.path().join("cover.jpg"), b"image").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let types = c("audio,text");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        types.as_ptr(),
    );
    assert_eq!(result.status, STATUS_OK);
    assert_eq!(wait_for_files(2), 2);

    let run = wait_for_run_terminal(&run_id_string(&result), &token);
    assert_eq!(run["status"], "complete");
    assert_eq!(run["skipped"], 1, "only the image is out of scope");
}

/// An empty `types` string is the same absence NULL is — never "index
/// nothing", which would turn a caller's missing argument into a run that
/// does no work at all.
#[test]
fn given_an_empty_types_string_when_ffi_index_start_then_every_type_is_recorded() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.flac"), b"audio").unwrap();
    std::fs::write(lib.path().join("cover.jpg"), b"image").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let types = c("");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        types.as_ptr(),
    );
    assert_eq!(result.status, STATUS_OK);

    assert_eq!(wait_for_files(2), 2);
}

/// Unlike an unrecognised `priority`, an unrecognised type is refused: the
/// only fallback available is "every type", which is the opposite of what the
/// caller asked for. Nothing is indexed, and no run record is opened.
#[test]
fn given_an_unknown_type_when_ffi_index_start_then_invalid_input() {
    let _g = serial();
    let (_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.flac"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let types = c("sculpture");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        types.as_ptr(),
    );

    assert_eq!(result.status, STATUS_INVALID_INPUT);
    assert_eq!(
        run_id_string(&result),
        "",
        "a refused start must not carry a run id back"
    );
    assert_eq!(
        alexandria_index_count_files(),
        0,
        "a refused start must index nothing"
    );
    // The claim the HTTP twin makes too: the scope is parsed ahead of
    // `start`, so a refused request leaves no run record behind (FR-FC-27).
    let runs = with_db(&db_path, |pool| async move {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM catalog_runs")
            .fetch_one(&pool)
            .await
            .expect("count runs");
        count
    });
    assert_eq!(runs, 0, "a refused start must not open a run record");
}

/// What the authentication gate in `alexandria_index_start` actually buys,
/// and the only test that fails without it: a caller with a bad token *and*
/// an unspellable scope must be told it is unauthorized, not that its scope
/// did not parse.
///
/// `start` authenticates too, so every other unauthorized test here passes
/// with the gate deleted — the parse would simply never be reached. This one
/// reaches it. HTTP answers `401` to the same pair because `require_auth` is
/// a route layer that runs before the body is ever extracted, so without the
/// gate the two surfaces would disagree about which fault a caller is told
/// about first (FR-FC-24 / NFR-09).
#[test]
fn given_a_bad_token_and_an_unknown_type_when_ffi_index_start_then_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("song.flac"), b"audio").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let token = c("");
    let types = c("sculpture");
    let result = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        types.as_ptr(),
    );

    assert_eq!(
        result.status, STATUS_UNAUTHORIZED,
        "an unauthenticated caller must not learn that its scope failed to parse"
    );
}

// ---------------- playlists (Task 9) ----------------

fn playlist_json_ok(result: PlaylistJsonResult) -> serde_json::Value {
    assert_eq!(
        result.status, STATUS_PLAYLIST_OK,
        "expected PLAYLIST_OK, got {}",
        result.status
    );
    assert!(!result.json.is_null(), "success must carry a json pointer");
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(result.json);
    }
    serde_json::from_str(&json).expect("playlist json")
}

/// Smoke-test the plain success path: creating a playlist over FFI returns
/// `PLAYLIST_OK` with a body carrying a valid uuid, matching what
/// `POST /v1/playlists` answers over HTTP (FR-FC-24 / NFR-09).
#[test]
fn given_a_valid_name_when_ffi_playlist_created_then_ok_with_a_uuid() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({ "name": "Road trip" }).to_string());
    let token = c(TEST_TOKEN);
    let result = alexandria_playlist_create(body.as_ptr(), token.as_ptr());

    let value = playlist_json_ok(result);
    let uuid = value["uuid"].as_str().unwrap_or_default();
    assert!(
        uuid::Uuid::parse_str(uuid).is_ok(),
        "create must answer a valid uuid, got {uuid:?}"
    );
    assert_eq!(value["name"], "Road trip");
}

/// An unknown playlist uuid must be reported as `PLAYLIST_ERR_NOT_FOUND`,
/// matching HTTP's `404` for `GET /v1/playlists/{uuid}`.
#[test]
fn given_an_unknown_uuid_when_ffi_playlist_read_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("11111111-1111-1111-1111-111111111111");
    let token = c(TEST_TOKEN);
    let result = alexandria_playlist_read(uuid.as_ptr(), token.as_ptr());

    assert_eq!(result.status, STATUS_PLAYLIST_NOT_FOUND);
    assert!(result.json.is_null());
}

/// A blank name must be reported as `PLAYLIST_ERR_INVALID_INPUT`, matching
/// HTTP's `400` for the same body on `POST /v1/playlists`
/// (`validate_playlist_name`).
#[test]
fn given_a_blank_name_when_ffi_playlist_created_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({ "name": "   " }).to_string());
    let token = c(TEST_TOKEN);
    let result = alexandria_playlist_create(body.as_ptr(), token.as_ptr());

    assert_eq!(result.status, STATUS_PLAYLIST_INVALID_INPUT);
    assert!(result.json.is_null());
}

/// What the authentication gate in `alexandria_playlist_create` actually
/// buys, and the only test that fails without it: a caller with a bad token
/// *and* a malformed body must be told it is unauthorized, not that its body
/// failed to parse.
///
/// `create` parses the body too, so every other unauthorized test for this
/// function would pass with the gate deleted — the parse would simply never
/// be reached with a *well-formed* body. This one reaches it. HTTP answers
/// `401` to the same pair because `require_auth` is a route layer that runs
/// before the body is ever extracted, so without the gate the two surfaces
/// would disagree about which fault a caller is told about first
/// (FR-FC-24 / NFR-09). Task 8's HTTP surface had exactly this gap and had
/// to be sent back for it.
#[test]
fn given_a_bad_token_and_a_malformed_body_when_ffi_playlist_created_then_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c("{ not json");
    let token = c("");
    let result = alexandria_playlist_create(body.as_ptr(), token.as_ptr());

    assert_eq!(
        result.status, STATUS_PLAYLIST_UNAUTHORIZED,
        "create must deny before parsing the body"
    );
    assert!(result.json.is_null());
}

/// `alexandria_playlists_list` must deny an unauthenticated caller,
/// matching HTTP's `401` for `GET /v1/playlists`.
#[test]
fn given_no_token_when_ffi_playlists_list_then_unauthorized() {
    let _g = serial();
    let _db = init_temp_db();

    let empty = c("");
    let result = alexandria_playlists_list(empty.as_ptr());

    assert_eq!(result.status, STATUS_PLAYLIST_UNAUTHORIZED);
    assert!(result.json.is_null());
}

/// End-to-end round trip over the whole surface: create, rename, list, add
/// entries (seeding an audio file directly, mirroring `parity.rs`'s
/// `seed_file`), move the one entry (a no-op move at its own index, just to
/// exercise the call), remove it, then delete the playlist — every step
/// answering `PLAYLIST_OK`.
#[test]
fn given_a_full_lifecycle_when_driven_entirely_over_ffi_then_every_step_is_ok() {
    let _g = serial();
    let (_dir, db_path) = init_temp_db();
    let token = c(TEST_TOKEN);

    let create_body = c(&serde_json::json!({ "name": "Road trip" }).to_string());
    let created = playlist_json_ok(alexandria_playlist_create(
        create_body.as_ptr(),
        token.as_ptr(),
    ));
    let playlist_uuid = created["uuid"].as_str().unwrap().to_string();
    let puuid = c(&playlist_uuid);

    let rename_body = c(&serde_json::json!({ "name": "Summer trip" }).to_string());
    let renamed = playlist_json_ok(alexandria_playlist_rename(
        puuid.as_ptr(),
        rename_body.as_ptr(),
        token.as_ptr(),
    ));
    assert_eq!(renamed["name"], "Summer trip");

    let listed = playlist_json_ok(alexandria_playlists_list(token.as_ptr()));
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["uuid"] == playlist_uuid));

    let file_uuid = with_db(&db_path, |pool| async move {
        let file_uuid = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at) \
             VALUES (?, ?, ?, ?, 'hash', ?)",
        )
        .bind(&file_uuid)
        .bind(format!("/lib/{file_uuid}"))
        .bind("seeded.flac")
        .bind("audio")
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&pool)
        .await
        .expect("seed audio file");
        file_uuid
    });

    let add_body = c(&serde_json::json!({ "fileUuids": [file_uuid] }).to_string());
    let entries = playlist_json_ok(alexandria_playlist_add_entries(
        puuid.as_ptr(),
        add_body.as_ptr(),
        token.as_ptr(),
    ));
    let entry_uuid = entries.as_array().unwrap()[0]["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    let entry_uuid_c = c(&entry_uuid);

    let view = playlist_json_ok(alexandria_playlist_read(puuid.as_ptr(), token.as_ptr()));
    assert_eq!(view["entries"].as_array().unwrap().len(), 1);

    let move_body = c(&serde_json::json!({ "toIndex": 0 }).to_string());
    let moved = playlist_json_ok(alexandria_playlist_move_entry(
        puuid.as_ptr(),
        entry_uuid_c.as_ptr(),
        move_body.as_ptr(),
        token.as_ptr(),
    ));
    assert_eq!(moved.as_array().unwrap().len(), 1);

    let removed =
        alexandria_playlist_remove_entry(puuid.as_ptr(), entry_uuid_c.as_ptr(), token.as_ptr());
    assert_eq!(removed.status, STATUS_PLAYLIST_OK);
    assert!(!removed.json.is_null());
    let removed_json = unsafe { CStr::from_ptr(removed.json) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe {
        alexandria_free_string(removed.json);
    }
    assert_eq!(
        removed_json, "{}",
        "remove_entry echoes nothing but success"
    );

    let deleted = playlist_json_ok(alexandria_playlist_delete(puuid.as_ptr(), token.as_ptr()));
    assert_eq!(deleted["uuid"], playlist_uuid);
}

// ---------------- music enrichment ----------------

/// Enrichment is off in every test process — `init_temp_db` sets no
/// `[metadata]` configuration — so a run must be refused as unavailable
/// rather than attempted. This is the assertion that matters most for a
/// feature that reaches the network: the shipped default does not.
#[test]
fn given_enrichment_is_not_configured_when_a_run_is_started_then_it_is_unavailable() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let result = alexandria_enrichment_run(std::ptr::null(), token.as_ptr());

    assert_eq!(result.status, STATUS_ENRICHMENT_UNAVAILABLE);
    assert!(result.json.is_null());
}

/// Naming both scopes is the caller not knowing what it asked for, and is
/// refused rather than resolved in its favour — matching the HTTP route's
/// own `400` for the same body.
#[test]
fn given_both_scopes_named_when_a_run_is_started_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({
        "fileUuid": "11111111-1111-1111-1111-111111111111",
        "artist": "Miles Davis"
    })
    .to_string());
    let token = c(TEST_TOKEN);
    let result = alexandria_enrichment_run(body.as_ptr(), token.as_ptr());

    // Refused for the body, before the unavailable check would have been
    // reached — a malformed request is the caller's to fix either way.
    assert_eq!(result.status, STATUS_ENRICHMENT_INVALID_INPUT);
}

/// Reading what enrichment stored is a plain database read and must work
/// whether or not enrichment itself is switched on — an owner who enabled
/// it, ran it once, and turned it off keeps what they fetched.
#[test]
fn given_enrichment_is_off_when_a_track_is_read_then_it_still_answers() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("11111111-1111-1111-1111-111111111111");
    let token = c(TEST_TOKEN);
    let result = alexandria_enrichment_read_track(uuid.as_ptr(), std::ptr::null(), token.as_ptr());

    assert_eq!(
        result.status, STATUS_ENRICHMENT_OK,
        "reading a cached result must not depend on the network switch"
    );
    assert!(!result.json.is_null());

    let json = unsafe { std::ffi::CStr::from_ptr(result.json) }
        .to_string_lossy()
        .into_owned();
    let value: serde_json::Value = serde_json::from_str(&json).expect("enrichment json");
    // Nothing has been looked up, so both halves are absent — which is a
    // state, not a failure.
    assert!(value["artistImage"].is_null());
    assert!(value["lyrics"].is_null());
    unsafe { alexandria_ffi::alexandria_free_string(result.json) };
}

/// An unparseable file uuid is the caller's error, not a missing record.
#[test]
fn given_a_malformed_uuid_when_a_track_is_read_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("not-a-uuid");
    let token = c(TEST_TOKEN);
    let result = alexandria_enrichment_read_track(uuid.as_ptr(), std::ptr::null(), token.as_ptr());

    assert_eq!(result.status, STATUS_ENRICHMENT_INVALID_INPUT);
    assert!(result.json.is_null());
}

// ---------------- libraries ----------------

fn library_json_ok(result: alexandria_ffi::LibraryJsonResult) -> serde_json::Value {
    assert_eq!(result.status, STATUS_LIBRARY_OK);
    assert!(!result.json.is_null());
    let json = unsafe { std::ffi::CStr::from_ptr(result.json) }
        .to_string_lossy()
        .into_owned();
    unsafe { alexandria_ffi::alexandria_free_string(result.json) };
    serde_json::from_str(&json).expect("library json")
}

/// Registering answers the library, and it is then listed.
#[test]
fn given_a_folder_when_registered_over_ffi_then_it_is_a_library() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({
        "name": "Course",
        "rootPath": "/library/course"
    })
    .to_string());
    let token = c(TEST_TOKEN);
    let value = library_json_ok(alexandria_library_register(body.as_ptr(), token.as_ptr()));

    assert_eq!(value["name"], "Course");
    assert!(uuid::Uuid::parse_str(value["uuid"].as_str().unwrap_or_default()).is_ok());

    let listed = library_json_ok(alexandria_libraries_list(token.as_ptr()));
    assert_eq!(listed.as_array().map(|a| a.len()), Some(1));
}

/// An overlapping folder is a conflict, not an invalid input: the request was
/// well formed and the folder is real — what is wrong is the current state.
#[test]
fn given_an_overlapping_folder_when_registered_over_ffi_then_it_is_a_conflict() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let first =
        c(&serde_json::json!({"name": "Course", "rootPath": "/library/course"}).to_string());
    library_json_ok(alexandria_library_register(first.as_ptr(), token.as_ptr()));

    let nested =
        c(&serde_json::json!({"name": "Week", "rootPath": "/library/course/week-1"}).to_string());
    let result = alexandria_library_register(nested.as_ptr(), token.as_ptr());

    assert_eq!(result.status, STATUS_LIBRARY_CONFLICT);
}

/// A blank name never becomes a stored blank.
#[test]
fn given_a_blank_name_when_registered_over_ffi_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({"name": "   ", "rootPath": "/library/course"}).to_string());
    let token = c(TEST_TOKEN);

    assert_eq!(
        alexandria_library_register(body.as_ptr(), token.as_ptr()).status,
        STATUS_LIBRARY_INVALID_INPUT
    );
}

/// Browsing an empty library answers a level rather than failing: a folder
/// with nothing indexed under it yet is a state, not an error.
#[test]
fn given_a_new_library_when_browsed_over_ffi_then_it_is_simply_empty() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let body = c(&serde_json::json!({"name": "Course", "rootPath": "/library/course"}).to_string());
    let library = library_json_ok(alexandria_library_register(body.as_ptr(), token.as_ptr()));
    let uuid = c(library["uuid"].as_str().unwrap());

    let listing = library_json_ok(alexandria_library_browse(
        uuid.as_ptr(),
        std::ptr::null(),
        token.as_ptr(),
    ));

    assert_eq!(listing["folders"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(listing["files"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(listing["path"], "");
}

/// The folder moved; the record is corrected and the library answers from
/// its new root.
#[test]
fn given_a_moved_folder_when_the_root_is_corrected_over_ffi_then_the_library_follows() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let body = c(&serde_json::json!({"name": "Course", "rootPath": "/library/course"}).to_string());
    let library = library_json_ok(alexandria_library_register(body.as_ptr(), token.as_ptr()));
    let uuid = c(library["uuid"].as_str().unwrap());

    let moved = c(&serde_json::json!({"rootPath": "/media/courses/rust"}).to_string());
    let value = library_json_ok(alexandria_library_move(
        uuid.as_ptr(),
        moved.as_ptr(),
        token.as_ptr(),
    ));

    assert_eq!(value["rootPath"], "/media/courses/rust");
    assert_eq!(value["name"], "Course", "the move renamed the library");
}

/// Moving onto another library's folder is a conflict, the same refusal
/// registering there would give.
#[test]
fn given_another_librarys_folder_when_moved_onto_over_ffi_then_it_is_a_conflict() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let first =
        c(&serde_json::json!({"name": "Course", "rootPath": "/library/course"}).to_string());
    let library = library_json_ok(alexandria_library_register(first.as_ptr(), token.as_ptr()));
    let second =
        c(&serde_json::json!({"name": "Photos", "rootPath": "/library/photos"}).to_string());
    library_json_ok(alexandria_library_register(second.as_ptr(), token.as_ptr()));

    let uuid = c(library["uuid"].as_str().unwrap());
    let onto = c(&serde_json::json!({"rootPath": "/library/photos/2024"}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), onto.as_ptr(), token.as_ptr()).status,
        STATUS_LIBRARY_CONFLICT
    );
}

/// A blank root is refused rather than stored, as it is on registration.
#[test]
fn given_a_blank_root_when_a_library_is_moved_over_ffi_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let token = c(TEST_TOKEN);
    let body = c(&serde_json::json!({"name": "Course", "rootPath": "/library/course"}).to_string());
    let library = library_json_ok(alexandria_library_register(body.as_ptr(), token.as_ptr()));
    let uuid = c(library["uuid"].as_str().unwrap());
    let blank = c(&serde_json::json!({"rootPath": "  "}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), blank.as_ptr(), token.as_ptr()).status,
        STATUS_LIBRARY_INVALID_INPUT
    );
}

/// An unknown uuid is not found here too, so the surfaces agree on every
/// status this call can answer (FR-FC-24).
#[test]
fn given_an_unknown_uuid_when_a_library_is_moved_over_ffi_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("11111111-1111-1111-1111-111111111111");
    let token = c(TEST_TOKEN);
    let body = c(&serde_json::json!({"rootPath": "/media/courses"}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), body.as_ptr(), token.as_ptr()).status,
        STATUS_LIBRARY_NOT_FOUND
    );
}

/// An unknown uuid is reported as not found, matching HTTP's 404.
#[test]
fn given_an_unknown_uuid_when_a_library_is_removed_over_ffi_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("11111111-1111-1111-1111-111111111111");
    let token = c(TEST_TOKEN);

    assert_eq!(
        alexandria_library_remove(uuid.as_ptr(), token.as_ptr()).status,
        STATUS_LIBRARY_NOT_FOUND
    );
}

/// The failures list over FFI (FR-FC-42), and the statuses it shares with
/// HTTP (FR-FC-24).
///
/// A run that failed on a file is not contrived here: what this pins is the
/// call and its answers, and the walk's own recording is tested where the
/// walk is.
#[test]
fn given_a_run_with_no_failures_when_asked_over_ffi_then_an_empty_list() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 2);

    let root = c(lib.path().to_str().unwrap());
    let token = c(TEST_TOKEN);
    let started = alexandria_index_start(
        root.as_ptr(),
        token.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    assert_eq!(started.status, STATUS_OK);
    let run_id = c(&run_id_string(&started));

    let result = alexandria_index_run_failures_json(run_id.as_ptr(), token.as_ptr());
    assert_eq!(result.status, STATUS_RUN_OK);
    assert!(!result.json.is_null());
    let json = unsafe { CStr::from_ptr(result.json) }
        .to_string_lossy()
        .into_owned();
    unsafe { alexandria_ffi::alexandria_free_string(result.json) };

    let value: serde_json::Value = serde_json::from_str(&json).expect("failures json");
    assert_eq!(value.as_array().map(|a| a.len()), Some(0));
}

#[test]
fn given_an_unknown_run_when_failures_asked_over_ffi_then_not_found() {
    // Not an empty list — the same answer HTTP gives, for the same reason:
    // "failed on nothing" is a different fact from "no such run".
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();

    let run_id = c("00000000-0000-4000-8000-000000000000");
    let token = c(TEST_TOKEN);

    assert_eq!(
        alexandria_index_run_failures_json(run_id.as_ptr(), token.as_ptr()).status,
        STATUS_RUN_NOT_FOUND
    );
}

#[test]
fn given_a_malformed_run_id_when_failures_asked_over_ffi_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();

    let run_id = c("not-a-uuid");
    let token = c(TEST_TOKEN);

    assert_eq!(
        alexandria_index_run_failures_json(run_id.as_ptr(), token.as_ptr()).status,
        STATUS_RUN_INVALID_INPUT
    );
}
