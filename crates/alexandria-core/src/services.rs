use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::auth::BearerAuthService;
use crate::catalog::clock::SystemClock;
use crate::catalog::commands::edit_metadata::EditMetadataHandler;
use crate::catalog::commands::index::IndexHandler;
use crate::catalog::commands::refresh::RefreshHandler;
use crate::catalog::commands::rename::RenameFileHandler;
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::fs::StdFilesystem;
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::repos::SqliteCatalogRepository;
use crate::config::Settings;

/// Concrete index handler wired with the runtime collaborators: the bearer
/// auth stub, the Sqlite catalog repository, the on-disk filesystem, and the
/// system clock. Both the HTTP and FFI surfaces depend on the same `Services`
/// instance so the two transports stay at parity (NFR-09).
pub type DefaultIndexHandler =
    IndexHandler<BearerAuthService, SqliteCatalogRepository, StdFilesystem, SystemClock>;

pub type DefaultRefreshHandler =
    RefreshHandler<BearerAuthService, SqliteCatalogRepository, StdFilesystem, SystemClock>;

pub type DefaultEditMetadataHandler =
    EditMetadataHandler<BearerAuthService, SqliteCatalogRepository>;

pub type DefaultRenameFileHandler =
    RenameFileHandler<BearerAuthService, SqliteCatalogRepository, StdFilesystem>;

pub type DefaultSoftDeleteFileHandler =
    SoftDeleteFileHandler<BearerAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultBrowseFilesHandler =
    BrowseFilesHandler<BearerAuthService, SqliteCatalogRepository>;

#[derive(Clone)]
pub struct Services {
    pub index_handler: Arc<DefaultIndexHandler>,
    pub refresh_handler: Arc<DefaultRefreshHandler>,
    pub edit_metadata_handler: Arc<DefaultEditMetadataHandler>,
    pub rename_file_handler: Arc<DefaultRenameFileHandler>,
    pub soft_delete_file_handler: Arc<DefaultSoftDeleteFileHandler>,
    pub browse_files_handler: Arc<DefaultBrowseFilesHandler>,
    /// The same auth service the handlers hold, exposed so a transport can
    /// reject an unauthenticated caller *before* it parses a request body or
    /// path (FR-AU-07 / SRD §7). Handlers still authenticate independently —
    /// this is the transport gate, not a replacement for the domain check.
    pub auth: BearerAuthService,
    pub pool: SqlitePool,
}

/// Build the shared services from the loaded settings and a connected SQLite
/// pool. Callers (the HTTP server binary, integration tests) are responsible
/// for running migrations first.
pub async fn build_services(settings: &Settings, pool: SqlitePool) -> Services {
    let _ = settings;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let auth = BearerAuthService;
    let fs = StdFilesystem;
    let clock = SystemClock;
    let index_handler = Arc::new(IndexHandler::new(auth, repo.clone(), fs, clock));
    let refresh_handler = Arc::new(RefreshHandler::new(auth, repo.clone(), fs, clock));
    let edit_metadata_handler = Arc::new(EditMetadataHandler::new(auth, repo.clone()));
    let rename_file_handler = Arc::new(RenameFileHandler::new(auth, repo.clone(), fs));
    let soft_delete_file_handler = Arc::new(SoftDeleteFileHandler::new(auth, repo.clone(), clock));
    let browse_files_handler = Arc::new(BrowseFilesHandler::new(auth, repo));
    Services {
        index_handler,
        refresh_handler,
        edit_metadata_handler,
        rename_file_handler,
        soft_delete_file_handler,
        browse_files_handler,
        auth,
        pool,
    }
}