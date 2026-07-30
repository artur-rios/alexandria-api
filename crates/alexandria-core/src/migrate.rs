use sqlx::migrate::MigrateError;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

use crate::errors::DomainError;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

pub async fn migrate_database(database_path: &str) -> Result<(), DomainError> {
    let url = format!("sqlite://{database_path}");
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .map_err(DomainError::Database)?;
    run_migrations(&pool)
        .await
        .map_err(DomainError::Migration)?;
    pool.close().await;
    Ok(())
}
