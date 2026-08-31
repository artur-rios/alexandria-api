//! UC-42 integration tests for run control over HTTP (Task 12): pause,
//! resume, cancel, the active-runs listing, and the `priority` field on both
//! start bodies — the HTTP twin of `alexandria-ffi/tests/smoke.rs`'s run
//! control coverage (Task 11), against the real axum router over a real temp
//! SQLite database.

mod common;

use std::time::{Duration, Instant};

use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::common::{test_app, wait_for_run_terminal, write_file, ASYNC_RUN_DEADLINE, TEST_TOKEN};

fn index_request(root: &str, priority: Option<&str>) -> Request<Body> {
    let mut body = serde_json::json!({ "root": root });
    if let Some(priority) = priority {
        body["priority"] = serde_json::Value::String(priority.to_string());
    }
    Request::builder()
        .method("POST")
        .uri("/v1/index")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn refresh_request(body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"));
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

/// Like `refresh_request`, but lets a test send a request axum's `Json`
/// extractor would reject outright — an empty body with a JSON
/// content-type, or a body that is not valid JSON at all — rather than the
/// "no body, no content-type" shape `refresh_request(None)` sends.
fn refresh_request_raw(content_type: Option<&str>, raw_body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/index/refresh")
        .header("authorization", format!("Bearer {TEST_TOKEN}"));
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder.body(Body::from(raw_body.to_string())).unwrap()
}

fn control_request(verb: &str, run_id: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/v1/index/runs/{run_id}/{verb}"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

/// Like [`control_request`], but with a body — including bodies axum's
/// `Json` extractor rejects outright, so the resume route's folding of every
/// `JsonRejection` into the default is exercised rather than assumed.
/// `content_type` is kept separate from `raw_body` for the reason
/// `refresh_request_raw` keeps them apart: on axum 0.8 it is the presence of
/// that header, not the body, that decides which failures are reachable.
fn control_request_raw(
    verb: &str,
    run_id: &str,
    content_type: Option<&str>,
    raw_body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/v1/index/runs/{run_id}/{verb}"))
        .header("authorization", format!("Bearer {TEST_TOKEN}"));
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder.body(Body::from(raw_body.to_string())).unwrap()
}

fn runs_request(query: Option<&str>, token: Option<&str>) -> Request<Body> {
    let uri = match query {
        Some(q) => format!("/v1/index/runs?{q}"),
        None => "/v1/index/runs".to_string(),
    };
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

async fn run_status_json(router: &axum::Router, run_id: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/index/runs/{run_id}"))
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("status");
    let status = response.status();
    (status, body_json(response).await)
}

/// A library with enough files that the walk has a real chance of still
/// being `running` (or, failing that, still landing in
/// `RunControlHandler::control`'s "no live cell yet" window) the instant a
/// pause/cancel/resume request is made right after `start` returns.
/// Mirrors `alexandria-ffi/tests/smoke.rs`'s `write_library`.
fn write_library(dir: &tempfile::TempDir, count: usize) {
    for i in 0..count {
        write_file(dir, &format!("track-{i}.mp3"), b"audio bytes");
    }
}

/// Poll `GET /v1/index/runs/{runId}` until its progress cell has published at
/// least one flush (`phase` non-null) while the run is still `running`.
/// Pausing/cancelling/resuming before this point can land in the "no live
/// cell" branch documented on `RunControlHandler::control`, which is a
/// legitimate outcome but not the one these tests are named for. Mirrors
/// `alexandria-ffi/tests/smoke.rs`'s `wait_for_run_cell_live`.
async fn wait_for_run_cell_live(router: &axum::Router, run_id: &str) {
    let deadline = Instant::now() + ASYNC_RUN_DEADLINE;
    loop {
        let (status, body) = run_status_json(router, run_id).await;
        assert_eq!(status, StatusCode::OK);
        if !body["phase"].is_null() {
            return;
        }
        if body["status"] != "running" {
            panic!(
                "run {run_id} left running before its cell ever went live; \
                 write_library needs more files to give the walk time"
            );
        }
        if Instant::now() > deadline {
            panic!("run {run_id}'s cell never went live");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn start_index(router: &axum::Router, root: &str) -> String {
    let response = router
        .clone()
        .oneshot(index_request(root, None))
        .await
        .expect("index");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    body_json(response).await["runId"]
        .as_str()
        .expect("runId")
        .to_string()
}

/// Start an index run at `priority` over `lib` and drive it to `paused`,
/// returning its id — the fixture every resume test below needs. Factored
/// out of `given_a_paused_run_when_resumed_then_202_same_run_id_and_it_finishes`
/// unchanged, including the 500-file library: that is what gives an *index*
/// walk (which parses tags per file, unlike a refresh) time to still be
/// running when the pause lands, and `wait_for_run_cell_live` panics with a
/// pointed message rather than flaking if it ever stops being enough.
async fn paused_index_run(
    router: &axum::Router,
    services: &std::sync::Arc<alexandria_core::services::Services>,
    lib: &tempfile::TempDir,
    priority: Option<&str>,
) -> String {
    let response = router
        .clone()
        .oneshot(index_request(lib.path().to_str().unwrap(), priority))
        .await
        .expect("index");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .expect("runId")
        .to_string();

    wait_for_run_cell_live(router, &run_id).await;
    let pause_response = router
        .clone()
        .oneshot(control_request("pause", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("pause");
    assert_eq!(pause_response.status(), StatusCode::OK);
    assert_eq!(
        wait_for_run_terminal(services, &run_id, TEST_TOKEN)
            .await
            .get("status")
            .unwrap(),
        "paused"
    );
    run_id
}

/// The user-facing goal of Task 15 over HTTP: a scan started at normal speed,
/// paused, and resumed `"low"` so the app is usable again — without losing the
/// run. The stored `concurrency` is the only place the resolved priority is
/// observable (`CatalogRun::concurrency` is `#[serde(skip)]`), and it is what
/// `execute` reads to pace the resumed walk.
#[tokio::test]
async fn given_a_paused_run_when_resumed_with_low_priority_then_the_run_is_repaced_low() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);
    let run_id = paused_index_run(&router, &test.services, &lib, None).await;
    assert_eq!(
        run_concurrency(&test.pool, &run_id).await,
        Some(4),
        "sanity: it started at indexing.concurrency"
    );

    let response = router
        .clone()
        .oneshot(control_request_raw(
            "resume",
            &run_id,
            Some("application/json"),
            &serde_json::json!({ "priority": "low" }).to_string(),
        ))
        .await
        .expect("resume");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        run_concurrency(&test.pool, &run_id).await,
        Some(1),
        "\"low\" must re-pace the run to indexing.low_priority_concurrency"
    );
    assert_eq!(
        wait_for_run_terminal(&test.services, &run_id, TEST_TOKEN).await["status"],
        "complete",
        "and the re-paced run still finishes under the same id"
    );
}

/// The other direction: `"normal"` is a real request to widen a throttled run
/// back out, which is exactly what distinguishes it from sending nothing.
#[tokio::test]
async fn given_a_low_priority_paused_run_when_resumed_with_normal_priority_then_it_is_widened() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);
    let run_id = paused_index_run(&router, &test.services, &lib, Some("low")).await;
    assert_eq!(run_concurrency(&test.pool, &run_id).await, Some(1));

    let response = router
        .clone()
        .oneshot(control_request_raw(
            "resume",
            &run_id,
            Some("application/json"),
            &serde_json::json!({ "priority": "normal" }).to_string(),
        ))
        .await
        .expect("resume");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        run_concurrency(&test.pool, &run_id).await,
        Some(4),
        "\"normal\" must widen the run to indexing.concurrency"
    );
}

/// Regression, and the reason absent is not spelled `normal`: every caller
/// before Task 15 posted no body at all to this route, and a run they had
/// throttled down must come back throttled down. Sending nothing is not a
/// request to speed up.
///
/// Three bodiless shapes at once, because on axum 0.8 they fail three
/// different ways and only one of them is what `Option<Json<..>>` collapses:
/// no content-type at all, a JSON content-type with an empty body, and a
/// JSON content-type with a body that is not JSON. All three must reach the
/// same default.
#[tokio::test]
async fn given_no_readable_body_when_a_low_priority_run_is_resumed_then_it_keeps_its_low_width() {
    for (content_type, raw_body) in [
        (None, ""),
        (Some("application/json"), ""),
        (Some("application/json"), "{not json"),
    ] {
        let test = test_app().await;
        let router = app(Settings::default(), test.services.clone());
        let lib = tempfile::tempdir().unwrap();
        write_library(&lib, 500);
        let run_id = paused_index_run(&router, &test.services, &lib, Some("low")).await;

        let response = router
            .clone()
            .oneshot(control_request_raw(
                "resume",
                &run_id,
                content_type,
                raw_body,
            ))
            .await
            .expect("resume");

        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "a resume with content-type {content_type:?} and body {raw_body:?} must \
             still be accepted"
        );
        assert_eq!(
            run_concurrency(&test.pool, &run_id).await,
            Some(1),
            "and must leave the run at the width it already had, not widen it to \
             indexing.concurrency"
        );
    }
}

/// An unrecognised priority is treated like an absent one — quietly, never as
/// a rejected call — the same leniency both start bodies already give
/// (FR-FC-24). "Keep the current width" is the fallback here rather than
/// `normal`, because the run already has a width to keep.
#[tokio::test]
async fn given_an_unrecognised_priority_when_a_low_priority_run_is_resumed_then_it_keeps_its_width()
{
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);
    let run_id = paused_index_run(&router, &test.services, &lib, Some("low")).await;

    let response = router
        .clone()
        .oneshot(control_request_raw(
            "resume",
            &run_id,
            Some("application/json"),
            &serde_json::json!({ "priority": "URGENT!!1" }).to_string(),
        ))
        .await
        .expect("resume");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(run_concurrency(&test.pool, &run_id).await, Some(1));
}

#[tokio::test]
async fn given_a_running_run_when_paused_then_200_and_the_run_reads_paused() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);

    let run_id = start_index(&router, lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &run_id).await;

    let response = router
        .clone()
        .oneshot(control_request("pause", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("pause");
    assert_eq!(response.status(), StatusCode::OK);

    // `pause` only raises the signal or writes the row; the walk's own drain
    // and terminal write can still be in flight when it returns.
    let body = wait_for_run_terminal(&test.services, &run_id, TEST_TOKEN).await;
    assert_eq!(body["status"], "paused");
    assert!(body["processed"].is_number(), "processed: {body}");
    assert!(body["activeMillis"].is_number(), "activeMillis: {body}");
}

#[tokio::test]
async fn given_a_completed_run_when_paused_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_file(&lib, "song.mp3", b"audio");

    let run_id = start_index(&router, lib.path().to_str().unwrap()).await;
    let body = wait_for_run_terminal(&test.services, &run_id, TEST_TOKEN).await;
    assert_eq!(body["status"], "complete", "sanity: run finished");

    let response = router
        .oneshot(control_request("pause", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("pause");
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "pausing a completed run must be refused with 409, not accepted or a generic error"
    );
}

#[tokio::test]
async fn given_an_unknown_run_when_paused_then_404() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(control_request(
            "pause",
            "00000000-0000-4000-8000-000000000000",
            Some(TEST_TOKEN),
        ))
        .await
        .expect("pause");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_a_malformed_run_id_when_paused_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(control_request("pause", "not-a-uuid", Some(TEST_TOKEN)))
        .await
        .expect("pause");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_no_token_when_paused_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(control_request(
            "pause",
            "00000000-0000-4000-8000-000000000000",
            None,
        ))
        .await
        .expect("pause");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_a_paused_run_when_resumed_then_202_same_run_id_and_it_finishes() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);

    // Through the shared fixture rather than its own copy of it.
    // `paused_index_run` says it was "factored out of
    // given_a_paused_run_when_resumed_then_202_same_run_id_and_it_finishes
    // unchanged" — the factoring happened, and this, the test it came from,
    // was the one caller left behind. Four tests got any hardening of the
    // fixture; this one did not.
    let run_id = paused_index_run(&router, &test.services, &lib, None).await;

    let resume_response = router
        .clone()
        .oneshot(control_request("resume", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("resume");
    assert_eq!(resume_response.status(), StatusCode::ACCEPTED);
    let resumed = body_json(resume_response).await;
    assert_eq!(
        resumed["runId"], run_id,
        "resume must hand back the same run id, not mint a fresh one"
    );

    // The resumed walk starts over from the root (no cursor is kept), and
    // finishes: everything already cataloged falls out as alreadyCataloged.
    let body = wait_for_run_terminal(&test.services, &run_id, TEST_TOKEN).await;
    assert_eq!(body["status"], "complete");
}

#[tokio::test]
async fn given_a_running_run_when_resumed_then_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);

    let run_id = start_index(&router, lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &run_id).await;

    let (status, body) = run_status_json(&router, &run_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["status"], "running",
        "sanity: the run is still running at the moment resume is called"
    );

    let response = router
        .oneshot(control_request("resume", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("resume");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_a_running_run_when_cancelled_then_200_and_terminal_and_a_second_cancel_is_409() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_library(&lib, 500);

    let run_id = start_index(&router, lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &run_id).await;

    let cancel_response = router
        .clone()
        .oneshot(control_request("cancel", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("cancel");
    assert_eq!(cancel_response.status(), StatusCode::OK);

    let body = wait_for_run_terminal(&test.services, &run_id, TEST_TOKEN).await;
    assert_eq!(body["status"], "cancelled");

    // Terminal: a second cancel finds nothing left to abandon.
    let second = router
        .oneshot(control_request("cancel", &run_id, Some(TEST_TOKEN)))
        .await
        .expect("cancel");
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

/// Exercises every `RunStatus` variant at once and proves the active-runs
/// listing keeps exactly the two non-terminal ones (`running`, `paused`) and
/// drops all three terminal ones (`complete`, `failed`, `cancelled`) — the
/// exact boundary `GetActiveRunsHandler::list`'s doc comment draws.
///
/// The terminal runs (`complete`, `cancelled`, and the directly-seeded
/// `failed` row) are set up *first*; the `running` run is started *last*,
/// immediately before the query. A small fixture library finishes in
/// milliseconds with no real hashing to do, so a `running` run created early
/// and then left alone while three more runs are built can legitimately
/// finish before this test ever gets to the listing — that is not a bug in
/// the route, it is the walk actually completing, and the fix is to shrink
/// the window between "confirmed running" and "listed," not to slow the
/// walk down. Assertions are presence/absence per id rather than a length
/// check, per the same reasoning: `ids.len() == 2` is a proxy for "exactly
/// the two non-terminal runs are here," and a proxy that can be defeated two
/// different ways (an unexpected extra row, or the expected `running` row
/// legitimately not being there yet) is worse than asserting the real claim
/// directly.
///
/// `failed` has no reachable path through this router (nothing here can make
/// a real walk fail), so that one row is seeded directly into
/// `catalog_runs` — the only state this test cannot produce by calling the
/// API. Every other row comes from a real request/response round trip.
#[tokio::test]
async fn given_runs_in_every_state_when_active_ones_are_requested_then_only_those_come_back() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    // paused
    let paused_lib = tempfile::tempdir().unwrap();
    write_library(&paused_lib, 500);
    let paused_id = start_index(&router, paused_lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &paused_id).await;
    let pause_response = router
        .clone()
        .oneshot(control_request("pause", &paused_id, Some(TEST_TOKEN)))
        .await
        .expect("pause");
    assert_eq!(pause_response.status(), StatusCode::OK);
    assert_eq!(
        wait_for_run_terminal(&test.services, &paused_id, TEST_TOKEN)
            .await
            .get("status")
            .unwrap(),
        "paused"
    );

    // complete
    let complete_lib = tempfile::tempdir().unwrap();
    write_file(&complete_lib, "song.mp3", b"audio");
    let complete_id = start_index(&router, complete_lib.path().to_str().unwrap()).await;
    assert_eq!(
        wait_for_run_terminal(&test.services, &complete_id, TEST_TOKEN)
            .await
            .get("status")
            .unwrap(),
        "complete"
    );

    // cancelled
    let cancelled_lib = tempfile::tempdir().unwrap();
    write_library(&cancelled_lib, 500);
    let cancelled_id = start_index(&router, cancelled_lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &cancelled_id).await;
    let cancel_response = router
        .clone()
        .oneshot(control_request("cancel", &cancelled_id, Some(TEST_TOKEN)))
        .await
        .expect("cancel");
    assert_eq!(cancel_response.status(), StatusCode::OK);
    assert_eq!(
        wait_for_run_terminal(&test.services, &cancelled_id, TEST_TOKEN)
            .await
            .get("status")
            .unwrap(),
        "cancelled"
    );

    // failed — no request through this router can force a walk to fail, so
    // this one row is seeded directly.
    let failed_id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO catalog_runs (id, kind, status, started_at, finished_at) \
         VALUES (?, 'index', 'failed', ?, ?)",
    )
    .bind(failed_id.to_string())
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&test.pool)
    .await
    .expect("seed failed run");

    // running — started last, and queried immediately: see the doc comment
    // on this test for why the gap between "confirmed running" and "listed"
    // has to stay small.
    let running_lib = tempfile::tempdir().unwrap();
    write_library(&running_lib, 500);
    let running_id = start_index(&router, running_lib.path().to_str().unwrap()).await;
    wait_for_run_cell_live(&router, &running_id).await;

    let response = router
        .clone()
        .oneshot(runs_request(Some("status=active"), Some(TEST_TOKEN)))
        .await
        .expect("active runs");
    assert_eq!(response.status(), StatusCode::OK);
    let runs = body_json(response).await;
    let ids: Vec<String> = runs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["runId"].as_str().unwrap().to_string())
        .collect();
    assert!(
        ids.contains(&running_id),
        "running run must be included: {ids:?}"
    );
    assert!(
        ids.contains(&paused_id),
        "paused run must be included: {ids:?}"
    );
    assert!(
        !ids.contains(&complete_id),
        "complete run must be excluded: {ids:?}"
    );
    assert!(
        !ids.contains(&cancelled_id),
        "cancelled run must be excluded: {ids:?}"
    );
    assert!(
        !ids.contains(&failed_id.to_string()),
        "failed run must be excluded: {ids:?}"
    );

    // An absent `status` answers identically — the only listing this route
    // can produce is "active", so that is the default, not a rejection.
    let default_response = router
        .oneshot(runs_request(None, Some(TEST_TOKEN)))
        .await
        .expect("active runs, default filter");
    assert_eq!(default_response.status(), StatusCode::OK);
    let default_runs = body_json(default_response).await;
    let default_ids: Vec<String> = default_runs
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["runId"].as_str().unwrap().to_string())
        .collect();
    assert!(default_ids.contains(&running_id), "{default_ids:?}");
    assert!(default_ids.contains(&paused_id), "{default_ids:?}");
    assert!(!default_ids.contains(&complete_id), "{default_ids:?}");
    assert!(!default_ids.contains(&cancelled_id), "{default_ids:?}");
    assert!(
        !default_ids.contains(&failed_id.to_string()),
        "{default_ids:?}"
    );
}

#[tokio::test]
async fn given_an_unrecognised_status_when_runs_listed_then_400() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(runs_request(Some("status=complete"), Some(TEST_TOKEN)))
        .await
        .expect("runs");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A query string axum's `Query` extractor itself cannot parse (duplicate
/// keys into a scalar field) must land in this surface's `{"error": …}`
/// envelope too, not axum's bare-text rejection — the same reason the path
/// segment and the JSON body on this router are taken as `Result<_,
/// _Rejection>` rather than the bare extractor.
#[tokio::test]
async fn given_a_malformed_query_string_when_runs_listed_then_400_with_error_envelope() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(runs_request(
            Some("status=active&status=low"),
            Some(TEST_TOKEN),
        ))
        .await
        .expect("runs");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert!(
        body.get("error").is_some(),
        "must be this surface's error envelope, not axum's bare-text rejection: {body:?}"
    );
}

#[tokio::test]
async fn given_no_token_when_runs_listed_then_401() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services);

    let response = router
        .oneshot(runs_request(Some("status=active"), None))
        .await
        .expect("runs");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Proves the `priority` field on `POST /v1/index` actually reaches
/// `IndexRequest::priority` and is resolved into the low-priority
/// concurrency — the only place a chosen priority is observable, since
/// `CatalogRun::concurrency` is `#[serde(skip)]` on the run body.
#[tokio::test]
async fn given_low_priority_when_index_started_then_run_recorded_at_low_concurrency() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_file(&lib, "song.mp3", b"audio");

    let response = router
        .oneshot(index_request(lib.path().to_str().unwrap(), Some("low")))
        .await
        .expect("index");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(
        concurrency,
        Some(1),
        "indexing.low_priority_concurrency defaults to 1"
    );
}

/// A priority value neither surface can spell falls back to `Normal` rather
/// than rejecting the request — the same leniency
/// `alexandria-ffi::parse_priority` (Task 11) applies to a garbage FFI
/// argument (FR-FC-24).
#[tokio::test]
async fn given_garbage_priority_when_index_started_then_falls_back_to_normal_concurrency() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());
    let lib = tempfile::tempdir().unwrap();
    write_file(&lib, "song.mp3", b"audio");

    let response = router
        .oneshot(index_request(
            lib.path().to_str().unwrap(),
            Some("URGENT!!1"),
        ))
        .await
        .expect("index");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(concurrency, Some(4), "indexing.concurrency defaults to 4");
}

#[tokio::test]
async fn given_low_priority_when_refresh_started_then_run_recorded_at_low_concurrency() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let response = router
        .oneshot(refresh_request(Some(
            serde_json::json!({ "priority": "low" }),
        )))
        .await
        .expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(concurrency, Some(1));
}

/// Regression: every caller before Task 12 posted no body at all to
/// `POST /v1/index/refresh` (there was nothing to send). Adding an optional
/// `priority` field must not turn that into a rejection.
#[tokio::test]
async fn given_no_body_when_refresh_started_then_still_202_at_normal_priority() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let response = router
        .oneshot(refresh_request(None))
        .await
        .expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(concurrency, Some(4));
}

/// Regression: a JSON content-type with a genuinely empty body is a shape
/// `Option<Json<..>>` on axum 0.8 does NOT collapse to `None` for (only a
/// wholly *absent* `content-type` does) — it is a real `Json` extraction
/// failure. Falling back to the default priority here has to come from
/// folding the rejection, not from the extractor quietly resolving to
/// `None`.
#[tokio::test]
async fn given_empty_body_with_json_content_type_when_refresh_started_then_still_202_at_normal_priority(
) {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let response = router
        .oneshot(refresh_request_raw(Some("application/json"), ""))
        .await
        .expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(concurrency, Some(4));
}

/// Regression: malformed JSON is also a real `Json` extraction failure, not
/// a missing-content-type one — same fold-into-default requirement as the
/// empty-body case above.
#[tokio::test]
async fn given_malformed_json_body_when_refresh_started_then_still_202_at_normal_priority() {
    let test = test_app().await;
    let router = app(Settings::default(), test.services.clone());

    let response = router
        .oneshot(refresh_request_raw(Some("application/json"), "{not json"))
        .await
        .expect("refresh");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let run_id = body_json(response).await["runId"]
        .as_str()
        .unwrap()
        .to_string();

    let concurrency = run_concurrency(&test.pool, &run_id).await;
    assert_eq!(concurrency, Some(4));
}

/// The `concurrency` column `catalog_runs` recorded for `run_id` — see
/// `alexandria-ffi/tests/smoke.rs`'s identical helper for why this reads the
/// column directly rather than the JSON body.
async fn run_concurrency(pool: &sqlx::sqlite::SqlitePool, run_id: &str) -> Option<i64> {
    let (concurrency,): (Option<i64>,) =
        sqlx::query_as("SELECT concurrency FROM catalog_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("run row");
    concurrency
}
