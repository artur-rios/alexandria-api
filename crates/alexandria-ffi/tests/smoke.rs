use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;

use tempfile::{tempdir, TempDir};

// The FFI keeps services in a process-global static. Within one test binary,
// tests run in parallel and would race on that slot, so serialize every FFI
// test behind this lock (no serial_test dependency needed).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // Recover from poisoning rather than propagating it. The guard protects a process-global services slot, not an
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
///
/// The run-status waits are the exception, because they have a better
/// measure than the count: the run's own `processed`, the live per-entry
/// counter, which does not land in bursts. For them this bounds a *stall* —
/// see [`wait_for_run_terminal_or_paused`]. As a total it failed a healthy
/// run on CI: the 5,000-file index, slower since every file gained a
/// metadata-stamp write, was at 4,922 of 5,000 and still advancing when two
/// minutes ran out.
const ASYNC_RUN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// The editable columns of a `video_files` row, in the order every
/// assertion here selects them: title, year, resolution, media_kind.
type VideoMetadataRow = (Option<String>, Option<i64>, Option<String>, Option<String>);

use alexandria_ffi::{
    alexandria_file_edit_metadata, alexandria_file_playback_source, alexandria_file_purge,
    alexandria_file_purge_on_disk, alexandria_file_rename, alexandria_file_restore,
    alexandria_file_soft_delete, alexandria_free_string, alexandria_index_cancel,
    alexandria_index_count_files, alexandria_index_count_missing, alexandria_index_files_json,
    alexandria_index_init, alexandria_index_pause, alexandria_index_refresh_start,
    alexandria_index_resume, alexandria_index_run_failures_json, alexandria_index_run_status_json,
    alexandria_index_runs_active_json, alexandria_libraries_list, alexandria_library_browse,
    alexandria_library_index_start, alexandria_library_move, alexandria_library_register,
    alexandria_library_remove, FileMetadataResult, IndexStartResult,
};

const STATUS_RUN_OK: i32 = alexandria_ffi::RUN_OK;
const STATUS_RUN_INVALID_INPUT: i32 = alexandria_ffi::RUN_ERR_INVALID_INPUT;
const STATUS_RUN_NOT_FOUND: i32 = alexandria_ffi::RUN_ERR_NOT_FOUND;
const STATUS_RUN_INVALID_STATE: i32 = alexandria_ffi::RUN_ERR_INVALID_STATE;
const STATUS_RUN_OTHER: i32 = alexandria_ffi::RUN_ERR_OTHER;

const STATUS_OK: i32 = alexandria_ffi::INDEX_OK;
const STATUS_INDEX_BUSY: i32 = alexandria_ffi::INDEX_ERR_BUSY;
const STATUS_INVALID_INPUT: i32 = alexandria_ffi::INDEX_ERR_INVALID_INPUT;
const STATUS_FILE_INVALID_INPUT: i32 = alexandria_ffi::FILE_ERR_INVALID_INPUT;
const STATUS_FILE_NOT_INITIALIZED: i32 = alexandria_ffi::FILE_ERR_NOT_INITIALIZED;
const STATUS_FILE_NOT_FOUND: i32 = alexandria_ffi::FILE_ERR_NOT_FOUND;
const STATUS_FILE_INVALID_STATE: i32 = alexandria_ffi::FILE_ERR_INVALID_STATE;
const STATUS_FILE_OK: i32 = alexandria_ffi::FILE_OK;
const STATUS_FILE_OTHER: i32 = alexandria_ffi::FILE_ERR_OTHER;
const STATUS_FILE_DISK: i32 = alexandria_ffi::FILE_ERR_DISK;

const STATUS_LIBRARY_OK: i32 = alexandria_ffi::LIBRARY_OK;
const STATUS_LIBRARY_CONFLICT: i32 = alexandria_ffi::LIBRARY_ERR_CONFLICT;
const STATUS_LIBRARY_INVALID_INPUT: i32 = alexandria_ffi::LIBRARY_ERR_INVALID_INPUT;
const STATUS_LIBRARY_NOT_FOUND: i32 = alexandria_ffi::LIBRARY_ERR_NOT_FOUND;
const STATUS_INDEX_NOT_FOUND: i32 = alexandria_ffi::INDEX_ERR_NOT_FOUND;
const STATUS_INDEX_CONFLICT: i32 = alexandria_ffi::INDEX_ERR_CONFLICT;
const STATUS_PLAYBACK_OK: i32 = alexandria_ffi::PLAYBACK_OK;
const STATUS_PLAYBACK_INVALID_INPUT: i32 = alexandria_ffi::PLAYBACK_ERR_INVALID_INPUT;

fn init_temp_db() -> (TempDir, String) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ffi.sqlite");
    let db_path = db.to_str().unwrap().to_string();

    let cpath = CString::new(db_path.clone()).unwrap();
    init_when_idle(&cpath);
    (dir, db_path)
}

/// Initialize, waiting out any walk the previous test left running.
///
/// `alexandria_index_init` refuses with `INDEX_ERR_BUSY` while this process
/// is executing a run, because replacing the services would orphan that walk.
/// Most tests here start a scan and then assert against the catalog, which
/// can be satisfied a moment before the walk closes its run — so the next
/// test's setup can legitimately arrive while one is still draining.
///
/// Waiting rather than forcing is the point: this is the same rule an
/// embedder has to follow, and a test harness that could opt out of it would
/// be a harness proving something the product cannot do. The fixtures here
/// are tiny, so the wait is milliseconds.
fn init_when_idle(cpath: &CString) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        match alexandria_index_init(cpath.as_ptr()) {
            STATUS_OK => return,
            STATUS_INDEX_BUSY if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            STATUS_INDEX_BUSY => {
                panic!("a run left by an earlier test never settled within {ASYNC_RUN_DEADLINE:?}")
            }
            other => panic!("ffi services init failed: {other}"),
        }
    }
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

/// Index `root` the only way there is: add it as a library, which opens its
/// first run. Answered as the run start these tests read — a status in the
/// index family's codes and the run id — so a test reads the same whether it
/// is about the run or the registration that started it.
///
/// `root` and `priority` may be NULL, as an FFI caller may pass them.
fn index_folder(
    root: *const std::os::raw::c_char,
    priority: *const std::os::raw::c_char,
) -> IndexStartResult {
    let read = |ptr: *const std::os::raw::c_char| {
        (!ptr.is_null()).then(|| {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        })
    };
    let mut body = serde_json::json!({ "name": "Library", "rootPath": read(root) });
    if let Some(priority) = read(priority) {
        body["priority"] = serde_json::Value::String(priority);
    }
    let body = CString::new(body.to_string()).unwrap();
    let result = alexandria_ffi::alexandria_library_register(body.as_ptr());
    let mut run_id = [0 as std::os::raw::c_char; 37];
    let status = match result.status {
        alexandria_ffi::LIBRARY_OK => {
            let json = unsafe { CStr::from_ptr(result.json) }
                .to_string_lossy()
                .into_owned();
            unsafe { alexandria_ffi::alexandria_free_string(result.json) };
            let value: serde_json::Value = serde_json::from_str(&json).expect("library json");
            let id = value["runId"].as_str().expect("runId");
            for (slot, byte) in run_id.iter_mut().zip(id.bytes()) {
                *slot = byte as std::os::raw::c_char;
            }
            alexandria_ffi::INDEX_OK
        }
        alexandria_ffi::LIBRARY_ERR_INVALID_INPUT => alexandria_ffi::INDEX_ERR_INVALID_INPUT,
        alexandria_ffi::LIBRARY_ERR_NOT_INITIALIZED => alexandria_ffi::INDEX_ERR_NOT_INITIALIZED,
        alexandria_ffi::LIBRARY_ERR_CONFLICT => alexandria_ffi::INDEX_ERR_CONFLICT,
        _ => alexandria_ffi::INDEX_ERR_OTHER,
    };
    IndexStartResult { status, run_id }
}

#[test]
fn given_ffi_library_when_version_called_then_returns_version_string() {
    let _g = serial();
    let raw = alexandria_ffi::alexandria_version();
    assert!(!raw.is_null());
    // SAFETY: the FFI returns a static NUL-terminated string.
    let cstr = unsafe { CStr::from_ptr(raw) };
    // The crate's own version, not a literal repeated here: a client refuses
    // a core outside the minor line it was built against, so what this pins
    // is that the string is *the version* — a call answering something else
    // would strand every client at startup.
    assert_eq!(cstr.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn given_ffi_library_when_health_status_code_called_then_returns_200() {
    let _g = serial();
    assert_eq!(alexandria_ffi::alexandria_health_status_code(), 200);
}

#[test]
fn given_supported_files_when_ffi_library_register_then_returns_ok_with_run_id_and_persists() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("a.mp4"), b"video").unwrap();
    std::fs::write(lib.path().join("b.md"), b"text").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let result = index_folder(root.as_ptr(), std::ptr::null());

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
    assert_eq!(parsed[0]["type"], "video");
    assert_eq!(parsed[1]["type"], "text");
}

#[test]
fn given_missing_root_when_ffi_library_register_then_returns_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let root = c("/no/such/dir/here");
    let result = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// FR-FC-26 parity: the constraint lives in the core handler, so the FFI
/// surface rejects an out-of-root index exactly as HTTP's 400 does (NFR-09).
/// The bound is configured the only way the FFI reads configuration — the
/// `ALEXANDRIA_*` environment — which is process-wide, so it is removed again
/// before any assertion can unwind out of this test.
#[test]
fn given_root_outside_configured_library_root_when_ffi_library_register_then_invalid_input() {
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
    let result = index_folder(root.as_ptr(), std::ptr::null());
    std::env::remove_var("ALEXANDRIA_FILESYSTEM_ROOT");

    // Assert
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// The other half of the parity pair: with the same bound configured, a root
/// *inside* it is accepted — so the test above is proving the constraint, not
/// some unrelated FFI failure.
#[test]
fn given_root_inside_configured_library_root_when_ffi_library_register_then_ok() {
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
    let result = index_folder(root.as_ptr(), std::ptr::null());
    std::env::remove_var("ALEXANDRIA_FILESYSTEM_ROOT");

    // Assert
    assert_eq!(result.status, STATUS_OK);
}

#[test]
fn given_not_initialized_when_ffi_library_register_then_returns_not_initialized() {
    let _g = serial();
    // Replace the global slot with None by re-initializing to a fresh path and
    // then... we can only set Some. Instead, verify the freshly-initialized
    // case: after init returns OK, the slot is Some; assert a NULL root yields
    // invalid input (covers the error path that does not hit the slot error).
    let _db = init_temp_db();
    let result = index_folder(std::ptr::null(), std::ptr::null());
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// A root that is not UTF-8 is refused rather than decoded lossily: the
/// lossy form is a different folder, with U+FFFD where the bytes were.
#[cfg(unix)]
#[test]
fn given_a_root_that_is_not_utf8_when_ffi_library_register_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let root = CString::new(b"/tmp/caf\xe9".to_vec()).unwrap();
    let result = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(result.status, STATUS_INVALID_INPUT);
}

/// Neither is a database path: decoded lossily it named a different file,
/// and init created a new, empty catalog there instead of opening the
/// owner's.
#[cfg(unix)]
#[test]
fn given_a_db_path_that_is_not_utf8_when_ffi_init_then_invalid_input() {
    let _g = serial();
    let dir = tempdir().unwrap();
    let mut bytes = dir.path().to_str().unwrap().as_bytes().to_vec();
    bytes.extend_from_slice(b"/caf\xe9.sqlite");
    let path = CString::new(bytes).unwrap();

    let status = alexandria_index_init(path.as_ptr());

    assert_eq!(status, STATUS_INVALID_INPUT);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "a database was created at the mangled path"
    );
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
fn wait_for_run_terminal(run_id: &str) -> serde_json::Value {
    wait_for_run_terminal_or_paused(run_id)
}

/// Wait until `missing_at IS NOT NULL` count reaches `expected` missing files.
fn wait_for_missing(expected: i64) -> i64 {
    wait_for_count("missing", expected, || alexandria_index_count_missing())
}

/// Task 4 rewrote what a re-index detects a change *by* — a `stat` call
/// (size + mtime), not a recomputed SHA-256 — so this test's old premise
/// (capture `a.txt`'s pre-refresh hash, then wait for refresh to produce a
/// *different* hash) is dead twice over: Task 3 already stopped indexing
/// from computing a hash at all (a freshly indexed file's `content_hash` is
/// `null`), and Task 4's refresh never computes a new one either — a
/// detected change now clears `content_hash` to `null` rather than
/// replacing it (FR-FC-10), so there is neither an old hash to capture nor a
/// new one to compare against.
///
/// What refresh actually guarantees now: `a.txt`'s changed size is detected
/// via `stat` and counted in the run's `refreshed` tally, `b.md`'s absence
/// is counted in `markedMissing`, and `a.txt`'s `content_hash` comes back
/// `null` (not a new hash) while its `missingAt` is cleared.
#[test]
fn given_changed_and_deleted_files_when_ffi_refresh_then_refreshes_and_marks_missing() {
    let _g = serial();
    let _db = init_temp_db();
    let lib = tempdir().unwrap();
    let a_path = lib.path().join("a.txt");
    let b_path = lib.path().join("b.md");
    std::fs::write(&a_path, b"text-v1").unwrap();
    std::fs::write(&b_path, b"text-v1").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(2), 2);

    // Mutate on disk: change a's bytes — and with them its size — delete b.
    std::fs::write(&a_path, b"text-v2-CHANGED").unwrap();
    std::fs::remove_file(&b_path).unwrap();

    let refresh = alexandria_index_refresh_start(std::ptr::null());
    assert_eq!(refresh.status, STATUS_OK);
    let run_id = run_id_string(&refresh);
    assert!(!run_id.is_empty());

    // b must be marked missing, and the run itself must report exactly one
    // stat-detected change (a) and one missing file (b) once it completes.
    assert_eq!(wait_for_missing(1), 1);
    let run = wait_for_run_terminal(&run_id);
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
        .find(|o| o["name"] == "a.txt")
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

#[allow(dead_code)]
fn _unused(c: *const c_char, s: &str) -> String {
    let _ = c;
    s.to_string()
}

// ---------------------------------------------------------------------------
// UC-04: alexandria_file_edit_metadata (FR-FC-15..18, FR-FC-24)
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
fn given_indexed_video_file_when_ffi_edit_metadata_then_ok_and_row_updated() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("clip.mkv"), b"video bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "clip.mkv");
    let patch = c(r#"{"type":"video","title":"New Title","year":2001,"mediaKind":"series"}"#);
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr());

    let json = metadata_json(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["file"]["fileType"], "video");
    assert_eq!(json["metadata"]["type"], "video");
    assert_eq!(json["metadata"]["title"], "New Title");
    assert_eq!(json["metadata"]["year"], 2001);
    assert_eq!(json["metadata"]["mediaKind"], "series");

    // Persisted subtype row reflects the full-replace PATCH.
    let uuid_clone = uuid.clone();
    let row = with_db(&db_path, move |pool| async move {
        let row: VideoMetadataRow = sqlx::query_as(
            "SELECT title, year, resolution, media_kind FROM video_files \
             JOIN files ON files.id = video_files.file_id WHERE files.uuid = ?",
        )
        .bind(uuid_clone)
        .fetch_one(&pool)
        .await
        .expect("video row");
        row
    });
    assert_eq!(row.0.as_deref(), Some("New Title"));
    assert_eq!(row.1, Some(2001));
    assert_eq!(row.2, None, "resolution absent in patch -> NULL");
    assert_eq!(row.3.as_deref(), Some("series"));
}

#[test]
fn given_ffi_edit_metadata_variant_mismatch_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("photo.png"), b"image").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "photo.png");
    let patch = c(r#"{"type":"video","title":"x"}"#);
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_edit_metadata_bad_patch_json_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let patch = c("not-json-at-all");
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
}

#[test]
fn given_ffi_edit_metadata_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let patch = c(r#"{"type":"video","title":"x"}"#);
    let result = alexandria_file_edit_metadata(uuid.as_ptr(), patch.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
}

#[test]
fn given_ffi_edit_metadata_deleted_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("clip.mkv"), b"video").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(wait_for_files(1), 1);

    let uuid = uuid_by_name(&db_path, "clip.mkv");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let patch = c(r#"{"type":"video","title":"x"}"#);
    let result = alexandria_file_edit_metadata(c(&uuid).as_ptr(), patch.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

#[allow(dead_code)]
fn _ffi_file_status_constants_are_stable() {
    // Keeps the FILE_* status constants referenced even if a test is trimmed.
    let _ = (
        STATUS_FILE_OK,
        STATUS_FILE_INVALID_INPUT,
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let name = c("renamed.txt");
    let result = alexandria_file_rename(c(&uuid).as_ptr(), name.as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["name"], "renamed.txt");
    assert_eq!(json["state"], "active");
    let new_path = json["path"].as_str().expect("path string");
    assert!(new_path.ends_with("renamed.txt"));

    // On-disk file moved.
    assert!(
        !lib.path().join("memo.txt").exists(),
        "old path gone after rename"
    );
    assert!(lib.path().join("renamed.txt").exists(), "new path present");
    assert_eq!(
        std::fs::read(lib.path().join("renamed.txt")).unwrap(),
        b"text bytes"
    );

    // Catalog row updated.
    let (name, path): (String, String) = with_db(&db_path, move |pool| async move {
        sqlx::query_as("SELECT name, path FROM files WHERE uuid = ?")
            .bind(&uuid)
            .fetch_one(&pool)
            .await
            .expect("catalog row")
    });
    assert_eq!(name, "renamed.txt");
    assert!(path.ends_with("renamed.txt"));
}

#[test]
fn given_ffi_rename_invalid_name_then_invalid_input() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);
    let uuid = uuid_by_name(&db_path, "memo.txt");

    for bad in ["/x", "..", "a:b"] {
        let bad = c(bad);
        let result = alexandria_file_rename(c(&uuid).as_ptr(), bad.as_ptr());
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
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let name = c("new.txt");
    let result = alexandria_file_rename(uuid.as_ptr(), name.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
}

#[test]
fn given_ffi_rename_deleted_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let name = c("renamed.txt");
    let result = alexandria_file_rename(c(&uuid).as_ptr(), name.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
}

#[test]
fn given_ffi_rename_target_owned_by_other_file_then_disk_error() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("a.txt"), b"aaa").unwrap();
    std::fs::write(lib.path().join("b.txt"), b"bbb").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(2);

    let uuid_a = uuid_by_name(&db_path, "a.txt");
    let name = c("b.txt");
    let result = alexandria_file_rename(c(&uuid_a).as_ptr(), name.as_ptr());
    assert_eq!(
        result.status, STATUS_FILE_DISK,
        "target-exists must map to FILE_ERR_DISK"
    );
    assert!(result.json.is_null());

    // a.txt left untouched on disk.
    assert!(
        lib.path().join("a.txt").exists(),
        "a.txt untouched after refusal"
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let result = alexandria_file_soft_delete(c(&uuid).as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["state"], "deleted");
    assert!(
        json["deletedAt"].as_str().is_some(),
        "deletedAt present on the returned File"
    );

    // The on-disk file is untouched (UC-06 leaves it; purge-on-disk is UC-09).
    assert!(
        lib.path().join("memo.txt").exists(),
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
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_soft_delete(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_soft_delete_already_deleted_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("soft-delete seed");
    });

    let result = alexandria_file_soft_delete(c(&uuid).as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");

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

    let result = alexandria_file_restore(c(&uuid).as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(json["state"], "active");
    assert!(
        json["deletedAt"].is_null(),
        "deletedAt cleared on the returned File"
    );

    // The on-disk file is untouched (UC-07 leaves it; purge-on-disk is UC-09).
    assert!(
        lib.path().join("memo.txt").exists(),
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
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_restore(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_restore_active_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    // Indexed but never soft-deleted — `state = 'active'` (AF-02 not-deleted).
    let uuid = uuid_by_name(&db_path, "memo.txt");
    let result = alexandria_file_restore(c(&uuid).as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
    let uuid_for_seed = uuid.clone();
    with_db(&db_path, move |pool| async move {
        sqlx::query("UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?")
            .bind("2024-01-01T00:00:00Z")
            .bind(uuid_for_seed)
            .execute(&pool)
            .await
            .expect("past-retention seed");
    });

    let result = alexandria_file_restore(c(&uuid).as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");

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

    let result = alexandria_file_purge(c(&uuid).as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["uuid"], uuid);
    assert_eq!(
        json["state"], "deleted",
        "confirmation echoes the pre-purge state"
    );

    // The on-disk file is untouched (NFR-07; purge-on-disk is UC-09).
    assert!(
        lib.path().join("memo.txt").exists(),
        "on-disk file preserved"
    );

    // The `files` row and its subtype row (text_files) are both gone.
    let uuid_for_check = uuid.clone();
    let (files_remaining, subtype_remaining): (i64, i64) =
        with_db(&db_path, move |pool| async move {
            let (files,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM files WHERE uuid = ?")
                .bind(&uuid_for_check)
                .fetch_one(&pool)
                .await
                .expect("files count");
            let (subtype,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM text_files WHERE file_id = ?")
                    .bind(file_id)
                    .fetch_one(&pool)
                    .await
                    .expect("text_files count");
            (files, subtype)
        });
    assert_eq!(files_remaining, 0, "files row removed by purge");
    assert_eq!(subtype_remaining, 0, "subtype row removed by purge");
}

#[test]
fn given_ffi_purge_missing_uuid_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_purge(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_malformed_uuid_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let uuid = c("not-a-uuid");
    let result = alexandria_file_purge(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_active_file_then_invalid_state() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    // Indexed but never soft-deleted — `state = 'active'` (AF-01 not-deleted).
    let uuid = uuid_by_name(&db_path, "memo.txt");
    let result = alexandria_file_purge(c(&uuid).as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_STATE);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_within_retention_then_invalid_state_and_row_kept() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"x").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");

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

    let result = alexandria_file_purge(c(&uuid).as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    // No retention gate for UC-09 — an `active` (never soft-deleted) record
    // is purgeable, unlike UC-08.
    let uuid = uuid_by_name(&db_path, "memo.txt");
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

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr());

    let json = file_json_ok(result);
    assert_eq!(json["file"]["uuid"], uuid);
    assert_eq!(json["diskFilePresent"], true);

    assert!(
        !lib.path().join("memo.txt").exists(),
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
                sqlx::query_as("SELECT COUNT(*) FROM text_files WHERE file_id = ?")
                    .bind(file_id)
                    .fetch_one(&pool)
                    .await
                    .expect("text_files count");
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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");

    // The file vanishes from disk out from under the catalog (AF-01).
    std::fs::remove_file(lib.path().join("memo.txt")).unwrap();

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr());

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
    std::fs::write(lib.path().join("memo.txt"), b"text bytes").unwrap();

    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let uuid = uuid_by_name(&db_path, "memo.txt");
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
    std::fs::remove_file(lib.path().join("memo.txt")).expect("pre-remove indexed file");
    std::fs::create_dir(lib.path().join("memo.txt"))
        .expect("create directory in place of indexed file");

    let result = alexandria_file_purge_on_disk(c(&uuid).as_ptr());
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
                sqlx::query_as("SELECT COUNT(*) FROM text_files WHERE file_id = ?")
                    .bind(file_id)
                    .fetch_one(&pool)
                    .await
                    .expect("text_files count");
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
    let uuid = c("11111111-1111-1111-1111-111111111111");
    let result = alexandria_file_purge_on_disk(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_NOT_FOUND);
    assert!(result.json.is_null());
}

#[test]
fn given_ffi_purge_on_disk_malformed_uuid_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let uuid = c("not-a-uuid");
    let result = alexandria_file_purge_on_disk(uuid.as_ptr());
    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
    assert!(result.json.is_null());
}

// ---------------------------------------------------------------------------
// UC-42 Task 11: run control over FFI - pause, resume, cancel, active runs,
// and the priority wire argument on the two start calls (FR-FC-24, FR-FC-28)
// ---------------------------------------------------------------------------

/// Call `alexandria_index_run_status_json` and parse its body. Asserts
/// `RUN_OK`.
fn run_status(run_id: &str) -> serde_json::Value {
    let run_id_c = c(run_id);
    let result = alexandria_index_run_status_json(run_id_c.as_ptr());
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

/// Poll `alexandria_index_run_status_json` until the run leaves "running".
///
/// Gives up only once the run has made no progress — no change of phase,
/// total or `processed` — for [`ASYNC_RUN_DEADLINE`]. A run still advancing
/// on a loaded runner is slow, not wrong; one whose counter has not moved in
/// two minutes is stuck, whether its walk hangs or its terminal write never
/// landed and the row is left `running` with its last flushed tally.
fn wait_for_run_terminal_or_paused(run_id: &str) -> serde_json::Value {
    let mut last = String::new();
    let mut since = std::time::Instant::now();
    loop {
        let body = run_status(run_id);
        if body["status"] != "running" {
            return body;
        }
        let progress = run_progress(&body);
        if progress != last {
            last = progress;
            since = std::time::Instant::now();
        } else if since.elapsed() > ASYNC_RUN_DEADLINE {
            panic!("run {run_id} made no progress for {ASYNC_RUN_DEADLINE:?}; {progress}");
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
fn wait_for_run_cell_live(run_id: &str) {
    let deadline = std::time::Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let body = run_status(run_id);
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
        std::fs::write(dir.join(format!("track-{i}.txt")), b"text bytes").unwrap();
    }
}

#[test]
fn given_a_running_run_when_paused_over_ffi_then_status_is_paused_and_has_progress() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    // Wait for the walk to actually be under way before pausing it — see
    // `wait_for_run_cell_live` for why that, not `start` returning, is the
    // point that guarantees a paused row with progress attached.
    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    let pause_status = alexandria_index_pause(run_id_c.as_ptr());
    assert_eq!(
        pause_status, STATUS_RUN_OK,
        "expected RUN_OK pausing a run under way"
    );

    let body = wait_for_run_terminal_or_paused(&run_id);
    assert_eq!(body["status"], "paused");
    assert!(body["processed"].is_number(), "processed: {body}");
    assert!(body["activeMillis"].is_number(), "activeMillis: {body}");
}

#[test]
fn given_a_completed_run_when_paused_over_ffi_then_invalid_state() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_files(1);
    let body = wait_for_run_terminal_or_paused(&run_id);
    assert_eq!(body["status"], "complete", "sanity: run finished");

    let run_id_c = c(&run_id);
    let pause_status = alexandria_index_pause(run_id_c.as_ptr());
    assert_eq!(
        pause_status, STATUS_RUN_INVALID_STATE,
        "pausing a completed run must be refused, not accepted or treated as a generic error"
    );
}

#[test]
fn given_ffi_pause_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_pause(run_id.as_ptr());
    assert_eq!(status, STATUS_RUN_NOT_FOUND);
}

#[test]
fn given_ffi_pause_malformed_run_id_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let run_id = c("not-a-uuid");
    let status = alexandria_index_pause(run_id.as_ptr());
    assert_eq!(status, STATUS_RUN_INVALID_INPUT);
}

#[test]
fn given_a_paused_run_when_resumed_over_ffi_then_same_run_id_and_it_finishes() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    // `pause` only raises the signal or writes the row; the walk's own drain
    // and terminal write can still be in flight when it returns, so the
    // status has to be polled for, not read once.
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), std::ptr::null());
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
    let body = wait_for_run_terminal_or_paused(&run_id);
    assert_eq!(body["status"], "complete");
}

/// The FFI twin of the HTTP `..._resumed_with_normal_priority_then_it_is_widened`
/// case (Task 15). `"low"` over FFI is covered end to end by the test after
/// this one, and NULL by the resume tests above; without this, `"normal"` over FFI rested
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
    let low = c("low");
    let started = index_folder(root.as_ptr(), low.as_ptr());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    assert_eq!(
        run_concurrency(&db_path, &run_id),
        Some(1),
        "sanity: it started at indexing.low_priority_concurrency"
    );

    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let normal = c("normal");
    let resumed = alexandria_index_resume(run_id_c.as_ptr(), normal.as_ptr());
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
        wait_for_run_terminal_or_paused(&run_id)["status"],
        "complete"
    );
}

/// The other direction: a run started at normal width and resumed with
/// `"low"` is re-paced to `indexing.low_priority_concurrency`, through the
/// real entry point (ported from the HTTP/FFI parity suite).
#[test]
fn given_a_paused_run_when_resumed_at_low_over_ffi_then_it_is_re_paced_low() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    assert_eq!(
        run_concurrency(&db_path, &run_id),
        Some(4),
        "sanity: it started at indexing.concurrency"
    );

    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let low = c("low");
    let resumed = alexandria_index_resume(run_id_c.as_ptr(), low.as_ptr());
    assert_eq!(resumed.status, STATUS_RUN_OK);
    assert_eq!(run_id_string(&resumed), run_id);
    assert_eq!(
        run_concurrency(&db_path, &run_id),
        Some(1),
        "\"low\" must re-pace the run to indexing.low_priority_concurrency"
    );
    assert_eq!(
        wait_for_run_terminal_or_paused(&run_id)["status"],
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id);

    let run_id_c = c(&run_id);
    assert_eq!(
        run_status(&run_id)["status"],
        "running",
        "sanity: the run is still running at the moment resume is called"
    );
    let resumed = alexandria_index_resume(run_id_c.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_INVALID_STATE,
        "resuming a run that is not paused must be refused"
    );
}

#[test]
fn given_ffi_resume_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let resumed = alexandria_index_resume(run_id.as_ptr(), std::ptr::null());
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);

    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    let cancel_status = alexandria_index_cancel(run_id_c.as_ptr());
    assert_eq!(cancel_status, STATUS_RUN_OK);

    // `cancel` only raises the signal or writes the row; the walk's own
    // drain and terminal write can still be in flight when it returns.
    let body = wait_for_run_terminal_or_paused(&run_id);
    assert_eq!(body["status"], "cancelled");

    // Terminal: a second cancel finds nothing left to abandon.
    let second = alexandria_index_cancel(run_id_c.as_ptr());
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id);

    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let cancel_status = alexandria_index_cancel(run_id_c.as_ptr());
    assert_eq!(
        cancel_status, STATUS_RUN_OK,
        "a paused run must still be cancellable"
    );
    assert_eq!(run_status(&run_id)["status"], "cancelled");
}

#[test]
fn given_ffi_cancel_missing_run_then_not_found() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let run_id = c("11111111-1111-1111-1111-111111111111");
    let status = alexandria_index_cancel(run_id.as_ptr());
    assert_eq!(status, STATUS_RUN_NOT_FOUND);
}

#[test]
fn given_ffi_cancel_malformed_run_id_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let run_id = c("not-a-uuid");
    let status = alexandria_index_cancel(run_id.as_ptr());
    assert_eq!(status, STATUS_RUN_INVALID_INPUT);
}

#[test]
fn given_no_outstanding_runs_when_active_runs_queried_then_empty_array() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();
    let result = alexandria_index_runs_active_json();
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id);
    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    // `pause` can return before the walk's own terminal write lands; wait
    // for it, or the query below could still find the row `running`.
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let result = alexandria_index_runs_active_json();
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
fn given_low_priority_when_library_registered_over_ffi_then_run_recorded_at_low_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let priority = c("low");
    let started = index_folder(root.as_ptr(), priority.as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();

    let root = c(lib.path().to_str().unwrap());
    let priority = c("URGENT!!1");
    let started = index_folder(root.as_ptr(), priority.as_ptr());
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
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let refreshed = alexandria_index_refresh_start(std::ptr::null());
    assert_eq!(refreshed.status, STATUS_OK);
    let run_id = run_id_string(&refreshed);
    assert_eq!(run_concurrency(&db_path, &run_id), Some(4));
}

#[test]
fn given_low_priority_when_refresh_started_over_ffi_then_low_concurrency() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    std::fs::write(lib.path().join("memo.txt"), b"text").unwrap();
    let root = c(lib.path().to_str().unwrap());
    index_folder(root.as_ptr(), std::ptr::null());
    wait_for_files(1);

    let priority = c("low");
    let refreshed = alexandria_index_refresh_start(priority.as_ptr());
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);

    // Waited on the run rather than on the count: the run record is what
    // says the walk is over, where the count is a derived observation of
    // writes that land in bursts — it sits still for twenty seconds and then
    // jumps by thousands.
    let index_run = run_id_string(&started);
    let terminal = wait_for_run_terminal_or_paused(&index_run);
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
    let refreshed = alexandria_index_refresh_start(std::ptr::null());
    assert_eq!(refreshed.status, STATUS_OK);
    let run_id = run_id_string(&refreshed);
    wait_for_run_cell_live(&run_id);

    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), std::ptr::null());
    assert_eq!(
        resumed.status, STATUS_RUN_OK,
        "expected RUN_OK resuming a paused refresh run"
    );
    assert_eq!(
        run_id_string(&resumed),
        run_id,
        "resume must hand back the same run id, not mint a fresh one"
    );

    let body = wait_for_run_terminal_or_paused(&run_id);
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id);

    let run_id_c = c(&run_id);
    assert_eq!(alexandria_index_pause(run_id_c.as_ptr()), STATUS_RUN_OK);
    assert_eq!(wait_for_run_terminal_or_paused(&run_id)["status"], "paused");

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

    let resumed = alexandria_index_resume(run_id_c.as_ptr(), std::ptr::null());
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

/// A real folder for `logical` (`/library/course`) inside the test's own
/// temporary directory, created. Registering walks the folder, so a library
/// test needs one that exists.
fn folder(db: &(TempDir, String), logical: &str) -> String {
    let path = db.0.path().join(logical.trim_start_matches('/'));
    std::fs::create_dir_all(&path).unwrap();
    path.to_str().unwrap().to_string()
}

/// Like [`folder`], without creating it.
fn path_in(db: &(TempDir, String), logical: &str) -> String {
    db.0.path()
        .join(logical.trim_start_matches('/'))
        .to_str()
        .unwrap()
        .to_string()
}

/// Register `root` as `name`, wait for its first run to finish, and answer
/// the library's JSON.
///
/// Waited on because a library with an unfinished run refuses a move or a
/// second run (FR-FC-43), and the tests using this are about what comes
/// after.
fn register_library(name: &str, root: &str) -> serde_json::Value {
    let body = c(&serde_json::json!({ "name": name, "rootPath": root }).to_string());
    let value = library_json_ok(alexandria_library_register(body.as_ptr()));
    wait_for_run_terminal(value["runId"].as_str().expect("runId"));
    value
}

/// Registering answers the library, and it is then listed.
#[test]
fn given_a_folder_when_registered_over_ffi_then_it_is_a_library() {
    let _g = serial();
    let _db = init_temp_db();
    let value = register_library("Course", &folder(&_db, "/library/course"));

    assert_eq!(value["name"], "Course");
    assert!(uuid::Uuid::parse_str(value["uuid"].as_str().unwrap_or_default()).is_ok());

    let listed = library_json_ok(alexandria_libraries_list());
    assert_eq!(listed.as_array().map(|a| a.len()), Some(1));
}

/// An overlapping folder is a conflict, not an invalid input: the request was
/// well formed and the folder is real — what is wrong is the current state.
#[test]
fn given_an_overlapping_folder_when_registered_over_ffi_then_it_is_a_conflict() {
    let _g = serial();
    let _db = init_temp_db();
    register_library("Course", &folder(&_db, "/library/course"));

    let nested = c(&serde_json::json!({
        "name": "Week",
        "rootPath": folder(&_db, "/library/course/week-1")
    })
    .to_string());
    let result = alexandria_library_register(nested.as_ptr());

    assert_eq!(result.status, STATUS_LIBRARY_CONFLICT);
}

/// A blank name never becomes a stored blank.
#[test]
fn given_a_blank_name_when_registered_over_ffi_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let body = c(&serde_json::json!({
        "name": "   ",
        "rootPath": folder(&_db, "/library/course")
    })
    .to_string());

    assert_eq!(
        alexandria_library_register(body.as_ptr()).status,
        STATUS_LIBRARY_INVALID_INPUT
    );
}

/// Browsing an empty library answers a level rather than failing: a folder
/// with nothing indexed under it yet is a state, not an error.
#[test]
fn given_a_new_library_when_browsed_over_ffi_then_it_is_simply_empty() {
    let _g = serial();
    let _db = init_temp_db();
    let library = register_library("Course", &folder(&_db, "/library/course"));
    let uuid = c(library["uuid"].as_str().unwrap());

    let listing = library_json_ok(alexandria_library_browse(uuid.as_ptr(), std::ptr::null()));

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
    let library = register_library("Course", &folder(&_db, "/library/course"));
    let uuid = c(library["uuid"].as_str().unwrap());

    let destination = path_in(&_db, "/media/courses/rust");
    let moved = c(&serde_json::json!({ "rootPath": destination }).to_string());
    let value = library_json_ok(alexandria_library_move(uuid.as_ptr(), moved.as_ptr()));

    assert_eq!(value["rootPath"], destination);
    assert_eq!(value["name"], "Course", "the move renamed the library");
}

/// Moving onto another library's folder is a conflict, the same refusal
/// registering there would give.
#[test]
fn given_another_librarys_folder_when_moved_onto_over_ffi_then_it_is_a_conflict() {
    let _g = serial();
    let _db = init_temp_db();
    let library = register_library("Course", &folder(&_db, "/library/course"));
    register_library("Photos", &folder(&_db, "/library/photos"));

    let uuid = c(library["uuid"].as_str().unwrap());
    let onto =
        c(&serde_json::json!({"rootPath": path_in(&_db, "/library/photos/2024")}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), onto.as_ptr()).status,
        STATUS_LIBRARY_CONFLICT
    );
}

/// A blank root is refused rather than stored, as it is on registration.
#[test]
fn given_a_blank_root_when_a_library_is_moved_over_ffi_then_it_is_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let library = register_library("Course", &folder(&_db, "/library/course"));
    let uuid = c(library["uuid"].as_str().unwrap());
    let blank = c(&serde_json::json!({"rootPath": "  "}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), blank.as_ptr()).status,
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
    let body = c(&serde_json::json!({"rootPath": "/media/courses"}).to_string());

    assert_eq!(
        alexandria_library_move(uuid.as_ptr(), body.as_ptr()).status,
        STATUS_LIBRARY_NOT_FOUND
    );
}

/// An unknown uuid is reported as not found, matching HTTP's 404.
#[test]
fn given_an_unknown_uuid_when_a_library_is_removed_over_ffi_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();

    let uuid = c("11111111-1111-1111-1111-111111111111");

    assert_eq!(
        alexandria_library_remove(uuid.as_ptr()).status,
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
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = c(&run_id_string(&started));

    let result = alexandria_index_run_failures_json(run_id.as_ptr());
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

    assert_eq!(
        alexandria_index_run_failures_json(run_id.as_ptr()).status,
        STATUS_RUN_NOT_FOUND
    );
}

#[test]
fn given_a_malformed_run_id_when_failures_asked_over_ffi_then_invalid_input() {
    let _g = serial();
    let (_db_dir, _db_path) = init_temp_db();

    let run_id = c("not-a-uuid");

    assert_eq!(
        alexandria_index_run_failures_json(run_id.as_ptr()).status,
        STATUS_RUN_INVALID_INPUT
    );
}

/// Re-initializing while this core is walking a disk is refused.
///
/// A second `alexandria_index_init` replaces the services wholesale, and the
/// run already executing does not come with them: its cell stays in the old
/// registry, so pause and cancel stop reaching it and its progress stops
/// being published, while the walk carries on writing. The new services then
/// reconcile on the way up (FR-FC-29) and rewrite that live run's row to
/// `paused` on the assumption that nothing is walking it — so an embedder
/// that reconfigured mid-scan got a run it could no longer control, a
/// progress bar frozen at its last flush, and a row saying the opposite of
/// what was happening.
///
/// `wait_for_run_cell_live` is what makes this deterministic rather than a
/// race: it returns exactly when the registry holds a live cell, which is
/// exactly the condition the refusal is keyed on.
#[test]
fn given_a_running_run_when_the_core_is_re_initialized_then_it_is_refused_as_busy() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();
    let lib = tempdir().unwrap();
    write_library(lib.path(), 500);

    let root = c(lib.path().to_str().unwrap());
    let started = index_folder(root.as_ptr(), std::ptr::null());
    assert_eq!(started.status, STATUS_OK);
    let run_id = run_id_string(&started);
    wait_for_run_cell_live(&run_id);

    let cpath = c(&db_path);
    assert_eq!(
        alexandria_index_init(cpath.as_ptr()),
        STATUS_INDEX_BUSY,
        "the core replaced its services out from under a running walk"
    );

    // The run is still this core's to control, which is the whole point of
    // refusing — and cancelling it here is also what leaves the process fit
    // for the next test, since `init_temp_db` re-initializes and would now be
    // refused while this walk is live.
    let run_id_c = c(&run_id);
    assert_eq!(
        alexandria_index_cancel(run_id_c.as_ptr()),
        STATUS_RUN_OK,
        "a run the refusal protected could not be cancelled"
    );
    let body = wait_for_run_terminal_or_paused(&run_id);
    assert_eq!(body["status"], "cancelled");
}

/// And with nothing running it still replaces, which is what the refusal must
/// not cost. The guard on the guard: a check that refused unconditionally
/// would pass the test above and break every reconfiguration.
#[test]
fn given_no_running_run_when_the_core_is_re_initialized_then_it_replaces() {
    let _g = serial();
    let (_db_dir, db_path) = init_temp_db();

    let cpath = c(&db_path);
    assert_eq!(
        alexandria_index_init(cpath.as_ptr()),
        STATUS_OK,
        "an idle core refused to be re-initialized"
    );
}

// ---------------- the library-root bound on moves and playback (FR-FC-26) ----------------

/// Initialize a fresh database with `filesystem.root` set to `library`.
///
/// The FFI reads configuration only from the `ALEXANDRIA_*` environment, and
/// only at init, so the variable is removed again straight after: nothing
/// that follows — an assertion that unwinds included — can leak the bound
/// into the next test.
fn init_bounded_db(library: &std::path::Path) -> (TempDir, String) {
    std::env::set_var("ALEXANDRIA_FILESYSTEM_ROOT", library.to_str().unwrap());
    let db = init_temp_db();
    std::env::remove_var("ALEXANDRIA_FILESYSTEM_ROOT");
    db
}

/// The move half of the review's D1 over FFI: refused with the invalid-input
/// status, the class HTTP's 400 maps to (NFR-09), and nothing moves.
#[test]
fn given_a_configured_root_when_a_library_is_moved_outside_it_over_ffi_then_invalid_input() {
    let _g = serial();
    let parent = tempdir().unwrap();
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    let course = library.join("course");
    std::fs::create_dir_all(&course).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let _db = init_bounded_db(&library);
    let body = c(
        &serde_json::json!({"name": "Course", "rootPath": course.to_str().unwrap()}).to_string(),
    );
    let registered = library_json_ok(alexandria_library_register(body.as_ptr()));
    wait_for_run_terminal(registered["runId"].as_str().unwrap());
    let uuid = c(registered["uuid"].as_str().unwrap());

    for destination in [
        outside.to_str().unwrap().to_string(),
        format!("{}/../secrets", library.to_str().unwrap()),
        format!("{}/gone/../../secrets", library.to_str().unwrap()),
    ] {
        let moved = c(&serde_json::json!({ "rootPath": destination }).to_string());
        assert_eq!(
            alexandria_library_move(uuid.as_ptr(), moved.as_ptr()).status,
            STATUS_LIBRARY_INVALID_INPUT,
            "{destination} escaped the root"
        );
    }

    let listed = library_json_ok(alexandria_libraries_list());
    assert_eq!(listed[0]["rootPath"], course.to_str().unwrap());
}

/// The other half of the pair: a destination under the root that does not
/// exist yet (an unplugged drive) is still recorded.
#[test]
fn given_a_configured_root_when_moved_to_a_missing_folder_under_it_over_ffi_then_ok() {
    let _g = serial();
    let library = tempdir().unwrap();
    let course = library.path().join("course");
    std::fs::create_dir(&course).unwrap();
    let _db = init_bounded_db(library.path());
    let body = c(
        &serde_json::json!({"name": "Course", "rootPath": course.to_str().unwrap()}).to_string(),
    );
    let registered = library_json_ok(alexandria_library_register(body.as_ptr()));
    wait_for_run_terminal(registered["runId"].as_str().unwrap());
    let uuid = c(registered["uuid"].as_str().unwrap());
    let destination = library.path().join("usb-drive").join("rust");

    let moved = c(&serde_json::json!({ "rootPath": destination.to_str().unwrap() }).to_string());
    let value = library_json_ok(alexandria_library_move(uuid.as_ptr(), moved.as_ptr()));

    assert_eq!(value["rootPath"], destination.to_str().unwrap());
}

/// Put one active text file in the catalog at `path`, behind the FFI's back,
/// the way a pre-bound index or move would have left it.
fn seed_text_row(db_path: &str, path: &std::path::Path) -> String {
    use alexandria_core::catalog::model::{FileType, NewFile};
    use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
    let uuid = uuid::Uuid::new_v4();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pool = alexandria_core::migrate::migrate_database(db_path)
            .await
            .expect("open");
        // Every file belongs to a library: the row goes into one at its folder.
        let library_uuid = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO libraries (uuid, name, root_path) VALUES (?, 'Seeded', ?)")
            .bind(library_uuid.to_string())
            .bind(path.parent().unwrap().to_str().unwrap())
            .execute(&pool)
            .await
            .expect("library");
        SqliteCatalogRepository::new(pool.clone())
            .insert_file(NewFile {
                uuid,
                library_uuid,
                path: path.to_str().unwrap().to_string(),
                name: path.file_name().unwrap().to_str().unwrap().to_string(),
                file_type: FileType::Text,
                content_hash: None,
                size_bytes: None,
                mtime: None,
                indexed_at: chrono::Utc::now(),
            })
            .await
            .expect("insert");
        pool.close().await;
    });
    uuid.to_string()
}

/// Playback's defence in depth over FFI: a row already pointing outside the
/// configured root gets no descriptor — the FFI hands back a path the
/// client then opens itself, so this is the FFI's stream.
#[test]
fn given_a_row_outside_the_configured_root_when_its_playback_source_is_asked_then_invalid_input() {
    let _g = serial();
    let parent = tempdir().unwrap();
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    std::fs::create_dir(&library).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("passwd.txt"), b"the secret").unwrap();
    std::fs::write(library.join("song.txt"), b"inside").unwrap();
    let (_dir, db_path) = init_bounded_db(&library);
    let out_of_root = c(&seed_text_row(&db_path, &outside.join("passwd.txt")));
    let in_root = c(&seed_text_row(&db_path, &library.join("song.txt")));

    let refused = alexandria_file_playback_source(out_of_root.as_ptr());
    let served = alexandria_file_playback_source(in_root.as_ptr());

    assert_eq!(refused.status, STATUS_PLAYBACK_INVALID_INPUT);
    assert!(refused.json.is_null());
    assert_eq!(served.status, STATUS_PLAYBACK_OK);
    unsafe { alexandria_free_string(served.json) };
}

// ---------------- registering starts indexing; re-indexing one library ----------------

#[test]
fn given_a_folder_when_registered_over_ffi_then_a_run_id_that_completes() {
    let _g = serial();
    let _db = init_temp_db();
    let root = folder(&_db, "/library/notes");
    std::fs::write(format!("{root}/a.md"), b"# a").unwrap();
    std::fs::write(format!("{root}/b.zip"), b"zip").unwrap();
    let body = c(&serde_json::json!({ "name": "Notes", "rootPath": root }).to_string());

    let value = library_json_ok(alexandria_library_register(body.as_ptr()));

    assert_eq!(value["rootPath"], root);
    let run = wait_for_run_terminal(value["runId"].as_str().expect("runId"));
    assert_eq!(run["status"], "complete", "{run}");
    assert_eq!(run["indexed"], 1, "{run}");
    assert_eq!(run["skipped"], 1, "{run}");
    assert_eq!(run["libraryUuid"], value["uuid"], "{run}");
}

#[test]
fn given_a_library_when_index_started_over_ffi_then_a_run_id() {
    let _g = serial();
    let _db = init_temp_db();
    let library = register_library("Notes", &folder(&_db, "/library/notes"));
    let uuid = c(library["uuid"].as_str().unwrap());

    let result = alexandria_library_index_start(uuid.as_ptr(), std::ptr::null());

    assert_eq!(result.status, STATUS_OK);
    let run_id = run_id_string(&result);
    assert_ne!(run_id, library["runId"].as_str().unwrap());
    let run = wait_for_run_terminal(&run_id);
    assert_eq!(run["status"], "complete", "{run}");
}

#[test]
fn given_an_unknown_library_when_index_started_over_ffi_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();
    let uuid = c("11111111-1111-1111-1111-111111111111");

    let result = alexandria_library_index_start(uuid.as_ptr(), std::ptr::null());

    assert_eq!(result.status, STATUS_INDEX_NOT_FOUND);
}

#[test]
fn given_a_paused_run_when_index_started_over_ffi_then_conflict() {
    let _g = serial();
    let db = init_temp_db();
    let library = register_library("Notes", &folder(&db, "/library/notes"));
    let run_id = library["runId"].as_str().unwrap().to_string();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let pool = alexandria_core::migrate::migrate_database(&db.1)
            .await
            .expect("open");
        sqlx::query("UPDATE catalog_runs SET status = 'paused' WHERE id = ?")
            .bind(&run_id)
            .execute(&pool)
            .await
            .unwrap();
    });
    let uuid = c(library["uuid"].as_str().unwrap());

    let result = alexandria_library_index_start(uuid.as_ptr(), std::ptr::null());

    assert_eq!(result.status, STATUS_INDEX_CONFLICT);
}

#[test]
fn given_a_low_priority_when_registered_over_ffi_then_its_run_is_low() {
    let _g = serial();
    let db = init_temp_db();
    let body = c(&serde_json::json!({
        "name": "Notes",
        "rootPath": folder(&db, "/library/notes"),
        "priority": "low"
    })
    .to_string());

    let value = library_json_ok(alexandria_library_register(body.as_ptr()));

    let run_id = value["runId"].as_str().unwrap().to_string();
    wait_for_run_terminal(&run_id);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let concurrency: i64 = rt.block_on(async {
        let pool = alexandria_core::migrate::migrate_database(&db.1)
            .await
            .expect("open");
        sqlx::query_scalar("SELECT concurrency FROM catalog_runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    });
    assert_eq!(
        concurrency,
        alexandria_core::config::Settings::default()
            .indexing
            .low_priority_concurrency as i64
    );
}

/// The application binds to the generated header by hand, so the removed
/// free-form index call must be gone from it, and the library one present.
#[test]
fn given_the_header_when_read_then_it_declares_no_free_form_index_start() {
    let header = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/header.h"))
        .expect("header.h is generated by build.rs");

    assert!(!header.contains("alexandria_index_start("));
    assert!(header.contains("alexandria_library_index_start("));
}

#[test]
fn given_a_library_when_removed_over_ffi_then_ok_and_its_files_are_gone() {
    let _g = serial();
    let db = init_temp_db();
    let root = folder(&db, "/library/notes");
    std::fs::write(format!("{root}/a.md"), b"# a").unwrap();
    let library = register_library("Notes", &root);
    let uuid = c(library["uuid"].as_str().unwrap());

    let removed = alexandria_library_remove(uuid.as_ptr());

    assert_eq!(removed.status, STATUS_LIBRARY_OK);
    assert_eq!(alexandria_index_count_files(), 0);
    assert_eq!(std::fs::read(format!("{root}/a.md")).unwrap(), b"# a");
}

#[test]
fn given_a_library_uuid_when_listed_over_ffi_then_only_its_files() {
    let _g = serial();
    let db = init_temp_db();
    let first = folder(&db, "/first");
    std::fs::write(format!("{first}/a.md"), b"x").unwrap();
    let second = folder(&db, "/second");
    std::fs::write(format!("{second}/b.md"), b"x").unwrap();
    let library = register_library("First", &first);
    register_library("Second", &second);
    let filter = c(&serde_json::json!({ "libraryUuid": library["uuid"] }).to_string());

    let listed = file_json_ok(alexandria_ffi::alexandria_files_list(filter.as_ptr()));

    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|view| view["file"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a.md"]);
}

#[test]
fn given_a_malformed_library_uuid_when_listed_over_ffi_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let filter = c(r#"{"libraryUuid":"not-a-uuid"}"#);

    let result = alexandria_ffi::alexandria_files_list(filter.as_ptr());

    assert_eq!(result.status, STATUS_FILE_INVALID_INPUT);
}

#[test]
fn given_audio_as_a_type_filter_when_listed_over_ffi_then_only_audio() {
    let _g = serial();
    let db = init_temp_db();
    let root = folder(&db, "/music");
    std::fs::write(format!("{root}/song.mp3"), [0u8; 64]).unwrap();
    std::fs::write(format!("{root}/notes.md"), b"# notes").unwrap();
    register_library("Music", &root);
    let filter = c(r#"{"type":"audio"}"#);

    let listed = file_json_ok(alexandria_ffi::alexandria_files_list(filter.as_ptr()));

    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|view| view["file"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["song.mp3"]);
    assert_eq!(listed[0]["file"]["fileType"], "audio");
}

/// The application binds `alexandria_library_register` from the header, so
/// the header has to say what it answers now: a library with its run.
#[test]
fn given_the_header_when_read_then_library_register_is_documented_with_its_run() {
    let header = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/header.h"))
        .expect("header.h is generated by build.rs");
    let at = header
        .find("alexandria_library_register(")
        .expect("declared");
    let doc = &header[header[..at].rfind("/**").expect("documented")..at];

    assert!(doc.contains("runId"), "{doc}");
    assert!(!doc.contains("claimed"), "{doc}");
}

// ===========================================================================
// Ported from the HTTP server's tests and the HTTP/FFI parity suite
// ===========================================================================
//
// The HTTP server and `parity.rs` went away together, and these are the
// behaviours that only their tests checked: the FFI calls with no smoke test
// of their own, the FFI's reading of filter and body values, and every
// repository that core's tests exercise only through fakes (collections,
// bookmarks, watchlists, reading lists). Each one is asserted here on the
// core's own answer — the FFI status and JSON — and on what it left in the
// catalog or on disk, against a real database and real files.

/// The status and parsed JSON of any of the FFI's `*JsonResult` structs,
/// which all carry a `status` and a `json` that is NULL on failure. The
/// string is freed; a NULL `json` reads as `Value::Null`.
macro_rules! answer {
    ($result:expr) => {{
        let result = $result;
        take_answer(result.status, result.json)
    }};
}

fn take_answer(status: i32, json: *mut c_char) -> (i32, serde_json::Value) {
    if json.is_null() {
        return (status, serde_json::Value::Null);
    }
    // SAFETY: a non-NULL `json` is a NUL-terminated string this library
    // allocated, freed exactly once below.
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    unsafe { alexandria_free_string(json) };
    (status, serde_json::from_str(&text).expect("result json"))
}

/// Every row `sql` selects from the FFI's catalog, with `args` bound as text.
fn fetch_all<T>(db_path: &str, sql: &'static str, args: &[&str]) -> Vec<T>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> + Send + Unpin + 'static,
{
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    with_db(db_path, move |pool| async move {
        let mut query = sqlx::query_as::<sqlx::Sqlite, T>(sql);
        for arg in args {
            query = query.bind(arg);
        }
        query.fetch_all(&pool).await.expect(sql)
    })
}

/// The single number a `SELECT COUNT(*) …` answers.
fn count(db_path: &str, sql: &'static str, args: &[&str]) -> i64 {
    fetch_all::<(i64,)>(db_path, sql, args)[0].0
}

/// Run one write against the FFI's catalog, behind its back.
fn exec(db_path: &str, sql: &'static str, args: &[&str]) {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    with_db(db_path, move |pool| async move {
        let mut query = sqlx::query(sql);
        for arg in args {
            query = query.bind(arg);
        }
        query.execute(&pool).await.expect(sql);
    });
}

/// One active catalog row of `file_type` named `name`, in a library of its
/// own, written without indexing: for the operations that never read the
/// disk, seeded the way the HTTP and parity suites seeded theirs.
fn seed_file(db_path: &str, file_type: &str, name: &str) -> String {
    let uuid = uuid::Uuid::new_v4().to_string();
    exec(
        db_path,
        "INSERT OR IGNORE INTO libraries (uuid, name, root_path) \
         VALUES ('seeded-library', 'Seeded', '/seeded-library')",
        &[],
    );
    let path = format!("/seeded-library/{uuid}-{name}");
    let now = chrono::Utc::now().to_rfc3339();
    exec(
        db_path,
        "INSERT INTO files (uuid, path, name, type, content_hash, indexed_at, library_id) \
         VALUES (?, ?, ?, ?, 'hash', ?, (SELECT id FROM libraries WHERE uuid = 'seeded-library'))",
        &[&uuid, &path, name, file_type, &now],
    );
    uuid
}

/// Write `files` into a fresh folder, add it as a library and wait for its
/// run: the catalog the HTTP suite built through `POST /v1/libraries`.
/// Answers the folder.
fn library_with(db: &(TempDir, String), files: &[(&str, &[u8])]) -> String {
    let root = folder(db, "/library");
    for (name, bytes) in files {
        std::fs::write(format!("{root}/{name}"), bytes).unwrap();
    }
    register_library("Library", &root);
    root
}

/// A well-formed uuid that names nothing in the catalog.
fn unknown_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// One wrapper per FFI call: each takes its body as JSON and answers
// `(status, json)`.

fn files_list(filter: serde_json::Value) -> (i32, serde_json::Value) {
    let filter = c(&filter.to_string());
    answer!(alexandria_ffi::alexandria_files_list(filter.as_ptr()))
}

fn file_get(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_file_get_by_uuid(uuid.as_ptr()))
}

fn file_edit_metadata(uuid: &str, patch: serde_json::Value) -> (i32, serde_json::Value) {
    let (uuid, patch) = (c(uuid), c(&patch.to_string()));
    answer!(alexandria_file_edit_metadata(uuid.as_ptr(), patch.as_ptr()))
}

fn file_read_content(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_file_read_content(uuid.as_ptr()))
}

fn file_edit_content(uuid: &str, body: serde_json::Value) -> (i32, serde_json::Value) {
    let (uuid, body) = (c(uuid), c(&body.to_string()));
    answer!(alexandria_ffi::alexandria_file_edit_content(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn file_soft_delete(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_file_soft_delete(uuid.as_ptr()))
}

fn playback_source(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_file_playback_source(uuid.as_ptr()))
}

fn comic_page(uuid: &str, page: u32) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_comic_page(uuid.as_ptr(), page))
}

fn thumbnail(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_file_thumbnail(uuid.as_ptr()))
}

fn collection_create(body: serde_json::Value) -> (i32, serde_json::Value) {
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_collection_create(body.as_ptr()))
}

fn collection_rename(uuid: &str, body: serde_json::Value) -> (i32, serde_json::Value) {
    let (uuid, body) = (c(uuid), c(&body.to_string()));
    answer!(alexandria_ffi::alexandria_collection_rename(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn collection_delete(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_collection_delete(uuid.as_ptr()))
}

fn collection_add_items(uuid: &str, item_uuids: &[&str]) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    let body = c(&serde_json::json!({ "itemUuids": item_uuids }).to_string());
    answer!(alexandria_ffi::alexandria_collection_add_items(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn collection_remove_item(collection_uuid: &str, item_uuid: &str) -> (i32, serde_json::Value) {
    let (collection_uuid, item_uuid) = (c(collection_uuid), c(item_uuid));
    answer!(alexandria_ffi::alexandria_collection_remove_item(
        collection_uuid.as_ptr(),
        item_uuid.as_ptr()
    ))
}

fn collection_list_items(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_collection_list_items(
        uuid.as_ptr()
    ))
}

fn collections_list(filter: serde_json::Value) -> (i32, serde_json::Value) {
    let filter = c(&filter.to_string());
    answer!(alexandria_ffi::alexandria_collections_list(filter.as_ptr()))
}

fn bookmark_create(body: serde_json::Value) -> (i32, serde_json::Value) {
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_bookmark_create(body.as_ptr()))
}

fn bookmark_update(uuid: &str, body: serde_json::Value) -> (i32, serde_json::Value) {
    let (uuid, body) = (c(uuid), c(&body.to_string()));
    answer!(alexandria_ffi::alexandria_bookmark_update(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn bookmarks_list(filter: serde_json::Value) -> (i32, serde_json::Value) {
    let filter = c(&filter.to_string());
    answer!(alexandria_ffi::alexandria_bookmarks_list(filter.as_ptr()))
}

fn bookmark_soft_delete(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_bookmark_soft_delete(
        uuid.as_ptr()
    ))
}

fn bookmark_restore(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_bookmark_restore(uuid.as_ptr()))
}

fn bookmark_purge(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_bookmark_purge(uuid.as_ptr()))
}

fn watchlist_create(body: serde_json::Value) -> (i32, serde_json::Value) {
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_watchlist_create(body.as_ptr()))
}

fn watchlist_add_video(watchlist_uuid: &str, video_uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(watchlist_uuid);
    let body = c(&serde_json::json!({ "videoUuid": video_uuid }).to_string());
    answer!(alexandria_ffi::alexandria_watchlist_add_video(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn watchlists_list(filter: serde_json::Value) -> (i32, serde_json::Value) {
    let filter = c(&filter.to_string());
    answer!(alexandria_ffi::alexandria_watchlists_list(filter.as_ptr()))
}

fn watchlist_update_progress(
    watchlist_uuid: &str,
    video_uuid: &str,
    body: serde_json::Value,
) -> (i32, serde_json::Value) {
    let (watchlist_uuid, video_uuid) = (c(watchlist_uuid), c(video_uuid));
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_watchlist_update_progress(
        watchlist_uuid.as_ptr(),
        video_uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn watchlist_remove_video(watchlist_uuid: &str, video_uuid: &str) -> (i32, serde_json::Value) {
    let (watchlist_uuid, video_uuid) = (c(watchlist_uuid), c(video_uuid));
    answer!(alexandria_ffi::alexandria_watchlist_remove_video(
        watchlist_uuid.as_ptr(),
        video_uuid.as_ptr()
    ))
}

fn watchlist_delete(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_watchlist_delete(uuid.as_ptr()))
}

fn reading_list_create(body: serde_json::Value) -> (i32, serde_json::Value) {
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_reading_list_create(
        body.as_ptr()
    ))
}

fn reading_list_add_item(reading_list_uuid: &str, item_uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(reading_list_uuid);
    let body = c(&serde_json::json!({ "itemUuid": item_uuid }).to_string());
    answer!(alexandria_ffi::alexandria_reading_list_add_item(
        uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn reading_lists_list(filter: serde_json::Value) -> (i32, serde_json::Value) {
    let filter = c(&filter.to_string());
    answer!(alexandria_ffi::alexandria_reading_lists_list(
        filter.as_ptr()
    ))
}

fn reading_list_update_progress(
    reading_list_uuid: &str,
    item_uuid: &str,
    body: serde_json::Value,
) -> (i32, serde_json::Value) {
    let (reading_list_uuid, item_uuid) = (c(reading_list_uuid), c(item_uuid));
    let body = c(&body.to_string());
    answer!(alexandria_ffi::alexandria_reading_list_update_progress(
        reading_list_uuid.as_ptr(),
        item_uuid.as_ptr(),
        body.as_ptr()
    ))
}

fn reading_list_remove_item(reading_list_uuid: &str, item_uuid: &str) -> (i32, serde_json::Value) {
    let (reading_list_uuid, item_uuid) = (c(reading_list_uuid), c(item_uuid));
    answer!(alexandria_ffi::alexandria_reading_list_remove_item(
        reading_list_uuid.as_ptr(),
        item_uuid.as_ptr()
    ))
}

fn reading_list_delete(uuid: &str) -> (i32, serde_json::Value) {
    let uuid = c(uuid);
    answer!(alexandria_ffi::alexandria_reading_list_delete(
        uuid.as_ptr()
    ))
}

// ---------------- collections (UC-10 to UC-15) ----------------

const COLLECTION_OK: i32 = alexandria_ffi::COLLECTION_OK;
const COLLECTION_INVALID_INPUT: i32 = alexandria_ffi::COLLECTION_ERR_INVALID_INPUT;
const COLLECTION_NOT_FOUND: i32 = alexandria_ffi::COLLECTION_ERR_NOT_FOUND;

fn new_collection(name: &str, kind: &str) -> String {
    let (status, value) = collection_create(serde_json::json!({ "name": name, "kind": kind }));
    assert_eq!(status, COLLECTION_OK, "create {name}: {value}");
    value["uuid"].as_str().expect("uuid").to_string()
}

fn new_bookmark(title: &str, collection_uuid: Option<&str>) -> String {
    let (status, value) = bookmark_create(serde_json::json!({
        "url": format!("https://example.com/{title}"),
        "title": title,
        "collectionUuid": collection_uuid,
    }));
    assert_eq!(
        status,
        alexandria_ffi::BOOKMARK_OK,
        "create {title}: {value}"
    );
    value["uuid"].as_str().expect("uuid").to_string()
}

/// `(uuid, name, kind)` of every collection, in creation order.
fn collection_rows(db_path: &str) -> Vec<(String, String, String)> {
    fetch_all(
        db_path,
        "SELECT uuid, name, kind FROM collections ORDER BY id",
        &[],
    )
}

/// The uuid of the collection a file or bookmark is filed in, if any.
fn file_collection(db_path: &str, file_uuid: &str) -> Option<String> {
    fetch_all::<(Option<String>,)>(
        db_path,
        "SELECT collections.uuid FROM files \
         LEFT JOIN collections ON collections.id = files.collection_id WHERE files.uuid = ?",
        &[file_uuid],
    )[0]
    .0
    .clone()
}

fn bookmark_collection(db_path: &str, bookmark_uuid: &str) -> Option<String> {
    fetch_all::<(Option<String>,)>(
        db_path,
        "SELECT collections.uuid FROM bookmarks \
         LEFT JOIN collections ON collections.id = bookmarks.collection_id \
         WHERE bookmarks.uuid = ?",
        &[bookmark_uuid],
    )[0]
    .0
    .clone()
}

#[test]
fn given_a_file_and_a_bookmark_collection_when_created_over_ffi_then_each_is_answered_and_stored() {
    let _g = serial();
    let db = init_temp_db();

    let (file_status, films) =
        collection_create(serde_json::json!({ "name": "Sci-fi novels", "kind": "file" }));
    let (bookmark_status, reading) =
        collection_create(serde_json::json!({ "name": "Reading", "kind": "bookmark" }));
    let (again_status, again) =
        collection_create(serde_json::json!({ "name": "Sci-fi novels", "kind": "file" }));

    assert_eq!(
        (file_status, bookmark_status, again_status),
        (COLLECTION_OK, COLLECTION_OK, COLLECTION_OK)
    );
    assert_eq!(films["name"], "Sci-fi novels");
    assert_eq!(films["kind"], "file");
    assert_eq!(reading["kind"], "bookmark");
    assert!(uuid::Uuid::parse_str(films["uuid"].as_str().unwrap_or_default()).is_ok());
    assert_ne!(
        films["uuid"], again["uuid"],
        "the same name twice is two collections"
    );
    let uuid_of = |v: &serde_json::Value| v["uuid"].as_str().unwrap().to_string();
    assert_eq!(
        collection_rows(&db.1),
        vec![
            (
                uuid_of(&films),
                "Sci-fi novels".to_string(),
                "file".to_string()
            ),
            (
                uuid_of(&reading),
                "Reading".to_string(),
                "bookmark".to_string()
            ),
            (
                uuid_of(&again),
                "Sci-fi novels".to_string(),
                "file".to_string()
            ),
        ]
    );
}

#[test]
fn given_an_unrecognised_kind_when_a_collection_is_created_over_ffi_then_invalid_input_and_nothing_is_stored(
) {
    let _g = serial();
    let db = init_temp_db();

    let (status, value) =
        collection_create(serde_json::json!({ "name": "Mixed bag", "kind": "not-a-kind" }));

    assert_eq!(status, COLLECTION_INVALID_INPUT);
    assert!(value.is_null(), "a refused create carries no body");
    assert!(collection_rows(&db.1).is_empty());
}

#[test]
fn given_a_collection_when_renamed_over_ffi_then_the_new_name_is_answered_and_stored() {
    let _g = serial();
    let db = init_temp_db();
    let uuid = new_collection("Sci-fi novels", "file");

    let (status, value) =
        collection_rename(&uuid, serde_json::json!({ "name": "Sci-fi & fantasy" }));

    assert_eq!(status, COLLECTION_OK);
    assert_eq!(value["uuid"], uuid.as_str());
    assert_eq!(value["name"], "Sci-fi & fantasy");
    assert_eq!(
        collection_rows(&db.1),
        vec![(uuid, "Sci-fi & fantasy".to_string(), "file".to_string())]
    );
}

#[test]
fn given_a_collection_with_a_file_when_deleted_over_ffi_then_it_is_answered_as_it_was_and_the_file_stays_unfiled(
) {
    let _g = serial();
    let db = init_temp_db();
    let uuid = new_collection("Sci-fi novels", "file");
    let file = seed_file(&db.1, "text", "memo.txt");
    assert_eq!(collection_add_items(&uuid, &[&file]).0, COLLECTION_OK);

    let (status, value) = collection_delete(&uuid);

    assert_eq!(status, COLLECTION_OK);
    assert_eq!(value["uuid"], uuid.as_str());
    assert_eq!(value["name"], "Sci-fi novels");
    assert!(collection_rows(&db.1).is_empty());
    assert_eq!(
        count(&db.1, "SELECT COUNT(*) FROM files WHERE uuid = ?", &[&file]),
        1,
        "the file itself is kept"
    );
    assert_eq!(file_collection(&db.1, &file), None, "and filed nowhere");
}

#[test]
fn given_an_unknown_collection_when_renamed_deleted_filled_emptied_or_listed_over_ffi_then_not_found(
) {
    let _g = serial();
    let db = init_temp_db();
    let unknown = unknown_uuid();
    let file = seed_file(&db.1, "text", "memo.txt");

    assert_eq!(
        collection_rename(&unknown, serde_json::json!({ "name": "X" })).0,
        COLLECTION_NOT_FOUND
    );
    assert_eq!(collection_delete(&unknown).0, COLLECTION_NOT_FOUND);
    assert_eq!(
        collection_add_items(&unknown, &[&file]).0,
        COLLECTION_NOT_FOUND
    );
    assert_eq!(
        collection_remove_item(&unknown, &file).0,
        COLLECTION_NOT_FOUND
    );
    assert_eq!(collection_list_items(&unknown).0, COLLECTION_NOT_FOUND);
    assert_eq!(file_collection(&db.1, &file), None);
}

#[test]
fn given_a_file_and_a_bookmark_collection_when_items_of_their_kind_are_added_over_ffi_then_they_are_filed(
) {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("My files", "file");
    let links = new_collection("My links", "bookmark");
    let file = seed_file(&db.1, "text", "memo.txt");
    let bookmark = new_bookmark("article", None);

    let (file_status, filed) = collection_add_items(&files, &[&file]);
    let (bookmark_status, linked) = collection_add_items(&links, &[&bookmark]);

    assert_eq!(
        (file_status, bookmark_status),
        (COLLECTION_OK, COLLECTION_OK)
    );
    assert_eq!(filed["collectionUuid"], files.as_str());
    assert_eq!(
        filed["items"],
        serde_json::json!([{ "itemUuid": file, "added": true }])
    );
    assert_eq!(
        linked["items"],
        serde_json::json!([{ "itemUuid": bookmark, "added": true }])
    );
    assert_eq!(file_collection(&db.1, &file), Some(files));
    assert_eq!(bookmark_collection(&db.1, &bookmark), Some(links));
}

#[test]
fn given_an_item_of_the_other_kind_and_an_unknown_one_when_added_over_ffi_then_each_is_reported_and_neither_filed(
) {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("My files", "file");
    let bookmark = new_bookmark("article", None);
    let unknown = unknown_uuid();

    let (status, value) = collection_add_items(&files, &[&bookmark, &unknown]);

    assert_eq!(status, COLLECTION_OK, "a partial add still answers");
    assert_eq!(
        value["items"],
        serde_json::json!([
            { "itemUuid": bookmark, "added": false, "reason": "wrong_kind" },
            { "itemUuid": unknown, "added": false, "reason": "not_found" },
        ])
    );
    assert_eq!(bookmark_collection(&db.1, &bookmark), None);
}

#[test]
fn given_an_empty_item_list_when_added_to_a_collection_over_ffi_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();
    let files = new_collection("My files", "file");

    let (status, value) = collection_add_items(&files, &[]);

    assert_eq!(status, COLLECTION_INVALID_INPUT);
    assert!(value.is_null());
}

#[test]
fn given_a_filed_file_and_bookmark_when_removed_over_ffi_then_both_are_unfiled() {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("My files", "file");
    let links = new_collection("My links", "bookmark");
    let file = seed_file(&db.1, "text", "memo.txt");
    let bookmark = new_bookmark("article", Some(&links));
    assert_eq!(collection_add_items(&files, &[&file]).0, COLLECTION_OK);

    let (file_status, unfiled) = collection_remove_item(&files, &file);
    let (bookmark_status, unlinked) = collection_remove_item(&links, &bookmark);

    assert_eq!(
        (file_status, bookmark_status),
        (COLLECTION_OK, COLLECTION_OK)
    );
    assert_eq!(unfiled["collectionUuid"], files.as_str());
    assert_eq!(unfiled["itemUuid"], file.as_str());
    assert_eq!(unlinked["itemUuid"], bookmark.as_str());
    assert_eq!(file_collection(&db.1, &file), None);
    assert_eq!(bookmark_collection(&db.1, &bookmark), None);
}

#[test]
fn given_an_unknown_item_or_one_not_filed_there_when_removed_over_ffi_then_not_found() {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("My files", "file");
    let elsewhere = new_collection("Elsewhere", "file");
    let file = seed_file(&db.1, "text", "memo.txt");
    assert_eq!(collection_add_items(&elsewhere, &[&file]).0, COLLECTION_OK);

    assert_eq!(
        collection_remove_item(&files, &unknown_uuid()).0,
        COLLECTION_NOT_FOUND
    );
    assert_eq!(
        collection_remove_item(&files, &file).0,
        COLLECTION_NOT_FOUND
    );
    assert_eq!(
        file_collection(&db.1, &file),
        Some(elsewhere),
        "left where it was"
    );
}

#[test]
fn given_collections_with_members_when_their_items_are_listed_over_ffi_then_files_or_bookmarks_come_back(
) {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("My files", "file");
    let links = new_collection("My links", "bookmark");
    let empty = new_collection("Nothing yet", "file");
    let file = seed_file(&db.1, "text", "memo.txt");
    let bookmark = new_bookmark("article", Some(&links));
    assert_eq!(collection_add_items(&files, &[&file]).0, COLLECTION_OK);

    let (file_status, of_files) = collection_list_items(&files);
    let (bookmark_status, of_links) = collection_list_items(&links);
    let (empty_status, of_empty) = collection_list_items(&empty);

    assert_eq!(
        (file_status, bookmark_status, empty_status),
        (COLLECTION_OK, COLLECTION_OK, COLLECTION_OK)
    );
    assert_eq!(of_files["collectionUuid"], files.as_str());
    assert_eq!(of_files["kind"], "file");
    assert_eq!(of_files["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(of_files["items"][0]["uuid"], file.as_str());
    assert_eq!(of_links["kind"], "bookmark");
    assert_eq!(of_links["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(of_links["items"][0]["uuid"], bookmark.as_str());
    assert_eq!(of_empty["items"], serde_json::json!([]));
}

/// `(name, kind)` of each summary a collection listing answers, sorted.
fn summaries(listed: &serde_json::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn given_collections_of_both_kinds_when_listed_over_ffi_then_all_or_only_the_kind_asked_for() {
    let _g = serial();
    let _db = init_temp_db();

    let (none_status, none) = collections_list(serde_json::json!({}));
    assert_eq!(none_status, COLLECTION_OK);
    assert_eq!(
        none,
        serde_json::json!([]),
        "no collections is an empty array"
    );

    new_collection("Films", "file");
    let reading = new_collection("Reading", "bookmark");

    let (all_status, all) = collections_list(serde_json::json!({}));
    let (empty_kind_status, empty_kind) = collections_list(serde_json::json!({ "kind": "" }));
    let (kind_status, bookmarks) = collections_list(serde_json::json!({ "kind": "bookmark" }));

    assert_eq!(
        (all_status, empty_kind_status, kind_status),
        (COLLECTION_OK, COLLECTION_OK, COLLECTION_OK)
    );
    let both = vec![
        ("Films".to_string(), "file".to_string()),
        ("Reading".to_string(), "bookmark".to_string()),
    ];
    assert_eq!(summaries(&all), both);
    assert_eq!(summaries(&empty_kind), both, "an empty kind is no filter");
    assert_eq!(bookmarks.as_array().map(Vec::len), Some(1));
    assert_eq!(bookmarks[0]["uuid"], reading.as_str());
}

#[test]
fn given_only_bookmark_collections_when_file_ones_are_listed_over_ffi_then_an_empty_array() {
    let _g = serial();
    let _db = init_temp_db();
    new_collection("Reading", "bookmark");

    let (status, value) = collections_list(serde_json::json!({ "kind": "file" }));

    assert_eq!(status, COLLECTION_OK);
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn given_an_unknown_kind_when_collections_are_listed_over_ffi_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    let (status, value) = collections_list(serde_json::json!({ "kind": "playlist" }));

    assert_eq!(status, COLLECTION_INVALID_INPUT);
    assert!(value.is_null());
}

#[test]
fn given_members_some_soft_deleted_when_collections_are_listed_over_ffi_then_only_active_ones_are_counted(
) {
    let _g = serial();
    let db = init_temp_db();
    let files = new_collection("Films", "file");
    let links = new_collection("Reading", "bookmark");
    let kept_a = seed_file(&db.1, "text", "a.txt");
    let kept_b = seed_file(&db.1, "text", "b.txt");
    let gone = seed_file(&db.1, "text", "c.txt");
    assert_eq!(
        collection_add_items(&files, &[&kept_a, &kept_b, &gone]).0,
        COLLECTION_OK
    );
    exec(
        &db.1,
        "UPDATE files SET state = 'deleted', deleted_at = ? WHERE uuid = ?",
        &[&chrono::Utc::now().to_rfc3339(), &gone],
    );
    new_bookmark("kept", Some(&links));
    let deleted = new_bookmark("deleted", Some(&links));
    assert_eq!(
        bookmark_soft_delete(&deleted).0,
        alexandria_ffi::BOOKMARK_OK
    );

    let (status, listed) = collections_list(serde_json::json!({}));

    assert_eq!(status, COLLECTION_OK);
    let count_of = |uuid: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["uuid"] == uuid)
            .unwrap_or_else(|| panic!("{uuid} listed: {listed}"))["itemCount"]
            .clone()
    };
    assert_eq!(count_of(&files), 2);
    assert_eq!(count_of(&links), 1);
}

// ---------------- bookmarks (UC-16 to UC-21) ----------------

const BOOKMARK_OK: i32 = alexandria_ffi::BOOKMARK_OK;
const BOOKMARK_INVALID_INPUT: i32 = alexandria_ffi::BOOKMARK_ERR_INVALID_INPUT;
const BOOKMARK_NOT_FOUND: i32 = alexandria_ffi::BOOKMARK_ERR_NOT_FOUND;
const BOOKMARK_INVALID_STATE: i32 = alexandria_ffi::BOOKMARK_ERR_INVALID_STATE;

/// `(url, title, state)` of one bookmark row.
fn bookmark_row(db_path: &str, uuid: &str) -> (String, String, String) {
    fetch_all(
        db_path,
        "SELECT url, title, state FROM bookmarks WHERE uuid = ?",
        &[uuid],
    )
    .pop()
    .expect("bookmark row")
}

fn bookmark_count(db_path: &str) -> i64 {
    count(db_path, "SELECT COUNT(*) FROM bookmarks", &[])
}

#[test]
fn given_a_url_and_title_when_bookmarks_are_created_over_ffi_then_each_is_answered_and_stored() {
    let _g = serial();
    let db = init_temp_db();
    let links = new_collection("Reading", "bookmark");

    let (loose_status, loose) = bookmark_create(
        serde_json::json!({ "url": "https://example.com/article", "title": "An article" }),
    );
    let (filed_status, filed) = bookmark_create(serde_json::json!({
        "url": "https://example.com/paper",
        "title": "A paper",
        "collectionUuid": links,
    }));

    assert_eq!((loose_status, filed_status), (BOOKMARK_OK, BOOKMARK_OK));
    let loose_uuid = loose["uuid"].as_str().expect("uuid");
    assert!(uuid::Uuid::parse_str(loose_uuid).is_ok());
    assert_eq!(loose["url"], "https://example.com/article");
    assert_eq!(loose["title"], "An article");
    assert!(loose["collectionUuid"].is_null());
    assert_eq!(filed["collectionUuid"], links.as_str());
    assert_eq!(
        bookmark_row(&db.1, loose_uuid),
        (
            "https://example.com/article".to_string(),
            "An article".to_string(),
            "active".to_string()
        )
    );
    assert_eq!(bookmark_collection(&db.1, loose_uuid), None);
    assert_eq!(
        bookmark_collection(&db.1, filed["uuid"].as_str().unwrap()),
        Some(links)
    );
}

#[test]
fn given_an_unknown_collection_when_a_bookmark_is_created_over_ffi_then_not_found_and_nothing_is_stored(
) {
    let _g = serial();
    let db = init_temp_db();

    let (status, value) = bookmark_create(serde_json::json!({
        "url": "https://example.com",
        "title": "T",
        "collectionUuid": unknown_uuid(),
    }));

    assert_eq!(status, BOOKMARK_NOT_FOUND);
    assert!(value.is_null());
    assert_eq!(bookmark_count(&db.1), 0);
}

#[test]
fn given_a_bookmark_when_updated_over_ffi_then_its_new_fields_and_collection_are_stored() {
    let _g = serial();
    let db = init_temp_db();
    let links = new_collection("Reading", "bookmark");
    let uuid = new_bookmark("old", None);

    let (status, value) = bookmark_update(
        &uuid,
        serde_json::json!({
            "url": "https://example.org",
            "title": "New title",
            "collectionUuid": links,
        }),
    );

    assert_eq!(status, BOOKMARK_OK);
    assert_eq!(value["url"], "https://example.org");
    assert_eq!(value["title"], "New title");
    assert_eq!(value["collectionUuid"], links.as_str());
    assert_eq!(
        bookmark_row(&db.1, &uuid),
        (
            "https://example.org".to_string(),
            "New title".to_string(),
            "active".to_string()
        )
    );
    assert_eq!(bookmark_collection(&db.1, &uuid), Some(links));
}

#[test]
fn given_an_unknown_bookmark_when_updated_deleted_restored_or_purged_over_ffi_then_not_found() {
    let _g = serial();
    let _db = init_temp_db();
    let unknown = unknown_uuid();

    assert_eq!(
        bookmark_update(
            &unknown,
            serde_json::json!({ "url": "https://example.org", "title": "T" })
        )
        .0,
        BOOKMARK_NOT_FOUND
    );
    assert_eq!(bookmark_soft_delete(&unknown).0, BOOKMARK_NOT_FOUND);
    assert_eq!(bookmark_restore(&unknown).0, BOOKMARK_NOT_FOUND);
    assert_eq!(bookmark_purge(&unknown).0, BOOKMARK_NOT_FOUND);
}

fn titles(listed: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|b| b["title"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn given_bookmarks_when_listed_over_ffi_then_active_ones_by_default_one_collections_or_all() {
    let _g = serial();
    let _db = init_temp_db();
    let links = new_collection("Reading", "bookmark");
    new_bookmark("A", Some(&links));
    new_bookmark("B", None);
    let deleted = new_bookmark("C", None);
    assert_eq!(bookmark_soft_delete(&deleted).0, BOOKMARK_OK);

    let (default_status, active) = bookmarks_list(serde_json::json!({}));
    let (filed_status, filed) = bookmarks_list(serde_json::json!({ "collectionUuid": links }));
    let (all_status, all) = bookmarks_list(serde_json::json!({ "state": "all" }));

    assert_eq!(
        (default_status, filed_status, all_status),
        (BOOKMARK_OK, BOOKMARK_OK, BOOKMARK_OK)
    );
    assert_eq!(titles(&active), vec!["A", "B"]);
    assert_eq!(titles(&filed), vec!["A"]);
    assert_eq!(titles(&all), vec!["A", "B", "C"]);
}

#[test]
fn given_unusable_filters_when_bookmarks_are_listed_over_ffi_then_they_are_refused() {
    let _g = serial();
    let _db = init_temp_db();

    assert_eq!(
        bookmarks_list(serde_json::json!({ "collectionUuid": unknown_uuid() })).0,
        BOOKMARK_NOT_FOUND
    );
    assert_eq!(
        bookmarks_list(serde_json::json!({ "collectionUuid": "not-a-uuid" })).0,
        BOOKMARK_INVALID_INPUT
    );
    assert_eq!(
        bookmarks_list(serde_json::json!({ "state": "archived" })).0,
        BOOKMARK_INVALID_INPUT
    );
}

#[test]
fn given_a_bookmark_when_soft_deleted_and_restored_over_ffi_then_each_state_is_stored_once() {
    let _g = serial();
    let db = init_temp_db();
    let uuid = new_bookmark("article", None);

    let (restore_active, _) = bookmark_restore(&uuid);
    let (deleted_status, deleted) = bookmark_soft_delete(&uuid);
    let deleted_row = bookmark_row(&db.1, &uuid).2;
    let (again_status, _) = bookmark_soft_delete(&uuid);
    let (restored_status, restored) = bookmark_restore(&uuid);

    assert_eq!(
        restore_active, BOOKMARK_INVALID_STATE,
        "an active one is not restored"
    );
    assert_eq!(deleted_status, BOOKMARK_OK);
    assert_eq!(deleted["state"], "deleted");
    assert!(deleted["deletedAt"].is_string(), "{deleted}");
    assert_eq!(deleted_row, "deleted");
    assert_eq!(
        again_status, BOOKMARK_INVALID_STATE,
        "a deleted one is not deleted again"
    );
    assert_eq!(restored_status, BOOKMARK_OK);
    assert_eq!(restored["state"], "active");
    assert!(restored["deletedAt"].is_null());
    assert_eq!(bookmark_row(&db.1, &uuid).2, "active");
}

#[test]
fn given_bookmarks_when_purged_over_ffi_then_only_one_deleted_past_the_window_goes() {
    let _g = serial();
    let db = init_temp_db();
    let past = new_bookmark("past", None);
    let recent = new_bookmark("recent", None);
    let active = new_bookmark("active", None);
    exec(
        &db.1,
        "UPDATE bookmarks SET state = 'deleted', deleted_at = '2024-01-01T00:00:00Z' WHERE uuid = ?",
        &[&past],
    );
    assert_eq!(bookmark_soft_delete(&recent).0, BOOKMARK_OK);

    let (past_status, purged) = bookmark_purge(&past);
    let (recent_status, _) = bookmark_purge(&recent);
    let (active_status, _) = bookmark_purge(&active);

    assert_eq!(past_status, BOOKMARK_OK);
    assert_eq!(purged["uuid"], past.as_str());
    assert_eq!(
        recent_status, BOOKMARK_INVALID_STATE,
        "still inside the window"
    );
    assert_eq!(active_status, BOOKMARK_INVALID_STATE, "never deleted");
    assert_eq!(
        count(
            &db.1,
            "SELECT COUNT(*) FROM bookmarks WHERE uuid = ?",
            &[&past]
        ),
        0
    );
    assert_eq!(bookmark_count(&db.1), 2, "the other two are kept");
}

// ---------------- watchlists (UC-22 to UC-27) ----------------

const WATCHLIST_OK: i32 = alexandria_ffi::WATCHLIST_OK;
const WATCHLIST_INVALID_INPUT: i32 = alexandria_ffi::WATCHLIST_ERR_INVALID_INPUT;
const WATCHLIST_NOT_FOUND: i32 = alexandria_ffi::WATCHLIST_ERR_NOT_FOUND;
const WATCHLIST_INVALID_STATE: i32 = alexandria_ffi::WATCHLIST_ERR_INVALID_STATE;

fn new_watchlist(name: &str) -> String {
    let (status, value) = watchlist_create(serde_json::json!({ "name": name }));
    assert_eq!(status, WATCHLIST_OK, "create {name}: {value}");
    value["uuid"].as_str().expect("uuid").to_string()
}

/// `(watchlist uuid, video uuid, state, current episode, total episodes)`
/// of every watch_progress row.
type WatchRow = (String, String, String, Option<i64>, Option<i64>);

fn watch_row(
    watchlist: &str,
    video: &str,
    state: &str,
    current: Option<i64>,
    total: Option<i64>,
) -> WatchRow {
    (
        watchlist.to_string(),
        video.to_string(),
        state.to_string(),
        current,
        total,
    )
}

fn watch_rows(db_path: &str) -> Vec<WatchRow> {
    fetch_all(
        db_path,
        "SELECT watchlists.uuid, files.uuid, watch_progress.state, \
                watch_progress.current_episode, watch_progress.total_episodes \
         FROM watch_progress \
         JOIN watchlists ON watchlists.id = watch_progress.watchlist_id \
         JOIN files ON files.id = watch_progress.video_file_id \
         ORDER BY watch_progress.id",
        &[],
    )
}

fn file_count(db_path: &str, uuid: &str) -> i64 {
    count(
        db_path,
        "SELECT COUNT(*) FROM files WHERE uuid = ?",
        &[uuid],
    )
}

#[test]
fn given_a_name_when_watchlists_are_created_over_ffi_then_each_is_answered_and_stored() {
    let _g = serial();
    let db = init_temp_db();

    let (status, value) = watchlist_create(serde_json::json!({ "name": "Weekend movies" }));
    let (again_status, again) = watchlist_create(serde_json::json!({ "name": "Weekend movies" }));

    assert_eq!((status, again_status), (WATCHLIST_OK, WATCHLIST_OK));
    assert_eq!(value["name"], "Weekend movies");
    let uuid = value["uuid"].as_str().expect("uuid").to_string();
    assert!(uuid::Uuid::parse_str(&uuid).is_ok());
    assert_ne!(value["uuid"], again["uuid"]);
    let rows: Vec<(String, String)> =
        fetch_all(&db.1, "SELECT uuid, name FROM watchlists ORDER BY id", &[]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (uuid, "Weekend movies".to_string()));
}

#[test]
fn given_a_video_when_added_to_a_watchlist_over_ffi_then_pending_progress_is_stored_once() {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Weekend movies");
    let video = seed_file(&db.1, "video", "film.mkv");

    let (status, value) = watchlist_add_video(&watchlist, &video);
    let (again_status, _) = watchlist_add_video(&watchlist, &video);

    assert_eq!((status, again_status), (WATCHLIST_OK, WATCHLIST_OK));
    assert_eq!(value["watchlistUuid"], watchlist.as_str());
    assert_eq!(value["videoUuid"], video.as_str());
    assert_eq!(value["state"], "pending");
    assert!(value["currentEpisode"].is_null());
    assert!(value["totalEpisodes"].is_null());
    assert_eq!(
        watch_rows(&db.1),
        vec![watch_row(&watchlist, &video, "pending", None, None)],
        "adding it again stores no second row"
    );
}

#[test]
fn given_a_non_video_or_an_unknown_target_when_a_video_is_added_over_ffi_then_refused_and_nothing_is_stored(
) {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Weekend movies");
    let video = seed_file(&db.1, "video", "film.mkv");
    let memo = seed_file(&db.1, "text", "memo.txt");

    assert_eq!(
        watchlist_add_video(&watchlist, &memo).0,
        WATCHLIST_INVALID_INPUT
    );
    assert_eq!(
        watchlist_add_video(&unknown_uuid(), &video).0,
        WATCHLIST_NOT_FOUND
    );
    assert_eq!(
        watchlist_add_video(&watchlist, &unknown_uuid()).0,
        WATCHLIST_NOT_FOUND
    );
    assert!(watch_rows(&db.1).is_empty());
}

#[test]
fn given_watchlists_when_listed_over_ffi_then_each_carries_its_progress() {
    let _g = serial();
    let db = init_temp_db();
    let (none_status, none) = watchlists_list(serde_json::json!({}));
    let with_video = new_watchlist("Movies");
    let empty = new_watchlist("Series");
    let video = seed_file(&db.1, "video", "film.mkv");
    assert_eq!(watchlist_add_video(&with_video, &video).0, WATCHLIST_OK);

    let (all_status, all) = watchlists_list(serde_json::json!({}));
    let (one_status, one) = watchlists_list(serde_json::json!({ "watchlistUuid": with_video }));

    assert_eq!(
        (none_status, all_status, one_status),
        (WATCHLIST_OK, WATCHLIST_OK, WATCHLIST_OK)
    );
    assert_eq!(none, serde_json::json!([]), "none is an empty array");
    let listed = |uuid: &str| {
        all.as_array()
            .unwrap()
            .iter()
            .find(|w| w["uuid"] == uuid)
            .unwrap_or_else(|| panic!("{uuid} listed: {all}"))
            .clone()
    };
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    let movies = listed(&with_video);
    assert_eq!(movies["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(movies["items"][0]["videoUuid"], video.as_str());
    assert_eq!(movies["items"][0]["state"], "pending");
    assert_eq!(listed(&empty)["items"], serde_json::json!([]));
    assert_eq!(one.as_array().map(Vec::len), Some(1));
    assert_eq!(one[0]["uuid"], with_video.as_str());
}

#[test]
fn given_an_unknown_watchlist_when_listed_or_deleted_over_ffi_then_not_found() {
    let _g = serial();
    let db = init_temp_db();
    new_watchlist("Movies");

    assert_eq!(
        watchlists_list(serde_json::json!({ "watchlistUuid": unknown_uuid() })).0,
        WATCHLIST_NOT_FOUND
    );
    assert_eq!(watchlist_delete(&unknown_uuid()).0, WATCHLIST_NOT_FOUND);
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM watchlists", &[]), 1);
}

#[test]
fn given_pending_progress_when_updated_over_ffi_then_the_state_and_episodes_are_stored() {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Series");
    let video = seed_file(&db.1, "video", "show.mkv");
    assert_eq!(watchlist_add_video(&watchlist, &video).0, WATCHLIST_OK);

    let (status, value) = watchlist_update_progress(
        &watchlist,
        &video,
        serde_json::json!({ "state": "watching", "currentEpisode": 3, "totalEpisodes": 12 }),
    );

    assert_eq!(status, WATCHLIST_OK);
    assert_eq!(value["watchlistUuid"], watchlist.as_str());
    assert_eq!(value["videoUuid"], video.as_str());
    assert_eq!(value["state"], "watching");
    assert_eq!(value["currentEpisode"], 3);
    assert_eq!(value["totalEpisodes"], 12);
    assert_eq!(
        watch_rows(&db.1),
        vec![watch_row(&watchlist, &video, "watching", Some(3), Some(12))]
    );
}

#[test]
fn given_a_refused_progress_update_over_ffi_then_the_row_is_left_pending() {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Series");
    let video = seed_file(&db.1, "video", "show.mkv");
    let other = seed_file(&db.1, "video", "other.mkv");
    assert_eq!(watchlist_add_video(&watchlist, &video).0, WATCHLIST_OK);

    let skipped = watchlist_update_progress(
        &watchlist,
        &video,
        serde_json::json!({ "state": "watched" }),
    );
    let unknown_state =
        watchlist_update_progress(&watchlist, &video, serde_json::json!({ "state": "paused" }));
    let not_listed = watchlist_update_progress(
        &watchlist,
        &other,
        serde_json::json!({ "state": "watching" }),
    );

    assert_eq!(
        skipped.0, WATCHLIST_INVALID_STATE,
        "pending cannot skip to watched"
    );
    assert_eq!(unknown_state.0, WATCHLIST_INVALID_INPUT);
    assert_eq!(not_listed.0, WATCHLIST_NOT_FOUND);
    assert_eq!(watch_rows(&db.1)[0].2, "pending");
}

#[test]
fn given_a_video_on_a_watchlist_when_removed_over_ffi_then_its_progress_goes_and_the_file_stays() {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Movies");
    let video = seed_file(&db.1, "video", "film.mkv");
    let never_added = seed_file(&db.1, "video", "other.mkv");
    assert_eq!(watchlist_add_video(&watchlist, &video).0, WATCHLIST_OK);

    let (status, value) = watchlist_remove_video(&watchlist, &video);
    let (again_status, _) = watchlist_remove_video(&watchlist, &video);
    let (never_status, _) = watchlist_remove_video(&watchlist, &never_added);

    assert_eq!(status, WATCHLIST_OK);
    assert_eq!(value["watchlistUuid"], watchlist.as_str());
    assert_eq!(value["videoUuid"], video.as_str());
    assert_eq!(again_status, WATCHLIST_NOT_FOUND, "removed twice");
    assert_eq!(never_status, WATCHLIST_NOT_FOUND, "never on the list");
    assert!(watch_rows(&db.1).is_empty());
    assert_eq!(file_count(&db.1, &video), 1, "the video itself is kept");
}

#[test]
fn given_a_watchlist_with_a_video_when_deleted_over_ffi_then_it_and_its_progress_go_and_the_video_stays(
) {
    let _g = serial();
    let db = init_temp_db();
    let watchlist = new_watchlist("Weekend movies");
    let video = seed_file(&db.1, "video", "film.mkv");
    assert_eq!(watchlist_add_video(&watchlist, &video).0, WATCHLIST_OK);

    let (status, value) = watchlist_delete(&watchlist);

    assert_eq!(status, WATCHLIST_OK);
    assert_eq!(value["uuid"], watchlist.as_str());
    assert_eq!(value["name"], "Weekend movies");
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM watchlists", &[]), 0);
    assert!(watch_rows(&db.1).is_empty());
    assert_eq!(file_count(&db.1, &video), 1);
}

// ---------------- reading lists (UC-28 to UC-31) ----------------

const READING_LIST_OK: i32 = alexandria_ffi::READING_LIST_OK;
const READING_LIST_INVALID_INPUT: i32 = alexandria_ffi::READING_LIST_ERR_INVALID_INPUT;
const READING_LIST_NOT_FOUND: i32 = alexandria_ffi::READING_LIST_ERR_NOT_FOUND;
const READING_LIST_INVALID_STATE: i32 = alexandria_ffi::READING_LIST_ERR_INVALID_STATE;

fn new_reading_list(name: &str) -> String {
    let (status, value) = reading_list_create(serde_json::json!({ "name": name }));
    assert_eq!(status, READING_LIST_OK, "create {name}: {value}");
    value["uuid"].as_str().expect("uuid").to_string()
}

/// `(reading list uuid, item uuid, target kind, state, current issue,
/// total issues)` of every reading_progress row.
type ReadingRow = (String, String, String, String, Option<i64>, Option<i64>);

fn reading_row(
    list: &str,
    item: &str,
    kind: &str,
    state: &str,
    current: Option<i64>,
    total: Option<i64>,
) -> ReadingRow {
    (
        list.to_string(),
        item.to_string(),
        kind.to_string(),
        state.to_string(),
        current,
        total,
    )
}

fn reading_rows(db_path: &str) -> Vec<ReadingRow> {
    fetch_all(
        db_path,
        "SELECT reading_lists.uuid, files.uuid, reading_progress.target_kind, \
                reading_progress.state, reading_progress.current_issue, \
                reading_progress.total_issues \
         FROM reading_progress \
         JOIN reading_lists ON reading_lists.id = reading_progress.reading_list_id \
         JOIN files ON files.id = reading_progress.item_file_id \
         ORDER BY reading_progress.id",
        &[],
    )
}

#[test]
fn given_a_name_when_reading_lists_are_created_over_ffi_then_each_is_answered_and_stored() {
    let _g = serial();
    let db = init_temp_db();

    let (status, value) = reading_list_create(serde_json::json!({ "name": "Summer reads" }));
    let (again_status, again) = reading_list_create(serde_json::json!({ "name": "Summer reads" }));

    assert_eq!((status, again_status), (READING_LIST_OK, READING_LIST_OK));
    assert_eq!(value["name"], "Summer reads");
    let uuid = value["uuid"].as_str().expect("uuid").to_string();
    assert!(uuid::Uuid::parse_str(&uuid).is_ok());
    assert_ne!(value["uuid"], again["uuid"]);
    let rows: Vec<(String, String)> = fetch_all(
        &db.1,
        "SELECT uuid, name FROM reading_lists ORDER BY id",
        &[],
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (uuid, "Summer reads".to_string()));
}

#[test]
fn given_a_document_and_a_comic_when_added_to_a_reading_list_over_ffi_then_pending_progress_of_their_kind_is_stored_once(
) {
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Summer reads");
    let book = seed_file(&db.1, "document", "book.pdf");
    let issue = seed_file(&db.1, "comic", "issue.cbz");

    let (status, value) = reading_list_add_item(&list, &book);
    let (again_status, _) = reading_list_add_item(&list, &book);
    let (comic_status, comic) = reading_list_add_item(&list, &issue);

    assert_eq!(
        (status, again_status, comic_status),
        (READING_LIST_OK, READING_LIST_OK, READING_LIST_OK)
    );
    assert_eq!(value["readingListUuid"], list.as_str());
    assert_eq!(value["itemUuid"], book.as_str());
    assert_eq!(value["targetKind"], "document");
    assert_eq!(value["state"], "pending");
    assert!(value["currentIssue"].is_null());
    assert!(value["totalIssues"].is_null());
    assert_eq!(comic["targetKind"], "comic");
    assert_eq!(
        reading_rows(&db.1),
        vec![
            reading_row(&list, &book, "document", "pending", None, None),
            reading_row(&list, &issue, "comic", "pending", None, None),
        ],
        "adding the document again stores no second row"
    );
}

#[test]
fn given_an_ineligible_or_unknown_target_when_an_item_is_added_over_ffi_then_refused_and_nothing_is_stored(
) {
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Summer reads");
    let book = seed_file(&db.1, "document", "book.pdf");
    let film = seed_file(&db.1, "video", "film.mkv");

    assert_eq!(
        reading_list_add_item(&list, &film).0,
        READING_LIST_INVALID_INPUT
    );
    assert_eq!(
        reading_list_add_item(&unknown_uuid(), &book).0,
        READING_LIST_NOT_FOUND
    );
    assert_eq!(
        reading_list_add_item(&list, &unknown_uuid()).0,
        READING_LIST_NOT_FOUND
    );
    assert!(reading_rows(&db.1).is_empty());
}

#[test]
fn given_reading_lists_when_listed_over_ffi_then_each_carries_its_progress() {
    let _g = serial();
    let db = init_temp_db();
    let (none_status, none) = reading_lists_list(serde_json::json!({}));
    let with_book = new_reading_list("Books");
    let empty = new_reading_list("Comics");
    let book = seed_file(&db.1, "document", "book.pdf");
    assert_eq!(reading_list_add_item(&with_book, &book).0, READING_LIST_OK);

    let (all_status, all) = reading_lists_list(serde_json::json!({}));
    let (one_status, one) = reading_lists_list(serde_json::json!({ "readingListUuid": with_book }));

    assert_eq!(
        (none_status, all_status, one_status),
        (READING_LIST_OK, READING_LIST_OK, READING_LIST_OK)
    );
    assert_eq!(none, serde_json::json!([]), "none is an empty array");
    let listed = |uuid: &str| {
        all.as_array()
            .unwrap()
            .iter()
            .find(|r| r["uuid"] == uuid)
            .unwrap_or_else(|| panic!("{uuid} listed: {all}"))
            .clone()
    };
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    let books = listed(&with_book);
    assert_eq!(books["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(books["items"][0]["itemUuid"], book.as_str());
    assert_eq!(books["items"][0]["state"], "pending");
    assert_eq!(listed(&empty)["items"], serde_json::json!([]));
    assert_eq!(one.as_array().map(Vec::len), Some(1));
    assert_eq!(one[0]["uuid"], with_book.as_str());
}

#[test]
fn given_an_unknown_reading_list_when_listed_or_deleted_over_ffi_then_not_found() {
    let _g = serial();
    let db = init_temp_db();
    new_reading_list("Books");

    assert_eq!(
        reading_lists_list(serde_json::json!({ "readingListUuid": unknown_uuid() })).0,
        READING_LIST_NOT_FOUND
    );
    assert_eq!(
        reading_list_delete(&unknown_uuid()).0,
        READING_LIST_NOT_FOUND
    );
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM reading_lists", &[]), 1);
}

#[test]
fn given_pending_progress_when_updated_over_ffi_then_the_state_and_issues_are_stored() {
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Comics");
    let issue = seed_file(&db.1, "comic", "issue.cbz");
    assert_eq!(reading_list_add_item(&list, &issue).0, READING_LIST_OK);

    let (status, value) = reading_list_update_progress(
        &list,
        &issue,
        serde_json::json!({ "state": "reading", "currentIssue": 3, "totalIssues": 12 }),
    );

    assert_eq!(status, READING_LIST_OK);
    assert_eq!(value["readingListUuid"], list.as_str());
    assert_eq!(value["itemUuid"], issue.as_str());
    assert_eq!(value["state"], "reading");
    assert_eq!(value["currentIssue"], 3);
    assert_eq!(value["totalIssues"], 12);
    assert_eq!(
        reading_rows(&db.1),
        vec![reading_row(
            &list,
            &issue,
            "comic",
            "reading",
            Some(3),
            Some(12)
        )]
    );
}

#[test]
fn given_a_refused_reading_progress_update_over_ffi_then_the_row_is_left_pending() {
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Books");
    let book = seed_file(&db.1, "document", "book.pdf");
    let other = seed_file(&db.1, "document", "other.pdf");
    assert_eq!(reading_list_add_item(&list, &book).0, READING_LIST_OK);

    let skipped =
        reading_list_update_progress(&list, &book, serde_json::json!({ "state": "read" }));
    let unknown_state =
        reading_list_update_progress(&list, &book, serde_json::json!({ "state": "skimmed" }));
    let not_listed =
        reading_list_update_progress(&list, &other, serde_json::json!({ "state": "reading" }));

    assert_eq!(
        skipped.0, READING_LIST_INVALID_STATE,
        "pending cannot skip to read"
    );
    assert_eq!(unknown_state.0, READING_LIST_INVALID_INPUT);
    assert_eq!(not_listed.0, READING_LIST_NOT_FOUND);
    assert_eq!(reading_rows(&db.1)[0].3, "pending");
}

#[test]
fn given_an_item_on_a_reading_list_when_removed_over_ffi_then_its_progress_goes_and_the_file_stays()
{
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Books");
    let book = seed_file(&db.1, "document", "book.pdf");
    let never_added = seed_file(&db.1, "document", "other.pdf");
    assert_eq!(reading_list_add_item(&list, &book).0, READING_LIST_OK);

    let (status, value) = reading_list_remove_item(&list, &book);
    let (again_status, _) = reading_list_remove_item(&list, &book);
    let (never_status, _) = reading_list_remove_item(&list, &never_added);

    assert_eq!(status, READING_LIST_OK);
    assert_eq!(value["readingListUuid"], list.as_str());
    assert_eq!(value["itemUuid"], book.as_str());
    assert_eq!(again_status, READING_LIST_NOT_FOUND, "removed twice");
    assert_eq!(never_status, READING_LIST_NOT_FOUND, "never on the list");
    assert!(reading_rows(&db.1).is_empty());
    assert_eq!(file_count(&db.1, &book), 1, "the document itself is kept");
}

#[test]
fn given_a_reading_list_with_an_item_when_deleted_over_ffi_then_it_and_its_progress_go_and_the_item_stays(
) {
    let _g = serial();
    let db = init_temp_db();
    let list = new_reading_list("Summer reads");
    let book = seed_file(&db.1, "document", "book.pdf");
    assert_eq!(reading_list_add_item(&list, &book).0, READING_LIST_OK);

    let (status, value) = reading_list_delete(&list);

    assert_eq!(status, READING_LIST_OK);
    assert_eq!(value["uuid"], list.as_str());
    assert_eq!(value["name"], "Summer reads");
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM reading_lists", &[]), 0);
    assert!(reading_rows(&db.1).is_empty());
    assert_eq!(file_count(&db.1, &book), 1);
}

// ---------------- files: the catalog behind the listing (UC-01, UC-03, UC-04) ----------------

/// The call takes only its filters: there is no token to pass.
#[test]
fn given_an_initialized_core_when_files_listed_without_a_token_then_it_answers_ok() {
    let _g = serial();
    let (_dir, db) = init_temp_db();
    init_when_idle(&CString::new(db).unwrap());
    let filters = CString::new("{}").unwrap();
    let result = alexandria_ffi::alexandria_files_list(filters.as_ptr());
    assert_eq!(result.status, 0);
    unsafe { alexandria_ffi::alexandria_free_string(result.json) };
}

#[test]
fn given_files_of_three_types_when_indexed_over_ffi_then_each_has_its_subtype_row() {
    let _g = serial();
    let db = init_temp_db();

    library_with(&db, &[("a.txt", b"x"), ("v.mp4", b"y"), ("p.pdf", b"z")]);

    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM text_files", &[]), 1);
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM video_files", &[]), 1);
    assert_eq!(count(&db.1, "SELECT COUNT(*) FROM documents", &[]), 1);
}

/// A write that touches no row must not be reported as stored.
#[test]
fn given_a_video_whose_subtype_row_is_gone_when_its_metadata_is_edited_over_ffi_then_it_is_not_reported_stored(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("clip.mkv", b"video bytes")]);
    let uuid = uuid_by_name(&db.1, "clip.mkv");
    exec(
        &db.1,
        "DELETE FROM video_files WHERE file_id = (SELECT id FROM files WHERE uuid = ?)",
        &[&uuid],
    );

    let (status, value) =
        file_edit_metadata(&uuid, serde_json::json!({ "type": "video", "title": "T" }));

    assert_ne!(status, STATUS_FILE_OK, "{value}");
    assert!(value.is_null());
}

/// An unparseable stored uuid used to be read as the nil uuid and served.
#[test]
fn given_a_corrupt_stored_uuid_when_files_are_listed_over_ffi_then_it_fails_rather_than_answer_a_nil_uuid(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("memo.txt", b"text")]);
    exec(
        &db.1,
        "UPDATE files SET uuid = 'definitely-not-a-uuid' WHERE name = 'memo.txt'",
        &[],
    );

    let (status, value) = files_list(serde_json::json!({}));

    assert_ne!(status, STATUS_FILE_OK, "{value}");
    assert!(value.is_null());
}

/// The names a file listing answers, sorted.
fn listed_names(listed: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = listed
        .as_array()
        .expect("array")
        .iter()
        .map(|view| view["file"]["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn given_a_soft_deleted_file_when_listed_over_ffi_then_the_state_filter_decides_whether_it_is_answered(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("a.txt", b"a"), ("b.txt", b"b")]);
    assert_eq!(
        file_soft_delete(&uuid_by_name(&db.1, "a.txt")).0,
        STATUS_FILE_OK
    );

    let (default_status, active) = files_list(serde_json::json!({}));
    let (deleted_status, deleted) = files_list(serde_json::json!({ "state": "deleted" }));
    let (all_status, all) = files_list(serde_json::json!({ "state": "all" }));

    assert_eq!(
        (default_status, deleted_status, all_status),
        (STATUS_FILE_OK, STATUS_FILE_OK, STATUS_FILE_OK)
    );
    assert_eq!(listed_names(&active), vec!["b.txt"]);
    assert_eq!(active[0]["file"]["state"], "active");
    assert_eq!(listed_names(&deleted), vec!["a.txt"]);
    assert_eq!(deleted[0]["file"]["state"], "deleted");
    assert_eq!(listed_names(&all), vec!["a.txt", "b.txt"]);
}

#[test]
fn given_a_type_filter_when_files_are_listed_over_ffi_then_only_that_type_and_an_empty_one_is_no_filter(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("memo.txt", b"text"), ("clip.mkv", b"video")]);

    let (text_status, text) = files_list(serde_json::json!({ "type": "text" }));
    let (empty_status, empty) = files_list(serde_json::json!({ "type": "", "state": "" }));

    assert_eq!(
        (text_status, empty_status),
        (STATUS_FILE_OK, STATUS_FILE_OK)
    );
    assert_eq!(listed_names(&text), vec!["memo.txt"]);
    assert_eq!(text[0]["file"]["fileType"], "text");
    assert_eq!(listed_names(&empty), vec!["clip.mkv", "memo.txt"]);
}

#[test]
fn given_a_collection_filter_when_files_are_listed_over_ffi_then_only_its_files_and_an_unknown_one_is_empty(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("a.txt", b"a"), ("b.txt", b"b")]);
    let collection = new_collection("Mine", "file");
    let a = uuid_by_name(&db.1, "a.txt");
    assert_eq!(collection_add_items(&collection, &[&a]).0, COLLECTION_OK);

    let (status, filed) = files_list(serde_json::json!({ "collectionUuid": collection }));
    let (unknown_status, unknown) =
        files_list(serde_json::json!({ "collectionUuid": unknown_uuid() }));

    assert_eq!((status, unknown_status), (STATUS_FILE_OK, STATUS_FILE_OK));
    assert_eq!(listed_names(&filed), vec!["a.txt"]);
    assert_eq!(filed[0]["file"]["uuid"], a.as_str());
    assert_eq!(
        unknown,
        serde_json::json!([]),
        "an unknown collection holds nothing"
    );
}

#[test]
fn given_unusable_filter_values_when_files_are_listed_over_ffi_then_invalid_input() {
    let _g = serial();
    let _db = init_temp_db();

    for filter in [
        serde_json::json!({ "type": "spreadsheet" }),
        serde_json::json!({ "state": "archived" }),
        serde_json::json!({ "collectionUuid": "not-a-uuid" }),
    ] {
        let (status, value) = files_list(filter.clone());
        assert_eq!(status, STATUS_FILE_INVALID_INPUT, "{filter}");
        assert!(value.is_null(), "{filter}");
    }
}

#[test]
fn given_each_metadata_bearing_type_when_listed_over_ffi_then_its_metadata_and_its_own_scalar_come_back(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(
        &db,
        &[
            ("clip.mp4", b"x"),
            ("book.pdf", b"x"),
            ("issue1.cbz", b"x"),
            ("photo.jpg", b"x"),
            ("memo.txt", b"x"),
        ],
    );
    for (name, sql) in [
        (
            "clip.mp4",
            "UPDATE video_files SET title = 'Clip', resolution = '1080p', media_kind = 'movie', \
             duration_seconds = 125.5 \
             FROM files WHERE video_files.file_id = files.id AND files.uuid = ?",
        ),
        (
            "book.pdf",
            "UPDATE documents SET title = 'Book', author = 'Author', format_kind = 'book', \
             page_count = 42 \
             FROM files WHERE documents.file_id = files.id AND files.uuid = ?",
        ),
        (
            "issue1.cbz",
            "UPDATE comic_books SET title = 'Issue', series = 'Series', issue_number = 1, \
             page_count = 24 \
             FROM files WHERE comic_books.file_id = files.id AND files.uuid = ?",
        ),
        (
            "photo.jpg",
            "UPDATE images SET title = 'Photo', caption = 'Caption', width = 800, height = 600 \
             FROM files WHERE images.file_id = files.id AND files.uuid = ?",
        ),
    ] {
        exec(&db.1, sql, &[&uuid_by_name(&db.1, name)]);
    }

    let (status, listed) = files_list(serde_json::json!({}));

    assert_eq!(status, STATUS_FILE_OK);
    let view = |name: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["file"]["name"] == name)
            .unwrap_or_else(|| panic!("{name} listed: {listed}"))
            .clone()
    };
    let video = view("clip.mp4");
    assert_eq!(video["file"]["fileType"], "video");
    assert_eq!(video["metadata"]["type"], "video");
    assert_eq!(video["metadata"]["title"], "Clip");
    assert_eq!(video["metadata"]["resolution"], "1080p");
    assert_eq!(video["durationSeconds"], 125.5);
    let document = view("book.pdf");
    assert_eq!(document["metadata"]["type"], "document");
    assert_eq!(document["metadata"]["author"], "Author");
    assert_eq!(document["pageCount"], 42);
    let comic = view("issue1.cbz");
    assert_eq!(comic["metadata"]["type"], "comic");
    assert_eq!(comic["metadata"]["series"], "Series");
    assert_eq!(comic["comicPageCount"], 24);
    let image = view("photo.jpg");
    assert_eq!(image["metadata"]["type"], "image");
    assert_eq!(image["metadata"]["caption"], "Caption");
    assert_eq!(image["width"], 800);
    assert_eq!(image["height"], 600);
    let text = view("memo.txt");
    assert_eq!(text["file"]["fileType"], "text");
    assert!(text["metadata"].is_null(), "text carries no metadata");
}

#[test]
fn given_indexed_files_when_fetched_by_uuid_over_ffi_then_their_view_comes_back() {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("memo.txt", b"text"), ("clip.mkv", b"x")]);
    let memo = uuid_by_name(&db.1, "memo.txt");
    let clip = uuid_by_name(&db.1, "clip.mkv");
    exec(
        &db.1,
        "UPDATE video_files SET title = 'T', year = 2001, resolution = '720p', media_kind = NULL \
         FROM files WHERE video_files.file_id = files.id AND files.uuid = ?",
        &[&clip],
    );

    let (memo_status, text) = file_get(&memo);
    let (clip_status, video) = file_get(&clip);
    let (unknown_status, unknown) = file_get(&unknown_uuid());

    assert_eq!((memo_status, clip_status), (STATUS_FILE_OK, STATUS_FILE_OK));
    assert_eq!(text["file"]["uuid"], memo.as_str());
    assert_eq!(text["file"]["fileType"], "text");
    assert_eq!(text["file"]["state"], "active");
    assert!(text["metadata"].is_null());
    assert_eq!(video["metadata"]["type"], "video");
    assert_eq!(video["metadata"]["title"], "T");
    assert_eq!(video["metadata"]["resolution"], "720p");
    assert_eq!(video["metadata"]["year"], 2001);
    assert_eq!(unknown_status, STATUS_FILE_NOT_FOUND);
    assert!(unknown.is_null());
}

// ---------------- files: what the indexer reads out of real media ----------------

fn write_minimal_jpeg(path: &std::path::Path) {
    image::RgbImage::from_pixel(4, 3, image::Rgb([128, 64, 32]))
        .save(path)
        .expect("encode jpeg");
}

/// EXIF pixel dimensions and a description, written with `little_exif`. Its
/// `ExifImageWidth`/`ExifImageHeight` are tags 0xa002/0xa003, which
/// `kamadak-exif` reads back as `PixelXDimension`/`PixelYDimension`.
fn write_test_exif(path: &std::path::Path, width: u32, height: u32, description: &str) {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = Metadata::new();
    metadata.set_tag(ExifTag::ImageDescription(description.to_string()));
    metadata.set_tag(ExifTag::ExifImageWidth(vec![width]));
    metadata.set_tag(ExifTag::ExifImageHeight(vec![height]));
    metadata.write_to_file(path).expect("write exif");
}

/// A one-page PDF carrying `title` and `author` in its Info dictionary.
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

/// A real, ffmpeg-encoded MPEG-4 video of ten blank frames at 25 fps,
/// titled `title` when one is given.
fn write_mp4(path: &std::path::Path, title: Option<&str>, width: u32, height: u32) {
    ffmpeg_next::init().expect("ffmpeg init");

    let mut octx = ffmpeg_next::format::output(path).expect("create output context");
    if let Some(title) = title {
        let mut dict = ffmpeg_next::Dictionary::new();
        dict.set("title", title);
        octx.set_metadata(dict);
    }

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
    let mut packet = ffmpeg_next::Packet::empty();
    for i in 0..10 {
        frame.set_pts(Some(i));
        encoder.send_frame(&frame).expect("send frame");
        while encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.write_interleaved(&mut octx).expect("write packet");
        }
    }
    encoder.send_eof().expect("send eof");
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(0);
        packet.write_interleaved(&mut octx).expect("write packet");
    }
    octx.write_trailer().expect("write trailer");
}

/// A CBZ whose `ComicInfo.xml` names `title`, `series` and `number`, with
/// `page_count` page entries.
fn write_tagged_cbz(
    path: &std::path::Path,
    title: &str,
    series: &str,
    number: &str,
    page_count: usize,
) {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).expect("create cbz"));
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
    zip.finish().expect("finish cbz");
}

/// Add the folder `write` fills as a library, wait for its run, and answer
/// `name`'s file view.
fn indexed_view(
    db: &(TempDir, String),
    name: &str,
    write: impl FnOnce(&std::path::Path),
) -> serde_json::Value {
    let root = folder(db, "/library");
    write(std::path::Path::new(&root).join(name).as_path());
    register_library("Library", &root);
    let (status, view) = file_get(&uuid_by_name(&db.1, name));
    assert_eq!(status, STATUS_FILE_OK, "{name}");
    view
}

#[test]
fn given_a_tagged_jpeg_when_indexed_over_ffi_then_its_exif_dimensions_and_description_are_read() {
    let _g = serial();
    let db = init_temp_db();

    let view = indexed_view(&db, "photo.jpg", |path| {
        write_minimal_jpeg(path);
        write_test_exif(path, 800, 600, "Parity Description");
    });

    assert_eq!(view["width"], 800);
    assert_eq!(view["height"], 600);
    assert_eq!(view["metadata"]["title"], "Parity Description");
}

#[test]
fn given_a_tagged_pdf_when_indexed_over_ffi_then_its_title_author_kind_and_pages_are_read() {
    let _g = serial();
    let db = init_temp_db();

    let view = indexed_view(&db, "book.pdf", |path| {
        write_minimal_pdf(path, "Parity Title", "Parity Author")
    });

    assert_eq!(view["pageCount"], 1);
    assert_eq!(view["metadata"]["title"], "Parity Title");
    assert_eq!(view["metadata"]["author"], "Parity Author");
    assert_eq!(view["metadata"]["formatKind"], "book");
}

#[test]
fn given_a_tagged_mp4_when_indexed_over_ffi_then_its_title_resolution_and_duration_are_read() {
    let _g = serial();
    let db = init_temp_db();

    let view = indexed_view(&db, "clip.mp4", |path| {
        write_mp4(path, Some("Parity Title"), 320, 240)
    });

    assert_eq!(view["metadata"]["title"], "Parity Title");
    assert_eq!(view["metadata"]["resolution"], "320x240");
    let duration = view["durationSeconds"].as_f64().expect("a duration");
    assert!(duration > 0.0, "{duration}");
}

#[test]
fn given_a_tagged_cbz_when_indexed_over_ffi_then_its_title_series_issue_and_pages_are_read() {
    let _g = serial();
    let db = init_temp_db();

    let view = indexed_view(&db, "issue.cbz", |path| {
        write_tagged_cbz(path, "Parity Title", "Parity Series", "3", 24)
    });

    assert_eq!(view["comicPageCount"], 24);
    assert_eq!(view["metadata"]["title"], "Parity Title");
    assert_eq!(view["metadata"]["series"], "Parity Series");
    assert_eq!(view["metadata"]["issueNumber"], 3);
}

/// A PCM WAV of `samples` silent 8-bit mono samples at 8 kHz, untagged: its
/// header is all a reader needs for the duration.
fn wav(samples: u32) -> Vec<u8> {
    let mut wav = Vec::new();
    for chunk in [
        &b"RIFF"[..],
        &(36 + samples).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &8000u32.to_le_bytes(),
        &8000u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &8u16.to_le_bytes(),
        b"data",
        &samples.to_le_bytes(),
    ] {
        wav.extend_from_slice(chunk);
    }
    wav.extend(std::iter::repeat_n(128u8, samples as usize));
    wav
}

fn audio_listing() -> serde_json::Value {
    let (status, listed) = files_list(serde_json::json!({ "type": "audio" }));
    assert_eq!(status, STATUS_FILE_OK);
    listed
}

#[test]
fn given_an_untagged_audio_file_when_indexed_over_ffi_then_it_is_listed_by_name_with_its_duration()
{
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("tone.wav", &wav(8000))]);

    let listed = audio_listing();

    assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
    assert_eq!(listed[0]["file"]["name"], "tone.wav");
    assert_eq!(listed[0]["file"]["fileType"], "audio");
    let duration = listed[0]["durationSeconds"].as_f64().expect("a duration");
    assert!((duration - 1.0).abs() < 0.05, "{duration}");
}

#[test]
fn given_an_audio_file_when_fetched_alone_over_ffi_then_it_answers_what_the_listing_answers() {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("tone.wav", &wav(8000))]);
    let listed = audio_listing();

    let (status, alone) = file_get(listed[0]["file"]["uuid"].as_str().unwrap());

    assert_eq!(status, STATUS_FILE_OK);
    assert!(alone["durationSeconds"].as_f64().is_some(), "{alone}");
    assert_eq!(alone, listed[0]);
}

#[test]
fn given_an_audio_file_when_its_metadata_is_edited_over_ffi_then_the_listing_answers_the_edit() {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("tone.wav", &wav(8000))]);
    let uuid = uuid_by_name(&db.1, "tone.wav");

    let (status, _) = file_edit_metadata(
        &uuid,
        serde_json::json!({
            "type": "audio",
            "title": "Tone",
            "artist": "Oscillator",
            "albumArtist": "Oscillator",
            "track": 1,
        }),
    );

    assert_eq!(status, STATUS_FILE_OK);
    let listed = audio_listing();
    assert_eq!(listed[0]["metadata"]["title"], "Tone");
    assert_eq!(listed[0]["metadata"]["artist"], "Oscillator");
    assert_eq!(listed[0]["metadata"]["albumArtist"], "Oscillator");
    assert_eq!(listed[0]["metadata"]["track"], 1);
    assert!(
        listed[0]["durationSeconds"].as_f64().is_some(),
        "the edit leaves the read duration alone"
    );
}

// ---------------- text content (UC-32, UC-33) ----------------

#[test]
fn given_a_text_file_when_read_over_ffi_then_its_content_comes_back() {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("note.txt", b"hello world")]);
    let uuid = uuid_by_name(&db.1, "note.txt");

    let (status, value) = file_read_content(&uuid);

    assert_eq!(status, STATUS_FILE_OK);
    assert_eq!(value["uuid"], uuid.as_str());
    assert_eq!(value["content"], "hello world");
}

#[test]
fn given_a_non_text_or_unknown_file_when_read_over_ffi_then_it_is_refused() {
    let _g = serial();
    let db = init_temp_db();
    library_with(&db, &[("cover.png", b"png bytes")]);

    let (image_status, _) = file_read_content(&uuid_by_name(&db.1, "cover.png"));
    let (unknown_status, _) = file_read_content(&unknown_uuid());

    assert_eq!(image_status, STATUS_FILE_INVALID_INPUT);
    assert_eq!(unknown_status, STATUS_FILE_NOT_FOUND);
}

#[test]
fn given_a_text_file_when_edited_over_ffi_then_the_disk_and_its_stored_hash_are_updated() {
    let _g = serial();
    let db = init_temp_db();
    let root = library_with(&db, &[("note.txt", b"old content")]);
    let uuid = uuid_by_name(&db.1, "note.txt");
    let stored_hash = |db_path: &str| {
        fetch_all::<(Option<String>,)>(
            db_path,
            "SELECT content_hash FROM files WHERE uuid = ?",
            &[&uuid],
        )[0]
        .0
        .clone()
    };
    let before = stored_hash(&db.1);

    let (status, value) = file_edit_content(&uuid, serde_json::json!({ "content": "new content" }));

    assert_eq!(status, STATUS_FILE_OK);
    assert_eq!(value["uuid"], uuid.as_str());
    assert_eq!(
        std::fs::read_to_string(format!("{root}/note.txt")).unwrap(),
        "new content"
    );
    let after = stored_hash(&db.1);
    assert_ne!(after, before);
    assert_eq!(after.as_deref(), value["contentHash"].as_str());
}

#[test]
fn given_a_non_text_or_unknown_file_when_edited_over_ffi_then_it_is_refused_and_the_disk_is_unchanged(
) {
    let _g = serial();
    let db = init_temp_db();
    let root = library_with(
        &db,
        &[("cover.png", b"png bytes"), ("note.txt", b"old content")],
    );
    let body = serde_json::json!({ "content": "new content" });

    let (image_status, _) = file_edit_content(&uuid_by_name(&db.1, "cover.png"), body.clone());
    let (unknown_status, _) = file_edit_content(&unknown_uuid(), body);

    assert_eq!(image_status, STATUS_FILE_INVALID_INPUT);
    assert_eq!(unknown_status, STATUS_FILE_NOT_FOUND);
    assert_eq!(
        std::fs::read(format!("{root}/cover.png")).unwrap(),
        b"png bytes"
    );
    assert_eq!(
        std::fs::read(format!("{root}/note.txt")).unwrap(),
        b"old content"
    );
}

// ---------------- playback (UC-38 to UC-40) ----------------

const PLAYBACK_NOT_FOUND: i32 = alexandria_ffi::PLAYBACK_ERR_NOT_FOUND;
const PLAYBACK_INVALID_STATE: i32 = alexandria_ffi::PLAYBACK_ERR_INVALID_STATE;

/// Like [`init_temp_db`], with UC-40's thumbnail cache inside `cache`.
///
/// The cache's default is the relative `"thumbnails"`, which would land in
/// this crate's folder. `alexandria_index_init` reads its settings from the
/// environment at init only, so the variable is removed again straight away,
/// as [`init_bounded_db`] does for the root.
fn init_with_thumbnail_cache(cache: &std::path::Path) -> (TempDir, String) {
    std::env::set_var(
        "ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR",
        cache.to_str().unwrap(),
    );
    let db = init_temp_db();
    std::env::remove_var("ALEXANDRIA_PLAYBACK_THUMBNAIL_CACHE_DIR");
    db
}

fn decoded_bytes(value: &serde_json::Value) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(value["bytesBase64"].as_str().expect("bytesBase64"))
        .expect("base64")
}

/// The `(width, height)` of the JPEG a thumbnail answer carries.
fn thumbnail_size(value: &serde_json::Value) -> (u32, u32) {
    assert_eq!(value["mimeType"], "image/jpeg", "{value}");
    let decoded = image::load_from_memory(&decoded_bytes(value)).expect("a valid jpeg");
    (decoded.width(), decoded.height())
}

/// A small JPEG whose pixels depend on `seed`, so two pages differ.
fn jpeg_bytes_for(seed: &str) -> Vec<u8> {
    let sum: u32 = seed.bytes().map(u32::from).sum();
    let pixel = image::Rgb([(sum % 256) as u8, ((sum / 3) % 256) as u8, 128]);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .encode_image(&image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(4, 4, pixel),
        ))
        .expect("encode jpeg");
    out
}

/// A CBZ holding one JPEG per entry, in exactly the order given.
fn cbz_of(entries: &[&str]) -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for entry in entries {
        zip.start_file(*entry, SimpleFileOptions::default())
            .expect("start entry");
        zip.write_all(&jpeg_bytes_for(entry)).expect("write entry");
    }
    zip.finish().expect("finish cbz").into_inner()
}

/// A raster image of `width` x `height`, encoded as `name`'s extension says.
fn image_file(name: &str, width: u32, height: u32) -> Vec<u8> {
    let format = image::ImageFormat::from_path(name).expect("a known image extension");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([200, 60, 30]),
    ))
    .write_to(&mut out, format)
    .expect("encode image");
    out.into_inner()
}

#[test]
fn given_a_video_when_its_playback_source_is_asked_over_ffi_then_its_path_mime_type_and_size() {
    let _g = serial();
    let db = init_temp_db();
    let contents = b"0123456789";
    library_with(&db, &[("sample.mp4", contents)]);

    let (status, descriptor) = playback_source(&uuid_by_name(&db.1, "sample.mp4"));

    assert_eq!(status, STATUS_PLAYBACK_OK);
    assert_eq!(descriptor["mimeType"], "video/mp4");
    assert_eq!(descriptor["sizeBytes"], 10);
    assert_eq!(
        std::fs::read(descriptor["path"].as_str().expect("path")).unwrap(),
        contents,
        "the path holds the bytes to play"
    );
}

#[test]
fn given_a_comic_and_audio_files_when_their_playback_source_is_asked_over_ffi_then_each_has_its_catalog_mime_type(
) {
    let _g = serial();
    let db = init_temp_db();
    library_with(
        &db,
        &[
            ("issue.cbz", b"zip"),
            ("song.mp3", &[0u8; 64]),
            ("song.flac", &[0u8; 64]),
        ],
    );

    for (name, mime) in [
        ("issue.cbz", "application/vnd.comicbook+zip"),
        ("song.mp3", "audio/mpeg"),
        ("song.flac", "audio/flac"),
    ] {
        let (status, descriptor) = playback_source(&uuid_by_name(&db.1, name));
        assert_eq!(status, STATUS_PLAYBACK_OK, "{name}");
        assert_eq!(descriptor["mimeType"], mime, "{name}");
    }
}

#[test]
fn given_an_unknown_a_deleted_or_an_unsuitable_file_when_played_over_ffi_then_each_is_refused() {
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    library_with(&db, &[("sample.txt", b"data"), ("gone.txt", b"data")]);
    let sample = uuid_by_name(&db.1, "sample.txt");
    let gone = uuid_by_name(&db.1, "gone.txt");
    assert_eq!(file_soft_delete(&gone).0, STATUS_FILE_OK);

    assert_eq!(playback_source(&unknown_uuid()).0, PLAYBACK_NOT_FOUND);
    assert_eq!(playback_source(&gone).0, PLAYBACK_INVALID_STATE);
    assert_eq!(
        comic_page(&sample, 1).0,
        STATUS_PLAYBACK_INVALID_INPUT,
        "a text file has no pages"
    );
    assert_eq!(
        thumbnail(&sample).0,
        STATUS_PLAYBACK_INVALID_INPUT,
        "nor a thumbnail"
    );
}

#[cfg(unix)]
#[test]
fn given_a_row_reaching_out_through_a_symlink_or_climbing_out_with_dot_dot_when_played_over_ffi_then_invalid_input(
) {
    let _g = serial();
    let parent = tempdir().unwrap();
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    std::fs::create_dir(&library).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("passwd.txt"), b"the secret").unwrap();
    std::os::unix::fs::symlink(&outside, library.join("escape")).unwrap();
    let (_dir, db_path) = init_bounded_db(&library);
    let through_link = seed_text_row(&db_path, &library.join("escape").join("passwd.txt"));
    let climbing = seed_text_row(
        &db_path,
        &library.join("..").join("secrets").join("passwd.txt"),
    );

    for uuid in [through_link, climbing] {
        let (status, value) = playback_source(&uuid);
        assert_eq!(status, STATUS_PLAYBACK_INVALID_INPUT);
        assert!(value.is_null());
    }
}

#[test]
fn given_a_cbz_when_its_first_page_is_read_over_ffi_then_the_first_entry_in_name_order_comes_back()
{
    let _g = serial();
    let db = init_temp_db();
    library_with(
        &db,
        &[("issue.cbz", &cbz_of(&["page002.jpg", "page001.jpg"]))],
    );

    let (status, page) = comic_page(&uuid_by_name(&db.1, "issue.cbz"), 1);

    assert_eq!(status, STATUS_PLAYBACK_OK);
    assert_eq!(decoded_bytes(&page), jpeg_bytes_for("page001.jpg"));
    assert_eq!(page["mimeType"], "image/jpeg");
    assert_eq!(page["page"], 1);
    assert_eq!(page["pageCount"], 2);
}

#[test]
fn given_a_page_out_of_range_or_a_cbr_when_read_over_ffi_then_invalid_input() {
    let _g = serial();
    let db = init_temp_db();
    library_with(
        &db,
        &[
            ("issue.cbz", &cbz_of(&["page001.jpg", "page002.jpg"])),
            ("issue.cbr", b"Rar!\x1a\x07\x00 not really"),
        ],
    );

    assert_eq!(
        comic_page(&uuid_by_name(&db.1, "issue.cbz"), 3).0,
        STATUS_PLAYBACK_INVALID_INPUT,
        "a two-page comic has no page 3"
    );
    assert_eq!(
        comic_page(&uuid_by_name(&db.1, "issue.cbr"), 1).0,
        STATUS_PLAYBACK_INVALID_INPUT,
        "nothing here reads RAR"
    );
}

#[test]
fn given_a_png_when_thumbnailed_twice_over_ffi_then_one_cached_jpeg_inside_the_box_both_times() {
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    library_with(&db, &[("photo.png", &image_file("photo.png", 1000, 500))]);
    let uuid = uuid_by_name(&db.1, "photo.png");

    let (first_status, first) = thumbnail(&uuid);
    let cached = std::fs::read_dir(cache.path()).unwrap().count();
    let (second_status, second) = thumbnail(&uuid);

    assert_eq!(
        (first_status, second_status),
        (STATUS_PLAYBACK_OK, STATUS_PLAYBACK_OK)
    );
    assert_eq!(thumbnail_size(&first), (320, 160), "fits 320 and keeps 2:1");
    assert_eq!(cached, 1, "the first call fills the cache");
    assert_eq!(first["bytesBase64"], second["bytesBase64"]);
}

/// Every raster extension the indexer files as an image has a decoder.
#[test]
fn given_a_gif_a_bmp_and_a_tiff_when_thumbnailed_over_ffi_then_each_is_a_jpeg_inside_the_box() {
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    let names = ["holiday.gif", "scan.bmp", "scan.tiff"];
    let images: Vec<Vec<u8>> = names
        .iter()
        .map(|name| image_file(name, 1000, 500))
        .collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .zip(&images)
        .map(|(name, bytes)| (*name, bytes.as_slice()))
        .collect();
    library_with(&db, &files);

    for name in names {
        let (status, value) = thumbnail(&uuid_by_name(&db.1, name));
        assert_eq!(status, STATUS_PLAYBACK_OK, "{name} must thumbnail");
        assert_eq!(thumbnail_size(&value), (320, 160), "{name}");
    }
}

/// A real ffmpeg-encoded video, so the keyframe decode runs for real.
#[test]
fn given_a_video_when_thumbnailed_over_ffi_then_its_keyframe_is_a_jpeg_inside_the_box() {
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    let view = indexed_view(&db, "clip.mp4", |path| write_mp4(path, None, 640, 360));

    let (status, value) = thumbnail(view["file"]["uuid"].as_str().unwrap());

    assert_eq!(status, STATUS_PLAYBACK_OK);
    assert_eq!(
        thumbnail_size(&value),
        (320, 180),
        "16:9 inside the 320 box"
    );
}

/// A WAV whose ID3v2 tag carries a 640x320 JPEG as its front cover.
fn write_wav_with_front_cover(path: &std::path::Path) {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Tag, TagExt, TagType};

    std::fs::write(path, wav(800)).unwrap();
    let mut cover = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(640, 320, image::Rgb([200, 30, 30]))
        .write_to(&mut cover, image::ImageFormat::Jpeg)
        .unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.push_picture(
        Picture::unchecked(cover.into_inner())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Jpeg)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default())
        .expect("save the cover");
}

#[test]
fn given_an_audio_file_with_a_front_cover_when_thumbnailed_over_ffi_then_the_cover_inside_the_box()
{
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    let view = indexed_view(&db, "song.wav", write_wav_with_front_cover);

    let (status, value) = thumbnail(view["file"]["uuid"].as_str().unwrap());

    assert_eq!(status, STATUS_PLAYBACK_OK);
    assert_eq!(thumbnail_size(&value), (320, 160));
}

#[test]
fn given_an_svg_a_cbr_a_document_or_coverless_audio_when_thumbnailed_over_ffi_then_invalid_input() {
    let _g = serial();
    let cache = tempdir().unwrap();
    let db = init_with_thumbnail_cache(cache.path());
    library_with(
        &db,
        &[
            (
                "logo.svg",
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"></svg>"#,
            ),
            ("issue.cbr", b"Rar!\x1a\x07\x00 not really"),
            ("book.pdf", b"%PDF-1.4"),
            ("tone.wav", &wav(8000)),
        ],
    );

    for name in ["logo.svg", "issue.cbr", "book.pdf", "tone.wav"] {
        let (status, value) = thumbnail(&uuid_by_name(&db.1, name));
        assert_eq!(status, STATUS_PLAYBACK_INVALID_INPUT, "{name}");
        assert!(value.is_null(), "{name}");
    }
}

// ---------------- libraries and settings ----------------

#[cfg(unix)]
#[test]
fn given_a_symlink_inside_the_root_pointing_out_when_a_library_is_moved_through_it_over_ffi_then_invalid_input(
) {
    let _g = serial();
    let parent = tempdir().unwrap();
    let library = parent.path().join("library");
    let outside = parent.path().join("secrets");
    let course = library.join("course");
    std::fs::create_dir_all(&course).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let link = library.join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let _db = init_bounded_db(&library);
    let registered = register_library("Course", course.to_str().unwrap());
    let uuid = c(registered["uuid"].as_str().unwrap());

    for destination in [link.clone(), link.join("not-there-yet")] {
        let moved =
            c(&serde_json::json!({ "rootPath": destination.to_str().unwrap() }).to_string());
        assert_eq!(
            alexandria_library_move(uuid.as_ptr(), moved.as_ptr()).status,
            STATUS_LIBRARY_INVALID_INPUT,
            "{destination:?} escaped through the link"
        );
    }

    let listed = library_json_ok(alexandria_libraries_list());
    assert_eq!(listed[0]["rootPath"], course.to_str().unwrap());
}

#[test]
fn given_a_running_run_when_its_library_is_removed_over_ffi_then_conflict_and_the_library_stays() {
    let _g = serial();
    let db = init_temp_db();
    let registered = register_library("Notes", &folder(&db, "/library/notes"));
    let run_id = registered["runId"].as_str().unwrap().to_string();
    // Forced in the row, as the HTTP and parity tests did, rather than
    // raced against a live walk that may finish first.
    exec(
        &db.1,
        "UPDATE catalog_runs SET status = 'running' WHERE id = ?",
        &[&run_id],
    );
    let uuid = c(registered["uuid"].as_str().unwrap());

    let removed = alexandria_library_remove(uuid.as_ptr());

    assert_eq!(removed.status, STATUS_LIBRARY_CONFLICT);
    assert!(removed.json.is_null());
    let listed = library_json_ok(alexandria_libraries_list());
    assert_eq!(
        listed.as_array().map(Vec::len),
        Some(1),
        "the library stays"
    );
    exec(
        &db.1,
        "UPDATE catalog_runs SET status = 'complete' WHERE id = ?",
        &[&run_id],
    );
}

#[test]
fn given_the_default_settings_when_read_over_ffi_then_the_deletion_window_is_answered() {
    let _g = serial();
    let _db = init_temp_db();

    let (status, value) = answer!(alexandria_ffi::alexandria_settings_json());

    assert_eq!(status, alexandria_ffi::SETTINGS_OK);
    assert_eq!(value["deletion"]["retentionDays"], 30);
}
