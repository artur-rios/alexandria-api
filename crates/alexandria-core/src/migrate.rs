use std::time::Duration;

use sqlx::migrate::MigrateError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::errors::DomainError;

pub async fn run_migrations(pool: &SqlitePool) -> Result<(), MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

pub async fn migrate_database(database_path: &str) -> Result<SqlitePool, DomainError> {
    // The path is handed to sqlx as a file name, never spliced into a
    // `sqlite://` URL. URL parsing splits on the first `?` and percent-decodes
    // what precedes it, so a database at `/data/100%41/x.sqlite` opened
    // `/data/100A/x.sqlite` instead and one under a folder holding a `?` was
    // cut short — on a path the FFI embedder passes through verbatim.
    // `create_if_missing` is the `mode=rwc` that URL used to carry.
    //
    // sqlx leaves `journal_mode` alone by default, which means SQLite's own
    // default: a rollback journal, where a writer takes an exclusive lock over
    // the whole database and readers block behind it. That was survivable while
    // indexing was sequential. It is not a good fit now that UC-01/UC-02 walk
    // several files at a time (`indexing.concurrency`) while the HTTP surface
    // is meant to keep answering reads (FR-FC-08). WAL lets readers proceed
    // against a snapshot while one writer works.
    //
    // WAL is a *persistent* property of the database file, not a per-connection
    // setting: switching an existing database into it happens once, here, on
    // the first connection that asks. sqlx declines to set it by default
    // precisely because the switch needs an exclusive lock that `busy_timeout`
    // cannot wait on — which is fine for a single-owner desktop database opened
    // by one process, and is why the choice belongs here rather than in sqlx.
    //
    // `busy_timeout` is set explicitly to the value sqlx already applies by
    // default. It is stated here rather than inherited because it is load
    // bearing: it is how long a writer blocked behind another writer waits
    // before SQLite gives up and answers `SQLITE_BUSY`, and the indexer's
    // bounded retry (`crate::retry`) is sized against it. Inheriting it
    // invisibly meant a sqlx upgrade could change the write path's timing with
    // nothing in this repository mentioning the number. Same duration as
    // before — this pins the current behaviour, it does not alter it.
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .map_err(DomainError::Database)?;
    run_migrations(&pool)
        .await
        .map_err(DomainError::Migration)?;
    Ok(pool)
}
