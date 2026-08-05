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
