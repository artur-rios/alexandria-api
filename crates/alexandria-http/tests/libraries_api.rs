//! HTTP integration tests for the libraries surface (`/v1/libraries`): the
//! real axum router over a real temp SQLite database.
//!
//! Written for the move — the one route whose refusals are easy to get wrong
//! — but covering the whole surface, because until now this one had no HTTP
//! tests at all and the FFI half was the only place any of it ran.

mod common;

use alexandria_core::catalog::model::{FileType, NewFile};
use alexandria_core::catalog::repos::{CatalogRepository, SqliteCatalogRepository};
use alexandria_core::config::Settings;
use alexandria_http::app;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::common::test_app;

fn authed_request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "authorization",
            format!("Bearer {}", common::TEST_TOKEN).as_str(),
        )
        .header("content-type", "application/json");
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// Put one active document in the catalog at `path`.
async fn seed_file(pool: &sqlx::sqlite::SqlitePool, path: &str) {
    SqliteCatalogRepository::new(pool.clone())
        .insert_file(NewFile {
            uuid: uuid::Uuid::new_v4(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            file_type: FileType::Document,
            content_hash: Some("0".repeat(64)),
            size_bytes: None,
            mtime: None,
            indexed_at: chrono::Utc::now(),
        })
        .await
        .expect("insert");
}

/// Register a library and answer its uuid.
async fn register(app: &axum::Router, name: &str, root: &str) -> String {
    let response = app
        .clone()
        .oneshot(authed_request(
            "POST",
            "/v1/libraries",
            Some(json!({"name": name, "rootPath": root})),
        ))
        .await
        .expect("register");
    assert_eq!(response.status(), StatusCode::CREATED);

    body_json(response).await["uuid"]
        .as_str()
        .expect("uuid")
        .to_string()
}

#[tokio::test]
async fn given_a_folder_when_posted_then_it_is_created_and_listed() {
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());

    register(&app, "Course", "/library/course").await;

    let listed = app
        .clone()
        .oneshot(authed_request("GET", "/v1/libraries", None))
        .await
        .expect("list");
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(body_json(listed).await.as_array().map(|a| a.len()), Some(1));
}

#[tokio::test]
async fn given_a_moved_folder_when_patched_then_the_library_answers_from_its_new_root() {
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    let uuid = register(&app, "Course", "/library/course").await;

    let response = app
        .clone()
        .oneshot(authed_request(
            "PATCH",
            &format!("/v1/libraries/{uuid}"),
            Some(json!({"rootPath": "/media/courses/rust"})),
        ))
        .await
        .expect("patch");

    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["rootPath"], "/media/courses/rust");
    assert_eq!(value["uuid"], uuid, "the move replaced the library");
    assert_eq!(value["name"], "Course", "the move renamed the library");
}

#[tokio::test]
async fn given_another_librarys_folder_when_patched_onto_then_it_is_a_conflict() {
    // A conflict rather than a bad request: the body is well formed and the
    // folder is real — what is wrong is the state it would leave behind, a
    // file belonging to two libraries at once.
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    let uuid = register(&app, "Course", "/library/course").await;
    register(&app, "Photos", "/library/photos").await;

    let response = app
        .clone()
        .oneshot(authed_request(
            "PATCH",
            &format!("/v1/libraries/{uuid}"),
            Some(json!({"rootPath": "/library/photos/2024"})),
        ))
        .await
        .expect("patch");

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn given_a_blank_root_when_patched_then_it_is_a_bad_request() {
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    let uuid = register(&app, "Course", "/library/course").await;

    let response = app
        .clone()
        .oneshot(authed_request(
            "PATCH",
            &format!("/v1/libraries/{uuid}"),
            Some(json!({"rootPath": "   "})),
        ))
        .await
        .expect("patch");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn given_an_unknown_library_when_patched_then_it_is_not_found() {
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());

    let response = app
        .oneshot(authed_request(
            "PATCH",
            "/v1/libraries/11111111-1111-1111-1111-111111111111",
            Some(json!({"rootPath": "/media/courses"})),
        ))
        .await
        .expect("patch");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn given_no_credential_when_a_library_is_patched_then_it_is_unauthorized() {
    // Refused before the body is read, like every other write here: an
    // unauthenticated caller learns nothing about whether the library exists.
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    let uuid = register(&app, "Course", "/library/course").await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/v1/libraries/{uuid}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"rootPath": "/media"}).to_string()))
                .unwrap(),
        )
        .await
        .expect("patch");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn given_a_library_when_the_listing_asks_to_reach_in_then_its_files_are_answered() {
    // The listing is what a client's search and its deleted-items review are
    // built from, so "absent from the type panels" must not mean "absent from
    // the catalog" (FR-FC-38). Both directions are asserted here, because
    // either one alone passes for the wrong reason.
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    seed_file(&harness.pool, "/library/course/syllabus.pdf").await;
    register(&app, "Course", "/library/course").await;

    let excluded = app
        .clone()
        .oneshot(authed_request("GET", "/v1/files?type=document", None))
        .await
        .expect("list");
    assert_eq!(
        body_json(excluded).await.as_array().map(|a| a.len()),
        Some(0),
        "the type panel listed a library's file"
    );

    let included = app
        .clone()
        .oneshot(authed_request(
            "GET",
            "/v1/files?type=document&includeLibraries=true",
            None,
        ))
        .await
        .expect("list");
    assert_eq!(
        body_json(included).await.as_array().map(|a| a.len()),
        Some(1),
        "a library's file could not be reached, so nothing could find it"
    );
}

#[tokio::test]
async fn given_anything_but_true_when_the_listing_is_asked_then_libraries_stay_excluded() {
    // A typo must fall towards the exclusion: a course leaking into the type
    // panels is the defect marking the folder was meant to prevent.
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    seed_file(&harness.pool, "/library/course/syllabus.pdf").await;
    register(&app, "Course", "/library/course").await;

    for query in [
        "includeLibraries=false",
        "includeLibraries=1",
        "includeLibraries=",
    ] {
        let response = app
            .clone()
            .oneshot(authed_request(
                "GET",
                &format!("/v1/files?type=document&{query}"),
                None,
            ))
            .await
            .expect("list");
        assert_eq!(
            body_json(response).await.as_array().map(|a| a.len()),
            Some(0),
            "`{query}` reached into libraries"
        );
    }
}

#[tokio::test]
async fn given_a_library_when_deleted_then_it_is_gone_from_the_listing() {
    let harness = test_app().await;
    let app = app(Settings::default(), harness.services.clone());
    let uuid = register(&app, "Course", "/library/course").await;

    let deleted = app
        .clone()
        .oneshot(authed_request(
            "DELETE",
            &format!("/v1/libraries/{uuid}"),
            None,
        ))
        .await
        .expect("delete");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let listed = app
        .clone()
        .oneshot(authed_request("GET", "/v1/libraries", None))
        .await
        .expect("list");
    assert_eq!(body_json(listed).await.as_array().map(|a| a.len()), Some(0));
}
