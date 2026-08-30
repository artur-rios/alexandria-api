use alexandria_core::catalog::model::{FileType, LibraryScope, StateFilter, SubtypeMetadata};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
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
        .bind("playlist")
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

/// The risk migration 15 (issue #120) names about itself: every
/// `audio_files` row that already existed when the column was added reads
/// `NULL` for `album_artist`, and nothing that indexes fresh data would ever
/// notice that — a fresh index writes every column, migrated or not, so a
/// test that only ever inserts through today's schema exercises a case the
/// real upgrade path never hits.
///
/// This reproduces the actual upgrade: apply migrations 0 … 14 by hand (the
/// same subset-`Migrator` technique
/// `given_a_populated_pre_14_database_when_migrated_then_the_credential_row_survives`
/// uses), insert a `files` row and an `audio_files` row through *that*
/// schema — one with no `album_artist` column to write to, because at
/// migration 14 there is none — then let `run_migrations` apply migration
/// 15 on top of data that already exists. The row's five other populated
/// columns must survive the `ALTER TABLE ADD COLUMN` untouched, and the new
/// column must read back as `None` through both of the repository's own
/// audio read paths: `find_metadata_by_uuid` (the single-file read) and
/// `list_filtered_view` (the batched listing read, issue #116) — the design
/// names both as the ones a client actually calls, and a bug isolated to
/// only one of the two hard-coded `SELECT`s would pass a test that checked
/// just the other.
#[tokio::test]
async fn given_a_pre_15_audio_row_when_migrated_then_album_artist_reads_null_not_missing_data() {
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
    for migration in migrator.iter().filter(|m| m.version <= 14) {
        conn.apply(&migrator.table_name, migration)
            .await
            .expect("apply");
    }
    drop(conn);

    // Seed a `files` row and its `audio_files` row exactly as an install
    // predating this branch would hold one: five of the six pre-existing
    // fields populated (the sixth, `track`, deliberately left `NULL` too,
    // proving that column's own pre-existing `NULL`s are undisturbed by the
    // migration alongside the new column's).
    let uuid = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO files \
         (uuid, path, name, type, content_hash, size_bytes, mtime, state, deleted_at, \
          indexed_at, missing_at) \
         VALUES (?, '/lib/old.mp3', 'old.mp3', 'audio', NULL, NULL, NULL, 'active', NULL, ?, NULL)",
    )
    .bind(uuid.to_string())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .expect("seed file");

    let (file_id,): (i64,) = sqlx::query_as("SELECT id FROM files WHERE uuid = ?")
        .bind(uuid.to_string())
        .fetch_one(&pool)
        .await
        .expect("resolve file id");

    // No `album_artist` in this INSERT — at migration 14 the column does
    // not exist, so this is the only INSERT a pre-branch install could ever
    // have run.
    sqlx::query(
        "INSERT INTO audio_files (file_id, title, artist, album, year, genre, track) \
         VALUES (?, 'Old Title', 'Old Artist', 'Old Album', 1999, 'Old Genre', NULL)",
    )
    .bind(file_id)
    .execute(&pool)
    .await
    .expect("seed pre-15 audio row");

    // The upgrade: migration 15 (and any later ones) applies on top of the
    // row that already exists.
    run_migrations(&pool).await.expect("migrate to head");

    let repo = SqliteCatalogRepository::new(pool);

    // Path 1: the single-file read.
    let metadata = repo
        .find_metadata_by_uuid(uuid)
        .await
        .expect("query")
        .expect("five populated fields keep this Some, not None");
    match metadata {
        SubtypeMetadata::Audio {
            title,
            artist,
            album,
            year,
            genre,
            track,
            album_artist,
        } => {
            assert_eq!(title.as_deref(), Some("Old Title"));
            assert_eq!(artist.as_deref(), Some("Old Artist"));
            assert_eq!(album.as_deref(), Some("Old Album"));
            assert_eq!(year, Some(1999));
            assert_eq!(genre.as_deref(), Some("Old Genre"));
            assert_eq!(
                track, None,
                "pre-existing NULL column, unrelated to the migration"
            );
            assert_eq!(
                album_artist, None,
                "a column that did not exist when this row was written must read \
                 null, not fail and not silently drop the row's other five fields"
            );
        }
        other => panic!("expected audio metadata, got {other:?}"),
    }

    // Path 2: the batched listing read (`list_filtered_view` /
    // `batch_audio`), issue #116's other hard-coded `SELECT` naming the same
    // columns — a mistake isolated to only one of the two would pass a test
    // that checked just the other.
    let views = repo
        .list_filtered_view(
            Some(FileType::Audio),
            StateFilter::Active,
            None,
            LibraryScope::OutsideLibraries,
        )
        .await
        .expect("list");
    assert_eq!(views.len(), 1);
    match &views[0].metadata {
        Some(SubtypeMetadata::Audio {
            title,
            album_artist,
            ..
        }) => {
            assert_eq!(title.as_deref(), Some("Old Title"));
            assert_eq!(
                *album_artist, None,
                "the batched listing path must read the same null the \
                 single-file path does"
            );
        }
        other => panic!("expected audio metadata in listing, got {other:?}"),
    }
}
