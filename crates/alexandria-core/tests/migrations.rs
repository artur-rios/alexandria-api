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
