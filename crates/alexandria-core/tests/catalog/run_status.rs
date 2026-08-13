//! Unit tests for the UC-42 GetRunStatusHandler against trait fakes — no
//! database. Coverage: the main flow plus AF-01 (unknown id) and AF-02
//! (unauthenticated), and AF-03's "running runs carry no counts".

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use alexandria_core::catalog::queries::run_status::GetRunStatusHandler;
use alexandria_core::catalog::runs::{CatalogRunRepository, RunCounts, RunKind, RunStatus};
use alexandria_core::errors::DomainError;

use crate::common::{FakeAuth, FakeCatalogRunRepository};

const TOKEN: &str = "owner-token";

fn t(hour: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0).unwrap()
}

#[tokio::test]
async fn given_a_completed_run_when_read_then_it_is_returned_with_its_counts() {
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1)).await.unwrap();
    runs.finish(
        id,
        RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0,
        },
        t(2),
    )
    .await
    .unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.id, id);
    assert_eq!(run.kind, RunKind::Refresh);
    assert_eq!(run.status, RunStatus::Complete);
    assert_eq!(
        run.counts,
        Some(RunCounts::Refresh {
            refreshed: 2,
            marked_missing: 1,
            unchanged: 4,
            failed: 0
        })
    );
}

#[tokio::test]
async fn given_a_running_run_when_read_then_it_has_no_counts_yet() {
    // AF-03: no tally exists until the walk finishes.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Index, Some("/library"), t(1))
        .await
        .unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, runs);

    let run = handler.get(id, TOKEN).await.expect("get");

    assert_eq!(run.status, RunStatus::Running);
    assert!(run.counts.is_none());
    assert!(run.finished_at.is_none());
}

#[tokio::test]
async fn given_an_unknown_run_id_when_read_then_not_found() {
    // AF-01.
    let handler = GetRunStatusHandler::new(FakeAuth::Allowing, FakeCatalogRunRepository::new());

    let err = handler
        .get(Uuid::new_v4(), TOKEN)
        .await
        .expect_err("must reject an unknown id");

    assert!(matches!(err, DomainError::NotFound), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_when_read_then_unauthorized() {
    // AF-02.
    let runs = FakeCatalogRunRepository::new();
    let id = Uuid::new_v4();
    runs.start(id, RunKind::Refresh, None, t(1)).await.unwrap();
    let handler = GetRunStatusHandler::new(FakeAuth::Denying, runs);

    let err = handler
        .get(id, "")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn given_an_unauthenticated_caller_and_an_unknown_run_id_when_read_then_unauthorized_not_not_found(
) {
    // AF-02: a caller who never authenticated must not learn whether the id
    // names a run at all — auth is checked before the repository, so an
    // unknown id behind a denied token must still surface as Unauthorized,
    // never NotFound.
    let handler = GetRunStatusHandler::new(FakeAuth::Denying, FakeCatalogRunRepository::new());

    let err = handler
        .get(Uuid::new_v4(), "")
        .await
        .expect_err("must reject an unauthenticated caller");

    assert!(matches!(err, DomainError::Unauthorized), "got {err:?}");
    assert!(
        !matches!(err, DomainError::NotFound),
        "must not leak whether the id names a run to an unauthenticated caller"
    );
}
