//! HTTP-level contract coverage for the browser readiness surface.

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use djinn_db::{
    ProjectRepository, ReadinessAreaResultCallback, ReadinessCallbackOutcome, ReadinessRepository,
    RepoGraphCacheInsert, RepoGraphCacheRepository, UserRepository,
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
async fn readiness_detail_route_serializes_the_authenticated_two_area_owner_run() {
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
    let repository = ReadinessRepository::new(db.clone());
    let fanout = repository
        .complete_identification(&run_id, &owner_id, two_area_identification())
        .await
        .expect("freeze the deterministic two-area run");
    let frontend = fanout
        .iter()
        .find(|area| area.area.area_key == "frontend")
        .unwrap();
    let backend = fanout
        .iter()
        .find(|area| area.area.area_key == "backend")
        .unwrap();
    assert_eq!(
        repository
            .ingest_area_result(callback(
                &run_id,
                frontend,
                frontend_result(&frontend.area.id)
            ))
            .await
            .unwrap(),
        ReadinessCallbackOutcome::Accepted
    );
    assert_eq!(
        repository
            .ingest_area_result(callback(&run_id, backend, backend_result(&backend.area.id)))
            .await
            .unwrap(),
        ReadinessCallbackOutcome::Accepted
    );
    repository
        .aggregate_run(&run_id, "authenticated-http-fixture")
        .await
        .unwrap();

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
    assert_eq!(json["areas"].as_array().unwrap().len(), 2);
    let frontend_json = json["areas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|area| area["area_key"] == "frontend")
        .unwrap();
    let backend_json = json["areas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|area| area["area_key"] == "backend")
        .unwrap();
    assert_eq!(
        frontend_json["composition"]["languages"],
        serde_json::json!(["TypeScript"])
    );
    assert_eq!(
        backend_json["composition"]["languages"],
        serde_json::json!(["Rust"])
    );
    assert_eq!(frontend_json["attempts"][0]["is_current"], true);
    assert_eq!(backend_json["attempts"][0]["is_current"], true);
    assert_eq!(
        frontend_json["accepted_findings"][0]["evidence"][0],
        serde_json::json!({"path":"web/auth.ts","line":12})
    );
    assert_eq!(
        backend_json["accepted_findings"][0]["evidence"][0],
        serde_json::json!({"path":"server/src/auth.rs","line":48})
    );
    assert!(
        json["area_scores"]
            .as_array()
            .unwrap()
            .iter()
            .any(|score| score["score"] == 0.8)
    );
    assert!(
        json["area_scores"]
            .as_array()
            .unwrap()
            .iter()
            .any(|score| score["score"] == 0.75)
    );
    assert_eq!(json["project_score"]["band"], "ready");
    assert_eq!(json["suggestions"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["suggestions"][0]["dedupe_key"],
        "shared-auth-remediation"
    );
    assert!(
        json["events"]
            .as_array()
            .expect("events serialize as an array")
            .iter()
            .any(|event| event["event_kind"] == "readiness_aggregated")
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

fn two_area_identification() -> ReadinessIdentificationOutput {
    ReadinessIdentificationOutput {
        areas: vec![
            ReadinessIdentifiedArea {
                area_key: "frontend".into(),
                path_scopes: vec!["web/".into()],
                languages: vec!["TypeScript".into()],
                roles: vec!["frontend".into()],
                frameworks: vec!["React".into()],
                key_libraries: vec!["zod".into()],
                confidence: 0.95,
                evidence: vec!["web/package.json".into()],
            },
            ReadinessIdentifiedArea {
                area_key: "backend".into(),
                path_scopes: vec!["server/".into()],
                languages: vec!["Rust".into()],
                roles: vec!["backend".into()],
                frameworks: vec!["Axum".into()],
                key_libraries: vec!["sqlx".into()],
                confidence: 0.97,
                evidence: vec!["server/Cargo.toml".into()],
            },
        ],
    }
}

fn callback(
    run_id: &str,
    area: &djinn_db::repositories::readiness::ReadinessAreaFanout,
    result: Value,
) -> ReadinessAreaResultCallback {
    ReadinessAreaResultCallback {
        run_id: run_id.into(),
        area_id: area.area.id.clone(),
        attempt_id: area.attempt.id.clone(),
        correlation_key: area.attempt.correlation_key.clone(),
        task_id: area.task.id.clone(),
        status: "succeeded".into(),
        result,
    }
}

fn frontend_result(area_id: &str) -> Value {
    serde_json::json!({"findings":[{"guardrail_key":"frontend-auth","status":"covered","severity":"high","confidence":0.95,"evidence":[{"path":"web/auth.ts","line":12}]},{"guardrail_key":"frontend-inputs","status":"partial","severity":"medium","confidence":0.80,"evidence":[{"path":"web/forms.ts","line":31}]}],"unsupported":[{"guardrail_key":"browser-session","reason":"not applicable to this frontend"}],"warnings":[{"reason":"legacy form remains outside migration scope"}],"remediation_suggestions":[{"dedupe_key":"shared-auth-remediation","action":"Apply shared authentication remediation","area_id":area_id,"guardrail_id":"frontend-auth","validation_guidance":"Verify web/auth.ts and server/src/auth.rs after the change."}]})
}

fn backend_result(area_id: &str) -> Value {
    serde_json::json!({"findings":[{"guardrail_key":"backend-auth","status":"covered","severity":"high","confidence":0.90,"evidence":[{"path":"server/src/auth.rs","line":48}]},{"guardrail_key":"backend-secrets","status":"analysis_error","severity":"low","confidence":0.85,"evidence":[{"path":"server/src/config.rs","line":9}]}],"errors":[{"reason":"secret rotation is not configured"}],"warnings":[{"reason":"secret rotation is not configured"}],"remediation_suggestions":[{"dedupe_key":"shared-auth-remediation","action":"Apply shared authentication remediation","area_id":area_id,"guardrail_id":"backend-auth","validation_guidance":"Verify web/auth.ts and server/src/auth.rs after the change."}]})
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
