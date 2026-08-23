#![deny(unsafe_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::runtime::{Builder, Runtime};

use alexandria_core::auth::windows_identity::{verify_owner, ProcessWindowsIdentity};
use alexandria_core::auth::AuthService;
use alexandria_core::catalog::commands::index::IndexRequest;
use alexandria_core::catalog::runs::{RunKind, RunPriority};
use alexandria_core::config::AuthMode;
use alexandria_core::config::Settings;
use alexandria_core::errors::{error_body, DomainError};
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
pub const FILE_ERR_INTEGRITY: c_int = 7;
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

/// FFI status codes returned by playback operations (UC-38…UC-40).
/// Deliberately separate from `INDEX_*`, `FILE_*`, and `COLLECTION_*` — per
/// the convention above — so F-10 can grow its own set without colliding;
/// `PLAYBACK_OK == FILE_OK == 0` by convention.
pub const PLAYBACK_OK: c_int = 0;
pub const PLAYBACK_ERR_INVALID_INPUT: c_int = 1;
pub const PLAYBACK_ERR_UNAUTHORIZED: c_int = 2;
pub const PLAYBACK_ERR_NOT_INITIALIZED: c_int = 3;
pub const PLAYBACK_ERR_NOT_FOUND: c_int = 4;
pub const PLAYBACK_ERR_INVALID_STATE: c_int = 5;
pub const PLAYBACK_ERR_DISK: c_int = 6;
pub const PLAYBACK_ERR_OTHER: c_int = 9;

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
///
/// Shared with `alexandria_index_resume` (UC-42), which reuses this same
/// struct shape for a call that is not starting anything new. That reuse
/// changes what `status` means: from `alexandria_index_start` and
/// `alexandria_index_refresh_start` it is one of the `INDEX_ERR_*`
/// constants, where `4` is `INDEX_ERR_OTHER`; from `alexandria_index_resume`
/// it is one of the `RUN_ERR_*` constants, where `4` is `RUN_ERR_NOT_FOUND`
/// instead. Check which function returned the value before reading `status`
/// against either family.
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

/// Parse a wire priority string into a [`RunPriority`].
///
/// `"low"` maps to `RunPriority::Low`; anything else — NULL, `"normal"`, an
/// unrecognised word, or malformed UTF-8 lossily decoded to garbage — maps to
/// `RunPriority::Normal`. A client that cannot spell the value gets the safe
/// default rather than a rejected call; the same lenient rule the HTTP body
/// (Task 12) uses, so the two surfaces agree on what an unreadable priority
/// means as well as on the words they both accept (FR-FC-24).
fn parse_priority(raw: Option<String>) -> RunPriority {
    match raw.as_deref() {
        Some("low") => RunPriority::Low,
        _ => RunPriority::Normal,
    }
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
    // Same gate as the HTTP binary: a misconfigured mode is a startup failure
    // on both surfaces (FR-AU-08).
    // Both messages are logged rather than discarded: the embedder only gets an
    // opaque status code back, and FFI is the surface an operator is most
    // likely to hit a misconfiguration on. A SID names an account rather than
    // authenticating one, so naming both of them costs nothing (FR-AU-21).
    if let Err(err) = settings.auth.validate() {
        tracing::error!(error = %err, "auth configuration is invalid; refusing to initialize");
        return INDEX_ERR_OTHER;
    }
    if settings.auth.mode == AuthMode::Windows {
        if let Err(err) = verify_owner(&ProcessWindowsIdentity, &settings.auth.windows_owner_sid) {
            tracing::error!(error = %err, "windows-mode startup check failed");
            return INDEX_ERR_OTHER;
        }
    }
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
///
/// `priority` is `"low"` or `"normal"` (case-sensitive, matching the HTTP
/// body's spelling exactly — FR-FC-24). NULL or any other string is treated
/// as `"normal"`: a client that cannot spell the value gets the safe default
/// rather than a rejected call.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_start(
    root: *const c_char,
    token: *const c_char,
    priority: *const c_char,
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
    let priority = parse_priority(cstr_lossy(priority));
    let rt = runtime();

    let started = rt.block_on(async {
        services
            .index_handler
            .start(
                IndexRequest {
                    root: root.clone(),
                    priority,
                },
                &token,
            )
            .await
    });

    match started {
        Ok(s) => {
            let run_id = s.run_id;
            let handler = services.index_handler.clone();
            rt.spawn(async move {
                // Per-file failures are counted inside `execute`; an `Err`
                // here means the run could not start at all. `execute` has
                // already written the `failed` run record on its own error
                // path (UC-42), so the failure is recorded, not lost. This
                // log line is for the operator.
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
///
/// `priority` is parsed exactly as `alexandria_index_start`'s is — see that
/// function's doc comment for the accepted spellings and the NULL/garbage
/// fallback.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_refresh_start(
    token: *const c_char,
    priority: *const c_char,
) -> IndexStartResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return IndexStartResult::err(INDEX_ERR_NOT_INITIALIZED),
    };
    let token = cstr_lossy(token).unwrap_or_default();
    let priority = parse_priority(cstr_lossy(priority));
    let rt = runtime();

    let started = rt.block_on(async { services.refresh_handler.start(priority, &token).await });

    match started {
        Ok(s) => {
            let run_id = s.run_id;
            let handler = services.refresh_handler.clone();
            rt.spawn(async move {
                // Per-file failures are counted inside `execute`; an `Err`
                // here means the run could not start at all. `execute` has
                // already written the `failed` run record on its own error
                // path (UC-42), so the failure is recorded, not lost. This
                // log line is for the operator.
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

/// JSON result for the playback functions. `json` is NULL on error and
/// `status` carries the mapped code. The caller must free `json` with
/// `alexandria_free_string`.
#[repr(C)]
pub struct PlaybackJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl PlaybackJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: PLAYBACK_OK,
            json: cstring.into_raw(),
        }
    }
}

fn map_playback_err(err: DomainError) -> PlaybackJsonResult {
    match err {
        DomainError::NotFound => PlaybackJsonResult::err(PLAYBACK_ERR_NOT_FOUND),
        DomainError::Unauthorized => PlaybackJsonResult::err(PLAYBACK_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => PlaybackJsonResult::err(PLAYBACK_ERR_INVALID_INPUT),
        DomainError::InvalidState => PlaybackJsonResult::err(PLAYBACK_ERR_INVALID_STATE),
        DomainError::Disk(_) => PlaybackJsonResult::err(PLAYBACK_ERR_DISK),
        _ => PlaybackJsonResult::err(PLAYBACK_ERR_OTHER),
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

/// Read a TextFile's content from disk (UC-32 / FR-TX-01).
///
/// `uuid` is the file's public UUID (NUL-terminated string). On success
/// `json` carries the `FileContent` — byte-for-byte the same shape HTTP
/// returns from `GET /v1/files/{uuid}/content` (parity, FR-FC-24 / NFR-09).
/// `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_read_content(
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
            .read_text_file_content_handler
            .read(uuid, &token)
            .await
    });

    match result {
        Ok(content) => {
            let json = serde_json::to_string(&content).unwrap_or_default();
            FileJsonResult::ok(json)
        }
        Err(err) => map_file_err(err),
    }
}

/// Resolve a File to everything a local player needs to open it
/// (UC-38 / FR-MP-01, FR-MP-06).
///
/// The FFI surface cannot carry a byte stream, so where HTTP streams
/// `GET /v1/files/{uuid}/stream`, this returns
/// `{"uuid":…,"path":…,"mimeType":…,"sizeBytes":…}` and the caller — Flutter
/// desktop, on the same machine as the file — opens that path directly.
/// Zero bytes cross this boundary. Parity with HTTP is defined on this
/// descriptor and on the authorization, state, and error decisions rather
/// than on byte transfer (FR-MP-06).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_playback_source(
    uuid: *const c_char,
    token: *const c_char,
) -> PlaybackJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether its uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return PlaybackJsonResult::err(PLAYBACK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.playback_source_handler.resolve(uuid, &token).await });

    match result {
        Ok(source) => {
            let json = serde_json::to_string(&source).unwrap_or_default();
            PlaybackJsonResult::ok(json)
        }
        Err(err) => map_playback_err(err),
    }
}

/// One page of a CBZ ComicBook (UC-39 / FR-MP-04).
///
/// Returns `{"uuid":…,"page":N,"pageCount":N,"mimeType":…,"bytesBase64":…}`.
/// Unlike UC-38, the bytes *do* cross the boundary: a comic page has no path
/// of its own — it lives inside an archive — and it is bounded, so
/// base64 inside the existing JSON payload keeps the FFI shape intact while
/// staying byte-exact with HTTP.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_comic_page(
    uuid: *const c_char,
    page: u32,
    token: *const c_char,
) -> PlaybackJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return PlaybackJsonResult::err(PLAYBACK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .comic_page_handler
            .read_page(uuid, page, &token)
            .await
    });

    match result {
        Ok(comic_page) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&comic_page.bytes);
            let json = serde_json::json!({
                "uuid": comic_page.uuid,
                "page": comic_page.page,
                "pageCount": comic_page.page_count,
                "mimeType": comic_page.mime_type,
                "bytesBase64": encoded,
            })
            .to_string();
            PlaybackJsonResult::ok(json)
        }
        Err(err) => map_playback_err(err),
    }
}

/// A downscaled thumbnail for a video, image, or comic
/// (UC-40 / FR-MP-05). Returns
/// `{"uuid":…,"mimeType":"image/jpeg","bytesBase64":…}`. Bounded derived
/// bytes, so the same base64 rule as `alexandria_comic_page` applies.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_thumbnail(
    uuid: *const c_char,
    token: *const c_char,
) -> PlaybackJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return PlaybackJsonResult::err(PLAYBACK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return PlaybackJsonResult::err(PLAYBACK_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.thumbnail_handler.thumbnail(uuid, &token).await });

    match result {
        Ok(thumb) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&thumb.bytes);
            let json = serde_json::json!({
                "uuid": thumb.uuid,
                "mimeType": thumb.mime_type,
                "bytesBase64": encoded,
            })
            .to_string();
            PlaybackJsonResult::ok(json)
        }
        Err(err) => map_playback_err(err),
    }
}

/// Request body accepted by `alexandria_file_edit_content` — the same JSON
/// `PUT /v1/files/{uuid}/content` takes: `{"content":"…"}`.
#[derive(Debug)]
struct EditContentBody {
    content: String,
}

impl EditContentBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let content = obj.get("content")?.as_str()?.to_string();
        Some(Self { content })
    }
}

/// Write edited content back to a TextFile on disk (UC-33 / FR-TX-02,
/// FR-TX-03).
///
/// `uuid` is the file's public UUID (NUL-terminated string). `json_body` is
/// the JSON body HTTP would send (`content`). The function deserializes it,
/// calls the same `EditTextFileContentHandler` the HTTP route uses, and on
/// success serializes the returned `File` back to JSON — so the FFI and
/// HTTP surfaces agree byte-for-byte modulo key ordering (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_file_edit_content(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> FileJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_NOT_INITIALIZED),
    };

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

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };
    let body = match EditContentBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return FileJsonResult::err(FILE_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .edit_text_file_content_handler
            .edit(uuid, body.content, &token)
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

/// Filters accepted by `alexandria_collections_list` (UC-46). Mirrors the
/// query string HTTP takes on the same route, so a caller writes the same
/// filter for either surface (FR-FC-24 / NFR-09).
#[derive(Debug, Default)]
struct CollectionsListFilter {
    kind: Option<String>,
}

impl CollectionsListFilter {
    /// Parse the JSON filter body. An empty string, a JSON `null`, and `{}`
    /// all mean "no filter"; anything that is not a JSON object is `None`,
    /// which the caller turns into invalid input.
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
            kind: obj
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// List the owner's collections (UC-46 / FR-CO-08).
///
/// `json_filters` is the JSON filter HTTP would build from its query string
/// (`kind`); an empty string or `null` means every collection. On success
/// `json` carries a JSON array of `CollectionSummary` — each collection with
/// the number of items it holds — byte-for-byte the same shape HTTP returns
/// from `GET /v1/collections` (parity, FR-FC-24 / NFR-09). `token` is the
/// bearer auth token.
///
/// An owner with no collections gets an empty array and `COLLECTION_OK`, not
/// an error (AF-01). An unrecognised `kind` is `COLLECTION_ERR_INVALID_INPUT`
/// and nothing is queried (AF-02).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_collections_list(
    json_filters: *const c_char,
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

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match CollectionsListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
    };

    // AF-02: refused before the handler is reached, exactly as HTTP refuses
    // the same value while parsing its query string.
    let kind = match parsed.kind.as_deref().filter(|k| !k.is_empty()) {
        None => None,
        Some(value) => match alexandria_core::collections::model::CollectionKind::parse(value) {
            Some(k) => Some(k),
            None => return CollectionJsonResult::err(COLLECTION_ERR_INVALID_INPUT),
        },
    };

    let result =
        runtime().block_on(async { services.list_collections_handler.list(kind, &token).await });

    match result {
        Ok(collections) => {
            let json = serde_json::to_string(&collections).unwrap_or_default();
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

/// Request body accepted by `alexandria_bookmark_update` — the same JSON
/// `PATCH /v1/bookmarks/{uuid}` takes:
/// `{"url":"…","title":"…","collectionUuid":"…"}` (`collectionUuid`
/// optional/nullable; absent or null clears the link).
#[derive(Debug)]
struct UpdateBookmarkBody {
    url: String,
    title: String,
    collection_uuid: Option<uuid::Uuid>,
}

impl UpdateBookmarkBody {
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

/// Update a bookmark's url, title, and containing collection (UC-16 /
/// FR-BM-02).
///
/// `uuid` is the bookmark's public UUID (NUL-terminated string). `json_body`
/// is the JSON body HTTP would send (`url` + `title` + `collectionUuid`).
/// The function deserializes it, calls the same `UpdateBookmarkHandler` the
/// HTTP route uses, and on success serializes the returned `Bookmark` back
/// to JSON — so the FFI and HTTP surfaces agree byte-for-byte modulo key
/// ordering (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmark_update(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };
    let body = match UpdateBookmarkBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .update_bookmark_handler
            .update(uuid, &body.url, &body.title, body.collection_uuid, &token)
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

/// JSON filter body accepted by `alexandria_bookmarks_list` (UC-17 /
/// FR-BM-06). Both fields optional; an empty/null body, omitted fields, or
/// empty-string values use the defaults (`collectionUuid = None`, `state =
/// "active"` — excludes deleted records per the use case's main-flow step
/// 2). An unrecognised `state` or malformed `collectionUuid` is rejected as
/// `BOOKMARK_ERR_INVALID_INPUT`, matching the HTTP surface's `400`
/// (FR-FC-24 / NFR-09).
#[derive(Debug, Default)]
struct BookmarksListFilter {
    collection_uuid: Option<String>,
    state: Option<String>,
}

impl BookmarksListFilter {
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
            collection_uuid: obj
                .get("collectionUuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            state: obj
                .get("state")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// Browse bookmarks, optionally filtered by containing collection (UC-17 /
/// FR-BM-06).
///
/// `json_filters` is a JSON string `{"collectionUuid":"…","state":"all"}`
/// (empty string or NULL for defaults). The function deserializes it, calls
/// the same `BrowseBookmarksHandler` the HTTP route uses, and on success
/// serializes the returned `Vec<Bookmark>` back to a JSON array — so the FFI
/// and HTTP surfaces agree byte-for-byte modulo key ordering (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmarks_list(
    json_filters: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match BookmarksListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let bookmark_state = match parsed.state.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match alexandria_core::catalog::model::StateFilter::parse(s) {
            Some(st) => st,
            None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
        },
        None => alexandria_core::catalog::model::StateFilter::Active,
    };
    let mut filter = alexandria_core::bookmarks::queries::browse::BookmarkFilter::new()
        .with_state(bookmark_state);
    if let Some(c) = parsed.collection_uuid.as_deref().filter(|s| !s.is_empty()) {
        let collection_uuid = match uuid::Uuid::parse_str(c) {
            Ok(u) => u,
            Err(_) => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
        };
        filter = filter.with_collection(collection_uuid);
    }

    let result =
        runtime().block_on(async { services.browse_bookmarks_handler.list(filter, &token).await });

    match result {
        Ok(bookmarks) => {
            let json = serde_json::to_string(&bookmarks).unwrap_or_default();
            BookmarkJsonResult::ok(json)
        }
        Err(err) => map_bookmark_err(err),
    }
}

/// Soft-delete a bookmark (UC-18 / FR-BM-03).
///
/// `uuid` is the bookmark's public UUID (NUL-terminated string). On success
/// `json` carries the updated `Bookmark` — byte-for-byte the same shape HTTP
/// returns from `DELETE /v1/bookmarks/{uuid}` (parity, FR-FC-24 / NFR-09).
/// `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmark_soft_delete(
    uuid: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .bookmark_lifecycle_handler
            .soft_delete(uuid, &token)
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

/// Restore a soft-deleted bookmark (UC-18 / FR-BM-05).
///
/// `uuid` is the bookmark's public UUID (NUL-terminated string). On success
/// `json` carries the restored `Bookmark` — byte-for-byte the same shape
/// HTTP returns from `POST /v1/bookmarks/{uuid}/restore` (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmark_restore(
    uuid: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .bookmark_lifecycle_handler
            .restore(uuid, &token)
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

/// Hard-purge a bookmark (UC-19 / FR-BM-04).
///
/// `uuid` is the bookmark's public UUID (NUL-terminated string). On success
/// `json` carries the pre-purge `Bookmark` as confirmation — byte-for-byte
/// the same shape HTTP returns from `DELETE /v1/bookmarks/{uuid}?purge=true`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_bookmark_purge(
    uuid: *const c_char,
    token: *const c_char,
) -> BookmarkJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return BookmarkJsonResult::err(BOOKMARK_ERR_UNAUTHORIZED);
    }

    let uuid = match cstr_lossy(uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return BookmarkJsonResult::err(BOOKMARK_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.purge_bookmark_handler.purge(uuid, &token).await });

    match result {
        Ok(bookmark) => {
            let json = serde_json::to_string(&bookmark).unwrap_or_default();
            BookmarkJsonResult::ok(json)
        }
        Err(err) => map_bookmark_err(err),
    }
}

/// FFI status codes returned by watchlist operations (UC-20+). Deliberately
/// separate from `BOOKMARK_*` — per the convention above — so watchlist use
/// cases can grow their own set without colliding; `WATCHLIST_OK ==
/// BOOKMARK_OK == 0` by convention.
pub const WATCHLIST_OK: c_int = 0;
pub const WATCHLIST_ERR_INVALID_INPUT: c_int = 1;
pub const WATCHLIST_ERR_UNAUTHORIZED: c_int = 2;
pub const WATCHLIST_ERR_NOT_INITIALIZED: c_int = 3;
pub const WATCHLIST_ERR_NOT_FOUND: c_int = 4;
pub const WATCHLIST_ERR_INVALID_STATE: c_int = 5;
pub const WATCHLIST_ERR_OTHER: c_int = 9;

/// Result of `alexandria_watchlist_create` (UC-20). On success `status` is
/// `WATCHLIST_OK` and `json` is a NUL-terminated JSON string of the
/// `Watchlist` body — byte-for-byte the same shape HTTP returns from `POST
/// /v1/watchlists` (FR-FC-24 / NFR-09). On failure `json` is NULL and
/// `status` carries the mapped error code. The caller must free `json` with
/// `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct WatchlistJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl WatchlistJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: WATCHLIST_OK,
            json: cstring.into_raw(),
        }
    }
}

fn map_watchlist_err(err: DomainError) -> WatchlistJsonResult {
    match err {
        DomainError::NotFound => WatchlistJsonResult::err(WATCHLIST_ERR_NOT_FOUND),
        DomainError::Unauthorized => WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
        DomainError::InvalidState => WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_STATE),
        _ => WatchlistJsonResult::err(WATCHLIST_ERR_OTHER),
    }
}

/// Request body accepted by `alexandria_watchlist_create` — the same JSON
/// `POST /v1/watchlists` takes: `{"name":"Weekend movies"}`. Parsed
/// field-by-field for the same reason `CreateCollectionBody` is: this crate
/// depends on `serde_json` but not `serde`'s derive.
#[derive(Debug)]
struct CreateWatchlistBody {
    name: String,
}

impl CreateWatchlistBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let name = obj.get("name")?.as_str()?.to_string();
        Some(Self { name })
    }
}

/// Create a named watchlist for tracking video consumption (UC-20 /
/// FR-WL-01).
///
/// `json_body` is the JSON body HTTP would send (`name`). The function
/// deserializes it, calls the same `CreateWatchlistHandler` the HTTP route
/// uses, and on success serializes the returned `Watchlist` back to JSON —
/// so the FFI and HTTP surfaces agree byte-for-byte modulo key ordering
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlist_create(
    json_body: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let body = match CreateWatchlistBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .create_watchlist_handler
            .create(&body.name, &token)
            .await
    });

    match result {
        Ok(watchlist) => {
            let json = serde_json::to_string(&watchlist).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
    }
}

/// Request body accepted by `alexandria_watchlist_add_video` — the same JSON
/// `POST /v1/watchlists/{uuid}/items` takes: `{"videoUuid":"…"}`.
#[derive(Debug)]
struct AddVideoBody {
    video_uuid: uuid::Uuid,
}

impl AddVideoBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let raw = obj.get("videoUuid")?.as_str()?;
        Some(Self {
            video_uuid: uuid::Uuid::parse_str(raw).ok()?,
        })
    }
}

/// Add a video to a watchlist (UC-22 / FR-WL-02, FR-WL-03).
///
/// `uuid` is the watchlist's public UUID (NUL-terminated string). `json_body`
/// is the JSON body HTTP would send (`videoUuid`). The function deserializes
/// it, calls the same `AddVideoToWatchlistHandler` the HTTP route uses, and
/// on success serializes the returned `WatchProgress` back to JSON — so the
/// FFI and HTTP surfaces agree byte-for-byte modulo key ordering (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlist_add_video(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let body = match AddVideoBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .add_video_to_watchlist_handler
            .add(uuid, body.video_uuid, &token)
            .await
    });

    match result {
        Ok(progress) => {
            let json = serde_json::to_string(&progress).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
    }
}

/// Filter accepted by `alexandria_watchlists_list` — the same JSON `GET
/// /v1/watchlists?watchlistUuid=…` takes: `{"watchlistUuid":"…"}`. An empty
/// or absent `watchlistUuid` means every watchlist.
#[derive(Debug, Default)]
struct WatchlistsListFilter {
    watchlist_uuid: Option<String>,
}

impl WatchlistsListFilter {
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
            watchlist_uuid: obj
                .get("watchlistUuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// Browse watchlists and their items' watch progress (UC-21 / FR-WL-08).
///
/// `json_filters` is the JSON filter HTTP would build from its query string
/// (`watchlistUuid`); an empty string or `null` means every watchlist. On
/// success `json` carries a JSON array of `WatchlistWithProgress` — byte-for-
/// byte the same shape HTTP returns from `GET /v1/watchlists` (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlists_list(
    json_filters: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match WatchlistsListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let watchlist_uuid = match parsed.watchlist_uuid.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(v) => match uuid::Uuid::parse_str(v) {
            Ok(u) => Some(u),
            Err(_) => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
        },
    };

    let result = runtime().block_on(async {
        services
            .browse_watchlists_handler
            .list(watchlist_uuid, &token)
            .await
    });

    match result {
        Ok(watchlists) => {
            let json = serde_json::to_string(&watchlists).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
    }
}

/// Request body accepted by `alexandria_watchlist_update_progress` — the
/// same JSON `PATCH /v1/watchlists/{uuid}/items/{videoUuid}` takes:
/// `{"state":"…","currentEpisode":…,"totalEpisodes":…}` (episode fields
/// optional/nullable; absent or null clears them).
#[derive(Debug)]
struct UpdateWatchProgressBody {
    state: String,
    current_episode: Option<i64>,
    total_episodes: Option<i64>,
}

impl UpdateWatchProgressBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            state: obj.get("state")?.as_str()?.to_string(),
            current_episode: obj.get("currentEpisode").and_then(|v| v.as_i64()),
            total_episodes: obj.get("totalEpisodes").and_then(|v| v.as_i64()),
        })
    }
}

/// Update watch progress (UC-23 / FR-WL-04, FR-WL-05).
///
/// `watchlist_uuid` and `video_uuid` are the watchlist's and video's public
/// UUIDs (NUL-terminated strings). `json_body` is the JSON body HTTP would
/// send (`state`, optional `currentEpisode`/`totalEpisodes`). On success
/// `json` carries the updated `WatchProgress` — byte-for-byte the same shape
/// HTTP returns from `PATCH /v1/watchlists/{uuid}/items/{videoUuid}`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlist_update_progress(
    watchlist_uuid: *const c_char,
    video_uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let watchlist_uuid_str = match cstr_lossy(watchlist_uuid) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let watchlist_uuid = match uuid::Uuid::parse_str(&watchlist_uuid_str) {
        Ok(u) => u,
        Err(_) => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let video_uuid_str = match cstr_lossy(video_uuid) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let video_uuid = match uuid::Uuid::parse_str(&video_uuid_str) {
        Ok(u) => u,
        Err(_) => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let body = match UpdateWatchProgressBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let new_state = match alexandria_core::watchlists::model::WatchState::parse(&body.state) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .update_watch_progress_handler
            .update(
                watchlist_uuid,
                video_uuid,
                new_state,
                body.current_episode,
                body.total_episodes,
                &token,
            )
            .await
    });

    match result {
        Ok(progress) => {
            let json = serde_json::to_string(&progress).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
    }
}

/// Remove a video from a watchlist (UC-24 / FR-WL-06).
///
/// `watchlist_uuid` and `video_uuid` are the watchlist's and video's public
/// UUIDs (NUL-terminated strings). On success `json` carries the
/// `watchlistUuid`/`videoUuid` confirmation — byte-for-byte the same shape
/// HTTP returns from `DELETE /v1/watchlists/{uuid}/items/{videoUuid}`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlist_remove_video(
    watchlist_uuid: *const c_char,
    video_uuid: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let watchlist_uuid =
        match cstr_lossy(watchlist_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
            Some(u) => u,
            None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
        };
    let video_uuid = match cstr_lossy(video_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .remove_video_from_watchlist_handler
            .remove(watchlist_uuid, video_uuid, &token)
            .await
    });

    match result {
        Ok(removed) => {
            let json = serde_json::to_string(&removed).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
    }
}

/// Delete a watchlist (UC-25 / FR-WL-07).
///
/// `uuid` is the watchlist's public UUID (NUL-terminated string). On success
/// `json` carries the pre-delete `Watchlist` — byte-for-byte the same shape
/// HTTP returns from `DELETE /v1/watchlists/{uuid}` (parity, FR-FC-24 /
/// NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_watchlist_delete(
    uuid: *const c_char,
    token: *const c_char,
) -> WatchlistJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether the uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return WatchlistJsonResult::err(WATCHLIST_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return WatchlistJsonResult::err(WATCHLIST_ERR_INVALID_INPUT),
    };

    let result =
        runtime().block_on(async { services.delete_watchlist_handler.delete(uuid, &token).await });

    match result {
        Ok(watchlist) => {
            let json = serde_json::to_string(&watchlist).unwrap_or_default();
            WatchlistJsonResult::ok(json)
        }
        Err(err) => map_watchlist_err(err),
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
        DomainError::Integrity(_) => FileJsonResult::err(FILE_ERR_INTEGRITY),
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

/// JSON array of `{"path","name","type","hash","missingAt"}` for every
/// indexed file, or a NUL pointer on error. Caller must free it with
/// `alexandria_free_string`.
///
/// `content_hash` is nullable (Task 3: indexing never computes one; Task 4:
/// neither does refresh) and is decoded as `Option<String>` — not `String` —
/// so a `NULL` row serializes as JSON `null` here, matching what the shared
/// `File`/`FileView` model emits over HTTP for the same column
/// (`GET /v1/files`, `catalog/model.rs`). Decoding it as a bare `String`
/// used to silently turn a SQL `NULL` into `""` instead (sqlx does not error
/// on that mismatch for this driver), which was a byte-for-byte parity
/// violation (FR-FC-24) for every indexed or refreshed file — not an edge
/// case, since neither indexing nor refresh have computed a hash since
/// Task 3/4.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_files_json() -> *mut c_char {
    // `(path, name, type, content_hash, missing_at)` — `content_hash` and
    // `missing_at` both nullable.
    type FileRow = (String, String, String, Option<String>, Option<String>);

    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let rows: Vec<FileRow> = runtime()
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

/// FFI status codes returned by reading list operations (UC-26+).
/// Deliberately separate from `WATCHLIST_*` — per the convention above — so
/// reading-list use cases can grow their own set without colliding;
/// `READING_LIST_OK == WATCHLIST_OK == 0` by convention.
pub const READING_LIST_OK: c_int = 0;
pub const READING_LIST_ERR_INVALID_INPUT: c_int = 1;
pub const READING_LIST_ERR_UNAUTHORIZED: c_int = 2;
pub const READING_LIST_ERR_NOT_INITIALIZED: c_int = 3;
pub const READING_LIST_ERR_NOT_FOUND: c_int = 4;
pub const READING_LIST_ERR_INVALID_STATE: c_int = 5;
pub const READING_LIST_ERR_OTHER: c_int = 9;

/// Result of `alexandria_reading_list_create` (UC-26). On success `status`
/// is `READING_LIST_OK` and `json` is a NUL-terminated JSON string of the
/// `ReadingList` body — byte-for-byte the same shape HTTP returns from
/// `POST /v1/reading-lists` (FR-FC-24 / NFR-09). On failure `json` is NULL
/// and `status` carries the mapped error code. The caller must free `json`
/// with `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct ReadingListJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl ReadingListJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: READING_LIST_OK,
            json: cstring.into_raw(),
        }
    }
}

fn map_reading_list_err(err: DomainError) -> ReadingListJsonResult {
    match err {
        DomainError::NotFound => ReadingListJsonResult::err(READING_LIST_ERR_NOT_FOUND),
        DomainError::Unauthorized => ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED),
        DomainError::InvalidInput(_) => ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
        DomainError::InvalidState => ReadingListJsonResult::err(READING_LIST_ERR_INVALID_STATE),
        _ => ReadingListJsonResult::err(READING_LIST_ERR_OTHER),
    }
}

/// Request body accepted by `alexandria_reading_list_create` — the same
/// JSON `POST /v1/reading-lists` takes: `{"name":"Summer reads"}`. Parsed
/// field-by-field for the same reason `CreateWatchlistBody` is: this crate
/// depends on `serde_json` but not `serde`'s derive.
#[derive(Debug)]
struct CreateReadingListBody {
    name: String,
}

impl CreateReadingListBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let name = obj.get("name")?.as_str()?.to_string();
        Some(Self { name })
    }
}

/// Create a named reading list for tracking book/comic consumption (UC-26 /
/// FR-RL-01).
///
/// `json_body` is the JSON body HTTP would send (`name`). The function
/// deserializes it, calls the same `CreateReadingListHandler` the HTTP
/// route uses, and on success serializes the returned `ReadingList` back to
/// JSON — so the FFI and HTTP surfaces agree byte-for-byte modulo key
/// ordering (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_list_create(
    json_body: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let body = match CreateReadingListBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .create_reading_list_handler
            .create(&body.name, &token)
            .await
    });

    match result {
        Ok(reading_list) => {
            let json = serde_json::to_string(&reading_list).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// Request body accepted by `alexandria_reading_list_add_item` — the same
/// JSON `POST /v1/reading-lists/{uuid}/items` takes: `{"itemUuid":"…"}`.
#[derive(Debug)]
struct AddItemToReadingListBody {
    item_uuid: uuid::Uuid,
}

impl AddItemToReadingListBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        let raw = obj.get("itemUuid")?.as_str()?;
        Some(Self {
            item_uuid: uuid::Uuid::parse_str(raw).ok()?,
        })
    }
}

/// Add a book or comic to a reading list (UC-28 / FR-RL-02, FR-RL-03).
///
/// `uuid` is the reading list's public UUID (NUL-terminated string).
/// `json_body` is the JSON body HTTP would send (`itemUuid`). The function
/// deserializes it, calls the same `AddItemToReadingListHandler` the HTTP
/// route uses, and on success serializes the returned `ReadingProgress`
/// back to JSON — so the FFI and HTTP surfaces agree byte-for-byte modulo
/// key ordering (parity, FR-FC-24 / NFR-09). `token` is the bearer auth
/// token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_list_add_item(
    uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let body = match AddItemToReadingListBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .add_item_to_reading_list_handler
            .add(uuid, body.item_uuid, &token)
            .await
    });

    match result {
        Ok(progress) => {
            let json = serde_json::to_string(&progress).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// Filter accepted by `alexandria_reading_lists_list` — the same JSON `GET
/// /v1/reading-lists?readingListUuid=…` takes:
/// `{"readingListUuid":"…"}`. An empty or absent `readingListUuid` means
/// every reading list.
#[derive(Debug, Default)]
struct ReadingListsListFilter {
    reading_list_uuid: Option<String>,
}

impl ReadingListsListFilter {
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
            reading_list_uuid: obj
                .get("readingListUuid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }
}

/// Browse reading lists and their items' read progress (UC-27 / FR-RL-08).
///
/// `json_filters` is the JSON filter HTTP would build from its query string
/// (`readingListUuid`); an empty string or `null` means every reading list.
/// On success `json` carries a JSON array of `ReadingListWithProgress` —
/// byte-for-byte the same shape HTTP returns from `GET /v1/reading-lists`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_lists_list(
    json_filters: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let filter_str = cstr_lossy(json_filters).unwrap_or_default();
    let parsed = match ReadingListsListFilter::from_json_str(&filter_str) {
        Some(f) => f,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let reading_list_uuid = match parsed
        .reading_list_uuid
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(v) => match uuid::Uuid::parse_str(v) {
            Ok(u) => Some(u),
            Err(_) => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
        },
    };

    let result = runtime().block_on(async {
        services
            .browse_reading_lists_handler
            .list(reading_list_uuid, &token)
            .await
    });

    match result {
        Ok(reading_lists) => {
            let json = serde_json::to_string(&reading_lists).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// Request body accepted by `alexandria_reading_list_update_progress` — the
/// same JSON `PATCH /v1/reading-lists/{uuid}/items/{itemUuid}` takes:
/// `{"state":"…","currentIssue":…,"totalIssues":…}` (issue fields
/// optional/nullable; absent or null clears them).
#[derive(Debug)]
struct UpdateReadingProgressBody {
    state: String,
    current_issue: Option<i64>,
    total_issues: Option<i64>,
}

impl UpdateReadingProgressBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            state: obj.get("state")?.as_str()?.to_string(),
            current_issue: obj.get("currentIssue").and_then(|v| v.as_i64()),
            total_issues: obj.get("totalIssues").and_then(|v| v.as_i64()),
        })
    }
}

/// Update reading progress (UC-29 / FR-RL-04, FR-RL-05).
///
/// `reading_list_uuid` and `item_uuid` are the reading list's and item's
/// public UUIDs (NUL-terminated strings). `json_body` is the JSON body HTTP
/// would send (`state`, optional `currentIssue`/`totalIssues`). On success
/// `json` carries the updated `ReadingProgress` — byte-for-byte the same
/// shape HTTP returns from `PATCH /v1/reading-lists/{uuid}/items/{itemUuid}`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_list_update_progress(
    reading_list_uuid: *const c_char,
    item_uuid: *const c_char,
    json_body: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let reading_list_uuid_str = match cstr_lossy(reading_list_uuid) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let reading_list_uuid = match uuid::Uuid::parse_str(&reading_list_uuid_str) {
        Ok(u) => u,
        Err(_) => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let item_uuid_str = match cstr_lossy(item_uuid) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let item_uuid = match uuid::Uuid::parse_str(&item_uuid_str) {
        Ok(u) => u,
        Err(_) => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let body = match UpdateReadingProgressBody::from_json_str(&body_str) {
        Some(b) => b,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let new_state = match alexandria_core::reading_lists::model::ReadingState::parse(&body.state) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .update_reading_progress_handler
            .update(
                reading_list_uuid,
                item_uuid,
                new_state,
                body.current_issue,
                body.total_issues,
                &token,
            )
            .await
    });

    match result {
        Ok(progress) => {
            let json = serde_json::to_string(&progress).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// Remove an item from a reading list (UC-30 / FR-RL-06).
///
/// `reading_list_uuid` and `item_uuid` are the reading list's and item's
/// public UUIDs (NUL-terminated strings). On success `json` carries the
/// `readingListUuid`/`itemUuid` confirmation — byte-for-byte the same shape
/// HTTP returns from `DELETE /v1/reading-lists/{uuid}/items/{itemUuid}`
/// (parity, FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_list_remove_item(
    reading_list_uuid: *const c_char,
    item_uuid: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let reading_list_uuid =
        match cstr_lossy(reading_list_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
            Some(u) => u,
            None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
        };
    let item_uuid = match cstr_lossy(item_uuid).and_then(|s| uuid::Uuid::parse_str(&s).ok()) {
        Some(u) => u,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .remove_item_from_reading_list_handler
            .remove(reading_list_uuid, item_uuid, &token)
            .await
    });

    match result {
        Ok(removed) => {
            let json = serde_json::to_string(&removed).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// Delete a reading list (UC-31 / FR-RL-07).
///
/// `uuid` is the reading list's public UUID (NUL-terminated string). On
/// success `json` carries the pre-delete `ReadingList` — byte-for-byte the
/// same shape HTTP returns from `DELETE /v1/reading-lists/{uuid}` (parity,
/// FR-FC-24 / NFR-09). `token` is the bearer auth token.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_reading_list_delete(
    uuid: *const c_char,
    token: *const c_char,
) -> ReadingListJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must not
    // learn whether the uuid would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return ReadingListJsonResult::err(READING_LIST_ERR_UNAUTHORIZED);
    }

    let uuid_str = match cstr_lossy(uuid) {
        Some(s) => s,
        None => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };
    let uuid = match uuid::Uuid::parse_str(&uuid_str) {
        Ok(u) => u,
        Err(_) => return ReadingListJsonResult::err(READING_LIST_ERR_INVALID_INPUT),
    };

    let result = runtime().block_on(async {
        services
            .delete_reading_list_handler
            .delete(uuid, &token)
            .await
    });

    match result {
        Ok(reading_list) => {
            let json = serde_json::to_string(&reading_list).unwrap_or_default();
            ReadingListJsonResult::ok(json)
        }
        Err(err) => map_reading_list_err(err),
    }
}

/// FFI status codes returned by local-auth operations (UC-34/UC-35).
/// Deliberately separate from the other `*_OK == 0` families — per the
/// convention above — so local-auth use cases can grow their own set
/// without colliding.
pub const AUTH_OK: c_int = 0;
pub const AUTH_ERR_INVALID_INPUT: c_int = 1;
pub const AUTH_ERR_UNAUTHORIZED: c_int = 2;
pub const AUTH_ERR_NOT_INITIALIZED: c_int = 3;
pub const AUTH_ERR_INVALID_STATE: c_int = 5;
pub const AUTH_ERR_CONFIG: c_int = 8;
pub const AUTH_ERR_OTHER: c_int = 9;
/// UC-41 AF-01/AF-02: the request conflicts with existing state — the
/// active auth mode is not local, or an account already exists. The FFI
/// counterpart of HTTP's `409`.
pub const AUTH_ERR_CONFLICT: c_int = 10;
/// Refused because it came too soon after some earlier request — a rate
/// limit. The FFI counterpart of HTTP's `429`. Its own code because it is a
/// "not yet", not a mistake the caller made.
pub const AUTH_ERR_RATE_LIMITED: c_int = 11;
/// A dependency the operation needs is unavailable. The FFI counterpart of
/// HTTP's `503`; the body's `code` says which.
pub const AUTH_ERR_SERVICE_UNAVAILABLE: c_int = 12;

/// Result of `alexandria_auth_local_login` / `alexandria_auth_local_set_credentials`
/// (UC-34/UC-35). On success `status` is `AUTH_OK` and `json` is a
/// NUL-terminated JSON string of the `LocalLoginResult` /
/// `LocalCredentialsResult` body — byte-for-byte the same shape HTTP
/// returns from the matching `/v1/auth/local/*` endpoint (FR-FC-24 /
/// NFR-09).
///
/// On failure `status` carries the mapped error code and `json` carries the
/// same error envelope HTTP returns for that failure (issue #101):
/// `{"error": …}`, plus `"code"` and `"params"` when the rejection has a
/// stable reason. `status` is the coarse class; `code` is the reason — six
/// distinct password-policy rejections all arrive as
/// `AUTH_ERR_INVALID_INPUT`, and only `code` tells them apart.
///
/// `json` is NULL only when the library was never initialized, so there was
/// no service to answer at all. The caller must free `json` with
/// `alexandria_free_string` on every path; freeing NULL is a no-op.
#[repr(C)]
#[derive(Debug)]
pub struct AuthJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl AuthJsonResult {
    /// A failure with no readable body — see the NULL case documented above.
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    /// A failure carrying the same error envelope HTTP would send.
    fn err_body(status: c_int, body: String) -> Self {
        match CString::new(body) {
            Ok(cstring) => Self {
                status,
                json: cstring.into_raw(),
            },
            // Unreachable in practice: the envelope is JSON, which never
            // contains an interior NUL. Degrade to the code-only failure
            // rather than panicking across an FFI boundary.
            Err(_) => Self::err(status),
        }
    }

    /// A rejection produced by this layer rather than by a handler, rendered
    /// through the same core renderer so it carries a code like every other.
    fn rejected(code: &'static str, message: impl Into<String>) -> Self {
        map_auth_err(DomainError::rejected(code, message))
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: AUTH_OK,
            json: cstring.into_raw(),
        }
    }
}

/// Map a `DomainError` to this surface's status code and error body.
///
/// The body comes from the core's `error_body`, the same renderer the HTTP
/// surface uses, so the bytes match (FR-FC-24 / NFR-09). The status stays a
/// match on the variant rather than on `ErrorClass`: this surface draws
/// distinctions HTTP does not — `InvalidState` and `Conflict` are both `409`
/// but different codes here, as are `Config` and the other internal failures —
/// and collapsing them would be a breaking change to callers for no gain.
fn map_auth_err(err: DomainError) -> AuthJsonResult {
    let status = match err {
        DomainError::Unauthorized => AUTH_ERR_UNAUTHORIZED,
        DomainError::InvalidInput(_) | DomainError::Rejected(_) => AUTH_ERR_INVALID_INPUT,
        DomainError::InvalidState => AUTH_ERR_INVALID_STATE,
        DomainError::Config(_) => AUTH_ERR_CONFIG,
        DomainError::Conflict(_) => AUTH_ERR_CONFLICT,
        DomainError::TooManyRequests(_) => AUTH_ERR_RATE_LIMITED,
        DomainError::ServiceUnavailable(_) | DomainError::Unavailable(_) => {
            AUTH_ERR_SERVICE_UNAVAILABLE
        }
        _ => AUTH_ERR_OTHER,
    };
    let (_, body) = error_body(&err);
    AuthJsonResult::err_body(status, body.to_json())
}

/// Request body accepted by both `alexandria_auth_local_login` and
/// `alexandria_auth_local_set_credentials` — the same JSON both
/// `/v1/auth/local/*` endpoints take: `{"email":"…","password":"…"}`.
#[derive(Debug)]
struct LocalCredentialsBody {
    email: String,
    password: String,
}

impl LocalCredentialsBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            email: obj.get("email")?.as_str()?.to_string(),
            password: obj.get("password")?.as_str()?.to_string(),
        })
    }
}

/// Local login (UC-34 / FR-AU-04): verify email + password and create a
/// session. `json_body` is the JSON body HTTP would send (`email`,
/// `password`). On success `json` carries the `LocalLoginResult` — the
/// caller presents its `sessionId` on subsequent requests instead of a
/// bearer token in local mode.
///
/// Deliberately takes no `token`: this is how a caller obtains credentials
/// in the first place (mirrors the HTTP route being outside the auth gate).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_login(json_body: *const c_char) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return AuthJsonResult::rejected("malformed_body", "login body is missing"),
    };
    let body = match LocalCredentialsBody::from_json_str(&body_str) {
        Some(b) => b,
        None => {
            return AuthJsonResult::rejected(
                "malformed_body",
                "invalid login body: expected an object with string 'email' and 'password'",
            )
        }
    };

    let result = runtime().block_on(async {
        services
            .local_login_handler
            .login(&body.email, &body.password)
            .await
    });

    match result {
        Ok(login) => {
            let json = serde_json::to_string(&login).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Windows login (UC-45 / FR-AU-20, FR-AU-22): open a session for the
/// Windows account this process runs as. Takes no credentials — the account
/// was already verified against the configured SID at startup. `json_body`
/// is accepted but ignored: it exists only for signature consistency with
/// this surface's other `alexandria_auth_*` neighbours, which all take a
/// body. On success `json` carries the `LocalLoginResult`, the same shape
/// `alexandria_auth_local_login` returns.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_windows_login(_json_body: *const c_char) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let result = runtime().block_on(async { services.windows_login_handler.login().await });

    match result {
        Ok(login) => {
            let json = serde_json::to_string(&login).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Set or change local-login credentials (UC-35 / FR-AU-05, FR-AU-06).
/// `json_body` is the JSON body HTTP would send (`email`, `password`).
/// `token` is required: this changes existing credentials. Creating the
/// account is `alexandria_auth_local_register` (UC-41).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_set_credentials(
    json_body: *const c_char,
    token: *const c_char,
) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return AuthJsonResult::rejected("malformed_body", "credentials body is missing"),
    };
    let body =
        match LocalCredentialsBody::from_json_str(&body_str) {
            Some(b) => b,
            None => return AuthJsonResult::rejected(
                "malformed_body",
                "invalid credentials body: expected an object with string 'email' and 'password'",
            ),
        };

    let result = runtime().block_on(async {
        services
            .set_local_credentials_handler
            .set(body.email, body.password, &token)
            .await
    });

    match result {
        Ok(credentials) => {
            let json = serde_json::to_string(&credentials).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Request body for `alexandria_auth_local_register` — the same JSON the
/// HTTP route takes: `{"email":"…","password":"…","passwordConfirmation":"…"}`.
#[derive(Debug)]
struct LocalRegisterBody {
    email: String,
    password: String,
    password_confirmation: String,
}

impl LocalRegisterBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            email: obj.get("email")?.as_str()?.to_string(),
            password: obj.get("password")?.as_str()?.to_string(),
            password_confirmation: obj.get("passwordConfirmation")?.as_str()?.to_string(),
        })
    }
}

/// Register the local account (UC-41 / FR-AU-10, FR-AU-11): create the
/// single owner's credentials and open a session. `json_body` is the JSON
/// body HTTP would send (`email`, `password`, `passwordConfirmation`). On
/// success `json` carries the `LocalRegisterResult`, whose `sessionId` the
/// caller presents on subsequent requests.
///
/// Deliberately takes no `token`: there is nothing to authenticate with
/// before an account exists. Succeeds only once — a second call returns
/// `AUTH_ERR_CONFLICT` (AF-02).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_register(json_body: *const c_char) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => return AuthJsonResult::rejected("malformed_body", "register body is missing"),
    };
    let body = match LocalRegisterBody::from_json_str(&body_str) {
        Some(b) => b,
        None => {
            return AuthJsonResult::rejected(
                "malformed_body",
                "invalid register body: expected an object with string 'email', 'password', and 'passwordConfirmation'",
            )
        }
    };

    let result = runtime().block_on(async {
        services
            .register_local_account_handler
            .register(body.email, body.password, body.password_confirmation)
            .await
    });

    match result {
        Ok(registration) => {
            let json = serde_json::to_string(&registration).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Report the authenticated owner's account state (FR-AU-18): the same body
/// `GET /v1/auth/local/account` returns. `token` is the session id.
///
/// This is the call a client makes to learn the stored address and how many
/// recovery codes remain unspent.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_account(token: *const c_char) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async { services.get_local_account_handler.get(&token).await });

    match result {
        Ok(account) => {
            let json = serde_json::to_string(&account).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Request body for `alexandria_auth_local_redeem_recovery_code` — the same
/// JSON the HTTP route takes.
#[derive(Debug)]
struct RedeemRecoveryCodeBody {
    code: String,
    new_password: String,
    password_confirmation: String,
}

impl RedeemRecoveryCodeBody {
    fn from_json_str(s: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(s).ok()?;
        let obj = value.as_object()?;
        Some(Self {
            code: obj.get("code")?.as_str()?.to_string(),
            new_password: obj.get("newPassword")?.as_str()?.to_string(),
            password_confirmation: obj.get("passwordConfirmation")?.as_str()?.to_string(),
        })
    }
}

/// Redeem a recovery code for a new password (UC-43 / FR-AU-14 … FR-AU-16).
/// `json_body` is the JSON body HTTP would send: an object with `code`,
/// `newPassword`, and `passwordConfirmation`.
///
/// Deliberately takes no session token: the code is the credential, and this
/// is the operation a caller who cannot authenticate uses to get back in.
/// Every session is invalidated on success.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_redeem_recovery_code(
    json_body: *const c_char,
) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let body_str = match cstr_lossy(json_body) {
        Some(s) => s,
        None => {
            return AuthJsonResult::rejected("malformed_body", "recovery redeem body is missing")
        }
    };
    let body = match RedeemRecoveryCodeBody::from_json_str(&body_str) {
        Some(b) => b,
        None => {
            return AuthJsonResult::rejected(
                "malformed_body",
                "invalid recovery redeem body: expected an object with string 'code', 'newPassword', and 'passwordConfirmation'",
            )
        }
    };

    let result = runtime().block_on(async {
        services
            .redeem_recovery_code_handler
            .redeem(body.code, body.new_password, body.password_confirmation)
            .await
    });

    match result {
        Ok(redemption) => {
            let json = serde_json::to_string(&redemption).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// Replace the owner's recovery codes with a fresh set of ten (UC-44 /
/// FR-AU-17), invalidating every old one. `token` is the session id the
/// caller authenticates with, exactly as `alexandria_auth_local_account`
/// takes it.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_auth_local_regenerate_recovery_codes(
    token: *const c_char,
) -> AuthJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return AuthJsonResult::err(AUTH_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async {
        services
            .regenerate_recovery_codes_handler
            .regenerate(&token)
            .await
    });

    match result {
        Ok(regeneration) => {
            let json = serde_json::to_string(&regeneration).unwrap_or_default();
            AuthJsonResult::ok(json)
        }
        Err(err) => map_auth_err(err),
    }
}

/// FFI status codes returned by the settings read (UC-47 / FR-FC-30).
/// Deliberately separate from every other family — per the convention above —
/// so this surface can grow independently; `SETTINGS_OK == INDEX_OK == 0` by
/// convention.
pub const SETTINGS_OK: c_int = 0;
pub const SETTINGS_ERR_UNAUTHORIZED: c_int = 2;
pub const SETTINGS_ERR_NOT_INITIALIZED: c_int = 3;
pub const SETTINGS_ERR_OTHER: c_int = 9;

/// Result of `alexandria_settings_json` (UC-47). On success `status` is
/// `SETTINGS_OK` and `json` is a NUL-terminated JSON string of the settings
/// body — byte-for-byte the same shape HTTP returns from `GET /v1/settings`
/// (FR-FC-24 / NFR-09). On failure `json` is NULL and `status` carries the
/// mapped error code. The caller must free `json` with
/// `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct SettingsJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl SettingsJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: SETTINGS_OK,
            json: cstring.into_raw(),
        }
    }
}

/// Report the client-relevant configuration (UC-47 / FR-FC-30).
///
/// On success `json` carries the same body HTTP returns from
/// `GET /v1/settings` — today `{"deletion":{"retentionDays":30}}`, the
/// soft-delete retention window this server enforces on every restore and
/// purge. `token` is the bearer auth token.
///
/// The boundary the number describes is the core's own: elapsed time up to and
/// including `retentionDays` leaves a record restorable and not yet purgeable;
/// strictly past it, the record is purgeable and no longer restorable.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_settings_json(token: *const c_char) -> SettingsJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return SettingsJsonResult::err(SETTINGS_ERR_NOT_INITIALIZED),
    };

    let token = cstr_lossy(token).unwrap_or_default();

    let result = runtime().block_on(async { services.get_settings_handler.get(&token).await });

    match result {
        Ok(settings) => {
            let json = serde_json::to_string(&settings).unwrap_or_default();
            SettingsJsonResult::ok(json)
        }
        // AF-01 is the only failure this read has: it reads a value the
        // process already holds, so there is nothing else to go wrong.
        Err(DomainError::Unauthorized) => SettingsJsonResult::err(SETTINGS_ERR_UNAUTHORIZED),
        Err(_) => SettingsJsonResult::err(SETTINGS_ERR_OTHER),
    }
}

/// FFI status codes returned by run-status operations (UC-42 / FR-FC-28).
/// Deliberately separate from `INDEX_*`, `FILE_*`, `COLLECTION_*`, `PLAYBACK_*`,
/// and `AUTH_*` — per the convention established above — so this surface can
/// grow independently; `RUN_OK == INDEX_OK == 0` by convention.
pub const RUN_OK: c_int = 0;
pub const RUN_ERR_INVALID_INPUT: c_int = 1;
pub const RUN_ERR_UNAUTHORIZED: c_int = 2;
pub const RUN_ERR_NOT_INITIALIZED: c_int = 3;
pub const RUN_ERR_NOT_FOUND: c_int = 4;
/// The run exists but is not in a state the requested verb permits — pausing
/// a run that is not `running`, or resuming one that is not `paused`
/// (`DomainError::InvalidState`, UC-42 Task 11). Distinct from
/// `RUN_ERR_OTHER` for the same reason `FILE_ERR_INVALID_STATE` and
/// `COLLECTION_ERR_INVALID_STATE` are distinct from their own catch-alls: a
/// caller retrying a transient failure and a caller that asked for an
/// impossible transition need different responses.
pub const RUN_ERR_INVALID_STATE: c_int = 5;
pub const RUN_ERR_OTHER: c_int = 9;

/// Result of `alexandria_index_run_status_json` (UC-42). On success `status`
/// is `RUN_OK` and `json` is a NUL-terminated JSON string of the `CatalogRun`
/// body — byte-for-byte the same shape HTTP returns from
/// `GET /v1/index/runs/{runId}` (FR-FC-24 / NFR-09). On failure `json` is
/// NULL and `status` carries the mapped error code. The caller must free
/// `json` with `alexandria_free_string`.
#[repr(C)]
#[derive(Debug)]
pub struct RunJsonResult {
    pub status: c_int,
    pub json: *mut c_char,
}

impl RunJsonResult {
    fn err(status: c_int) -> Self {
        Self {
            status,
            json: std::ptr::null_mut(),
        }
    }

    fn ok(json: String) -> Self {
        let cstring = CString::new(json).unwrap_or_default();
        Self {
            status: RUN_OK,
            json: cstring.into_raw(),
        }
    }
}

/// Map a `DomainError` from a run-control or run-query handler to a
/// `RUN_ERR_*` code. The one mapping every FFI export in this section shares,
/// including `alexandria_index_resume`'s `IndexStartResult::status`, which is
/// `c_int`-typed exactly like every other code here despite the struct's name
/// (see the corrections to Task 11 — this surface's errors are `RUN_ERR_*`,
/// not `INDEX_ERR_*`, because resume is part of run control, not of starting
/// a fresh run).
fn map_run_err_code(err: DomainError) -> c_int {
    match err {
        DomainError::NotFound => RUN_ERR_NOT_FOUND,
        DomainError::Unauthorized => RUN_ERR_UNAUTHORIZED,
        DomainError::InvalidInput(_) => RUN_ERR_INVALID_INPUT,
        DomainError::InvalidState => RUN_ERR_INVALID_STATE,
        _ => RUN_ERR_OTHER,
    }
}

fn map_run_err(err: DomainError) -> RunJsonResult {
    RunJsonResult::err(map_run_err_code(err))
}

/// Report an index or re-index run's status and outcome (UC-42 / FR-FC-28).
/// `run_id` is the id `alexandria_index_start` or
/// `alexandria_index_refresh_start` returned. On success `json` carries the
/// same body the HTTP `GET /v1/index/runs/{runId}` route returns (FR-FC-24).
///
/// Returns `RUN_ERR_NOT_FOUND` for an id naming no run (AF-01),
/// `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02), and
/// `RUN_ERR_INVALID_INPUT` when `run_id` is not a uuid.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_run_status_json(
    run_id: *const c_char,
    token: *const c_char,
) -> RunJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return RunJsonResult::err(RUN_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its run id would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return RunJsonResult::err(RUN_ERR_UNAUTHORIZED);
    }

    let raw = match cstr_lossy(run_id) {
        Some(s) => s,
        None => return RunJsonResult::err(RUN_ERR_INVALID_INPUT),
    };
    let Ok(run_id) = uuid::Uuid::parse_str(raw.trim()) else {
        return RunJsonResult::err(RUN_ERR_INVALID_INPUT);
    };

    let result =
        runtime().block_on(async { services.get_run_status_handler.get(run_id, &token).await });

    match result {
        Ok(run) => {
            let json = serde_json::to_string(&run).unwrap_or_default();
            RunJsonResult::ok(json)
        }
        Err(err) => map_run_err(err),
    }
}

/// Pause a running index or re-index run where it stands, leaving it
/// resumable (UC-42 / FR-FC-28). `run_id` is the id `alexandria_index_start`
/// or `alexandria_index_refresh_start` returned; `token` is the bearer auth
/// token. Calls the same `RunControlHandler::pause` the HTTP route (Task 12)
/// calls.
///
/// Returns `RUN_ERR_NOT_FOUND` for an id naming no run (AF-01),
/// `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02),
/// `RUN_ERR_INVALID_INPUT` when `run_id` is not a uuid, and
/// `RUN_ERR_INVALID_STATE` when the run is not currently `running` — pausing
/// an already-paused or already-finished run is refused rather than silently
/// accepted.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_pause(run_id: *const c_char, token: *const c_char) -> c_int {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return RUN_ERR_NOT_INITIALIZED,
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its run id would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return RUN_ERR_UNAUTHORIZED;
    }

    let raw = match cstr_lossy(run_id) {
        Some(s) => s,
        None => return RUN_ERR_INVALID_INPUT,
    };
    let Ok(run_id) = uuid::Uuid::parse_str(raw.trim()) else {
        return RUN_ERR_INVALID_INPUT;
    };

    match runtime().block_on(async { services.run_control_handler.pause(run_id, &token).await }) {
        Ok(()) => RUN_OK,
        Err(err) => map_run_err_code(err),
    }
}

/// Abandon a running or paused index or re-index run (UC-42 / FR-FC-28).
/// Terminal — a cancelled run is never resumed. `run_id` is the id
/// `alexandria_index_start` or `alexandria_index_refresh_start` returned;
/// `token` is the bearer auth token. Calls the same
/// `RunControlHandler::cancel` the HTTP route (Task 12) calls.
///
/// Returns `RUN_ERR_NOT_FOUND` for an id naming no run (AF-01),
/// `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02),
/// `RUN_ERR_INVALID_INPUT` when `run_id` is not a uuid, and
/// `RUN_ERR_INVALID_STATE` when the run is already terminal (`complete`,
/// `failed`, or already `cancelled`) — there is nothing left to abandon.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_cancel(run_id: *const c_char, token: *const c_char) -> c_int {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return RUN_ERR_NOT_INITIALIZED,
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its run id would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return RUN_ERR_UNAUTHORIZED;
    }

    let raw = match cstr_lossy(run_id) {
        Some(s) => s,
        None => return RUN_ERR_INVALID_INPUT,
    };
    let Ok(run_id) = uuid::Uuid::parse_str(raw.trim()) else {
        return RUN_ERR_INVALID_INPUT;
    };

    match runtime().block_on(async { services.run_control_handler.cancel(run_id, &token).await }) {
        Ok(()) => RUN_OK,
        Err(err) => map_run_err_code(err),
    }
}

/// Put a paused index or re-index run back to work (UC-42 / FR-FC-28).
/// `run_id` is the id `alexandria_index_start` or `alexandria_index_refresh_start`
/// returned; `token` is the bearer auth token. Returns the *same* `run_id` on
/// success — a resume does not mint a fresh run, it continues the one it was
/// given — wrapped in the same `IndexStartResult` `alexandria_index_start`
/// returns (parity of shape, not of meaning: `status` here is a `RUN_ERR_*`
/// code, because resume is part of run control, not of starting a fresh run).
///
/// `RunControlHandler::resume` only records the state transition; it does not
/// walk anything. Spawning the walk is this function's job, exactly as
/// `alexandria_index_start` spawns its own — the handler is kept free of the
/// runtime so `execute` is always spawned by whichever transport owns one.
/// Which handler gets spawned depends on `RunResumed::kind`: an index run
/// resumes into `index_handler.execute(&root, run_id)`, a refresh into
/// `refresh_handler.execute(run_id)` (a refresh carries no root — it touches
/// everything cataloged). A resumed index run whose stored `root` is somehow
/// absent — it should never be, every row `RunKind::Index` writes carries one
/// — is refused with `RUN_ERR_OTHER` and logged at `error`, rather than
/// silently doing nothing: a caller told `RUN_OK` for a run that never
/// actually resumes would have no way to notice.
///
/// Returns `RUN_ERR_NOT_FOUND` for an id naming no run (AF-01),
/// `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02),
/// `RUN_ERR_INVALID_INPUT` when `run_id` is not a uuid, and
/// `RUN_ERR_INVALID_STATE` when the run is not currently `paused`.
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_resume(
    run_id: *const c_char,
    token: *const c_char,
) -> IndexStartResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return IndexStartResult::err(RUN_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload — an unauthenticated caller must
    // not learn whether its run id would have parsed.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return IndexStartResult::err(RUN_ERR_UNAUTHORIZED);
    }

    let raw = match cstr_lossy(run_id) {
        Some(s) => s,
        None => return IndexStartResult::err(RUN_ERR_INVALID_INPUT),
    };
    let Ok(run_id) = uuid::Uuid::parse_str(raw.trim()) else {
        return IndexStartResult::err(RUN_ERR_INVALID_INPUT);
    };

    let rt = runtime();
    let resumed =
        match rt.block_on(async { services.run_control_handler.resume(run_id, &token).await }) {
            Ok(resumed) => resumed,
            Err(err) => return IndexStartResult::err(map_run_err_code(err)),
        };

    match resumed.kind {
        RunKind::Index => {
            let root = match resumed.root {
                Some(root) => root,
                None => {
                    // Every row `RunKind::Index` writes carries a root
                    // (`start` requires one); reaching this means the stored
                    // row and its kind have drifted apart. Fail loudly rather
                    // than resume nothing and tell the caller it worked.
                    tracing::error!(
                        run_id = %resumed.run_id,
                        "resumed index run has no stored root; refusing to spawn"
                    );
                    return IndexStartResult::err(RUN_ERR_OTHER);
                }
            };
            let handler = services.index_handler.clone();
            let spawned_run_id = resumed.run_id;
            rt.spawn(async move {
                // Same shape as `alexandria_index_start`'s own spawn: an
                // `Err` here means the run could not resume at all;
                // `execute` has already written its own terminal row on that
                // path (UC-42), so the failure is recorded, not lost.
                if let Err(err) = handler.execute(&root, spawned_run_id).await {
                    tracing::error!(run_id = %spawned_run_id, error = %err, "resumed index run aborted");
                }
            });
        }
        RunKind::Refresh => {
            let handler = services.refresh_handler.clone();
            let spawned_run_id = resumed.run_id;
            rt.spawn(async move {
                if let Err(err) = handler.execute(spawned_run_id).await {
                    tracing::error!(run_id = %spawned_run_id, error = %err, "resumed re-index run aborted");
                }
            });
        }
    }

    IndexStartResult::ok(&resumed.run_id.to_string())
}

/// Every outstanding (`running` or `paused`) index and re-index run at once,
/// each with live progress overlaid exactly as `alexandria_index_run_status_json`
/// overlays a single run (UC-42 / FR-FC-28). `token` is the bearer auth
/// token. On success `json` is a NUL-terminated JSON array of `CatalogRun`
/// bodies, newest first — byte-for-byte the same shape the HTTP
/// `GET /v1/index/runs?status=active` route (Task 12) returns (FR-FC-24 / NFR-09).
/// The caller must free `json` with `alexandria_free_string`.
///
/// A caller with nothing outstanding gets `RUN_OK` and an empty JSON array,
/// not an error — an idle library is the normal case, not a failure.
///
/// Returns `RUN_ERR_UNAUTHORIZED` for an unauthenticated caller (AF-02).
#[allow(unsafe_code)] // `#[no_mangle]` is itself gated by `deny(unsafe_code)`
#[no_mangle]
pub extern "C" fn alexandria_index_runs_active_json(token: *const c_char) -> RunJsonResult {
    let services = match services_slot().lock().unwrap().clone() {
        Some(s) => s,
        None => return RunJsonResult::err(RUN_ERR_NOT_INITIALIZED),
    };

    // Deny before touching the payload, same as every other run-control call
    // in this section.
    let token = cstr_lossy(token).unwrap_or_default();
    if !authenticated(&services, &token) {
        return RunJsonResult::err(RUN_ERR_UNAUTHORIZED);
    }

    let result = runtime().block_on(async { services.get_active_runs_handler.list(&token).await });

    match result {
        Ok(runs) => {
            let json = serde_json::to_string(&runs).unwrap_or_default();
            RunJsonResult::ok(json)
        }
        Err(err) => map_run_err(err),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // `parse_priority` and `map_run_err_code` are pure logic with no
    // filesystem or database behind them, so they are unit-tested here
    // rather than through an FFI call in `tests/`.

    #[test]
    fn given_low_when_priority_parsed_then_low() {
        assert_eq!(parse_priority(Some("low".to_string())), RunPriority::Low);
    }

    #[test]
    fn given_normal_when_priority_parsed_then_normal() {
        assert_eq!(
            parse_priority(Some("normal".to_string())),
            RunPriority::Normal
        );
    }

    #[test]
    fn given_none_when_priority_parsed_then_normal() {
        assert_eq!(parse_priority(None), RunPriority::Normal);
    }

    #[test]
    fn given_garbage_string_when_priority_parsed_then_normal() {
        assert_eq!(
            parse_priority(Some("URGENT!!1".to_string())),
            RunPriority::Normal
        );
    }

    #[test]
    fn given_uppercase_low_when_priority_parsed_then_normal() {
        // Case-sensitive on purpose — the HTTP body Task 12 adds spells it
        // lowercase, and matching that spelling exactly is what keeps the
        // two surfaces at parity (FR-FC-24) rather than accepting a wider
        // set FFI understands and HTTP does not.
        assert_eq!(parse_priority(Some("Low".to_string())), RunPriority::Normal);
    }

    #[test]
    fn given_invalid_state_when_run_err_mapped_then_run_err_invalid_state() {
        assert_eq!(
            map_run_err_code(DomainError::InvalidState),
            RUN_ERR_INVALID_STATE
        );
    }

    #[test]
    fn given_not_found_when_run_err_mapped_then_run_err_not_found() {
        assert_eq!(map_run_err_code(DomainError::NotFound), RUN_ERR_NOT_FOUND);
    }

    #[test]
    fn given_unauthorized_when_run_err_mapped_then_run_err_unauthorized() {
        assert_eq!(
            map_run_err_code(DomainError::Unauthorized),
            RUN_ERR_UNAUTHORIZED
        );
    }

    #[test]
    fn given_invalid_input_when_run_err_mapped_then_run_err_invalid_input() {
        assert_eq!(
            map_run_err_code(DomainError::InvalidInput("bad".into())),
            RUN_ERR_INVALID_INPUT
        );
    }
}
