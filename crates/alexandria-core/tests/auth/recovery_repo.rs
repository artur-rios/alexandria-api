use alexandria_core::auth::local::{
    RecoveryCodeOutcome, RecoveryCodeRepository, SqliteRecoveryCodeRepository,
};
use alexandria_core::auth::recovery::{generate_recovery_codes, hash_recovery_code};
use alexandria_core::migrate::migrate_database;
use chrono::Utc;

/// A migrated database in a fresh temporary directory, the way
/// `tests/playback.rs` does it. The `TempDir` is returned so the caller holds
/// it: dropping it deletes the database out from under the pool.
async fn repo() -> (SqliteRecoveryCodeRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("recovery.sqlite");
    let pool = migrate_database(path.to_str().expect("utf-8 path"))
        .await
        .expect("migrate");
    (SqliteRecoveryCodeRepository::new(pool), dir)
}

#[tokio::test]
async fn given_stored_codes_when_remaining_then_counts_the_unconsumed() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();

    repo.replace_all(&hashes, Utc::now()).await.unwrap();

    assert_eq!(repo.remaining().await.unwrap(), 10);
}

#[tokio::test]
async fn given_an_unconsumed_code_when_consumed_then_consumed_and_the_count_drops() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    repo.replace_all(&hashes, Utc::now()).await.unwrap();

    let outcome = repo.consume(&hashes[3], Utc::now()).await.unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::Consumed);
    assert_eq!(repo.remaining().await.unwrap(), 9);
}

#[tokio::test]
async fn given_an_already_consumed_code_when_consumed_again_then_already_used() {
    let (repo, _dir) = repo().await;
    let codes = generate_recovery_codes();
    let hashes: Vec<String> = codes.iter().map(|c| hash_recovery_code(c)).collect();
    repo.replace_all(&hashes, Utc::now()).await.unwrap();
    repo.consume(&hashes[0], Utc::now()).await.unwrap();

    let outcome = repo.consume(&hashes[0], Utc::now()).await.unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::AlreadyUsed);
    assert_eq!(repo.remaining().await.unwrap(), 9);
}

#[tokio::test]
async fn given_a_hash_that_was_never_stored_when_consumed_then_unknown() {
    let (repo, _dir) = repo().await;
    repo.replace_all(&[hash_recovery_code("ABCDE-FGHJK")], Utc::now())
        .await
        .unwrap();

    let outcome = repo
        .consume(&hash_recovery_code("MNPQR-STVWX"), Utc::now())
        .await
        .unwrap();

    assert_eq!(outcome, RecoveryCodeOutcome::Unknown);
}

/// Regeneration must invalidate the codes the owner still holds, not just the
/// spent ones — a partial refill would leave them unsure which of their
/// written codes still work (FR-AU-17).
#[tokio::test]
async fn given_existing_codes_when_replaced_then_every_old_code_is_gone_including_unused() {
    let (repo, _dir) = repo().await;
    let first: Vec<String> = generate_recovery_codes()
        .iter()
        .map(|c| hash_recovery_code(c))
        .collect();
    repo.replace_all(&first, Utc::now()).await.unwrap();
    repo.consume(&first[0], Utc::now()).await.unwrap();

    let second: Vec<String> = generate_recovery_codes()
        .iter()
        .map(|c| hash_recovery_code(c))
        .collect();
    repo.replace_all(&second, Utc::now()).await.unwrap();

    assert_eq!(repo.remaining().await.unwrap(), 10);
    assert_eq!(
        repo.consume(&first[5], Utc::now()).await.unwrap(),
        RecoveryCodeOutcome::Unknown,
        "an unused code from the previous set survived regeneration"
    );
}

#[tokio::test]
async fn given_no_codes_when_remaining_then_zero() {
    let (repo, _dir) = repo().await;
    assert_eq!(repo.remaining().await.unwrap(), 0);
}
