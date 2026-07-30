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
