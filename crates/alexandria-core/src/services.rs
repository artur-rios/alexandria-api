use std::sync::Arc;

use sqlx::sqlite::SqlitePool;

use crate::auth::commands::account_status::GetLocalAccountHandler;
use crate::auth::commands::complete_password_reset::CompletePasswordResetHandler;
use crate::auth::commands::confirm_email::ConfirmEmailHandler;
use crate::auth::commands::login::LocalLoginHandler;
use crate::auth::commands::register::RegisterLocalAccountHandler;
use crate::auth::commands::request_password_reset::RequestPasswordResetHandler;
use crate::auth::commands::resend_confirmation::ResendConfirmationHandler;
use crate::auth::commands::set_credentials::SetLocalCredentialsHandler;
use crate::auth::external::{ExternalAuthService, HttpJwksProvider};
use crate::auth::local::{
    LocalAuthService, SqliteLocalCredentialRepository, SqliteSessionRepository,
};
use crate::auth::mail::{RuntimeMailSender, UnconfiguredMailSender};
use crate::auth::tokens::SqliteAuthTokenRepository;
use crate::auth::RuntimeAuthService;
use crate::bookmarks::commands::create::CreateBookmarkHandler;
use crate::bookmarks::commands::lifecycle::BookmarkLifecycleHandler;
use crate::bookmarks::commands::purge::PurgeBookmarkHandler;
use crate::bookmarks::commands::update::UpdateBookmarkHandler;
use crate::bookmarks::queries::browse::BrowseBookmarksHandler;
use crate::bookmarks::repos::SqliteBookmarkRepository;
use crate::catalog::audio_tags::LoftyAudioMetadataReader;
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
use crate::catalog::commands::soft_delete::SoftDeleteFileHandler;
use crate::catalog::document_tags::PdfEpubMetadataReader;
use crate::catalog::fs::StdFilesystem;
use crate::catalog::image_tags::ExifImageMetadataReader;
use crate::catalog::queries::browse::BrowseFilesHandler;
use crate::catalog::queries::read_content::ReadTextFileContentHandler;
use crate::catalog::queries::run_status::GetRunStatusHandler;
use crate::catalog::repos::SqliteCatalogRepository;
use crate::catalog::runs::{CatalogRunRepository, SqliteCatalogRunRepository};
use crate::catalog::video_tags::FfmpegVideoMetadataReader;
use crate::collections::commands::add_items::AddItemsToCollectionHandler;
use crate::collections::commands::create::CreateCollectionHandler;
use crate::collections::commands::delete::DeleteCollectionHandler;
use crate::collections::commands::remove_item::RemoveItemFromCollectionHandler;
use crate::collections::commands::rename::RenameCollectionHandler;
use crate::collections::queries::list_items::ListCollectionItemsHandler;
use crate::collections::repos::SqliteCollectionRepository;
use crate::config::Settings;
use crate::config::{AuthMode, MailProvider};
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
    GetRunStatusHandler<RuntimeAuthService, SqliteCatalogRunRepository>;

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

pub type DefaultThumbnailHandler = ThumbnailHandler<
    RuntimeAuthService,
    SqliteCatalogRepository,
    ZipComicArchive,
    ImageThumbnailRenderer,
    DiskThumbnailCache,
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

pub type DefaultSetLocalCredentialsHandler =
    SetLocalCredentialsHandler<RuntimeAuthService, SqliteLocalCredentialRepository, SystemClock>;

pub type DefaultLocalLoginHandler =
    LocalLoginHandler<SqliteLocalCredentialRepository, SqliteSessionRepository, SystemClock>;

pub type DefaultRegisterLocalAccountHandler = RegisterLocalAccountHandler<
    SqliteLocalCredentialRepository,
    SqliteSessionRepository,
    SqliteAuthTokenRepository,
    RuntimeMailSender,
    SystemClock,
>;

pub type DefaultGetLocalAccountHandler =
    GetLocalAccountHandler<RuntimeAuthService, SqliteLocalCredentialRepository>;

pub type DefaultConfirmEmailHandler =
    ConfirmEmailHandler<SqliteLocalCredentialRepository, SqliteAuthTokenRepository, SystemClock>;

pub type DefaultResendConfirmationHandler = ResendConfirmationHandler<
    RuntimeAuthService,
    SqliteLocalCredentialRepository,
    SqliteAuthTokenRepository,
    RuntimeMailSender,
    SystemClock,
>;

pub type DefaultRequestPasswordResetHandler = RequestPasswordResetHandler<
    SqliteLocalCredentialRepository,
    SqliteAuthTokenRepository,
    RuntimeMailSender,
    SystemClock,
>;

pub type DefaultCompletePasswordResetHandler = CompletePasswordResetHandler<
    SqliteLocalCredentialRepository,
    SqliteSessionRepository,
    SqliteAuthTokenRepository,
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
    pub set_local_credentials_handler: Arc<DefaultSetLocalCredentialsHandler>,
    pub local_login_handler: Arc<DefaultLocalLoginHandler>,
    pub register_local_account_handler: Arc<DefaultRegisterLocalAccountHandler>,
    pub get_local_account_handler: Arc<DefaultGetLocalAccountHandler>,
    pub confirm_email_handler: Arc<DefaultConfirmEmailHandler>,
    pub resend_confirmation_handler: Arc<DefaultResendConfirmationHandler>,
    pub request_password_reset_handler: Arc<DefaultRequestPasswordResetHandler>,
    pub complete_password_reset_handler: Arc<DefaultCompletePasswordResetHandler>,
    /// The same auth service the handlers hold, exposed so a transport can
    /// reject an unauthenticated caller *before* it parses a request body or
    /// path (FR-AU-07 / SRD §7). Handlers still authenticate independently —
    /// this is the transport gate, not a replacement for the domain check.
    pub auth: RuntimeAuthService,
    pub pool: SqlitePool,
}

/// Build the shared services from the loaded settings and a connected SQLite
/// pool. Callers (the HTTP server binary, integration tests) are responsible
/// for running migrations first.
pub async fn build_services(settings: &Settings, pool: SqlitePool) -> Services {
    let retention_days = settings.deletion.retention_days;
    let repo = SqliteCatalogRepository::new(pool.clone());
    let run_repo = SqliteCatalogRunRepository::new(pool.clone());
    let session_repo = SqliteSessionRepository::new(pool.clone());
    let credential_repo = SqliteLocalCredentialRepository::new(pool.clone());
    let token_repo = SqliteAuthTokenRepository::new(pool.clone());
    // Issue #102: the only provider today refuses every send and says why.
    // The external mail service becomes a second variant here, with no change
    // to any handler.
    let mail = match settings.mail.provider {
        MailProvider::None => RuntimeMailSender::Unconfigured(UnconfiguredMailSender),
    };
    let fs = StdFilesystem;
    let clock = SystemClock;
    // FR-FC-29: any run still recorded as `running` belongs to a process that
    // is gone — runs execute in-process and are never resumed. Reconcile them
    // now, so a client polling one gets a terminal answer instead of waiting
    // forever. A failure here must not stop startup: the catalog is still
    // fully usable, and the stale rows are reconciled on the next boot.
    match run_repo.interrupt_running(clock.now()).await {
        Ok(0) => {}
        Ok(reconciled) => {
            tracing::info!(
                reconciled,
                "marked interrupted runs left by a previous process"
            )
        }
        Err(err) => tracing::warn!(error = %err, "could not reconcile interrupted runs"),
    }
    // FR-AU-01/FR-AU-03: exactly one auth mode is active, selected once here
    // from startup configuration.
    let auth = match settings.auth.mode {
        AuthMode::Local => {
            RuntimeAuthService::Local(LocalAuthService::new(session_repo.clone(), clock))
        }
        AuthMode::External => RuntimeAuthService::External(ExternalAuthService::new(
            HttpJwksProvider::new(settings.auth.jwks_url.clone()),
        )),
    };
    let audio_tags = LoftyAudioMetadataReader;
    let image_tags = ExifImageMetadataReader;
    let document_tags = PdfEpubMetadataReader;
    let video_tags = FfmpegVideoMetadataReader;
    let comic_tags = CbzComicMetadataReader;
    // UC-01 and UC-02 are the same hash-every-file workload, so both walks
    // take the same `indexing.concurrency` bound.
    let indexing_concurrency = settings.indexing.concurrency;
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
        settings.filesystem.root.clone(),
        run_repo.clone(),
    ));
    let refresh_handler = Arc::new(RefreshHandler::new(
        auth.clone(),
        repo.clone(),
        fs,
        clock,
        indexing_concurrency,
        run_repo.clone(),
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
    let get_run_status_handler = Arc::new(GetRunStatusHandler::new(auth.clone(), run_repo.clone()));
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
    let register_local_account_handler = Arc::new(RegisterLocalAccountHandler::new(
        credential_repo.clone(),
        session_repo.clone(),
        token_repo.clone(),
        mail,
        clock,
        settings.auth.mode,
        settings.auth.session_ttl_hours,
        settings.auth.confirmation_ttl_hours,
    ));
    let get_local_account_handler = Arc::new(GetLocalAccountHandler::new(
        auth.clone(),
        credential_repo.clone(),
    ));
    let confirm_email_handler = Arc::new(ConfirmEmailHandler::new(
        credential_repo.clone(),
        token_repo.clone(),
        clock,
        settings.auth.mode,
    ));
    let resend_confirmation_handler = Arc::new(ResendConfirmationHandler::new(
        auth.clone(),
        credential_repo.clone(),
        token_repo.clone(),
        mail,
        clock,
        settings.auth.mode,
        settings.auth.confirmation_ttl_hours,
        settings.auth.resend_interval_seconds,
    ));
    let request_password_reset_handler = Arc::new(RequestPasswordResetHandler::new(
        credential_repo.clone(),
        token_repo.clone(),
        mail,
        clock,
        settings.auth.mode,
        settings.auth.password_reset_ttl_minutes,
    ));
    let complete_password_reset_handler = Arc::new(CompletePasswordResetHandler::new(
        credential_repo,
        session_repo,
        token_repo,
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
        set_local_credentials_handler,
        local_login_handler,
        register_local_account_handler,
        get_local_account_handler,
        confirm_email_handler,
        resend_confirmation_handler,
        request_password_reset_handler,
        complete_password_reset_handler,
        auth,
        pool,
    }
}
