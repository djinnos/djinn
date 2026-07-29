//! HTTP-level contract coverage for the browser readiness surface.

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use djinn_db::{
    ProjectRepository, ReadinessRepository, RepoGraphCacheInsert, RepoGraphCacheRepository,
    UserRepository,
    repositories::readiness::{ReadinessIdentificationOutput, ReadinessIdentifiedArea},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::test_helpers;

const SNAPSHOT: &str = "d34db33fd34db33fd34db33fd34db33fd34db33f";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_routes_require_auth_and_authorize_the_project_owner() {
    let (app, project_ref, db, _project_dir) =
        test_helpers::create_test_app_with_project_and_db().await;
    let project_id = project_id(&db, &project_ref).await;

    assert_status(
        get(&app, &project_id, None).await,
        StatusCode::UNAUTHORIZED,
        "authentication_required",
    )
    .await;
    assert_status(
        post_kickoff(&app, &project_id, None, "key").await,
        StatusCode::UNAUTHORIZED,
        "authentication_required",
    )
    .await;

    // The fixture project's GitHub owner is `test`; this credential takes the
    // browser cookie path but resolves to a different owner login.
    let non_owner = test_helpers::seed_session_cookie(&db, "not-the-project-owner").await;
    assert_status(
        get(&app, &project_id, Some(&non_owner)).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_routes_cover_empty_not_found_blank_and_kickoff_reuse() {
    let (app, project_ref, db, _project_dir) =
        test_helpers::create_test_app_with_project_and_db().await;
    let project_id = project_id(&db, &project_ref).await;
    let owner = test_helpers::seed_session_cookie(&db, "test").await;
    seed_snapshot(&db, &project_id).await;

    let empty = get(&app, &project_id, Some(&owner)).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(response_json(empty).await, Value::Null);
    assert_status(
        get(&app, "missing-project", Some(&owner)).await,
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;
    assert_status(
        post_kickoff(&app, &project_id, Some(&owner), " \t ").await,
        StatusCode::BAD_REQUEST,
        "blank_idempotency_key",
    )
    .await;

    let first_response = post_kickoff(&app, &project_id, Some(&owner), "browser-key").await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let first = response_json(first_response).await;
    assert_eq!(first["created"], true);
    assert_eq!(first["reused"], false);
    assert_eq!(first["run"]["project_id"], project_id);
    assert_eq!(first["run"]["status"], "identifying");
    assert!(first["identification_task_id"].as_str().is_some());

    let repeated_response = post_kickoff(&app, &project_id, Some(&owner), "browser-key").await;
    assert_eq!(repeated_response.status(), StatusCode::OK);
    let repeated = response_json(repeated_response).await;
    assert_eq!(repeated["created"], false);
    assert_eq!(repeated["reused"], true);
    assert_eq!(repeated["run"]["id"], first["run"]["id"]);

    let latest = get(&app, &project_id, Some(&owner)).await;
    assert_eq!(latest.status(), StatusCode::OK);
    let latest = response_json(latest).await;
    assert_eq!(latest["id"], first["run"]["id"]);
    assert_eq!(latest["repository_snapshot"], SNAPSHOT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_detail_route_scopes_runs_and_serializes_complete_projection() {
    let (app, project_ref, db, _project_dir) =
        test_helpers::create_test_app_with_project_and_db().await;
    let project_id = project_id(&db, &project_ref).await;
    let owner = test_helpers::seed_session_cookie(&db, "test").await;
    let owner_id = UserRepository::new(db.clone())
        .get_by_github_id(424_242)
        .await
        .unwrap()
        .unwrap()
        .id;
    seed_snapshot(&db, &project_id).await;

    let kickoff =
        response_json(post_kickoff(&app, &project_id, Some(&owner), "detail-key").await).await;
    let run_id = kickoff["run"]["id"].as_str().unwrap().to_owned();
    let fanout = ReadinessRepository::new(db.clone())
        .complete_identification(&run_id, &owner_id, one_area_identification())
        .await
        .expect("seed a materialized readiness area");
    let area = fanout.first().expect("one area");
    djinn_db::test_support::seed_readiness_detail_projection_for_test(
        &db,
        &run_id,
        &area.area.id,
        &area.attempt.id,
    )
    .await;

    assert_status(
        detail(&app, &project_id, "missing-run", Some(&owner)).await,
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;
    let other = ProjectRepository::new(db.clone(), test_helpers::test_events())
        .create("other-project", "test", "other-project")
        .await
        .unwrap();
    assert_status(
        detail(&app, &other.id, &run_id, Some(&owner)).await,
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;

    let response = detail(&app, &project_id, &run_id, Some(&owner)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    assert_eq!(json["run"]["id"], run_id);
    assert_eq!(json["areas"][0]["area_key"], "frontend");
    assert_eq!(json["areas"][0]["attempts"][0]["is_current"], true);
    assert_eq!(
        json["areas"][0]["accepted_findings"][0]["guardrail_key"],
        "auth"
    );
    assert_eq!(
        json["areas"][0]["accepted_outputs"][0]["result"]["warnings"][0],
        "preserved"
    );
    assert_eq!(json["area_scores"][0]["score"], 0.75);
    assert_eq!(json["project_score"]["band"], "ready");
    assert_eq!(json["suggestions"][0]["dedupe_key"], "auth-remediation");
    assert!(
        json["events"]
            .as_array()
            .expect("events serialize as an array")
            .iter()
            .any(|event| event["event_kind"] == "fixture_completed")
    );
}

async fn project_id(db: &djinn_db::Database, project_ref: &str) -> String {
    ProjectRepository::new(db.clone(), test_helpers::test_events())
        .resolve(project_ref)
        .await
        .unwrap()
        .expect("fixture project")
}

async fn seed_snapshot(db: &djinn_db::Database, project_id: &str) {
    RepoGraphCacheRepository::new(db.clone())
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha: SNAPSHOT,
            graph_blob: b"readiness HTTP test graph",
        })
        .await
        .unwrap();
}

fn one_area_identification() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![ReadinessIdentifiedArea {
            area_key: "frontend".into(),
            path_scopes: vec!["web/".into()],
            languages: vec!["TypeScript".into()],
            roles: vec!["frontend".into()],
            frameworks: vec!["React".into()],
            key_libraries: vec!["zod".into()],
            confidence: 0.95,
            evidence: vec!["web/package.json".into()],
        }],
    }
}

async fn get(
    app: &axum::Router,
    project_id: &str,
    cookie: Option<&str>,
) -> axum::response::Response {
    request(
        app,
        Request::builder().uri(format!("/api/projects/{project_id}/readiness")),
        cookie,
        Body::empty(),
    )
    .await
}

async fn detail(
    app: &axum::Router,
    project_id: &str,
    run_id: &str,
    cookie: Option<&str>,
) -> axum::response::Response {
    request(
        app,
        Request::builder().uri(format!("/api/projects/{project_id}/readiness/{run_id}")),
        cookie,
        Body::empty(),
    )
    .await
}

async fn post_kickoff(
    app: &axum::Router,
    project_id: &str,
    cookie: Option<&str>,
    key: &str,
) -> axum::response::Response {
    request(
        app,
        Request::builder()
            .method("POST")
            .uri(format!("/api/projects/{project_id}/readiness/kickoff"))
            .header(header::CONTENT_TYPE, "application/json"),
        cookie,
        Body::from(serde_json::json!({ "idempotency_key": key }).to_string()),
    )
    .await
}

async fn request(
    app: &axum::Router,
    mut builder: axum::http::request::Builder,
    cookie: Option<&str>,
    body: Body,
) -> axum::response::Response {
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("djinn_session={cookie}"));
    }
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn assert_status(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(response_json(response).await["code"], code);
}
