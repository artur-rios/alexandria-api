use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::auth::commands::account_status::GetLocalAccountHandler;
use crate::auth::commands::login::LocalLoginHandler;
use crate::auth::commands::redeem_recovery_code::RedeemRecoveryCodeHandler;
use crate::auth::commands::regenerate_recovery_codes::RegenerateRecoveryCodesHandler;
use crate::auth::commands::register::RegisterLocalAccountHandler;
use crate::auth::commands::set_credentials::SetLocalCredentialsHandler;
use crate::auth::commands::windows_login::WindowsLoginHandler;
use crate::auth::heimdall::HeimdallAuthService;
use crate::auth::local::{
    LocalAuthService, SqliteLocalCredentialRepository, SqliteRecoveryCodeRepository,
    SqliteSessionRepository,
};
use crate::auth::RuntimeAuthService;
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
use crate::config::AuthMode;
use crate::config::Settings;
use crate::enrichment::commands::{EnrichHandler, FsArtistImageStore};
use crate::enrichment::providers::commons::CommonsImageClient;
use crate::enrichment::providers::lrclib::LrclibClient;
use crate::enrichment::providers::musicbrainz::MusicBrainzClient;
use crate::enrichment::queries::ReadEnrichmentHandler;
use crate::enrichment::repos::SqliteEnrichmentRepository;
use crate::libraries::commands::{
    MoveLibraryHandler, RegisterLibraryHandler, RemoveLibraryHandler,
};
use crate::libraries::queries::{BrowseLibraryHandler, ListLibrariesHandler};
use crate::libraries::repos::SqliteLibraryRepository;
use crate::playback::comic_page::{ComicPageHandler, ZipComicArchive};
use crate::playback::energy::{EnergyHandler, FfmpegEnergyAnalyzer, SqliteEnergyStore};
use crate::playback::source::PlaybackSourceHandler;
use crate::playback::thumbnail::{DiskThumbnailCache, ImageThumbnailRenderer, ThumbnailHandler};
use crate::playback::StdFileStat;
use crate::playlists::commands::add_entries::AddEntriesHandler;
use crate::playlists::commands::create::CreatePlaylistHandler;
use crate::playlists::commands::delete::DeletePlaylistHandler;
use crate::playlists::commands::remove_entry::RemoveEntryHandler;
use crate::playlists::commands::rename::RenamePlaylistHandler;
use crate::playlists::commands::reorder::ReorderPlaylistHandler;
use crate::playlists::queries::browse::BrowsePlaylistsHandler;
use crate::playlists::repos::SqlitePlaylistRepository;
use crate::plays::commands::record::RecordPlayHandler;
use crate::plays::queries::stats::MusicStatsHandler;
use crate::plays::repos::SqlitePlayRepository;
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

/// Concrete index handler wired with the runtime collaborators: the bearer
/// auth stub, the Sqlite catalog repository, the on-disk filesystem, and the
/// system clock. Both the HTTP and FFI surfaces depend on the same `Services`
/// instance so the two transports stay at parity (NFR-09).
pub type DefaultIndexHandler = IndexHandler<
    RuntimeAuthService,
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
    RuntimeAuthService,
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

pub type DefaultEditMetadataHandler =
    EditMetadataHandler<RuntimeAuthService, SqliteCatalogRepository>;

pub type DefaultRenameFileHandler =
    RenameFileHandler<RuntimeAuthService, SqliteCatalogRepository, StdFilesystem>;

pub type DefaultSoftDeleteFileHandler =
    SoftDeleteFileHandler<RuntimeAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultRestoreFileHandler =
    RestoreFileHandler<RuntimeAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileHandler =
    PurgeFileHandler<RuntimeAuthService, SqliteCatalogRepository, SystemClock>;

pub type DefaultPurgeFileOnDiskHandler =
    PurgeFileOnDiskHandler<RuntimeAuthService, SqliteCatalogRepository, StdFilesystem>;

pub type DefaultBrowseFilesHandler =
    BrowseFilesHandler<RuntimeAuthService, SqliteCatalogRepository>;

pub type DefaultReadTextFileContentHandler =
    ReadTextFileContentHandler<RuntimeAuthService, SqliteCatalogRepository, StdFilesystem>;

pub type DefaultGetRunStatusHandler =
    GetRunStatusHandler<RuntimeAuthService, SqliteCatalogRunRepository, SystemClock>;

pub type DefaultGetActiveRunsHandler =
    GetActiveRunsHandler<RuntimeAuthService, SqliteCatalogRunRepository, SystemClock>;

pub type DefaultRunControlHandler =
    RunControlHandler<RuntimeAuthService, SqliteCatalogRunRepository, SystemClock>;

pub type DefaultEditTextFileContentHandler = EditTextFileContentHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    StdFilesystem,
    SystemClock,
>;

pub type DefaultPlaybackSourceHandler =
    PlaybackSourceHandler<RuntimeAuthService, SqliteCatalogRepository, StdFileStat>;

pub type DefaultComicPageHandler =
    ComicPageHandler<RuntimeAuthService, SqliteCatalogRepository, ZipComicArchive>;

/// UC-21 — a track's own sound, measured once and kept.
pub type DefaultEnergyHandler = EnergyHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    SqliteEnergyStore,
    FfmpegEnergyAnalyzer,
>;

pub type DefaultThumbnailHandler = ThumbnailHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    ZipComicArchive,
    ImageThumbnailRenderer,
    DiskThumbnailCache,
    LoftyCoverArtReader,
>;

pub type DefaultCreateCollectionHandler =
    CreateCollectionHandler<RuntimeAuthService, SqliteCollectionRepository>;

pub type DefaultRenameCollectionHandler =
    RenameCollectionHandler<RuntimeAuthService, SqliteCollectionRepository>;

pub type DefaultDeleteCollectionHandler =
    DeleteCollectionHandler<RuntimeAuthService, SqliteCollectionRepository>;

pub type DefaultCreateBookmarkHandler =
    CreateBookmarkHandler<RuntimeAuthService, SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultUpdateBookmarkHandler =
    UpdateBookmarkHandler<RuntimeAuthService, SqliteBookmarkRepository, SqliteCollectionRepository>;

pub type DefaultBrowseBookmarksHandler = BrowseBookmarksHandler<
    RuntimeAuthService,
    SqliteBookmarkRepository,
    SqliteCollectionRepository,
>;

pub type DefaultBookmarkLifecycleHandler =
    BookmarkLifecycleHandler<RuntimeAuthService, SqliteBookmarkRepository, SystemClock>;

pub type DefaultPurgeBookmarkHandler =
    PurgeBookmarkHandler<RuntimeAuthService, SqliteBookmarkRepository, SystemClock>;

pub type DefaultAddItemsToCollectionHandler = AddItemsToCollectionHandler<
    RuntimeAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultRemoveItemFromCollectionHandler = RemoveItemFromCollectionHandler<
    RuntimeAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultGetSettingsHandler = GetSettingsHandler<RuntimeAuthService>;

pub type DefaultListCollectionsHandler =
    ListCollectionsHandler<RuntimeAuthService, SqliteCollectionRepository>;

pub type DefaultListCollectionItemsHandler = ListCollectionItemsHandler<
    RuntimeAuthService,
    SqliteCollectionRepository,
    SqliteCatalogRepository,
    SqliteBookmarkRepository,
>;

pub type DefaultCreateWatchlistHandler =
    CreateWatchlistHandler<RuntimeAuthService, SqliteWatchlistRepository>;

pub type DefaultAddVideoToWatchlistHandler = AddVideoToWatchlistHandler<
    RuntimeAuthService,
    SqliteWatchlistRepository,
    SqliteCatalogRepository,
>;

pub type DefaultBrowseWatchlistsHandler =
    BrowseWatchlistsHandler<RuntimeAuthService, SqliteWatchlistRepository>;

pub type DefaultUpdateWatchProgressHandler =
    UpdateWatchProgressHandler<RuntimeAuthService, SqliteWatchlistRepository>;

pub type DefaultRemoveVideoFromWatchlistHandler =
    RemoveVideoFromWatchlistHandler<RuntimeAuthService, SqliteWatchlistRepository>;

pub type DefaultDeleteWatchlistHandler =
    DeleteWatchlistHandler<RuntimeAuthService, SqliteWatchlistRepository>;

pub type DefaultCreateReadingListHandler =
    CreateReadingListHandler<RuntimeAuthService, SqliteReadingListRepository>;

pub type DefaultAddItemToReadingListHandler = AddItemToReadingListHandler<
    RuntimeAuthService,
    SqliteReadingListRepository,
    SqliteCatalogRepository,
>;

pub type DefaultBrowseReadingListsHandler =
    BrowseReadingListsHandler<RuntimeAuthService, SqliteReadingListRepository>;

pub type DefaultUpdateReadingProgressHandler =
    UpdateReadingProgressHandler<RuntimeAuthService, SqliteReadingListRepository>;

pub type DefaultRemoveItemFromReadingListHandler =
    RemoveItemFromReadingListHandler<RuntimeAuthService, SqliteReadingListRepository>;

pub type DefaultDeleteReadingListHandler =
    DeleteReadingListHandler<RuntimeAuthService, SqliteReadingListRepository>;

/// The enrichment run, over the three real service clients.
pub type DefaultEnrichHandler = EnrichHandler<
    RuntimeAuthService,
    SqliteEnrichmentRepository,
    MusicBrainzClient,
    CommonsImageClient,
    LrclibClient,
    FsArtistImageStore,
    SystemClock,
>;

pub type DefaultReadEnrichmentHandler =
    ReadEnrichmentHandler<RuntimeAuthService, SqliteEnrichmentRepository>;

pub type DefaultRegisterLibraryHandler =
    RegisterLibraryHandler<RuntimeAuthService, SqliteLibraryRepository>;

pub type DefaultRemoveLibraryHandler =
    RemoveLibraryHandler<RuntimeAuthService, SqliteLibraryRepository>;

pub type DefaultMoveLibraryHandler =
    MoveLibraryHandler<RuntimeAuthService, SqliteLibraryRepository>;

pub type DefaultBrowseLibraryHandler =
    BrowseLibraryHandler<RuntimeAuthService, SqliteLibraryRepository, SqliteCatalogRepository>;

pub type DefaultListLibrariesHandler =
    ListLibrariesHandler<RuntimeAuthService, SqliteLibraryRepository>;

pub type DefaultCreatePlaylistHandler =
    CreatePlaylistHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultRenamePlaylistHandler =
    RenamePlaylistHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultDeletePlaylistHandler =
    DeletePlaylistHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultAddEntriesHandler = AddEntriesHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultRemoveEntryHandler =
    RemoveEntryHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultReorderPlaylistHandler =
    ReorderPlaylistHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultBrowsePlaylistsHandler =
    BrowsePlaylistsHandler<RuntimeAuthService, SqlitePlaylistRepository>;

pub type DefaultRecordPlayHandler =
    RecordPlayHandler<RuntimeAuthService, SystemClock, SqlitePlayRepository>;

pub type DefaultMusicStatsHandler = MusicStatsHandler<RuntimeAuthService, SqlitePlayRepository>;

pub type DefaultSetLocalCredentialsHandler =
    SetLocalCredentialsHandler<RuntimeAuthService, SqliteLocalCredentialRepository, SystemClock>;

pub type DefaultLocalLoginHandler =
    LocalLoginHandler<SqliteLocalCredentialRepository, SqliteSessionRepository, SystemClock>;

pub type DefaultWindowsLoginHandler = WindowsLoginHandler<SqliteSessionRepository, SystemClock>;

pub type DefaultRegisterLocalAccountHandler = RegisterLocalAccountHandler<
    SqliteLocalCredentialRepository,
    SqliteSessionRepository,
    SystemClock,
    SqliteRecoveryCodeRepository,
>;

pub type DefaultGetLocalAccountHandler = GetLocalAccountHandler<
    RuntimeAuthService,
    SqliteLocalCredentialRepository,
    SqliteRecoveryCodeRepository,
>;

pub type DefaultRedeemRecoveryCodeHandler = RedeemRecoveryCodeHandler<
    SqliteLocalCredentialRepository,
    SqliteSessionRepository,
    SqliteRecoveryCodeRepository,
    SystemClock,
>;

pub type DefaultRegenerateRecoveryCodesHandler = RegenerateRecoveryCodesHandler<
    RuntimeAuthService,
    SqliteLocalCredentialRepository,
    SqliteRecoveryCodeRepository,
    SystemClock,
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
    pub read_text_file_content_handler: Arc<DefaultReadTextFileContentHandler>,
    pub get_run_status_handler: Arc<DefaultGetRunStatusHandler>,
    pub get_active_runs_handler: Arc<DefaultGetActiveRunsHandler>,
    pub run_control_handler: Arc<DefaultRunControlHandler>,
    pub edit_text_file_content_handler: Arc<DefaultEditTextFileContentHandler>,
    pub playback_source_handler: Arc<DefaultPlaybackSourceHandler>,
    pub comic_page_handler: Arc<DefaultComicPageHandler>,
    pub thumbnail_handler: Arc<DefaultThumbnailHandler>,
    pub energy_handler: Arc<DefaultEnergyHandler>,
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
    /// `None` when enrichment is unavailable — switched off, or on with no
    /// contact configured (`MetadataSettings::unavailable_reason`).
    ///
    /// An `Option` rather than a handler that always exists and refuses,
    /// because the three service clients are only built when they can
    /// legitimately be used: constructing a MusicBrainz client with no
    /// contact would produce an agent string its own terms forbid sending,
    /// and having one to hand is how it eventually gets sent.
    pub enrich_handler: Option<Arc<DefaultEnrichHandler>>,
    /// Always present: reading what was already cached is not a network
    /// operation and stays available after enrichment is switched back off,
    /// so an owner who turns it on, runs it once and turns it off keeps what
    /// they fetched.
    pub read_enrichment_handler: Arc<DefaultReadEnrichmentHandler>,
    pub register_library_handler: Arc<DefaultRegisterLibraryHandler>,
    pub remove_library_handler: Arc<DefaultRemoveLibraryHandler>,
    pub move_library_handler: Arc<DefaultMoveLibraryHandler>,
    pub browse_library_handler: Arc<DefaultBrowseLibraryHandler>,
    pub list_libraries_handler: Arc<DefaultListLibrariesHandler>,
    pub create_playlist_handler: Arc<DefaultCreatePlaylistHandler>,
    pub rename_playlist_handler: Arc<DefaultRenamePlaylistHandler>,
    pub delete_playlist_handler: Arc<DefaultDeletePlaylistHandler>,
    pub add_entries_handler: Arc<DefaultAddEntriesHandler>,
    pub remove_entry_handler: Arc<DefaultRemoveEntryHandler>,
    pub reorder_playlist_handler: Arc<DefaultReorderPlaylistHandler>,
    pub browse_playlists_handler: Arc<DefaultBrowsePlaylistsHandler>,
    /// Recording a play and reading the rankings it feeds (play history
    /// design). Two handlers over one repository: everything the statistics
    /// say is an aggregate of what this one write put there.
    pub record_play_handler: Arc<DefaultRecordPlayHandler>,
    pub music_stats_handler: Arc<DefaultMusicStatsHandler>,
    pub set_local_credentials_handler: Arc<DefaultSetLocalCredentialsHandler>,
    pub local_login_handler: Arc<DefaultLocalLoginHandler>,
    pub windows_login_handler: Arc<DefaultWindowsLoginHandler>,
    pub register_local_account_handler: Arc<DefaultRegisterLocalAccountHandler>,
    pub get_local_account_handler: Arc<DefaultGetLocalAccountHandler>,
    pub redeem_recovery_code_handler: Arc<DefaultRedeemRecoveryCodeHandler>,
    pub regenerate_recovery_codes_handler: Arc<DefaultRegenerateRecoveryCodesHandler>,
    /// The same auth service the handlers hold, exposed so a transport can
    /// reject an unauthenticated caller *before* it parses a request body or
    /// path (FR-AU-07 / SRD §7). Handlers still authenticate independently —
    /// this is the transport gate, not a replacement for the domain check.
    pub auth: RuntimeAuthService,
    /// The same registry the two walks publish into, exposed so an embedder
    /// can ask whether this process is executing anything before it tears
    /// these services down. `alexandria_index_init` is the caller: see
    /// [`RunRegistry::live_runs`] for what replacing a live registry costs.
    pub run_registry: RunRegistry,
    pub pool: SqlitePool,
}

/// Build the shared services from the loaded settings and a connected SQLite
/// pool. Callers (the HTTP server binary, integration tests) are responsible
/// for running migrations first.
pub async fn build_services(settings: &Settings, pool: SqlitePool) -> Services {
    let retention_days = settings.deletion.retention_days;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let run_repo = SqliteCatalogRunRepository::new(pool.clone());
    // FR-FC-28: one registry, shared by the handlers that publish live run
    // progress and the query that reads it back.
    let run_registry = RunRegistry::new();
    let session_repo = SqliteSessionRepository::new(pool.clone());
    let credential_repo = SqliteLocalCredentialRepository::new(pool.clone());
    let recovery_code_repo = SqliteRecoveryCodeRepository::new(pool.clone());
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
    // FR-AU-01/FR-AU-03: exactly one auth mode is active, selected once here
    // from startup configuration.
    let auth = match settings.auth.mode {
        AuthMode::Local => {
            RuntimeAuthService::Local(LocalAuthService::new(session_repo.clone(), clock))
        }
        // UC-36: Heimdall signs HS256 with a secret this process is
        // configured with, so verification is offline and needs no
        // collaborator. `AuthSettings::validate` has already refused to let a
        // misconfigured process get this far.
        AuthMode::External => {
            RuntimeAuthService::External(HeimdallAuthService::from_settings(&settings.auth))
        }
        // UC-45: the account this process runs as was verified at startup,
        // before this point. What remains is session validation, which is
        // exactly local mode's — hence the same service behind a different
        // variant.
        AuthMode::Windows => {
            RuntimeAuthService::Windows(LocalAuthService::new(session_repo.clone(), clock))
        }
    };
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
    // FR-FC-26: `filesystem.root` bounds which trees UC-01 will index. It is
    // logged here, once per process, because this is the single startup path
    // both transports go through — the HTTP binary and `alexandria_index_init`
    // — so the two surfaces cannot disagree about whether the bound is on.
    // UC-02 takes no root (it re-walks paths already in the catalog), so it
    // needs no equivalent.
    if settings.filesystem.root.trim().is_empty() {
        tracing::warn!(
            "filesystem.root is unset: indexing is unconstrained and will catalog any \
             absolute path a caller supplies; set filesystem.root to bound it"
        );
    } else if let Err(err) = std::fs::canonicalize(settings.filesystem.root.trim()) {
        // Not fatal: the server still starts and every other operation still
        // works, exactly as `check_root_within_library` does at request time
        // (FR-FC-26/AF-07). Logged here too so a broken bound is visible at
        // startup rather than only on the first index attempt.
        tracing::error!(
            root = %settings.filesystem.root,
            error = %err,
            "configured filesystem.root cannot be resolved; indexing will be refused until it is fixed"
        );
    }
    let index_handler = Arc::new(IndexHandler::new(
        auth.clone(),
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
        auth.clone(),
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
    let edit_metadata_handler = Arc::new(EditMetadataHandler::new(auth.clone(), repo.clone()));
    let rename_file_handler = Arc::new(RenameFileHandler::new(auth.clone(), repo.clone(), fs));
    let soft_delete_file_handler = Arc::new(SoftDeleteFileHandler::new(
        auth.clone(),
        repo.clone(),
        clock,
    ));
    let restore_file_handler = Arc::new(RestoreFileHandler::new(
        auth.clone(),
        repo.clone(),
        clock,
        retention_days,
    ));
    let purge_file_handler = Arc::new(PurgeFileHandler::new(
        auth.clone(),
        repo.clone(),
        clock,
        retention_days,
    ));
    let purge_file_on_disk_handler =
        Arc::new(PurgeFileOnDiskHandler::new(auth.clone(), repo.clone(), fs));
    let browse_files_handler = Arc::new(BrowseFilesHandler::new(auth.clone(), repo.clone()));
    let read_text_file_content_handler = Arc::new(ReadTextFileContentHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
    ));
    let get_run_status_handler = Arc::new(GetRunStatusHandler::new(
        auth.clone(),
        run_repo.clone(),
        clock,
        run_registry.clone(),
    ));
    // Same repository and registry as the single-run query above: a client
    // listing outstanding runs wants the same live-overlaid numbers a
    // single-run read gives, not a second, differently-sourced answer.
    let get_active_runs_handler = Arc::new(GetActiveRunsHandler::new(
        auth.clone(),
        run_repo.clone(),
        clock,
        run_registry.clone(),
    ));
    // The same registry the two walks publish into: pausing a run means
    // writing a signal into the very cell its own loop is reading, so a
    // second registry here would signal nothing at all.
    let run_control_handler = Arc::new(RunControlHandler::new(
        auth.clone(),
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
        auth.clone(),
        repo.clone(),
        fs,
        clock,
    ));
    let playback_source_handler = Arc::new(PlaybackSourceHandler::new(
        auth.clone(),
        repo.clone(),
        StdFileStat,
    ));
    let comic_page_handler = Arc::new(ComicPageHandler::new(
        auth.clone(),
        repo.clone(),
        ZipComicArchive,
    ));
    let thumbnail_handler = Arc::new(ThumbnailHandler::new(
        auth.clone(),
        repo.clone(),
        ZipComicArchive,
        ImageThumbnailRenderer,
        DiskThumbnailCache::new(settings.playback.thumbnail_cache_dir.clone()),
        // Beside `audio_tags` (`LoftyAudioMetadataReader`), the reader it
        // sits next to: both are lofty-backed reads of the same tagged
        // file, at two different times for two different reasons (see
        // `CoverArtReader`'s own doc comment).
        LoftyCoverArtReader,
    ));
    let energy_handler = Arc::new(EnergyHandler::new(
        auth.clone(),
        repo.clone(),
        SqliteEnergyStore::new(pool.clone()),
        FfmpegEnergyAnalyzer,
    ));
    let create_collection_handler = Arc::new(CreateCollectionHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let rename_collection_handler = Arc::new(RenameCollectionHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let delete_collection_handler = Arc::new(DeleteCollectionHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let create_bookmark_handler = Arc::new(CreateBookmarkHandler::new(
        auth.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let update_bookmark_handler = Arc::new(UpdateBookmarkHandler::new(
        auth.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let browse_bookmarks_handler = Arc::new(BrowseBookmarksHandler::new(
        auth.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let bookmark_lifecycle_handler = Arc::new(BookmarkLifecycleHandler::new(
        auth.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
        clock,
    ));
    let purge_bookmark_handler = Arc::new(PurgeBookmarkHandler::new(
        auth.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
        clock,
        retention_days,
    ));
    let add_items_to_collection_handler = Arc::new(AddItemsToCollectionHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let remove_item_from_collection_handler = Arc::new(RemoveItemFromCollectionHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let list_collection_items_handler = Arc::new(ListCollectionItemsHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
        repo.clone(),
        SqliteBookmarkRepository::new(pool.clone()),
    ));
    let list_collections_handler = Arc::new(ListCollectionsHandler::new(
        auth.clone(),
        SqliteCollectionRepository::new(pool.clone()),
    ));
    let get_settings_handler = Arc::new(GetSettingsHandler::new(auth.clone(), settings));
    let watchlist_repo = SqliteWatchlistRepository::new(pool.clone());
    let create_watchlist_handler = Arc::new(CreateWatchlistHandler::new(
        auth.clone(),
        watchlist_repo.clone(),
    ));
    let add_video_to_watchlist_handler = Arc::new(AddVideoToWatchlistHandler::new(
        auth.clone(),
        watchlist_repo.clone(),
        repo.clone(),
    ));
    let browse_watchlists_handler = Arc::new(BrowseWatchlistsHandler::new(
        auth.clone(),
        watchlist_repo.clone(),
    ));
    let update_watch_progress_handler = Arc::new(UpdateWatchProgressHandler::new(
        auth.clone(),
        watchlist_repo.clone(),
    ));
    let remove_video_from_watchlist_handler = Arc::new(RemoveVideoFromWatchlistHandler::new(
        auth.clone(),
        watchlist_repo.clone(),
    ));
    let delete_watchlist_handler =
        Arc::new(DeleteWatchlistHandler::new(auth.clone(), watchlist_repo));
    let reading_list_repo = SqliteReadingListRepository::new(pool.clone());
    let create_reading_list_handler = Arc::new(CreateReadingListHandler::new(
        auth.clone(),
        reading_list_repo.clone(),
    ));
    let add_item_to_reading_list_handler = Arc::new(AddItemToReadingListHandler::new(
        auth.clone(),
        reading_list_repo.clone(),
        repo.clone(),
    ));
    let browse_reading_lists_handler = Arc::new(BrowseReadingListsHandler::new(
        auth.clone(),
        reading_list_repo.clone(),
    ));
    let update_reading_progress_handler = Arc::new(UpdateReadingProgressHandler::new(
        auth.clone(),
        reading_list_repo.clone(),
    ));
    let remove_item_from_reading_list_handler = Arc::new(RemoveItemFromReadingListHandler::new(
        auth.clone(),
        reading_list_repo.clone(),
    ));
    let delete_reading_list_handler = Arc::new(DeleteReadingListHandler::new(
        auth.clone(),
        reading_list_repo,
    ));
    let enrichment_repo = SqliteEnrichmentRepository::new(pool.clone());
    let read_enrichment_handler = Arc::new(ReadEnrichmentHandler::new(
        auth.clone(),
        SqliteEnrichmentRepository::new(pool.clone()),
        &settings.metadata.image_cache_dir,
    ));
    // Built only when it can actually run. A client that cannot lawfully
    // send its own User-Agent is one this process should not be holding.
    let enrich_handler = if settings.metadata.is_available() {
        match (
            MusicBrainzClient::new(&settings.metadata.contact),
            CommonsImageClient::new(&settings.metadata.contact),
            LrclibClient::new(&settings.metadata.contact),
        ) {
            (Ok(identity), Ok(images), Ok(lyrics)) => Some(Arc::new(EnrichHandler::new(
                auth.clone(),
                enrichment_repo,
                identity,
                images,
                lyrics,
                FsArtistImageStore::new(&settings.metadata.image_cache_dir),
                clock,
                settings.metadata.clone(),
            ))),
            // A client that will not build is a configuration this process
            // cannot honour; enrichment stays unavailable rather than
            // half-wired, and the reason is logged once here rather than
            // rediscovered on every call.
            _ => {
                tracing::warn!(
                    "music enrichment is enabled but its service clients could not be built; \
                     it will report as unavailable"
                );
                None
            }
        }
    } else {
        None
    };

    let list_libraries_handler = Arc::new(ListLibrariesHandler::new(
        auth.clone(),
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let register_library_handler = Arc::new(RegisterLibraryHandler::new(
        auth.clone(),
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let remove_library_handler = Arc::new(RemoveLibraryHandler::new(
        auth.clone(),
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let move_library_handler = Arc::new(MoveLibraryHandler::new(
        auth.clone(),
        SqliteLibraryRepository::new(pool.clone()),
    ));
    let browse_library_handler = Arc::new(BrowseLibraryHandler::new(
        auth.clone(),
        SqliteLibraryRepository::new(pool.clone()),
        SqliteCatalogRepository::new(pool.clone()),
    ));

    let playlist_repo = SqlitePlaylistRepository::new(pool.clone());
    let create_playlist_handler = Arc::new(CreatePlaylistHandler::new(
        auth.clone(),
        playlist_repo.clone(),
    ));
    let rename_playlist_handler = Arc::new(RenamePlaylistHandler::new(
        auth.clone(),
        playlist_repo.clone(),
    ));
    let delete_playlist_handler = Arc::new(DeletePlaylistHandler::new(
        auth.clone(),
        playlist_repo.clone(),
    ));
    let add_entries_handler = Arc::new(AddEntriesHandler::new(auth.clone(), playlist_repo.clone()));
    let remove_entry_handler =
        Arc::new(RemoveEntryHandler::new(auth.clone(), playlist_repo.clone()));
    let reorder_playlist_handler = Arc::new(ReorderPlaylistHandler::new(
        auth.clone(),
        playlist_repo.clone(),
    ));
    let browse_playlists_handler =
        Arc::new(BrowsePlaylistsHandler::new(auth.clone(), playlist_repo));

    let play_repo = SqlitePlayRepository::new(pool.clone());
    let record_play_handler = Arc::new(RecordPlayHandler::new(
        auth.clone(),
        clock,
        play_repo.clone(),
    ));
    let music_stats_handler = Arc::new(MusicStatsHandler::new(auth.clone(), play_repo));
    let set_local_credentials_handler = Arc::new(SetLocalCredentialsHandler::new(
        auth.clone(),
        credential_repo.clone(),
        clock,
        settings.auth.mode,
    ));
    let local_login_handler = Arc::new(LocalLoginHandler::new(
        credential_repo.clone(),
        session_repo.clone(),
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
    ));
    let windows_login_handler = Arc::new(WindowsLoginHandler::new(
        session_repo.clone(),
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
    ));
    let register_local_account_handler = Arc::new(RegisterLocalAccountHandler::new(
        credential_repo.clone(),
        session_repo.clone(),
        recovery_code_repo.clone(),
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
    ));
    let get_local_account_handler = Arc::new(GetLocalAccountHandler::new(
        auth.clone(),
        credential_repo.clone(),
        recovery_code_repo.clone(),
        settings.auth.mode,
    ));
    let redeem_recovery_code_handler = Arc::new(RedeemRecoveryCodeHandler::new(
        credential_repo.clone(),
        session_repo,
        recovery_code_repo.clone(),
        clock,
        settings.auth.mode,
    ));
    let regenerate_recovery_codes_handler = Arc::new(RegenerateRecoveryCodesHandler::new(
        auth.clone(),
        credential_repo,
        recovery_code_repo,
        clock,
        settings.auth.mode,
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
        energy_handler,
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
        enrich_handler,
        read_enrichment_handler,
        register_library_handler,
        remove_library_handler,
        move_library_handler,
        browse_library_handler,
        list_libraries_handler,
        create_playlist_handler,
        rename_playlist_handler,
        delete_playlist_handler,
        add_entries_handler,
        remove_entry_handler,
        reorder_playlist_handler,
        browse_playlists_handler,
        record_play_handler,
        music_stats_handler,
        set_local_credentials_handler,
        local_login_handler,
        windows_login_handler,
        register_local_account_handler,
        get_local_account_handler,
        redeem_recovery_code_handler,
        regenerate_recovery_codes_handler,
        auth,
        run_registry,
        pool,
    }
}
