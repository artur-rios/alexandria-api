#![deny(unsafe_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::runtime::{Builder, Runtime};

use alexandria_core::auth::AuthService;
use alexandria_core::catalog::commands::index::IndexRequest;
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
pub const FILE_ERR_DISK: c_int = 6;
pub const FILE_ERR_OTHER: c_int = 9;

/// FFI status codes returned by collection operations (UC-10+). Deliberately
/// separate from `INDEX_*` and `FILE_*` — per the convention above — so the
/// Collections use cases can grow their own set without colliding;
/// `COLLECTION_OK == FILE_OK == 0` by convention. There is no disk code: a
/// collection is catalog-only metadata with nothing on disk.
pub const COLLECTION_OK: c_int = 0;
pub const COLLECTION_ERR_INVALID_INPUT: c_int = 1;
pub const COLLECTION_ERR_UNAUTHORIZED: c_int = 2;
pub const COLLECTION_ERR_NOT_INITIALIZED: c_int = 3;
pub const COLLECTION_ERR_NOT_FOUND: c_int = 4;
pub const COLLECTION_ERR_INVALID_STATE: c_int = 5;
pub const COLLECTION_ERR_OTHER: c_int = 9;

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

/// Authenticate the caller before any payload is looked at.
///
/// The handlers authenticate too, but they can only do so *after* their
/// arguments have been parsed — which let a caller with no credentials learn
/// that its UUID or JSON was malformed. SRD §7 and FR-AU-07 deny an
/// unauthenticated call outright, and the HTTP surface gates the same way in
/// its `require_auth` middleware (FR-FC-24 / NFR-09).
fn authenticated(services: &Services, token: &str) -> bool {
    runtime().block_on(async { services.auth.authenticate(token).await.is_ok() })
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
            &self.run_id[..n]
                .iter()
                .map(|&c| c as u8)
                .collect::<Vec<u8>>(),
        )
        .into_owned()
    }
}

#[allow(unsafe_code)] // dereferences a caller-supplied raw pointer
fn cstr_lossy(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller passes a valid NUL-terminated C string.
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    Some(s)
}

#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_version() -> *const c_char {
    VERSION_CSTRING.as_ptr() as *const c_char
}

#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_health_status_code() -> i32 {
    200
}

/// Initialize the FFI services against a database path (created/migrated on
/// demand). Safe to call again to point at a different database (replaces).
/// Returns 0 on success, a non-zero status otherwise.
///
/// Configuration is loaded the same way `alexandria-http` loads it — from the
/// path in `ALEXANDRIA_CONFIG` (default `config.toml`), with `ALEXANDRIA_*`
/// environment overrides applied — so a setting such as the auth mode or the
/// retention window means the same thing on both surfaces (FR-FC-24 / NFR-09).
/// A missing or unreadable config file falls back to defaults rather than
/// failing, matching the HTTP binary. `db_path` wins over the config's
/// `database.path`: the embedder passed it explicitly.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_init(db_path: *const c_char) -> c_int {
    let path = match cstr_lossy(db_path) {
        Some(p) => p,
        None => return INDEX_ERR_INVALID_INPUT,
    };
    let mut settings = load_settings();
    settings.database.path = path.clone();
    let _ = runtime();
    let result = runtime().block_on(async {
        let pool = migrate_database(&path).await?;
        let services = Arc::new(build_services(&settings, pool).await);
        *services_slot().lock().unwrap() = Some(services);
        Ok::<(), DomainError>(())
    });
    match result {
        Ok(()) => INDEX_OK,
        Err(_) => INDEX_ERR_OTHER,
    }
}

/// Load settings exactly as the HTTP binary does: `ALEXANDRIA_CONFIG` or
/// `config.toml`, then `ALEXANDRIA_*` environment overrides.
fn load_settings() -> Settings {
    let config_path = std::env::var("ALEXANDRIA_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"));
    Settings::load_or_default(&config_path)
}

/// Start an asynchronous index scan of `root`. Returns a `IndexStartResult`
/// with a `run_id` and `status` (parity with HTTP 202 body). The scan runs in
/// the background on the FFI runtime; read results via the accessor functions.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
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
                // Per-file failures are counted inside `execute`; an `Err` here
                // means the run could not start at all. Log it — nothing else
                // observes this task's result.
                if let Err(err) = handler.execute(&root, run_id).await {
                    tracing::error!(%run_id, error = %err, "index run aborted");
                }
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
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_refresh_start(token: *const c_char) -> IndexStartResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return IndexStartResult::err(INDEX_ERR_NOT_INITIALIZED),
    };
    let token = cstr_lossy(token).unwrap_or_default();
    let rt = runtime();

    let started = rt.block_on(async { services.refresh_handler.start(&token).await });

    match started {
        Ok(s) => {
            let run_id = s.run_id;
            let handler = services.refresh_handler.clone();
            rt.spawn(async move {
                // Per-file failures are counted inside `execute`; an `Err` here
                // means the run could not start at all. Log it — nothing else
                // observes this task's result.
                if let Err(err) = handler.execute(run_id).await {
                    tracing::error!(%run_id, error = %err, "re-index run aborted");
                }
            });
            IndexStartResult::ok(&s.run_id.to_string())
        }
        Err(DomainError::Unauthorized) => IndexStartResult::err(INDEX_ERR_UNAUTHORIZED),
        Err(_) => IndexStartResult::err(INDEX_ERR_OTHER),
    }
}

/// Count of indexed files. For tests waiting for the background scan.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_count_files() -> i64 {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return -1,
    };
    runtime().block_on(async {
        let row: Result<(i64,), _> = sqlx::query_as("SELECT COUNT(*) FROM files")
            .fetch_one(&services.pool)
            .await;
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
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
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

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its uuid or body would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileMetadataResult::err(FILE_ERR_UNAUTHORIZED);
    }

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
/// Both fields optional; an empty/null body, omitted fields, or empty-string
/// values use the defaults (`file_type = None`, `state = "active"` — excludes
/// deleted records per the use case's main-flow step 2). An unrecognised
/// `type` or `state` value is rejected as `FILE_ERR_INVALID_INPUT`, matching
/// the HTTP surface's `400` (FR-FC-24 / NFR-09).
#[derive(Debug, Default)]
struct FilesListFilter {
    file_type: Option<String>,
    state: Option<String>,
    collection_uuid: Option<String>,
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
            state: obj
                .get("state")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            collection_uuid: obj
                .get("collectionUuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
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
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_files_list(
    json_filters: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its filters would have validated.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match FilesListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    // An unrecognised filter value is invalid input, not a silently dropped
    // filter — HTTP answers `400` for these and the two surfaces must agree
    // (FR-FC-24 / NFR-09). An empty string means "no filter", as on HTTP.
    let file_type = match parsed.file_type.as_deref().filter(|s| !s.is_empty()) {
        Some(t) => match parse_file_type(t) {
            Some(ft) => Some(ft),
            None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
        },
        None => None,
    };
    let state = match parsed.state.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match alexandria_core::catalog::model::StateFilter::parse(s) {
            Some(st) => st,
            None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
        },
        None => alexandria_core::catalog::model::StateFilter::Active,
    };

    let mut filter = alexandria_core::catalog::queries::browse::FileFilter::new().with_state(state);
    if let Some(t) = file_type {
        filter = filter.with_type(t);
    }
    if let Some(c) = parsed.collection_uuid.as_deref().filter(|s| !s.is_empty()) {
        let collection_uuid = match uuid::Uuid::parse_str(c) {
            Ok(u) => u,
            Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
        };
        filter = filter.with_collection(collection_uuid);
    }

    let result =
        runtime().block_on(async { services.browse_files_handler.list(filter, &token).await });

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
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_get_by_uuid(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .browse_files_handler
            .get_by_uuid(uuid, &token)
            .await
    });

    match result {
        Ok(view) => {
            let json = serde_json::to_string(&view).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Rename a file (and its on-disk file) (UC-05 / FR-FC-19).
///
/// `uuid` is the file's public UUID string; `name` is the new file name. The
/// function calls the same `RenameFileHandler` the HTTP route uses and on
/// success serializes the returned `File` back to JSON — the same shape HTTP
/// returns from `POST /v1/files/{uuid}/rename`, so the FFI and HTTP surfaces
/// agree byte-for-byte modulo key ordering (parity, FR-FC-24 / NFR-09).
/// `token` is the bearer auth token. A disk failure (AF-02) maps to
/// `FILE_ERR_DISK`; the catalog is left untouched in that case.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_rename(
    uuid: *const c_char,
    name: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its uuid or name would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let name = match cstr_lossy(name) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    // An empty/whitespace name is rejected by the handler's validator, so it
    // surfaces as `FILE_ERR_INVALID_INPUT` — consistent with the HTTP `400`.

    let result = runtime().block_on(async {
        services
            .rename_file_handler
            .rename(uuid, name, &token)
            .await
    });

    match result {
        Ok(file) => {
            let json = serde_json::to_string(&file).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Soft-delete a file (UC-06 / FR-FC-20).
///
/// `uuid` is the file's public UUID string; `token` is the bearer auth token.
/// The function calls the same `SoftDeleteFileHandler` the HTTP route uses
/// and on success serializes the returned `File` back to JSON — the same
/// shape HTTP returns from `DELETE /v1/files/{uuid}`, so the FFI and HTTP
/// surfaces agree byte-for-byte modulo key ordering (parity, FR-FC-24 /
/// NFR-09). The on-disk file is untouched (only `state` and `deleted_at`
/// change on the catalog row).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_soft_delete(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .soft_delete_file_handler
            .soft_delete(uuid, &token)
            .await
    });

    match result {
        Ok(file) => {
            let json = serde_json::to_string(&file).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Restore a soft-deleted file (UC-07 / FR-FC-21).
///
/// `uuid` is the file's public UUID string; `token` is the bearer auth token.
/// The function calls the same `RestoreFileHandler` the HTTP route uses and
/// on success serializes the returned `File` back to JSON — the same shape
/// HTTP returns from `POST /v1/files/{uuid}/restore`, so the FFI and HTTP
/// surfaces agree byte-for-byte modulo key ordering (parity, FR-FC-24 /
/// NFR-09). The on-disk file is untouched (only `state` and `deleted_at`
/// change on the catalog row). The retention window (default 30 days,
/// NFR-10) is enforced; a record past it is reported as `NotFound`
/// (`FILE_ERR_NOT_FOUND`) since UC-08 owns the actual hard purge.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_restore(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.restore_file_handler.restore(uuid, &token).await });

    match result {
        Ok(file) => {
            let json = serde_json::to_string(&file).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Hard-purge a soft-deleted file's catalog row (UC-08 / FR-FC-22).
///
/// `uuid` is the file's public UUID string; `token` is the bearer auth
/// token. The function calls the same `PurgeFileHandler` the HTTP route
/// uses (`DELETE /v1/files/{uuid}?purge=true`) and on success serializes
/// the pre-delete `File` back to JSON as confirmation — the same shape
/// HTTP returns, so the FFI and HTTP surfaces agree byte-for-byte modulo
/// key ordering (parity, FR-FC-24 / NFR-09). Only the catalog row (and its
/// subtype row) is removed; the on-disk file is untouched (NFR-07). The
/// retention window (default 30 days, NFR-10) is enforced: a record still
/// within it, or not `deleted`, is reported as `FILE_ERR_INVALID_STATE`.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_purge(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.purge_file_handler.purge(uuid, &token).await });

    match result {
        Ok(file) => {
            let json = serde_json::to_string(&file).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Purge a file both on disk and in the catalog (UC-09 / FR-FC-23).
///
/// `uuid` is the file's public UUID string; `token` is the bearer auth
/// token. The function calls the same `PurgeFileOnDiskHandler` the HTTP
/// route uses (`DELETE /v1/files/{uuid}?purge-on-disk=true`) and on success
/// serializes the returned [`PurgeOnDiskOutcome`] back to JSON — the same
/// shape HTTP returns, so the FFI and HTTP surfaces agree byte-for-byte
/// modulo key ordering (parity, FR-FC-24 / NFR-09). Unlike UC-08 there is no
/// retention gate: an `active` or `deleted` record is purgeable, the only
/// precondition is that it exists. A missing on-disk file is still a
/// success, reported via `diskFilePresent: false` (AF-01); a disk failure
/// is reported as `FILE_ERR_DISK` (AF-02) and leaves the record untouched.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_purge_on_disk(
    uuid: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return FileJsonResult::err(FILE_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .purge_file_on_disk_handler
            .purge_on_disk(uuid, &token)
            .await
    });

    match result {
        Ok(outcome) => {
            let json = serde_json::to_string(&outcome).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Result of `alexandria_collection_create` (UC-10). On success `status` is
/// `COLLECTION_OK` and `json` is a NUL-terminated JSON string of the
/// `Collection` body — byte-for-byte the same shape HTTP returns from
/// `POST /v1/collections` (FR-FC-24 / NFR-09). On failure `json` is NULL and
/// `status` carries the mapped error code. The caller must free `json` with
/// `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct CollectionJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl CollectionJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: COLLECTION_OK,
            json: cstring.into_raw(),
        }
    }
}

/// Request body accepted by `alexandria_collection_create` — the same JSON
/// `POST /v1/collections` takes: `{"name":"Sci-fi novels","kind":"file"}`.
/// Parsing here is what rejects a missing field or an unrecognised `kind` as
/// `COLLECTION_ERR_INVALID_INPUT`, matching the HTTP surface's `400`
/// (FR-FC-24 / NFR-09).
///
/// Parsed field-by-field off a `serde_json::Value` rather than by `derive`,
/// like `FilesListFilter` above: this crate depends on `serde_json` but not on
/// `serde` itself. Both fields are required and must be strings, and unknown
/// fields are ignored — the same three decisions axum's `Json` extractor makes
/// for the HTTP body.
#[derive(Debug)]
struct CreateCollectionBody {
    name: String,
    kind: alexandria_core::collections::model::CollectionKind,
}

impl CreateCollectionBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let name = obj.get("name")?.as_str()?.to_string();
        let kind =
            alexandria_core::collections::model::CollectionKind::parse(obj.get("kind")?.as_str()?)?;
        Some(Self { name, kind })
    }
}

/// Create a flat file or bookmark collection (UC-10 / FR-CO-01, FR-CO-02).
///
/// `json_body` is the JSON body HTTP would send (`name` + `kind`). The
/// function deserializes it, calls the same `CreateCollectionHandler` the HTTP
/// route uses, and on success serializes the returned `Collection` back to
/// JSON — so the FFI and HTTP surfaces agree byte-for-byte modulo key ordering
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_create(
    json_body: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its body would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let body = match CreateCollectionBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    // An empty or otherwise invalid name is rejected by the handler's
    // validator, so it surfaces as `COLLECTION_ERR_INVALID_INPUT` —
    // consistent with the HTTP `400`.

    let result = runtime().block_on(async {
        services
            .create_collection_handler
            .create(&body.name, body.kind, &token)
            .await
    });

    match result {
        Ok(collection) => {
            let json = serde_json::to_string(&collection).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

fn map_collection_err(err: DomainError) -> CollectionJsonResult {
    match err {
        DomainError::NotFound => CollectionJsonResult::err(COLLECTION_ERR_NOT_FOUND),
        DomainError::Unauthorized => CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
        DomainError::InvalidState => CollectionJsonResult::err(COLLECTION_ERR_INVALID_STATE),
        _ => CollectionJsonResult::err(COLLECTION_ERR_OTHER),
    }
}

/// Request body accepted by `alexandria_collection_rename` — the same JSON
/// `PATCH /v1/collections/{uuid}` takes: `{"name":"Sci-fi novels"}`. Parsed
/// field-by-field for the same reason `CreateCollectionBody` is: this crate
/// depends on `serde_json` but not `serde`'s derive.
#[derive(Debug)]
struct RenameCollectionBody {
    name: String,
}

impl RenameCollectionBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let name = obj.get("name")?.as_str()?.to_string();
        Some(Self { name })
    }
}

/// Rename a collection (UC-11 / FR-CO-03).
///
/// `uuid` is the collection's public UUID (NUL-terminated string). `json_body`
/// is the JSON body HTTP would send (`{"name": …}`). The function
/// deserializes it, calls the same `RenameCollectionHandler` the HTTP route
/// uses, and on success serializes the returned `Collection` back to JSON —
/// so the FFI and HTTP surfaces agree byte-for-byte modulo key ordering
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_rename(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid or body would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let body = match RenameCollectionBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .rename_collection_handler
            .rename(uuid, &body.name, &token)
            .await
    });

    match result {
        Ok(collection) => {
            let json = serde_json::to_string(&collection).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

/// Delete a collection, unlinking its items (UC-12 / FR-CO-04).
///
/// `uuid` is the collection's public UUID (NUL-terminated string). On success
/// `json` carries the pre-delete `Collection` as confirmation — byte-for-byte
/// the same shape HTTP returns from `DELETE /v1/collections/{uuid}` (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_delete(
    uuid: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether the uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .delete_collection_handler
            .delete(uuid, &token)
            .await
    });

    match result {
        Ok(collection) => {
            let json = serde_json::to_string(&collection).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

/// Request body accepted by `alexandria_collection_add_items` — the same
/// JSON `POST /v1/collections/{uuid}/items` takes:
/// `{"itemUuids":["…","…"]}`. Parsed field-by-field for the same reason
/// `CreateCollectionBody` is: this crate depends on `serde_json` but not
/// `serde`'s derive.
#[derive(Debug)]
struct AddItemsBody {
    item_uuids: Vec<uuid::Uuid>,
}

impl AddItemsBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let raw = obj.get("itemUuids")?.as_array()?;
        let mut item_uuids = Vec::with_capacity(raw.len());
        for v in raw {
            item_uuids.push(uuid::Uuid::parse_str(v.as_str()?).ok()?);
        }
        Some(Self { item_uuids })
    }
}

/// Add items to a collection (UC-13 / FR-CO-05).
///
/// `uuid` is the collection's public UUID (NUL-terminated string). `json_body`
/// is the JSON body HTTP would send (`itemUuids`). The function deserializes
/// it, calls the same `AddItemsToCollectionHandler` the HTTP route uses, and
/// on success serializes the returned `CollectionItemsResult` back to JSON —
/// so the FFI and HTTP surfaces agree byte-for-byte modulo key ordering
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_add_items(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid or body would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };
    let body = match AddItemsBody::from_json_str(&body_str) {
        Some(b) if !b.item_uuids.is_empty() => b,
        _ => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .add_items_to_collection_handler
            .add(uuid, body.item_uuids, &token)
            .await
    });

    match result {
        Ok(added) => {
            let json = serde_json::to_string(&added).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

/// Remove an item from a collection (UC-14 / FR-CO-06).
///
/// `collection_uuid` and `item_uuid` are the collection's and item's public
/// UUIDs (NUL-terminated strings). On success `json` carries the
/// `collectionUuid`/`itemUuid` confirmation — byte-for-byte the same shape
/// HTTP returns from `DELETE /v1/collections/{uuid}/items/{itemUuid}`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_remove_item(
    collection_uuid: *const c_char,
    item_uuid: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let collection_uuid =
        match cstr_lossy(collection_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
            Some(u) => u,
            None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
        };
    let item_uuid = match cstr_lossy(item_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .remove_item_from_collection_handler
            .remove(collection_uuid, item_uuid, &token)
            .await
    });

    match result {
        Ok(removed) => {
            let json = serde_json::to_string(&removed).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

/// List the items in a collection (UC-14 / FR-CO-07).
///
/// `uuid` is the collection's public UUID (NUL-terminated string). On
/// success `json` carries the `kind` and current members — byte-for-byte
/// the same shape HTTP returns from `GET /v1/collections/{uuid}/items`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collection_list_items(
    uuid: *const c_char,
    token: *const c_char,
) -> CollectionJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return CollectionJsonResult::err(COLLECTION_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return CollectionJsonResult::err(COLLECTION_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .list_collection_items_handler
            .list(uuid, &token)
            .await
    });

    match result {
        Ok(members) => {
            let json = serde_json::to_string(&members).unwrap_or_default();
            CollectionJsonResult::ok(json)
        }
        Err(err) => map_collection_err(err),
    }
}

/// FFI status codes returned by bookmark operations (UC-15+). Deliberately
/// separate from `COLLECTION_*` — per the convention above — so bookmark use
/// cases can grow their own set without colliding; `BOOKMARK_OK ==
/// COLLECTION_OK == 0` by convention. There is no disk code: a bookmark is
/// catalog-only metadata with nothing on disk.
pub const BOOKMARK_OK: c_int = 0;
pub const BOOKMARK_ERR_INVALID_INPUT: c_int = 1;
pub const BOOKMARK_ERR_UNAUTHORIZED: c_int = 2;
pub const BOOKMARK_ERR_NOT_INITIALIZED: c_int = 3;
pub const BOOKMARK_ERR_NOT_FOUND: c_int = 4;
pub const BOOKMARK_ERR_INVALID_STATE: c_int = 5;
pub const BOOKMARK_ERR_OTHER: c_int = 9;

/// Result of `alexandria_bookmark_create` (UC-15). On success `status` is
/// `BOOKMARK_OK` and `json` is a NUL-terminated JSON string of the `Bookmark`
/// body — byte-for-byte the same shape HTTP returns from `POST
/// /v1/bookmarks` (FR-FC-24 / NFR-09). On failure `json` is NULL and `status`
/// carries the mapped error code. The caller must free `json` with
/// `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct BookmarkJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl BookmarkJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: BOOKMARK_OK,
            json: cstring.into_raw(),
        }
    }
}

fn map_bookmark_err(err: DomainError) -> BookmarkJsonResult {
    match err {
        DomainError::NotFound => BookmarkJsonResult::err(BOOKMARK_ERR_NOT_FOUND),
        DomainError::Unauthorized => BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
        DomainError::InvalidState => BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_STATE),
        _ => BookmarkJsonResult::err(BOOKMARK_ERR_OTHER),
    }
}

/// Request body accepted by `alexandria_bookmark_create` — the same JSON
/// `POST /v1/bookmarks` takes: `{"url":"…","title":"…","collectionUuid":"…"}`
/// (`collectionUuid` optional/nullable). Parsed field-by-field for the same
/// reason `CreateCollectionBody` is: this crate depends on `serde_json` but
/// not `serde`'s derive.
#[derive(Debug)]
struct CreateBookmarkBody {
    url: String,
    title: String,
    collection_uuid: Option<uuid::Uuid>,
}

impl CreateBookmarkBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let url = obj.get("url")?.as_str()?.to_string();
        let title = obj.get("title")?.as_str()?.to_string();
        let collection_uuid = match obj.get("collectionUuid") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(uuid::Uuid::parse_str(v.as_str()?).ok()?),
        };
        Some(Self {
            url,
            title,
            collection_uuid,
        })
    }
}

/// Create a browser bookmark, optionally in an existing bookmark collection
/// (UC-15 / FR-BM-01).
///
/// `json_body` is the JSON body HTTP would send (`url` + `title` +
/// `collectionUuid`). The function deserializes it, calls the same
/// `CreateBookmarkHandler` the HTTP route uses, and on success serializes the
/// returned `Bookmark` back to JSON — so the FFI and HTTP surfaces agree
/// byte-for-byte modulo key ordering (parity, FR-FC-24 / NFR-09). `token` is
/// the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmark_create(
    json_body: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its body would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };
    let body = match CreateBookmarkBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .create_bookmark_handler
            .create(&body.url, &body.title, body.collection_uuid, &token)
            .await
    });

    match result {
        Ok(bookmark) => {
            let json = serde_json::to_string(&bookmark).unwrap_or_default();
            BookmarkJsonResult::ok(json)
        }
        Err(err) => map_bookmark_err(err),
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
        DomainError::Disk(_) => FileJsonResult::err(FILE_ERR_DISK),
        _ => FileJsonResult::err(FILE_ERR_OTHER),
    }
}

/// Count of cataloged files currently marked missing on disk (UC-02 AF-01).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
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
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
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
///
/// # Safety
///
/// `ptr` must be null, or a pointer returned by one of this library's
/// accessors and not yet freed. Passing anything else — a pointer this library
/// did not produce, or one already freed — is undefined behaviour. Declared
/// `unsafe` because that obligation is the caller's and cannot be checked here.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub unsafe extern "C" fn alexandria_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        // SAFETY: `ptr` came from `CString::into_raw` above.
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}
