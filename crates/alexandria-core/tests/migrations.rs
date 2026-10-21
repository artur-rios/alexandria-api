use alexandria_core::migrate::run_migrations;
use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn given_fresh_in_memory_db_when_migrate_then_app_meta_table_exists() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");

    run_migrations(&pool).await.expect("migrate");

    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM app_meta")
        .fetch_one(&pool)
        .await
        .expect("query");

    assert_eq!(row.0, 0);
    pool.close().await;
}

/// UC-10 — the collections migration creates the table the create-collection
/// handler persists into, with the `kind` discriminator constrained to the two
/// values the domain enum can represent (SRD §4.3).
#[tokio::test]
async fn given_fresh_in_memory_db_when_migrate_then_collections_table_exists() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");

    run_migrations(&pool).await.expect("migrate");

    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collections")
        .fetch_one(&pool)
        .await
        .expect("query");
    assert_eq!(row.0, 0);

    // The CHECK constraint rejects a `kind` outside the enum, so no write can
    // leave behind a discriminator the domain type cannot represent.
    let bad = sqlx::query("INSERT INTO collections (uuid, name, kind) VALUES (?, ?, ?)")
        .bind(uuid::Uuid::new_v4().to_string())
        .bind("mixed")
        .bind("gallery")
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "kind outside ('file','bookmark') is rejected");

    pool.close().await;
}

/// The connection settings `migrate_database` establishes are load-bearing,
/// and both were previously assumed wrong in comments across this crate.
///
/// `journal_mode = wal` is what lets reads proceed while an indexing run
/// writes (FR-FC-08); sqlx does not set it, so it is our choice and a
/// regression here would silently reintroduce whole-database write locks.
///
/// `foreign_keys = 1` is set by sqlx, not by us — which means the subtype
/// tables' `ON DELETE CASCADE` is live, while the tables that declare no
/// foreign key (`watch_progress`, `reading_progress`, both `collection_id`
/// columns) get no cleanup for free. The whole codebase assumed the opposite
/// until this was measured, so the behaviour is pinned here rather than
/// described in prose.
#[tokio::test]
async fn given_migrated_database_when_connected_then_wal_and_foreign_keys_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pragmas.sqlite");
    let pool = alexandria_core::migrate::migrate_database(path.to_str().expect("utf-8 path"))
        .await
        .expect("migrate");

    let (journal_mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .expect("journal_mode");
    assert_eq!(
        journal_mode.to_ascii_lowercase(),
        "wal",
        "reads must not block behind an indexing write (FR-FC-08)"
    );

    let (foreign_keys,): (i64,) = sqlx::query_as("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .expect("foreign_keys");
    assert_eq!(
        foreign_keys, 1,
        "sqlx enables foreign keys; the subtype ON DELETE CASCADE is live"
    );

    pool.close().await;
}

/// Migration 14 is the one irreversible step on this branch: it drops
/// `local_login_credentials.email_confirmed_at`, which SQLite implements as a
/// rewrite of the whole table — and that table holds the owner's only
/// credential row. Every other test in this file migrates an empty database,
/// where a table rewrite has nothing to lose. This one exercises the upgrade
/// path the migration exists for.
///
/// The subset is applied through sqlx's own `Migrator`, read from the same
/// `./migrations` directory the `migrate!` macro embeds, so the checksums
/// recorded here are the ones `run_migrations` verifies afterwards. Applying
/// 0 … 13 by hand and then letting `run_migrations` apply only 14 is what
/// makes this a real upgrade rather than a fresh install.
#[tokio::test]
async fn given_a_populated_pre_14_database_when_migrated_then_the_credential_row_survives() {
    use sqlx::migrate::{Migrate, Migrator};

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");

    let migrator = Migrator::new(std::path::Path::new("./migrations"))
        .await
        .expect("read migrations");

    let mut conn = pool.acquire().await.expect("acquire");
    conn.ensure_migrations_table(&migrator.table_name)
        .await
        .expect("meta table");
    for migration in migrator.iter().filter(|m| m.version <= 13) {
        conn.apply(&migrator.table_name, migration)
            .await
            .expect("apply");
    }
    drop(conn);

    // The state an install that predates this branch is actually in: one
    // credential row, unconfirmed (nothing could ever confirm it, since the
    // mail provider only ever had a `None` variant), and an undelivered
    // token beside it.
    sqlx::query(
        "INSERT INTO local_login_credentials (id, email, password_hash, updated_at, email_confirmed_at)
         VALUES (1, ?, ?, ?, NULL)",
    )
    .bind("owner@example.com")
    .bind("$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$0000000000000000000000000000000000000000000")
    .bind("2026-01-01T00:00:00Z")
    .execute(&pool)
    .await
    .expect("seed credential");

    sqlx::query(
        "INSERT INTO auth_tokens (purpose, token_hash, email, created_at, expires_at)
         VALUES ('email_confirmation', 'deadbeef', ?, ?, ?)",
    )
    .bind("owner@example.com")
    .bind("2026-01-01T00:00:00Z")
    .bind("2026-01-02T00:00:00Z")
    .execute(&pool)
    .await
    .expect("seed token");

    // Only migration 14 is outstanding.
    run_migrations(&pool).await.expect("migrate to head");

    let (email, password_hash, updated_at): (String, String, String) =
        sqlx::query_as("SELECT email, password_hash, updated_at FROM local_login_credentials")
            .fetch_one(&pool)
            .await
            .expect("the credential row survives the table rewrite");
    assert_eq!(email, "owner@example.com");
    assert!(
        password_hash.starts_with("$argon2id$"),
        "the owner must still be able to log in with the password they had"
    );
    assert_eq!(updated_at, "2026-01-01T00:00:00Z");

    // The dropped column is gone and the dropped table with it, so the
    // rewrite did what it was for and not merely nothing.
    assert!(
        sqlx::query("SELECT email_confirmed_at FROM local_login_credentials")
            .fetch_optional(&pool)
            .await
            .is_err(),
        "email_confirmed_at must be dropped"
    );
    assert!(
        sqlx::query("SELECT 1 FROM auth_tokens")
            .fetch_optional(&pool)
            .await
            .is_err(),
        "auth_tokens must be dropped"
    );

    pool.close().await;
}

/// The database path is a file name, not a URL. It used to be spliced into
/// `sqlite://{path}?mode=rwc`, which sqlx splits on the first `?` and
/// percent-decodes — so `a%41b.sqlite` opened `aAb.sqlite`, and a folder named
/// with a `?` lost everything after it. The FFI embedder passes its path
/// through verbatim, so either spelling is one an owner's data folder can have.
#[tokio::test]
async fn given_a_path_with_url_metacharacters_when_migrated_then_that_exact_file_is_used() {
    let dir = tempfile::tempdir().expect("tempdir");
    let folder = dir.path().join("what?");
    std::fs::create_dir(&folder).expect("folder with a question mark");
    let path = folder.join("a%41b.sqlite");

    let pool = alexandria_core::migrate::migrate_database(path.to_str().expect("utf-8 path"))
        .await
        .expect("migrate");
    pool.close().await;

    assert!(path.exists(), "the database was not created at {path:?}");
    assert!(
        !folder.join("aAb.sqlite").exists(),
        "the path was percent-decoded"
    );
}

// ---------------------------------------------------------------------------
// Migration 25: removing stored music.
//
// Every test below upgrades a populated version-24 database, the state a real
// install that held music is in, rather than migrating an empty one: a table
// rebuild and a scope rewrite have nothing to lose on an empty database.
// ---------------------------------------------------------------------------

const RUN_AUDIO_ONLY: &str = "00000000-0000-0000-0000-000000000001";
const RUN_AUDIO_IMAGE: &str = "00000000-0000-0000-0000-000000000002";
const RUN_IMAGE_AUDIO_VIDEO: &str = "00000000-0000-0000-0000-000000000003";
const RUN_VIDEO: &str = "00000000-0000-0000-0000-000000000004";
const RUN_EVERY_TYPE: &str = "00000000-0000-0000-0000-000000000005";

/// The ids the seed gives its files. The two audio files sit on either side
/// of a surviving one, and the higher one is not the high-water mark either:
/// `seed_music_library` also inserts and purges file 9, so the
/// `AUTOINCREMENT` sequence stands above every id still in the table.
const VIDEO_ID: i64 = 1;
const AUDIO_ID: i64 = 2;
const TEXT_ID: i64 = 3;
const DOCUMENT_ID: i64 = 4;
const SECOND_AUDIO_ID: i64 = 5;
const PURGED_FILE_ID: i64 = 9;

/// A single-connection in-memory pool migrated to version 24 — the last
/// version before music was removed — through sqlx's own `Migrator`, so the
/// checksums it records are the ones `run_migrations` verifies afterwards.
/// One connection, so every query in a test (the `PRAGMA` reads included)
/// runs on the connection the migration ran on.
async fn pool_at_version_24() -> sqlx::SqlitePool {
    use sqlx::migrate::{Migrate, Migrator};

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");

    let migrator = Migrator::new(std::path::Path::new("./migrations"))
        .await
        .expect("read migrations");

    let mut conn = pool.acquire().await.expect("acquire");
    conn.ensure_migrations_table(&migrator.table_name)
        .await
        .expect("meta table");
    for migration in migrator.iter().filter(|m| m.version <= 24) {
        conn.apply(&migrator.table_name, migration)
            .await
            .expect("apply");
    }
    drop(conn);

    pool
}

/// A library that held music: two audio files with every kind of row music
/// owned, a video with watch progress, a text file, a document with reading
/// progress, and index runs whose scopes name audio alone, alongside other
/// types, and not at all.
///
/// The audio file also carries a stray row in each of the six surviving
/// subtype tables and a progress row in each progress table. No code path
/// writes those, but the migration deletes them explicitly — foreign keys are
/// off while it runs, so nothing would cascade — and this is what proves it.
async fn seed_music_library(pool: &sqlx::SqlitePool) {
    sqlx::raw_sql(
        "INSERT INTO libraries (id, uuid, name, root_path)
              VALUES (1, 'library-uuid', 'Films', '/media/films');
         INSERT INTO collections (id, uuid, name, kind)
              VALUES (1, 'collection-uuid', 'Favourites', 'file');

         INSERT INTO files (id, uuid, path, name, type, content_hash, size_bytes, mtime,
                            state, deleted_at, indexed_at, collection_id, missing_at,
                            library_id, metadata_version)
         VALUES
           (1, 'video-uuid', '/media/films/film.mkv', 'film.mkv', 'video', 'hash-v', 1000,
            '2026-01-01T00:00:00Z', 'active', NULL, '2026-01-02T00:00:00Z', 1, NULL, 1, 3),
           (2, 'audio-uuid', '/music/song.mp3', 'song.mp3', 'audio', 'hash-a', 2000,
            '2026-01-01T00:00:00Z', 'active', NULL, '2026-01-02T00:00:00Z', 1, NULL, NULL, 2),
           (3, 'text-uuid', '/notes/readme.txt', 'readme.txt', 'text', NULL, 20,
            '2026-02-01T00:00:00Z', 'deleted', '2026-03-01T00:00:00Z', '2026-02-02T00:00:00Z',
            NULL, '2026-03-02T00:00:00Z', NULL, 0),
           (4, 'document-uuid', '/books/book.pdf', 'book.pdf', 'document', 'hash-d', 3000,
            '2026-01-05T00:00:00Z', 'active', NULL, '2026-01-06T00:00:00Z', NULL, NULL, NULL, 1),
           (5, 'second-audio-uuid', '/music/other.flac', 'other.flac', 'audio', NULL, 4000,
            '2026-01-01T00:00:00Z', 'active', NULL, '2026-01-02T00:00:00Z', NULL, NULL, NULL, 0);

         -- Raise the AUTOINCREMENT high-water mark above every surviving id:
         -- file 9 existed once and was purged.
         INSERT INTO files (id, uuid, path, name, type, indexed_at)
              VALUES (9, 'purged-uuid', '/pics/gone.png', 'gone.png', 'image', '2026-01-01T00:00:00Z');
         DELETE FROM files WHERE id = 9;

         INSERT INTO audio_files (file_id, title, artist, album, year, genre, track,
                                  album_artist, duration_seconds)
         VALUES (2, 'Song', 'Artist', 'Album', 2020, 'Rock', 1, 'Artist', 180.5),
                (5, 'Other', 'Artist', 'Album', 2020, 'Rock', 2, NULL, NULL);
         INSERT INTO video_files (file_id, title, year, resolution, media_kind, episode_count,
                                  duration_seconds)
         VALUES (1, 'Film', 2019, '1080p', 'movie', NULL, 5400.0),
                (2, NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO text_files (file_id) VALUES (3), (2);
         INSERT INTO documents (file_id, title, author, year, format_kind, page_count)
         VALUES (4, 'Book', 'Author', 2001, 'ebook', 300),
                (2, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO html_pages (file_id) VALUES (2);
         INSERT INTO comic_books (file_id) VALUES (2);
         INSERT INTO images (file_id) VALUES (2);

         INSERT INTO playlists (id, uuid, name) VALUES (1, 'playlist-uuid', 'Mix');
         INSERT INTO playlist_entries (id, uuid, playlist_id, file_id, position)
              VALUES (1, 'entry-uuid', 1, 2, 0), (2, 'entry-2-uuid', 1, 5, 1);
         INSERT INTO play_events (file_id, played_at) VALUES (2, '2026-04-01T00:00:00Z');
         INSERT INTO track_lyrics (file_id, plain, outcome, fetched_at)
              VALUES (2, 'la la la', 'found', '2026-04-01T00:00:00Z');
         INSERT INTO track_energy (file_id, bands, frame_ms, version, levels, computed_at)
              VALUES (2, 16, 100, 1, X'00FF', '2026-04-01T00:00:00Z');
         INSERT INTO artist_images (artist_name, outcome, fetched_at)
              VALUES ('Artist', 'not_found', '2026-04-01T00:00:00Z');

         INSERT INTO watchlists (id, uuid, name) VALUES (1, 'watchlist-uuid', 'Queue');
         INSERT INTO watch_progress (id, watchlist_id, video_file_id, state, current_episode,
                                     total_episodes)
         VALUES (1, 1, 1, 'watching', 2, 10),
                (2, 1, 2, 'pending', NULL, NULL);
         INSERT INTO reading_lists (id, uuid, name) VALUES (1, 'reading-list-uuid', 'Shelf');
         INSERT INTO reading_progress (id, reading_list_id, item_file_id, target_kind, state,
                                       current_issue, total_issues)
         VALUES (1, 1, 4, 'document', 'reading', NULL, NULL),
                (2, 1, 5, 'document', 'pending', NULL, NULL);

         INSERT INTO catalog_runs (id, kind, status, root, started_at, paused_at, scope)
         VALUES
           ('00000000-0000-0000-0000-000000000001', 'index', 'paused', '/music',
            '2026-05-01T00:00:00+00:00', '2026-05-01T00:10:00+00:00', 'audio'),
           ('00000000-0000-0000-0000-000000000002', 'index', 'paused', '/media',
            '2026-05-02T00:00:00+00:00', '2026-05-02T00:10:00+00:00', 'audio,image'),
           ('00000000-0000-0000-0000-000000000003', 'index', 'paused', '/media',
            '2026-05-03T00:00:00+00:00', '2026-05-03T00:10:00+00:00', 'image,audio,video'),
           ('00000000-0000-0000-0000-000000000004', 'index', 'complete', '/media',
            '2026-05-04T00:00:00+00:00', NULL, 'video'),
           ('00000000-0000-0000-0000-000000000005', 'index', 'complete', '/media',
            '2026-05-05T00:00:00+00:00', NULL, NULL);
         INSERT INTO catalog_run_failures (id, run_id, path, reason, failed_at)
         VALUES (1, '00000000-0000-0000-0000-000000000001', '/music/bad.mp3', 'unreadable',
                 '2026-05-01T00:05:00+00:00'),
                (2, '00000000-0000-0000-0000-000000000002', '/media/bad.png', 'unreadable',
                 '2026-05-02T00:05:00+00:00');",
    )
    .execute(pool)
    .await
    .expect("seed a library holding music");
}

/// Every column of a `files` row, in declaration order.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct FileRow {
    id: i64,
    uuid: String,
    path: String,
    name: String,
    r#type: String,
    content_hash: Option<String>,
    size_bytes: Option<i64>,
    mtime: Option<String>,
    state: String,
    deleted_at: Option<String>,
    indexed_at: String,
    collection_id: Option<i64>,
    missing_at: Option<String>,
    library_id: Option<i64>,
    metadata_version: i64,
}

async fn file_rows(pool: &sqlx::SqlitePool, filter: &'static str) -> Vec<FileRow> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, uuid, path, name, type, content_hash, size_bytes, mtime, state, deleted_at,
                indexed_at, collection_id, missing_at, library_id, metadata_version
           FROM files WHERE {filter} ORDER BY id"
    )))
    .fetch_all(pool)
    .await
    .expect("read files")
}

/// The shape of `files` a rebuild must reproduce: every column's name, type,
/// nullability, default and key position.
async fn files_columns(pool: &sqlx::SqlitePool) -> Vec<(String, String, i64, Option<String>, i64)> {
    sqlx::query_as(
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('files') ORDER BY cid",
    )
    .fetch_all(pool)
    .await
    .expect("table_info")
}

/// Every index on `files` — the named ones and the two `UNIQUE` columns'
/// automatic ones — with whether it is unique and which column it covers.
async fn files_indexes(pool: &sqlx::SqlitePool) -> Vec<(String, i64, String, String)> {
    sqlx::query_as(
        "SELECT il.name, il.\"unique\", il.origin, ii.name
           FROM pragma_index_list('files') AS il, pragma_index_info(il.name) AS ii
          ORDER BY il.name, ii.seqno",
    )
    .fetch_all(pool)
    .await
    .expect("index_list")
}

async fn ids(pool: &sqlx::SqlitePool, sql: &'static str) -> Vec<i64> {
    sqlx::query_scalar(sql)
        .fetch_all(pool)
        .await
        .expect("read ids")
}

async fn foreign_keys_pragma(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await
        .expect("foreign_keys")
}

#[tokio::test]
async fn given_a_database_with_music_when_migrated_then_music_is_gone_and_everything_else_survives()
{
    let pool = pool_at_version_24().await;
    seed_music_library(&pool).await;
    let surviving_before = file_rows(&pool, "type <> 'audio'").await;
    let columns_before = files_columns(&pool).await;
    let indexes_before = files_indexes(&pool).await;

    run_migrations(&pool).await.expect("migrate to head");

    // Only the audio files are gone, and every other file is exactly the row
    // it was, id included.
    assert_eq!(
        ids(&pool, "SELECT id FROM files ORDER BY id").await,
        vec![VIDEO_ID, TEXT_ID, DOCUMENT_ID]
    );
    assert_eq!(file_rows(&pool, "1 = 1").await, surviving_before);

    // The rebuilt table has exactly the shape the old one had.
    assert_eq!(files_columns(&pool).await, columns_before);
    assert_eq!(files_indexes(&pool).await, indexes_before);

    // Subtype rows: the surviving files keep theirs; the audio file's stray
    // ones are gone.
    let video: Vec<(i64, Option<String>, Option<f64>)> =
        sqlx::query_as("SELECT file_id, title, duration_seconds FROM video_files")
            .fetch_all(&pool)
            .await
            .expect("video_files");
    assert_eq!(
        video,
        vec![(VIDEO_ID, Some("Film".to_owned()), Some(5400.0))]
    );
    assert_eq!(
        ids(&pool, "SELECT file_id FROM text_files").await,
        vec![TEXT_ID]
    );
    let documents: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT file_id, title FROM documents")
            .fetch_all(&pool)
            .await
            .expect("documents");
    assert_eq!(documents, vec![(DOCUMENT_ID, Some("Book".to_owned()))]);
    assert!(ids(&pool, "SELECT file_id FROM html_pages")
        .await
        .is_empty());
    assert!(ids(&pool, "SELECT file_id FROM comic_books")
        .await
        .is_empty());
    assert!(ids(&pool, "SELECT file_id FROM images").await.is_empty());

    // Progress: the video's and the document's survive untouched; the rows
    // pointing at audio files are gone.
    type WatchRow = (i64, i64, String, Option<i64>, Option<i64>);
    let watch: Vec<WatchRow> = sqlx::query_as(
        "SELECT id, video_file_id, state, current_episode, total_episodes FROM watch_progress",
    )
    .fetch_all(&pool)
    .await
    .expect("watch_progress");
    assert_eq!(
        watch,
        vec![(1, VIDEO_ID, "watching".to_owned(), Some(2), Some(10))]
    );
    let reading: Vec<(i64, i64, String)> =
        sqlx::query_as("SELECT id, item_file_id, state FROM reading_progress")
            .fetch_all(&pool)
            .await
            .expect("reading_progress");
    assert_eq!(reading, vec![(1, DOCUMENT_ID, "reading".to_owned())]);

    // The music tables are gone.
    for table in [
        "audio_files",
        "playlists",
        "playlist_entries",
        "track_lyrics",
        "track_energy",
        "play_events",
        "artist_images",
    ] {
        let found: Option<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
                .bind(table)
                .fetch_optional(&pool)
                .await
                .expect("sqlite_master");
        assert_eq!(found, None, "{table} must be dropped");
    }

    // Runs: the audio-only run and its failure are gone; the mixed ones lose
    // audio and keep the rest in order; the others are untouched.
    let runs: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, scope FROM catalog_runs ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("catalog_runs");
    assert_eq!(
        runs,
        vec![
            (RUN_AUDIO_IMAGE.to_owned(), Some("image".to_owned())),
            (
                RUN_IMAGE_AUDIO_VIDEO.to_owned(),
                Some("image,video".to_owned())
            ),
            (RUN_VIDEO.to_owned(), Some("video".to_owned())),
            (RUN_EVERY_TYPE.to_owned(), None),
        ]
    );
    let failures: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, run_id FROM catalog_run_failures ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("catalog_run_failures");
    assert_eq!(failures, vec![(2, RUN_AUDIO_IMAGE.to_owned())]);

    // Referential integrity holds, and the connection the migration ran on
    // has foreign keys back on.
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .expect("foreign_key_check");
    assert!(violations.is_empty(), "foreign_key_check must be empty");
    assert_eq!(
        foreign_keys_pragma(&pool).await,
        1,
        "the migration must turn foreign keys back on"
    );

    // The type CHECK no longer admits audio.
    let audio = sqlx::query(
        "INSERT INTO files (uuid, path, name, type, indexed_at)
         VALUES ('new-audio-uuid', '/music/new.mp3', 'new.mp3', 'audio', '2026-06-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .expect_err("an audio row must be refused");
    assert!(
        audio.to_string().contains("CHECK constraint failed"),
        "refused by the type CHECK, not something else: {audio}"
    );

    // The migration was recorded even though it ran outside sqlx's
    // transaction, so the next start does not run it again.
    let success: bool =
        sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = 25")
            .fetch_one(&pool)
            .await
            .expect("migration 25 is recorded");
    assert!(success);
    run_migrations(&pool)
        .await
        .expect("a second start is a no-op");

    // The subtype tables' foreign keys follow the rebuilt table: deleting a
    // surviving file still cascades to its subtype row.
    sqlx::query("DELETE FROM files WHERE id = ?")
        .bind(VIDEO_ID)
        .execute(&pool)
        .await
        .expect("delete the video");
    assert!(ids(&pool, "SELECT file_id FROM video_files")
        .await
        .is_empty());

    pool.close().await;
}

#[tokio::test]
async fn given_a_migrated_database_with_mixed_scope_runs_when_runs_are_listed_then_none_is_corrupt()
{
    use alexandria_core::catalog::index_scope::IndexScope;
    use alexandria_core::catalog::model::FileType;
    use alexandria_core::catalog::runs::{CatalogRunRepository, SqliteCatalogRunRepository};
    use uuid::Uuid;

    let pool = pool_at_version_24().await;
    seed_music_library(&pool).await;

    run_migrations(&pool).await.expect("migrate to head");

    let runs = SqliteCatalogRunRepository::new(pool.clone());
    let audio_image = Uuid::parse_str(RUN_AUDIO_IMAGE).expect("uuid");
    let image_audio_video = Uuid::parse_str(RUN_IMAGE_AUDIO_VIDEO).expect("uuid");

    let first = runs
        .get(audio_image)
        .await
        .expect("an 'audio,image' run reads back")
        .expect("and still exists");
    assert_eq!(
        first.scope,
        IndexScope::parse([FileType::Image.as_str()]).expect("scope")
    );

    let second = runs
        .get(image_audio_video)
        .await
        .expect("an 'image,audio,video' run reads back")
        .expect("and still exists");
    assert_eq!(
        second.scope,
        IndexScope::parse([FileType::Image.as_str(), FileType::Video.as_str()]).expect("scope")
    );

    // The audio-only run is gone rather than widened to every type.
    let audio_only = Uuid::parse_str(RUN_AUDIO_ONLY).expect("uuid");
    assert!(runs.get(audio_only).await.expect("get").is_none());

    // Listing the paused runs — what the UI offers to resume — reads every
    // row through the same parser and must not trip on any of them.
    let active = runs.list_active().await.expect("no run is corrupt");
    let active_ids: Vec<Uuid> = active.iter().map(|run| run.id).collect();
    assert_eq!(active_ids, vec![image_audio_video, audio_image]);

    pool.close().await;
}

/// `watch_progress` and `reading_progress` hold file ids with no foreign key,
/// so a new file that reused a removed file's id would inherit whatever still
/// pointed at it. The rebuild must carry the `AUTOINCREMENT` sequence over,
/// not restart it from the highest surviving id.
#[tokio::test]
async fn given_a_database_with_music_when_migrated_then_a_new_file_never_reuses_an_old_id() {
    let pool = pool_at_version_24().await;
    seed_music_library(&pool).await;
    let sequence_before: i64 =
        sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'files'")
            .fetch_one(&pool)
            .await
            .expect("files has a sequence");
    assert_eq!(sequence_before, PURGED_FILE_ID);

    run_migrations(&pool).await.expect("migrate to head");

    let sequences: Vec<(String, i64)> = sqlx::query_as(
        "SELECT name, seq FROM sqlite_sequence WHERE name IN ('files', 'files_new')",
    )
    .fetch_all(&pool)
    .await
    .expect("sqlite_sequence");
    assert_eq!(sequences, vec![("files".to_owned(), sequence_before)]);

    let new_id: i64 = sqlx::query_scalar(
        "INSERT INTO files (uuid, path, name, type, indexed_at)
         VALUES ('new-video-uuid', '/media/new.mkv', 'new.mkv', 'video', '2026-06-01T00:00:00Z')
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert a new file");
    assert!(
        new_id > sequence_before,
        "new file id {new_id} must be above every id ever used ({sequence_before}), \
         the removed audio files {AUDIO_ID} and {SECOND_AUDIO_ID} included"
    );

    pool.close().await;
}

/// sqlx records a migration that runs outside its transaction only after the
/// script has committed, so a process killed between the two leaves the
/// cleanup done but unrecorded, and the next start runs it again. That rerun
/// must succeed rather than fail every start from then on.
#[tokio::test]
async fn given_migration_25_applied_but_unrecorded_when_migrated_again_then_it_succeeds() {
    let pool = pool_at_version_24().await;
    seed_music_library(&pool).await;
    run_migrations(&pool).await.expect("migrate to head");
    let surviving = file_rows(&pool, "1 = 1").await;

    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 25")
        .execute(&pool)
        .await
        .expect("forget migration 25");
    run_migrations(&pool).await.expect("the rerun succeeds");

    assert_eq!(file_rows(&pool, "1 = 1").await, surviving);
    assert_eq!(foreign_keys_pragma(&pool).await, 1);

    pool.close().await;
}

/// A fresh install runs migration 25 against an empty catalog: nothing to
/// delete and no sequence to carry, and it must still leave `files` refusing
/// audio and foreign keys on.
#[tokio::test]
async fn given_fresh_in_memory_db_when_migrate_then_files_refuses_audio() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect");

    run_migrations(&pool).await.expect("migrate");

    let audio = sqlx::query(
        "INSERT INTO files (uuid, path, name, type, indexed_at)
         VALUES ('audio-uuid', '/music/a.mp3', 'a.mp3', 'audio', '2026-06-01T00:00:00Z')",
    )
    .execute(&pool)
    .await;
    assert!(audio.is_err(), "type 'audio' is refused on a fresh install");
    assert_eq!(foreign_keys_pragma(&pool).await, 1);

    pool.close().await;
}
