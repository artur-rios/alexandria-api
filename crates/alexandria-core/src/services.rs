use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::bookmarks::commands::create::CreateBookmarkHandler;
use crate::bookmarks::commands::lifecycle::BookmarkLifecycleHandler;
use crate::bookmarks::commands::purge::PurgeBookmarkHandler;
use crate::bookmarks::commands::update::UpdateBookmarkHandler;
use crate::bookmarks::queries::browse::BrowseBookmarksHandler;
use crate::bookmarks::repos::SqliteBookmarkRepository;
use crate::catalog::audio_tags::{LoftyAudioMetadataReader, LoftyCoverArtReader};
use crate::catalog::clock::{Clock, SystemClock};
use crate::catalog::comic_tags::CbzComicMetadataReader;
use crate::catalog::commands::edit_content::EditTextFileContentHandler;
use crate::catalog::commands::edit_metadata::EditMetadataHandler;
use crate::catalog::commands::index::IndexHandler;
use crate::catalog::commands::purge::PurgeFileHandler;
use crate::catalog::commands::purge_on_disk::PurgeFileOnDiskHandler;
use crate::catalog::commands::refresh::RefreshHandler;
use crate::catalog::commands::rename::RenameFileHandler;
use crate::catalog::commands::restore::RestoreFileHandler;
use crate::catalog::commands::run_control::RunControlHandler;
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::document_tags::PdfEpubMetadataReader;
use crate::catalog::fs::StdFilesystem;
use crate::catalog::image_tags::ExifImageMetadataReader;
use crate::catalog::library_root::LibraryRoot;
use crate::catalog::queries::active_runs::GetActiveRunsHandler;
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::queries::read_content::ReadTextFileContentHandler;
use crate::catalog::queries::run_status::GetRunStatusHandler;
use crate::catalog::repos::SqliteCatalogRepository;
use crate::catalog::run_registry::RunRegistry;
use crate::catalog::runs::{CatalogRunRepository, SqliteCatalogRunRepository};
use crate::catalog::video_tags::FfmpegVideoMetadataReader;
use crate::collections::commands::add_items::AddItemsToCollectionHandler;
use crate::collections::commands::create::CreateCollectionHandler;
use crate::collections::commands::delete::DeleteCollectionHandler;
use crate::collections::commands::remove_item::RemoveItemFromCollectionHandler;
use crate::collections::commands::rename::RenameCollectionHandler;
use crate::collections::queries::list::ListCollectionsHandler;
use crate::collections::queries::list_items::ListCollectionItemsHandler;
use crate::collections::repos::SqliteCollectionRepository;
use crate::config::Settings;
use crate::libraries::commands::{
    IndexLibraryHandler, MoveLibraryHandler, RegisterLibraryHandler, RemoveLibraryHandler,
    RenameLibraryHandler, SetLibraryFavoriteHandler, SetLibraryPinnedHandler,
};
use crate::libraries::queries::{BrowseLibraryHandler, CatalogStatsQuery, ListLibrariesHandler};
use crate::libraries::repos::SqliteLibraryRepository;
use crate::playback::comic_page::{ComicPageHandler, ZipComicArchive};
use crate::playback::source::PlaybackSourceHandler;
use crate::playback::thumbnail::{DiskThumbnailCache, ImageThumbnailRenderer, ThumbnailHandler};
use crate::playback::StdFileStat;
use crate::reading_lists::commands::add_item::AddItemToReadingListHandler;
use crate::reading_lists::commands::create::CreateReadingListHandler;
use crate::reading_lists::commands::delete::DeleteReadingListHandler;
use crate::reading_lists::commands::remove_item::RemoveItemFromReadingListHandler;
use crate::reading_lists::commands::update_progress::UpdateReadingProgressHandler;
use crate::reading_lists::queries::browse::BrowseReadingListsHandler;
use crate::reading_lists::repos::SqliteReadingListRepository;
use crate::settings::GetSettingsHandler;
use crate::watchlists::commands::add_video::AddVideoToWatchlistHandler;
use crate::watchlists::commands::create::CreateWatchlistHandler;
use crate::watchlists::commands::delete::DeleteWatchlistHandler;
use crate::watchlists::commands::remove_video::RemoveVideoFromWatchlistHandler;
use crate::watchlists::commands::update_progress::UpdateWatchProgressHandler;
use crate::watchlists::queries::browse::BrowseWatchlistsHandler;
use crate::watchlists::repos::SqliteWatchlistRepository;

/// Concrete index handler wired with the runtime collaborators: the Sqlite
/// catalog repository, the on-disk filesystem, and the system clock. Both the
/// HTTP and FFI surfaces depend on the same `Services` instance so the two
/// transports stay at parity (NFR-09).
pub type DefaultIndexHandler = IndexHandler<
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    LoftyAudioMetadataReader,
    ExifImageMetadataReader,
    PdfEpubMetadataReader,
    FfmpegVideoMetadataReader,
    CbzComicMetadataReader,
    SqliteCatalogRunRepository,
>;

pub type DefaultRefreshHandler = RefreshHandler<
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
    SqliteCatalogRunRepository,
    LoftyAudioMetadataReader,
    ExifImageMetadataReader,
    PdfEpubMetadataReader,
    FfmpegVideoMetadataReader,
    CbzComicMetadataReader,
>;

pub type DefaultEditMetadataHandler = EditMetadataHandler<SqliteCatalogRepository>;

pub type DefaultRenameFileHandler = RenameFileHandler<SqliteCatalogRepository, StdFilesystem>;

pub type DefaultSoftDeleteFileHandler = SoftDeleteFileHandler<SqliteCatalogRepository, SystemClock>;

pub type DefaultRestoreFileHandler = RestoreFileHandler<SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileHandler = PurgeFileHandler<SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileOnDiskHandler =
    PurgeFileOnDiskHandler<SqliteCatalogRepository, StdFilesystem>;

pub type DefaultBrowseFilesHandler = BrowseFilesHandler<SqliteCatalogRepository>;

pub type DefaultReadTextFileContentHandler =
    ReadTextFileContentHandler<SqliteCatalogRepository, StdFilesystem>;

pub type DefaultGetRunStatusHandler = GetRunStatusHandler<SqliteCatalogRunRepository, SystemClock>;

pub type DefaultGetActiveRunsHandler =
    GetActiveRunsHandler<SqliteCatalogRunRepository, SystemClock>;

pub type DefaultRunControlHandler = RunControlHandler<SqliteCatalogRunRepository, SystemClock>;

pub type DefaultEditTextFileContentHandler =
    EditTextFileContentHandler<SqliteCatalogRepository, StdFilesystem, SystemClock>;

pub type DefaultPlaybackSourceHandler = PlaybackSourceHandler<SqliteCatalogRepository, StdFileStat>;

pub type DefaultComicPageHandler = ComicPageHandler<SqliteCatalogRepository, ZipComicArchive>;

pub type DefaultThumbnailHandler = ThumbnailHandler<
    SqliteCatalogRepository,
    ZipComicArchive,
    ImageThumbnailRenderer,
    DiskThumbnailCache,
    LoftyCoverArtReader,
>;

pub type DefaultCreateCollectionHandler = CreateCollectionHandler<SqliteCollectionRepository>;

pub type DefaultRenameCollectionHandler = RenameCollectionHandler<SqliteCollectionRepository>;

pub type DefaultDeleteCollectionHandler = DeleteCollectionHandler<SqliteCollectionRepository>;

pub type DefaultCreateBookmarkHandler =
    CreateBookmarkHandler<SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultUpdateBookmarkHandler =
    UpdateBookmarkHandler<SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultBrowseBookmarksHandler =
    BrowseBookmarksHandler<SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultBookmarkLifecycleHandler =
    BookmarkLifecycleHandler<SqliteBookmarkRepository, SystemClock>;

pub type DefaultPurgeBookmarkHandler = PurgeBookmarkHandler<SqliteBookmarkRepository, SystemClock>;

pub type DefaultAddItemsToCollectionHandler = AddItemsToCollectionHandler<
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultRemoveItemFromCollectionHandler = RemoveItemFromCollectionHandler<
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultGetSettingsHandler = GetSettingsHandler;

pub type DefaultListCollectionsHandler = ListCollectionsHandler<SqliteCollectionRepository>;

pub type DefaultListCollectionItemsHandler = ListCollectionItemsHandler<
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultCreateWatchlistHandler = CreateWatchlistHandler<SqliteWatchlistRepository>;

pub type DefaultAddVideoToWatchlistHandler =
    AddVideoToWatchlistHandler<SqliteWatchlistRepository, SqliteCatalogRepository>;

pub type DefaultBrowseWatchlistsHandler = BrowseWatchlistsHandler<SqliteWatchlistRepository>;

pub type DefaultUpdateWatchProgressHandler = UpdateWatchProgressHandler<SqliteWatchlistRepository>;

pub type DefaultRemoveVideoFromWatchlistHandler =
    RemoveVideoFromWatchlistHandler<SqliteWatchlistRepository>;

pub type DefaultDeleteWatchlistHandler = DeleteWatchlistHandler<SqliteWatchlistRepository>;

pub type DefaultCreateReadingListHandler = CreateReadingListHandler<SqliteReadingListRepository>;

pub type DefaultAddItemToReadingListHandler =
    AddItemToReadingListHandler<SqliteReadingListRepository, SqliteCatalogRepository>;

pub type DefaultBrowseReadingListsHandler = BrowseReadingListsHandler<SqliteReadingListRepository>;

pub type DefaultUpdateReadingProgressHandler =
    UpdateReadingProgressHandler<SqliteReadingListRepository>;

pub type DefaultRemoveItemFromReadingListHandler =
    RemoveItemFromReadingListHandler<SqliteReadingListRepository>;

pub type DefaultDeleteReadingListHandler = DeleteReadingListHandler<SqliteReadingListRepository>;

pub type DefaultRegisterLibraryHandler =
    RegisterLibraryHandler<SqliteLibraryRepository, Arc<DefaultIndexHandler>>;

pub type DefaultIndexLibraryHandler = IndexLibraryHandler<
    SqliteLibraryRepository,
    Arc<DefaultIndexHandler>,
    SqliteCatalogRunRepository,
>;

pub type DefaultRemoveLibraryHandler =
    RemoveLibraryHandler<SqliteLibraryRepository, SqliteCatalogRunRepository>;

pub type DefaultMoveLibraryHandler =
    MoveLibraryHandler<SqliteLibraryRepository, SqliteCatalogRunRepository>;

pub type DefaultRenameLibraryHandler = RenameLibraryHandler<SqliteLibraryRepository>;

pub type DefaultSetLibraryPinnedHandler = SetLibraryPinnedHandler<SqliteLibraryRepository>;

pub type DefaultSetLibraryFavoriteHandler = SetLibraryFavoriteHandler<SqliteLibraryRepository>;

pub type DefaultBrowseLibraryHandler =
    BrowseLibraryHandler<SqliteLibraryRepository, SqliteCatalogRepository>;

pub type DefaultListLibrariesHandler = ListLibrariesHandler<SqliteLibraryRepository>;

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
    pub read_text_file_content_handler: Arc<DefaultReadTextFileContentHandler>,
    pub get_run_status_handler: Arc<DefaultGetRunStatusHandler>,
    pub get_active_runs_handler: Arc<DefaultGetActiveRunsHandler>,
    pub run_control_handler: Arc<DefaultRunControlHandler>,
    pub edit_text_file_content_handler: Arc<DefaultEditTextFileContentHandler>,
    pub playback_source_handler: Arc<DefaultPlaybackSourceHandler>,
    pub comic_page_handler: Arc<DefaultComicPageHandler>,
    pub thumbnail_handler: Arc<DefaultThumbnailHandler>,
    pub create_collection_handler: Arc<DefaultCreateCollectionHandler>,
    pub rename_collection_handler: Arc<DefaultRenameCollectionHandler>,
    pub delete_collection_handler: Arc<DefaultDeleteCollectionHandler>,
    pub create_bookmark_handler: Arc<DefaultCreateBookmarkHandler>,
    pub update_bookmark_handler: Arc<DefaultUpdateBookmarkHandler>,
    pub browse_bookmarks_handler: Arc<DefaultBrowseBookmarksHandler>,
    pub bookmark_lifecycle_handler: Arc<DefaultBookmarkLifecycleHandler>,
    pub purge_bookmark_handler: Arc<DefaultPurgeBookmarkHandler>,
    pub add_items_to_collection_handler: Arc<DefaultAddItemsToCollectionHandler>,
    pub remove_item_from_collection_handler: Arc<DefaultRemoveItemFromCollectionHandler>,
    pub list_collection_items_handler: Arc<DefaultListCollectionItemsHandler>,
    pub list_collections_handler: Arc<DefaultListCollectionsHandler>,
    pub get_settings_handler: Arc<DefaultGetSettingsHandler>,
    pub create_watchlist_handler: Arc<DefaultCreateWatchlistHandler>,
    pub add_video_to_watchlist_handler: Arc<DefaultAddVideoToWatchlistHandler>,
    pub browse_watchlists_handler: Arc<DefaultBrowseWatchlistsHandler>,
    pub update_watch_progress_handler: Arc<DefaultUpdateWatchProgressHandler>,
    pub remove_video_from_watchlist_handler: Arc<DefaultRemoveVideoFromWatchlistHandler>,
    pub delete_watchlist_handler: Arc<DefaultDeleteWatchlistHandler>,
    pub create_reading_list_handler: Arc<DefaultCreateReadingListHandler>,
    pub add_item_to_reading_list_handler: Arc<DefaultAddItemToReadingListHandler>,
    pub browse_reading_lists_handler: Arc<DefaultBrowseReadingListsHandler>,
    pub update_reading_progress_handler: Arc<DefaultUpdateReadingProgressHandler>,
    pub remove_item_from_reading_list_handler: Arc<DefaultRemoveItemFromReadingListHandler>,
    pub delete_reading_list_handler: Arc<DefaultDeleteReadingListHandler>,
    pub register_library_handler: Arc<DefaultRegisterLibraryHandler>,
    pub index_library_handler: Arc<DefaultIndexLibraryHandler>,
    pub remove_library_handler: Arc<DefaultRemoveLibraryHandler>,
    pub move_library_handler: Arc<DefaultMoveLibraryHandler>,
    pub rename_library_handler: Arc<DefaultRenameLibraryHandler>,
    pub set_library_pinned_handler: Arc<DefaultSetLibraryPinnedHandler>,
    pub set_library_favorite_handler: Arc<DefaultSetLibraryFavoriteHandler>,
    pub browse_library_handler: Arc<DefaultBrowseLibraryHandler>,
    pub list_libraries_handler: Arc<DefaultListLibrariesHandler>,
    pub catalog_stats_query: Arc<CatalogStatsQuery>,
    /// The same registry the two walks publish into, exposed so an embedder
    /// can ask whether this process is executing anything before it tears
    /// these services down. `alexandria_index_init` is the caller: see
    /// [`RunRegistry::live_runs`] for what replacing a live registry costs.
    pub run_registry: RunRegistry,
    pub pool: SqlitePool,
}

/// Build the shared services from the loaded settings and a connected SQLite
/// pool. Callers (the FFI, integration tests) are responsible for running
/// migrations first.
pub async fn build_services(settings: &Settings, pool: SqlitePool) -> Services {
    let retention_days = settings.deletion.retention_days;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let run_repo = SqliteCatalogRunRepository::new(pool.clone());
    // FR-FC-28: one registry, shared by the handlers that publish live run
    // progress and the query that reads it back.
    let run_registry = RunRegistry::new();
    let fs = StdFilesystem;
    let clock = SystemClock;
    // FR-FC-29: any run still recorded as `running` belongs to a process that
    // is gone — runs execute in-process, so nothing is walking it. Reconcile
    // them into `paused`, so a client polling one gets a definite answer
    // instead of waiting forever, and the owner is offered the run back
    // rather than told it was lost. Nothing is started here: resuming is an
    // explicit act. A failure must not stop startup — the catalog is still
    // fully usable, and the stale rows are reconciled on the next boot.
    match run_repo.pause_running(clock.now()).await {
        Ok(0) => {}
        Ok(reconciled) => {
            tracing::info!(
                reconciled,
                "paused runs left by a previous process; they can be resumed"
            )
        }
        Err(err) => {
            tracing::warn!(error = %err, "could not reconcile runs left by a previous process")
        }
    }
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let document_tags = PdfEpubMetadataReader;
    let video_tags = FfmpegVideoMetadataReader;
    let comic_tags = CbzComicMetadataReader;
    // UC-01 and UC-02 are the same hash-every-file workload, so both walks
    // take the same `indexing.concurrency` bound, and the same
    // `indexing.low_priority_concurrency` bound for a `RunPriority::Low` run
    // (FR-FC-08).
    let indexing_concurrency = settings.indexing.concurrency;
    let indexing_low_priority_concurrency = settings.indexing.low_priority_concurrency;
    // FR-FC-26: `filesystem.root` bounds which trees UC-01 will index, where
    // UC-51 may move a library to, and which cataloged files playback and the
    // file-content operations will read or change. It is logged here, once
    // per process, because this is the single startup path both transports go
    // through — the HTTP binary and `alexandria_index_init` — so the two
    // surfaces cannot disagree about whether the bound is on. UC-02 takes no
    // root (it re-walks paths already in the catalog), so it needs no
    // equivalent.
    let library_root = LibraryRoot::new(&settings.filesystem.root);
    if !library_root.is_configured() {
        tracing::warn!(
            "filesystem.root is unset: indexing, library moves and file access are \
             unconstrained and will reach any absolute path a caller supplies; set \
             filesystem.root to bound them"
        );
    } else if let Err(err) = std::fs::canonicalize(settings.filesystem.root.trim()) {
        // Not fatal: the server still starts and every other operation still
        // works, exactly as `check_root_within_library` does at request time
        // (FR-FC-26/AF-07). Logged here too so a broken bound is visible at
        // startup rather than only on the first index attempt.
        tracing::error!(
            root = %settings.filesystem.root,
            error = %err,
            "configured filesystem.root cannot be resolved; indexing, library moves and file \
             access will be refused until it is fixed"
        );
    }
    let index_handler = Arc::new(IndexHandler::new(
        repo.clone(),
        fs,
        clock,
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        indexing_concurrency,
        indexing_low_priority_concurrency,
        settings.filesystem.root.clone(),
        run_repo.clone(),
        run_registry.clone(),
    ));
    let refresh_handler = Arc::new(RefreshHandler::new(
        repo.clone(),
        fs,
        clock,
        // The same readers the index uses: a refresh that re-read a file
        // differently from the run that first catalogued it would fill the
        // gaps with a second opinion.
        audio_tags,
        image_tags,
        document_tags,
        video_tags,
        comic_tags,
        indexing_concurrency,
        indexing_low_priority_concurrency,
        run_repo.clone(),
        run_registry.clone(),
    ));
    let edit_metadata_handler = Arc::new(EditMetadataHandler::new(repo.clone()));
    let rename_file_handler = Arc::new(RenameFileHandler::new(
        repo.clone(),
        fs,
        library_root.clone(),
    ));
    let soft_delete_file_handler = Arc::new(SoftDeleteFileHandler::new(repo.clone(), clock));
    let restore_file_handler =
        Arc::new(RestoreFileHandler::new(repo.clone(), clock, retention_days));
    let purge_file_handler = Arc::new(PurgeFileHandler::new(repo.clone(), clock, retention_days));
    let purge_file_on_disk_handler = Arc::new(PurgeFileOnDiskHandler::new(
        repo.clone(),
        fs,
        library_root.clone(),
    ));
    let browse_files_handler = Arc::new(BrowseFilesHandler::new(repo.clone()));
    let read_text_file_content_handler = Arc::new(ReadTextFileContentHandler::new(
        repo.clone(),
        fs,
        library_root.clone(),
    ));
    let get_run_status_handler = Arc::new(GetRunStatusHandler::new(
        run_repo.clone(),
        clock,
        run_registry.clone(),
    ));
    // Same repository and registry as the single-run query above: a client
    // listing outstanding runs wants the same live-overlaid numbers a
    // single-run read gives, not a second, differently-sourced answer.
    let get_active_runs_handler = Arc::new(GetActiveRunsHandler::new(
        run_repo.clone(),
        clock,
        run_registry.clone(),
    ));
    // The same registry the two walks publish into: pausing a run means
    // writing a signal into the very cell its own loop is reading, so a
    // second registry here would signal nothing at all.
    let run_control_handler = Arc::new(RunControlHandler::new(
        run_repo.clone(),
        clock,
        run_registry.clone(),
        // The same two widths both walks are built with, so a run resumed at
        // a priority lands on exactly the width a run *started* at that
        // priority would have — and a resumed run whose row records no width
        // goes back to the configured `indexing.concurrency`.
        indexing_concurrency,
        indexing_low_priority_concurrency,
    ));
    let edit_text_file_content_handler = Arc::new(EditTextFileContentHandler::new(
        repo.clone(),
        fs,
        clock,
        library_root.clone(),
    ));
    let playback_source_handler = Arc::new(PlaybackSourceHandler::new(
        repo.clone(),
        StdFileStat,
        library_root.clone(),
    ));
    let comic_page_handler = Arc::new(ComicPageHandler::new(
        repo.clone(),
        ZipComicArchive,
        library_root.clone(),
    ));
    let thumbnail_handler = Arc::new(ThumbnailHandler::new(
        repo.clone(),
        ZipComicArchive,
        ImageThumbnailRenderer,
        DiskThumbnailCache::new(settings.playback.thumbnail_cache_dir.clone()),
        // Beside `audio_tags` (`LoftyAudioMetadataReader`), the reader it
        // sits next to: both are lofty-backed reads of the same tagged
        // file, at two different times for two different reasons (see
        // `CoverArtReader`'s own doc comment).
        LoftyCoverArtReader,
        library_root.clone(),
    ));
    let create_collection_handler = Arc::new(CreateCollectionHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let rename_collection_handler = Arc::new(RenameCollectionHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let delete_collection_handler = Arc::new(DeleteCollectionHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let create_bookmark_handler = Arc::new(CreateBookmarkHandler::new(
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let update_bookmark_handler = Arc::new(UpdateBookmarkHandler::new(
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let browse_bookmarks_handler = Arc::new(BrowseBookmarksHandler::new(
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let bookmark_lifecycle_handler = Arc::new(BookmarkLifecycleHandler::new(
        SqliteBookmarkRepository::new(pool.clone()),
        clock,
    ));
    let purge_bookmark_handler = Arc::new(PurgeBookmarkHandler::new(
        SqliteBookmarkRepository::new(pool.clone()),
        clock,
        retention_days,
    ));
    let add_items_to_collection_handler = Arc::new(AddItemsToCollectionHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let remove_item_from_collection_handler = Arc::new(RemoveItemFromCollectionHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let list_collection_items_handler = Arc::new(ListCollectionItemsHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let list_collections_handler = Arc::new(ListCollectionsHandler::new(
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let get_settings_handler = Arc::new(GetSettingsHandler::new(settings));
    let watchlist_repo = SqliteWatchlistRepository::new(pool.clone());
    let create_watchlist_handler = Arc::new(CreateWatchlistHandler::new(watchlist_repo.clone()));
    let add_video_to_watchlist_handler = Arc::new(AddVideoToWatchlistHandler::new(
        watchlist_repo.clone(),
        repo.clone(),
    ));
    let browse_watchlists_handler = Arc::new(BrowseWatchlistsHandler::new(watchlist_repo.clone()));
    let update_watch_progress_handler =
        Arc::new(UpdateWatchProgressHandler::new(watchlist_repo.clone()));
    let remove_video_from_watchlist_handler =
        Arc::new(RemoveVideoFromWatchlistHandler::new(watchlist_repo.clone()));
    let delete_watchlist_handler = Arc::new(DeleteWatchlistHandler::new(watchlist_repo));
    let reading_list_repo = SqliteReadingListRepository::new(pool.clone());
    let create_reading_list_handler =
        Arc::new(CreateReadingListHandler::new(reading_list_repo.clone()));
    let add_item_to_reading_list_handler = Arc::new(AddItemToReadingListHandler::new(
        reading_list_repo.clone(),
        repo.clone(),
    ));
    let browse_reading_lists_handler =
        Arc::new(BrowseReadingListsHandler::new(reading_list_repo.clone()));
    let update_reading_progress_handler =
        Arc::new(UpdateReadingProgressHandler::new(reading_list_repo.clone()));
    let remove_item_from_reading_list_handler = Arc::new(RemoveItemFromReadingListHandler::new(
        reading_list_repo.clone(),
    ));
    let delete_reading_list_handler = Arc::new(DeleteReadingListHandler::new(reading_list_repo));
    let catalog_stats_query = Arc::new(CatalogStatsQuery::new(pool.clone()));
    let list_libraries_handler = Arc::new(ListLibrariesHandler::new(SqliteLibraryRepository::new(
        pool.clone(),
    )));
    let register_library_handler = Arc::new(RegisterLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        index_handler.clone(),
        library_root.clone(),
    ));
    let index_library_handler = Arc::new(IndexLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        index_handler.clone(),
        SqliteCatalogRunRepository::new(pool.clone()),
    ));
    let remove_library_handler = Arc::new(RemoveLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRunRepository::new(pool.clone()),
    ));
    let move_library_handler = Arc::new(MoveLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        library_root.clone(),
        SqliteCatalogRunRepository::new(pool.clone()),
    ));
    let rename_library_handler = Arc::new(RenameLibraryHandler::new(SqliteLibraryRepository::new(
        pool.clone(),
    )));
    let set_library_pinned_handler = Arc::new(SetLibraryPinnedHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let set_library_favorite_handler = Arc::new(SetLibraryFavoriteHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let browse_library_handler = Arc::new(BrowseLibraryHandler::new(
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
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
        read_text_file_content_handler,
        get_run_status_handler,
        get_active_runs_handler,
        run_control_handler,
        edit_text_file_content_handler,
        playback_source_handler,
        comic_page_handler,
        thumbnail_handler,
        create_collection_handler,
        rename_collection_handler,
        delete_collection_handler,
        create_bookmark_handler,
        update_bookmark_handler,
        browse_bookmarks_handler,
        bookmark_lifecycle_handler,
        purge_bookmark_handler,
        add_items_to_collection_handler,
        remove_item_from_collection_handler,
        list_collection_items_handler,
        list_collections_handler,
        get_settings_handler,
        create_watchlist_handler,
        add_video_to_watchlist_handler,
        browse_watchlists_handler,
        update_watch_progress_handler,
        remove_video_from_watchlist_handler,
        delete_watchlist_handler,
        create_reading_list_handler,
        add_item_to_reading_list_handler,
        browse_reading_lists_handler,
        update_reading_progress_handler,
        remove_item_from_reading_list_handler,
        delete_reading_list_handler,
        register_library_handler,
        index_library_handler,
        remove_library_handler,
        move_library_handler,
        rename_library_handler,
        set_library_pinned_handler,
        set_library_favorite_handler,
        browse_library_handler,
        list_libraries_handler,
        catalog_stats_query,
        run_registry,
        pool,
    }
}
