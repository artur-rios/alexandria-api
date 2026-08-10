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
