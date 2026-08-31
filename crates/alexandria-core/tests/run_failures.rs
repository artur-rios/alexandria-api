//! Which files a run could not record, against a real migrated database
//! (FR-FC-42, Testing Specification §6.4).
//!
//! The tally has always said how many. This says which — and the two must
//! agree, because an owner who reads "2 files could not be read" and opens
//! the list expects to find two files there.

use alexandria_core::catalog::runs::{
    CatalogRunRepository, RunKind, SqliteCatalogRunRepository, MAX_RECORDED_FAILURES,
};
use alexandria_core::migrate::migrate_database;
use chrono::{TimeZone, Utc};
use uuid::Uuid;

async fn repository() -> (SqliteCatalogRunRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let pool = migrate_database(dir.path().join("a.sqlite").to_str().unwrap())
        .await
        .expect("migrate");

    (SqliteCatalogRunRepository::new(pool), dir)
}

/// A started run to hang failures off. The foreign key is real — sqlx sets
/// `PRAGMA foreign_keys = ON` — so a failure for a run that was never
/// started would be refused rather than silently orphaned.
async fn started(repo: &SqliteCatalogRunRepository) -> Uuid {
    let id = Uuid::new_v4();
    repo.start(id, RunKind::Index, Some("/library"), Utc::now(), 4, None)
        .await
        .expect("start");

    id
}

#[tokio::test]
async fn given_files_a_run_could_not_record_when_read_back_then_each_is_named_with_its_reason() {
    let (repo, _dir) = repository().await;
    let run = started(&repo).await;
    let at = Utc.with_ymd_and_hms(2026, 8, 31, 9, 0, 0).unwrap();

    repo.record_failure(run, "/library/a.mp3", "permission denied", at)
        .await
        .expect("record");
    repo.record_failure(run, "/library/b.mp3", "database is locked", at)
        .await
        .expect("record");

    let failures = repo.failures(run).await.expect("read");

    assert_eq!(
        failures
            .iter()
            .map(|f| (f.path.as_str(), f.reason.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("/library/a.mp3", "permission denied"),
            ("/library/b.mp3", "database is locked"),
        ],
        "the order is the order the walk gave up in, which is the order the \
         owner watched it happen"
    );
}

#[tokio::test]
async fn given_another_runs_failures_when_one_is_read_then_only_its_own_come_back() {
    // Two scans of two folders, one bad file each. A list that mixed them
    // would send the owner looking in the wrong folder.
    let (repo, _dir) = repository().await;
    let first = started(&repo).await;
    let second = started(&repo).await;
    let at = Utc::now();

    repo.record_failure(first, "/music/a.mp3", "permission denied", at)
        .await
        .expect("record");
    repo.record_failure(second, "/books/b.pdf", "permission denied", at)
        .await
        .expect("record");

    let failures = repo.failures(first).await.expect("read");

    assert_eq!(
        failures.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["/music/a.mp3"]
    );
}

#[tokio::test]
async fn given_a_run_that_failed_on_everything_when_recorded_then_the_list_is_bounded() {
    // A mount going read-only fails every file. Unbounded, one bad scan
    // writes a row per file and leaves a table larger than the catalog it
    // failed to build — so the list stops naming and the tally keeps
    // counting.
    let (repo, _dir) = repository().await;
    let run = started(&repo).await;
    let at = Utc::now();
    let over = MAX_RECORDED_FAILURES + 50;

    for index in 0..over {
        repo.record_failure(run, &format!("/library/{index}.mp3"), "read-only", at)
            .await
            .expect("record");
    }

    let failures = repo.failures(run).await.expect("read");

    assert_eq!(
        failures.len(),
        MAX_RECORDED_FAILURES as usize,
        "the bound did not hold, so a bad mount can outgrow the catalog"
    );
    assert_eq!(
        failures.first().map(|f| f.path.as_str()),
        Some("/library/0.mp3"),
        "the bound dropped the earliest failures rather than the later ones; \
         the first thing that went wrong is the one worth keeping"
    );
}

#[tokio::test]
async fn given_a_run_with_no_failures_when_read_then_the_list_is_empty() {
    let (repo, _dir) = repository().await;
    let run = started(&repo).await;

    assert!(repo.failures(run).await.expect("read").is_empty());
}
