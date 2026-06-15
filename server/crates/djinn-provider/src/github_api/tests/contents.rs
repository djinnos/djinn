use reqwest::StatusCode;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{GitHubApiClient, GitHubApiError, GitHubErrorSource};

use super::seed_installation_token;

fn create_ref_body() -> (&'static str, &'static str) {
    (
        "refs/heads/task/test",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
}

async fn create_ref_error(status: u16, body: serde_json::Value) -> GitHubApiError {
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
    client
        .create_ref("djinnos", "server", ref_name, sha)
        .await
        .expect_err("create_ref should surface a typed error")
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
async fn create_ref_422_validation_failure_is_typed_http() {
    let err = create_ref_error(
        422,
        serde_json::json!({
            "message": "Validation Failed",
            "errors": [{"resource": "Reference", "field": "sha", "code": "invalid"}]
        }),
    )
    .await;

    assert_eq!(err.source, GitHubErrorSource::Http);
    assert_eq!(err.status, Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert_eq!(err.method, "create_ref");
    assert_eq!(err.path, "/repos/djinnos/server/git/refs");
    assert!(err.body.contains("Validation Failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_404_is_typed_not_found() {
    let err = create_ref_error(404, serde_json::json!({"message": "Not Found"})).await;

    assert_eq!(err.source, GitHubErrorSource::Http);
    assert_eq!(err.status, Some(StatusCode::NOT_FOUND));
    assert_eq!(err.method, "create_ref");
    assert_eq!(err.path, "/repos/djinnos/server/git/refs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_permission_and_rate_limit_classify_from_status() {
    let permission = create_ref_error(
        403,
        serde_json::json!({"message": "Resource not accessible by integration"}),
    )
    .await;
    assert_eq!(permission.source, GitHubErrorSource::Http);
    assert_eq!(permission.status, Some(StatusCode::FORBIDDEN));

    let rate_limited = create_ref_error(
        429,
        serde_json::json!({"message": "API rate limit exceeded for installation"}),
    )
    .await;
    assert_eq!(rate_limited.source, GitHubErrorSource::Transport);
    assert_eq!(rate_limited.status, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_statusless_transport_failure_is_typed_transport() {
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

    assert_eq!(err.source, GitHubErrorSource::Transport);
    assert_eq!(err.status, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_ref_long_failure_body_preserves_raw_detail() {
    let long_tail = "x".repeat(500);
    let err = create_ref_error(
        422,
        serde_json::json!({
            "message": format!("Validation Failed {long_tail}"),
            "errors": [{"resource": "Reference", "code": "invalid"}]
        }),
    )
    .await;

    assert_eq!(err.source, GitHubErrorSource::Http);
    assert_eq!(err.status, Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert!(err.body.contains(&"x".repeat(300)));
    assert!(err.to_string().contains("…[+"));
}
