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

/// FFI status codes returned by file operations (UC-04+). Deliberately
/// separate from `INDEX_*` so a future use case can grow either set without
/// colliding; `FILE_OK == INDEX_OK == 0` by convention.
pub const FILE_OK: c_int = 0;
pub const FILE_ERR_INVALID_INPUT: c_int = 1;
pub const FILE_ERR_UNAUTHORIZED: c_int = 2;
pub const FILE_ERR_NOT_INITIALIZED: c_int = 3;
pub const FILE_ERR_NOT_FOUND: c_int = 4;
pub const FILE_ERR_INVALID_STATE: c_int = 5;
pub const FILE_ERR_OTHER: c_int = 9;

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

/// Start an asynchronous re-index/refresh of every cataloged path (UC-02).
/// Takes only a token (no root — refresh touches everything cataloged) and
/// returns a `IndexStartResult` with a `run_id` and `status` (parity with the
/// HTTP `POST /v1/index/refresh` 202 body). The refresh runs in the background
/// on the FFI runtime; read results via the accessor functions.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_refresh_start(token: *const c_char) -> IndexStartResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return IndexStartResult::err(INDEX_ERR_NOT_INITIALIZED),
    };
    let token = cstr_lossy(token).unwrap_or_default();
    let rt = runtime();

    let started = rt
        .block_on(async { services.refresh_handler.start(&token).await });

    match started {
        Ok(s) => {
            let run_id = s.run_id;
            let handler = services.refresh_handler.clone();
            rt.spawn(async move {
                let _ = handler.execute(run_id).await;
            });
            IndexStartResult::ok(&s.run_id.to_string())
        }
        Err(DomainError::Unauthorized) => IndexStartResult::err(INDEX_ERR_UNAUTHORIZED),
        Err(_) => IndexStartResult::err(INDEX_ERR_OTHER),
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

/// Result of `alexandria_file_edit_metadata` (UC-04). On success `status` is
/// `FILE_OK` and `json` is a NUL-terminated JSON string of the `FileMetadata`
/// body — byte-for-byte the same shape HTTP returns from
/// `PATCH /v1/files/{uuid}/metadata` (FR-FC-24 / NFR-09). On failure `json`
/// is NULL and `status` carries the mapped error code. The caller must free
/// `json` with `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct FileMetadataResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl FileMetadataResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: FILE_OK,
            json: cstring.into_raw(),
        }
    }
}

/// Edit a file's type-specific metadata (UC-04 / FR-FC-14..18).
///
/// `uuid` is the file's public UUID string; `json_patch` is the JSON body
/// (the `SubtypeMetadata` enum, internally tagged by `type`) that HTTP would
/// send. The function deserializes it, calls the same `EditMetadataHandler`
/// the HTTP route uses, and on success serializes the returned `FileMetadata`
/// back to JSON — so the FFI and HTTP surfaces agree byte-for-byte modulo key
/// ordering (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_file_edit_metadata(
    uuid: *const c_char,
    json_patch: *const c_char,
    token: *const c_char,
) -> FileMetadataResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileMetadataResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileMetadataResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileMetadataResult::err(FILE_ERR_INVALID_INPUT),
    };

    let patch_str = match cstr_lossy(json_patch) {
        Some(s) => s,
        None => return FileMetadataResult::err(FILE_ERR_INVALID_INPUT),
    };
    let metadata: alexandria_core::catalog::model::SubtypeMetadata =
        match serde_json::from_str(&patch_str) {
            Ok(m) => m,
            Err(_) => return FileMetadataResult::err(FILE_ERR_INVALID_INPUT),
        };

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async {
        services
            .edit_metadata_handler
            .edit(uuid, metadata, &token)
            .await
    });

    match result {
        Ok(file_metadata) => {
            let json = serde_json::to_string(&file_metadata).unwrap_or_default();
            FileMetadataResult::ok(json)
        }
        Err(err) => match err {
            DomainError::NotFound => FileMetadataResult::err(FILE_ERR_NOT_FOUND),
            DomainError::Unauthorized => FileMetadataResult::err(FILE_ERR_UNAUTHORIZED),
            DomainError::InvalidInput(_) => FileMetadataResult::err(FILE_ERR_INVALID_INPUT),
            DomainError::InvalidState => FileMetadataResult::err(FILE_ERR_INVALID_STATE),
            _ => FileMetadataResult::err(FILE_ERR_OTHER),
        },
    }
}

/// Result of `alexandria_files_list` and `alexandria_file_get_by_uuid` (UC-03).
/// On success `status` is `FILE_OK` and `json` is a NUL-terminated JSON string
/// — byte-for-byte the same shape HTTP returns from `GET /v1/files` (a JSON
/// array of `File` records) or `GET /v1/files/{uuid}` (a `FileView` object),
/// so the FFI and HTTP surfaces agree modulo key ordering (parity, FR-FC-24 /
/// NFR-09). On failure `json` is NULL and `status` carries the mapped error
/// code. The caller must free `json` with `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct FileJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl FileJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: FILE_OK,
            json: cstring.into_raw(),
        }
    }
}

/// JSON filter body accepted by `alexandria_files_list` (UC-03 / FR-FC-12).
/// Both fields optional; an empty/null body or omitted fields use the defaults
/// (`file_type = None`, `state = "active"` — excludes deleted records per
/// the use case's main-flow step 2). Unknown `type` values map to no type
/// filter; unknown `state` values default to `active`.
#[derive(Debug, Default)]
struct FilesListFilter {
    file_type: Option<String>,
    state: Option<String>,
}

impl FilesListFilter {
    fn from_json_str(s: &str) -> Option<Self> {
        if s.trim().is_empty() {
            return Some(Self::default());
        }
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        if value.is_null() {
            return Some(Self::default());
        }
        let obj = value.as_object()?;
        Some(Self {
            file_type: obj
                .get("type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            state: obj.get("state").and_then(|v| v.as_str()).map(|s| s.to_string()),
        })
    }
}

/// List/query files filtered by type and lifecycle state (UC-03 / FR-FC-12).
///
/// `json_filters` is a JSON string `{"type":"audio","state":"all"}` (empty
/// string or NULL for defaults). The function deserializes it, calls the same
/// `BrowseFilesHandler` the HTTP route uses, and on success serializes the
/// returned `Vec<File>` back to a JSON array — so the FFI and HTTP surfaces
/// agree byte-for-byte modulo key ordering (parity, FR-FC-24 / NFR-09).
/// `token` is the bearer auth token.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_files_list(
    json_filters: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match FilesListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let file_type = parsed.file_type.as_deref().and_then(parse_file_type);
    let state = parsed
        .state
        .as_deref()
        .and_then(alexandria_core::catalog::model::StateFilter::parse)
        .unwrap_or(alexandria_core::catalog::model::StateFilter::Active);

    let mut filter =
        alexandria_core::catalog::queries::browse::FileFilter::new().with_state(state);
    if let Some(t) = file_type {
        filter = filter.with_type(t);
    }

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async {
        services.browse_files_handler.list(filter, &token).await
    });

    match result {
        Ok(files) => {
            let json = serde_json::to_string(&files).unwrap_or_else(|_| "[]".to_string());
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Get a single file's metadata by its public UUID (UC-03 / FR-FC-13).
///
/// `uuid` is the file's public UUID string. The function calls the same
/// `BrowseFilesHandler::get_by_uuid` the HTTP route uses, and on success
/// serializes the returned `FileView` back to JSON — the same shape HTTP
/// returns from `GET /v1/files/{uuid}` (parity, FR-FC-24 / NFR-09). `token`
/// is the bearer auth token.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_file_get_by_uuid(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async {
        services.browse_files_handler.get_by_uuid(uuid, &token).await
    });

    match result {
        Ok(view) => {
            let json = serde_json::to_string(&view).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

fn parse_file_type(s: &str) -> Option<alexandria_core::catalog::model::FileType> {
    use alexandria_core::catalog::model::FileType;
    match s {
        "audio" => Some(FileType::Audio),
        "video" => Some(FileType::Video),
        "html" => Some(FileType::Html),
        "text" => Some(FileType::Text),
        "document" => Some(FileType::Document),
        "comic" => Some(FileType::Comic),
        "image" => Some(FileType::Image),
        _ => None,
    }
}

fn map_file_err(err: DomainError) -> FileJsonResult {
    match err {
        DomainError::NotFound => FileJsonResult::err(FILE_ERR_NOT_FOUND),
        DomainError::Unauthorized => FileJsonResult::err(FILE_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => FileJsonResult::err(FILE_ERR_INVALID_INPUT),
        DomainError::InvalidState => FileJsonResult::err(FILE_ERR_INVALID_STATE),
        _ => FileJsonResult::err(FILE_ERR_OTHER),
    }
}

/// Count of cataloged files currently marked missing on disk (UC-02 AF-01).
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn alexandria_index_count_missing() -> i64 {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return -1,
    };
    runtime().block_on(async {
        let row: Result<(i64,), _> =
            sqlx::query_as("SELECT COUNT(*) FROM files WHERE missing_at IS NOT NULL")
                .fetch_one(&services.pool)
                .await;
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
    let rows: Vec<(String, String, String, String, Option<String>)> = runtime()
        .block_on(async {
            sqlx::query_as(
                "SELECT path, name, type, content_hash, missing_at \
                 FROM files ORDER BY path",
            )
            .fetch_all(&services.pool)
            .await
        })
        .unwrap_or_default();

    let arr: Vec<_> = rows
        .iter()
        .map(|(p, n, t, h, m)| {
            serde_json::json!({
                "path": p,
                "name": n,
                "type": t,
                "hash": h,
                "missingAt": m,
            })
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