#![deny(unsafe_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::runtime::{Builder, Runtime};

use alexandria_core::catalog::commands::index::{IndexRequest};
use alexandria_core::config::Settings;
use alexandria_core::errors::DomainError;
use alexandria_core::migrate::migrate_database;
use alexandria_core::services::{build_services, Services};

static VERSION_CSTRING: &[u8] = b"0.1.0\0";

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static SERVICES: OnceLock<Mutex<Option<Arc<Services>>>> = OnceLock::new();

/// FFI status codes returned by index operations.
pub const INDEX_OK: c_int = 0;
pub const INDEX_ERR_INVALID_INPUT: c_int = 1;
pub const INDEX_ERR_UNAUTHORIZED: c_int = 2;
pub const INDEX_ERR_NOT_INITIALIZED: c_int = 3;
pub const INDEX_ERR_OTHER: c_int = 4;

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("ffi tokio runtime")
    })
}

fn services_slot() -> &'static Mutex<Option<Arc<Services>>> {
    SERVICES.get_or_init(|| Mutex::new(None))
}

/// Result of starting an index run. `run_id` is a NUL-terminated UUID string
/// on success (empty on failure).
#[repr(C)]
#[derive(Debug)]
pub struct IndexStartResult {
    pub status: c_int,
    pub run_id: [c_char; 37],
}

impl IndexStartResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            run_id: [0; 37],
        }
    }

    fn ok(run_id_str: &str) -> Self {
        let mut run_id = [0; 37];
        let bytes = run_id_str.as_bytes();
        let n = bytes.len().min(36);
        for (i, b) in bytes[..n].iter().enumerate() {
            run_id[i] = *b as c_char;
        }
        Self {
            status: INDEX_OK,
            run_id,
        }
    }

    #[allow(dead_code)]
    fn run_id_string(&self) -> String {
        let n = self.run_id.iter().position(|&c| c == 0).unwrap_or(37);
        String::from_utf8_lossy(
            &self.run_id[..n].iter().map(|&c| c as u8).collect::<Vec<u8>>(),
        )
        .into_owned()
    }
}

#[allow(unsafe_code)]
fn cstr_lossy(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller passes a valid NUL-terminated C string.
    let s = unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned();
    Some(s)
}

#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_version() -> *const c_char {
    VERSION_CSTRING.as_ptr() as *const c_char
}

#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_health_status_code() -> i32 {
    200
}

/// Initialize the FFI services against a database path (created/migrated on
/// demand). Safe to call again to point at a different database (replaces).
/// Returns 0 on success, a non-zero status otherwise.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_init(db_path: *const c_char) -> c_int {
    let path = match cstr_lossy(db_path) {
        Some(p) => p,
        None => return INDEX_ERR_INVALID_INPUT,
    };
    let _ = runtime();
    let result = runtime().block_on(async {
        let pool = migrate_database(&path).await?;
        let services = Arc::new(build_services(&Settings::default(), pool).await);
        *services_slot().lock().unwrap() = Some(services);
        Ok::<(), DomainError>(())
    });
    match result {
        Ok(()) => INDEX_OK,
        Err(_) => INDEX_ERR_OTHER,
    }
}

/// Start an asynchronous index scan of `root`. Returns a `IndexStartResult`
/// with a `run_id` and `status` (parity with HTTP 202 body). The scan runs in
/// the background on the FFI runtime; read results via the accessor functions.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_start(
    root: *const c_char,
    token: *const c_char,
) -> IndexStartResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return IndexStartResult::err(INDEX_ERR_NOT_INITIALIZED),
    };
    let root = match cstr_lossy(root) {
        Some(r) => r,
        None => return IndexStartResult::err(INDEX_ERR_INVALID_INPUT),
    };
    let token = cstr_lossy(token).unwrap_or_default();
    let rt = runtime();

    let started = rt.block_on(async {
        services
            .index_handler
            .start(IndexRequest { root: root.clone() }, &token)
            .await
    });

    match started {
        Ok(s) => {
            let run_id = s.run_id;
            let handler = services.index_handler.clone();
            rt.spawn(async move {
                let _ = handler.execute(&root, run_id).await;
            });
            IndexStartResult::ok(&s.run_id.to_string())
        }
        Err(err) => match err {
            DomainError::InvalidInput(_) => IndexStartResult::err(INDEX_ERR_INVALID_INPUT),
            DomainError::Unauthorized => IndexStartResult::err(INDEX_ERR_UNAUTHORIZED),
            _ => IndexStartResult::err(INDEX_ERR_OTHER),
        },
    }
}

/// Count of indexed files. For tests waiting for the background scan.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_count_files() -> i64 {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return -1,
    };
    runtime().block_on(async {
        let row: Result<(i64,), _> =
            sqlx::query_as("SELECT COUNT(*) FROM files").fetch_one(&services.pool).await;
        row.map(|(c,)| c).unwrap_or(-1)
    })
}

/// JSON array of `{"path","name","type","hash"}` for every indexed file, or a
/// NUL pointer on error. Caller must free it with `alexandria_free_string`.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_files_json() -> *mut c_char {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let rows: Vec<(String, String, String, String)> = runtime()
        .block_on(async {
            sqlx::query_as("SELECT path, name, type, content_hash FROM files ORDER BY path")
                .fetch_all(&services.pool)
                .await
        })
        .unwrap_or_default();

    let arr: Vec<_> = rows
        .iter()
        .map(|(p, n, t, h)| {
            serde_json::json!({ "path": p, "name": n, "type": t, "hash": h })
        })
        .collect();
    let json = serde_json::Value::Array(arr).to_string();
    let cstring = CString::new(json).unwrap_or_default();
    cstring.into_raw()
}

/// Free a string previously returned by an FFI accessor.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: `ptr` came from `CString::into_raw` above.
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}