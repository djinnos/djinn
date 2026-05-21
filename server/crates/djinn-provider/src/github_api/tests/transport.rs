use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{GitHubApiClient, UserTokenExpired};

use super::seed_installation_token;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_with_retry_refreshes_installation_token_on_401() {
    // Installation-scoped clients retry once with a fresh token after a 401.
    // Our mocked server responds 401 every time, so we expect an error after
    // the retry — and the error surfaces the downstream failure, not the
    // legacy "re-authenticate" message.
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let result = client.get_pull_request("djinnos", "server", 1).await;

    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_token_client_sends_literal_bearer() {
    // Use `get_ref` — a single-call endpoint, not the composite
    // `get_pull_request` (which fans out to /check-runs and would need
    // multiple mocks). The header matcher proves the literal token (no
    // installation-mint indirection) ended up in the Authorization header.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/git/ref/heads/main"))
        .and(header("authorization", "Bearer ghu_literal_token_xyz"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ref": "refs/heads/main",
            "object": {"sha": "abc123"}
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_user_token_with_base_url(
        "ghu_literal_token_xyz".to_string(),
        server.uri(),
    );
    let sha = client
        .get_ref("djinnos", "server", "heads/main")
        .await
        .expect("get_ref");
    assert_eq!(sha.as_deref(), Some("abc123"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_token_401_surfaces_typed_expired_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_user_token_with_base_url(
        "ghu_expired_token".to_string(),
        server.uri(),
    );
    let err = client
        .get_ref("djinnos", "server", "heads/main")
        .await
        .expect_err("expected 401 to fail");

    assert!(
        err.downcast_ref::<UserTokenExpired>().is_some(),
        "expected UserTokenExpired downcast, got: {err:?}"
    );
}
