use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::auth::BearerAuthService;
use crate::bookmarks::commands::create::CreateBookmarkHandler;
use crate::bookmarks::commands::lifecycle::BookmarkLifecycleHandler;
use crate::bookmarks::commands::update::UpdateBookmarkHandler;
use crate::bookmarks::queries::browse::BrowseBookmarksHandler;
use crate::bookmarks::repos::SqliteBookmarkRepository;
use crate::catalog::clock::SystemClock;
use crate::catalog::commands::edit_metadata::EditMetadataHandler;
use crate::catalog::commands::index::IndexHandler;
use crate::catalog::commands::purge::PurgeFileHandler;
use crate::catalog::commands::purge_on_disk::PurgeFileOnDiskHandler;
use crate::catalog::commands::refresh::RefreshHandler;
use crate::catalog::commands::rename::RenameFileHandler;
use crate::catalog::commands::restore::RestoreFileHandler;
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::fs::StdFilesystem;
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::repos::SqliteCatalogRepository;
use crate::collections::commands::add_items::AddItemsToCollectionHandler;
use crate::collections::commands::create::CreateCollectionHandler;
use crate::collections::commands::delete::DeleteCollectionHandler;
use crate::collections::commands::remove_item::RemoveItemFromCollectionHandler;
use crate::collections::commands::rename::RenameCollectionHandler;
use crate::collections::queries::list_items::ListCollectionItemsHandler;
use crate::collections::repos::SqliteCollectionRepository;
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

pub type DefaultRestoreFileHandler =
    RestoreFileHandler<BearerAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileHandler =
    PurgeFileHandler<BearerAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileOnDiskHandler =
    PurgeFileOnDiskHandler<BearerAuthService, SqliteCatalogRepository, StdFilesystem>;

pub type DefaultBrowseFilesHandler = BrowseFilesHandler<BearerAuthService, SqliteCatalogRepository>;

pub type DefaultCreateCollectionHandler =
    CreateCollectionHandler<BearerAuthService, SqliteCollectionRepository>;

pub type DefaultRenameCollectionHandler =
    RenameCollectionHandler<BearerAuthService, SqliteCollectionRepository>;

pub type DefaultDeleteCollectionHandler =
    DeleteCollectionHandler<BearerAuthService, SqliteCollectionRepository>;

pub type DefaultCreateBookmarkHandler =
    CreateBookmarkHandler<BearerAuthService, SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultUpdateBookmarkHandler =
    UpdateBookmarkHandler<BearerAuthService, SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultBrowseBookmarksHandler =
    BrowseBookmarksHandler<BearerAuthService, SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultBookmarkLifecycleHandler =
    BookmarkLifecycleHandler<BearerAuthService, SqliteBookmarkRepository, SystemClock>;

pub type DefaultAddItemsToCollectionHandler = AddItemsToCollectionHandler<
    BearerAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultRemoveItemFromCollectionHandler = RemoveItemFromCollectionHandler<
    BearerAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultListCollectionItemsHandler = ListCollectionItemsHandler<
    BearerAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

#[derive(Clone)]
pub struct Services {
    pub index_handler: Arc<DefaultIndexHandler>,
    pub refresh_handler: Arc<DefaultRefreshHandler>,
    pub edit_metadata_handler: Arc<DefaultEditMetadataHandler>,
    pub rename_file_handler: Arc<DefaultRenameFileHandler>,
    pub soft_delete_file_handler: Arc<DefaultSoftDeleteFileHandler>,
    pub restore_file_handler: Arc<DefaultRestoreFileHandler>,
    pub purge_file_handler: Arc<DefaultPurgeFileHandler>,
    pub purge_file_on_disk_handler: Arc<DefaultPurgeFileOnDiskHandler>,
    pub browse_files_handler: Arc<DefaultBrowseFilesHandler>,
    pub create_collection_handler: Arc<DefaultCreateCollectionHandler>,
    pub rename_collection_handler: Arc<DefaultRenameCollectionHandler>,
    pub delete_collection_handler: Arc<DefaultDeleteCollectionHandler>,
    pub create_bookmark_handler: Arc<DefaultCreateBookmarkHandler>,
    pub update_bookmark_handler: Arc<DefaultUpdateBookmarkHandler>,
    pub browse_bookmarks_handler: Arc<DefaultBrowseBookmarksHandler>,
    pub bookmark_lifecycle_handler: Arc<DefaultBookmarkLifecycleHandler>,
    pub add_items_to_collection_handler: Arc<DefaultAddItemsToCollectionHandler>,
    pub remove_item_from_collection_handler: Arc<DefaultRemoveItemFromCollectionHandler>,
    pub list_collection_items_handler: Arc<DefaultListCollectionItemsHandler>,
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
    let retention_days = settings.deletion.retention_days;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let auth = BearerAuthService;
    let fs = StdFilesystem;
    let clock = SystemClock;
    let index_handler = Arc::new(IndexHandler::new(auth, repo.clone(), fs, clock));
    let refresh_handler = Arc::new(RefreshHandler::new(auth, repo.clone(), fs, clock));
    let edit_metadata_handler = Arc::new(EditMetadataHandler::new(auth, repo.clone()));
    let rename_file_handler = Arc::new(RenameFileHandler::new(auth, repo.clone(), fs));
    let soft_delete_file_handler = Arc::new(SoftDeleteFileHandler::new(auth, repo.clone(), clock));
    let restore_file_handler = Arc::new(RestoreFileHandler::new(
        auth,
        repo.clone(),
        clock,
        retention_days,
    ));
    let purge_file_handler = Arc::new(PurgeFileHandler::new(
        auth,
        repo.clone(),
        clock,
        retention_days,
    ));
    let purge_file_on_disk_handler = Arc::new(PurgeFileOnDiskHandler::new(auth, repo.clone(), fs));
    let browse_files_handler = Arc::new(BrowseFilesHandler::new(auth, repo.clone()));
    let create_collection_handler = Arc::new(CreateCollectionHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let rename_collection_handler = Arc::new(RenameCollectionHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let delete_collection_handler = Arc::new(DeleteCollectionHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let create_bookmark_handler = Arc::new(CreateBookmarkHandler::new(
        auth,
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let update_bookmark_handler = Arc::new(UpdateBookmarkHandler::new(
        auth,
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let browse_bookmarks_handler = Arc::new(BrowseBookmarksHandler::new(
        auth,
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let bookmark_lifecycle_handler = Arc::new(BookmarkLifecycleHandler::new(
        auth,
        SqliteBookmarkRepository::new(pool.clone()),
        clock,
    ));
    let add_items_to_collection_handler = Arc::new(AddItemsToCollectionHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let remove_item_from_collection_handler = Arc::new(RemoveItemFromCollectionHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let list_collection_items_handler = Arc::new(ListCollectionItemsHandler::new(
        auth,
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    Services {
        index_handler,
        refresh_handler,
        edit_metadata_handler,
        rename_file_handler,
        soft_delete_file_handler,
        restore_file_handler,
        purge_file_handler,
        purge_file_on_disk_handler,
        browse_files_handler,
        create_collection_handler,
        rename_collection_handler,
        delete_collection_handler,
        create_bookmark_handler,
        update_bookmark_handler,
        browse_bookmarks_handler,
        bookmark_lifecycle_handler,
        add_items_to_collection_handler,
        remove_item_from_collection_handler,
        list_collection_items_handler,
        auth,
        pool,
    }
}
