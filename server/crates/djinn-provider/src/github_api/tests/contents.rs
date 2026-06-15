use djinn_core::tool_error::ErrorClass;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{GitHubApiClient, GitHubWriteErrorEnvelope};

use super::seed_installation_token;

fn create_ref_body() -> (&'static str, &'static str) {
    (
        "refs/heads/task/test",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
}

async fn create_ref_error(status: u16, body: serde_json::Value) -> GitHubWriteErrorEnvelope {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    let (ref_name, sha) = create_ref_body();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/git/refs"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .create_ref("djinnos", "server", ref_name, sha)
        .await
        .expect_err("create_ref should surface a typed envelope");

    err.downcast_ref::<GitHubWriteErrorEnvelope>()
        .expect("create_ref error should be a GitHubWriteErrorEnvelope")
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_422_reference_already_exists_is_idempotent_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    let (ref_name, sha) = create_ref_body();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/git/refs"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Reference already exists",
            "errors": [{"resource": "Reference", "code": "already_exists"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    client
        .create_ref("djinnos", "server", ref_name, sha)
        .await
        .expect("422 Reference already exists must remain idempotent success");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_422_validation_failure_is_typed_validation() {
    let err = create_ref_error(
        422,
        serde_json::json!({
            "message": "Validation Failed",
            "errors": [{"resource": "Reference", "field": "sha", "code": "invalid"}]
        }),
    )
    .await;

    assert_eq!(err.error_class, Some(ErrorClass::Validation));
    assert_eq!(err.status.as_deref(), Some("422"));
    assert_eq!(err.method.as_deref(), Some("POST"));
    assert_eq!(err.path.as_deref(), Some("/repos/djinnos/server/git/refs"));
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("Fix the rejected GitHub request parameters"),
        "hint was: {:?}",
        err.hint
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_404_is_typed_not_found() {
    let err = create_ref_error(404, serde_json::json!({"message": "Not Found"})).await;

    assert_eq!(err.error_class, Some(ErrorClass::NotFound));
    assert_eq!(err.status.as_deref(), Some("404"));
    assert_eq!(err.method.as_deref(), Some("POST"));
    assert_eq!(err.path.as_deref(), Some("/repos/djinnos/server/git/refs"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_permission_and_rate_limit_classify_from_response_detail() {
    let permission = create_ref_error(
        403,
        serde_json::json!({"message": "Resource not accessible by integration"}),
    )
    .await;
    assert_eq!(permission.error_class, Some(ErrorClass::Permission));
    assert_eq!(permission.status.as_deref(), Some("403"));

    let rate_limited = create_ref_error(
        403,
        serde_json::json!({"message": "API rate limit exceeded for installation"}),
    )
    .await;
    assert_eq!(rate_limited.error_class, Some(ErrorClass::RateLimited));
    assert_eq!(rate_limited.status.as_deref(), Some("403"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_statusless_transport_failure_is_typed_internal() {
    let install_id = seed_installation_token();
    let client = GitHubApiClient::for_installation_with_base_url(
        install_id,
        "http://127.0.0.1:1".to_string(),
    );
    let (ref_name, sha) = create_ref_body();

    let err = client
        .create_ref("djinnos", "server", ref_name, sha)
        .await
        .expect_err("unreachable transport should fail");
    let envelope = err
        .downcast_ref::<GitHubWriteErrorEnvelope>()
        .expect("transport failure should become a GitHubWriteErrorEnvelope");

    assert_eq!(envelope.error_class, Some(ErrorClass::Internal));
    assert_eq!(envelope.status, None);
    assert_eq!(envelope.method.as_deref(), Some("POST"));
    assert_eq!(
        envelope.path.as_deref(),
        Some("/repos/djinnos/server/git/refs")
    );
    assert!(envelope.compact().contains("status=none"));
    assert!(envelope.compact().contains("error_class=internal"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_long_failure_body_is_bounded_in_rendered_detail() {
    let long_tail = "x".repeat(500);
    let err = create_ref_error(
        422,
        serde_json::json!({
            "message": format!("Validation Failed {long_tail}"),
            "errors": [{"resource": "Reference", "code": "invalid"}]
        }),
    )
    .await;
    let rendered = err.compact();

    assert_eq!(err.error_class, Some(ErrorClass::Validation));
    assert!(rendered.contains("method=POST"));
    assert!(rendered.contains("path=/repos/djinnos/server/git/refs"));
    assert!(rendered.contains("status=422"));
    assert!(rendered.contains("error_class=validation"));
    assert!(rendered.contains("body="));
    assert!(rendered.contains('…'));
    assert!(
        err.body.as_ref().unwrap().chars().count() <= 241,
        "body excerpt was not bounded: {:?}",
        err.body
    );
    assert!(
        !rendered.contains(&"x".repeat(300)),
        "long body tail should be bounded: {rendered}"
    );
}
