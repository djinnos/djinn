use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{GitHubApiClient, GitHubApiError, GitHubErrorSource, UserTokenRefresh};

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

/// Test-only refresher that hands out a fixed access token and counts
/// how many times the transport asked for a refresh.
#[derive(Clone, Default)]
struct CannedRefresher {
    new_token: String,
    calls: Arc<AtomicUsize>,
    fail: bool,
}

#[async_trait]
impl UserTokenRefresh for CannedRefresher {
    async fn refresh(&self) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(anyhow!("refresh upstream said no"))
        } else {
            Ok(self.new_token.clone())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_token_401_then_refresh_success_retries_with_new_token() {
    let server = MockServer::start().await;
    // First call with the original token: 401. The transport invokes
    // the refresher, gets `ghu_after_refresh`, and retries — that
    // second request hits the 200 branch below.
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/git/ref/heads/main"))
        .and(header("authorization", "Bearer ghu_before"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/git/ref/heads/main"))
        .and(header("authorization", "Bearer ghu_after_refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ref": "refs/heads/main",
            "object": {"sha": "rotated-sha"}
        })))
        .mount(&server)
        .await;

    let refresher = CannedRefresher {
        new_token: "ghu_after_refresh".to_string(),
        calls: Arc::new(AtomicUsize::new(0)),
        fail: false,
    };
    let call_counter = refresher.calls.clone();

    let client = GitHubApiClient::for_user_session_with_base_url(
        "ghu_before".to_string(),
        Arc::new(refresher),
        server.uri(),
    );
    let sha = client
        .get_ref("djinnos", "server", "heads/main")
        .await
        .expect("get_ref should succeed after refresh");
    assert_eq!(sha.as_deref(), Some("rotated-sha"));
    assert_eq!(
        call_counter.load(Ordering::SeqCst),
        1,
        "refresher should be invoked exactly once on the 401",
    );

    // Subsequent calls reuse the rotated token in the shared cell, so
    // they hit the 200 branch directly — no extra refresh.
    let again = client
        .get_ref("djinnos", "server", "heads/main")
        .await
        .expect("second call should succeed without another refresh");
    assert_eq!(again.as_deref(), Some("rotated-sha"));
    assert_eq!(
        call_counter.load(Ordering::SeqCst),
        1,
        "refresher must not be re-invoked once the new token is stored",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_token_401_refresh_failure_surfaces_expired() {
    // Refresher refuses → transport must surface UserTokenExpired so
    // the caller can route the user back to /auth/github/start.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/git/ref/heads/main"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let refresher = CannedRefresher {
        new_token: String::new(),
        calls: Arc::new(AtomicUsize::new(0)),
        fail: true,
    };

    let client = GitHubApiClient::for_user_session_with_base_url(
        "ghu_dead".to_string(),
        Arc::new(refresher.clone()),
        server.uri(),
    );
    let err = client
        .get_ref("djinnos", "server", "heads/main")
        .await
        .expect_err("expected refresh failure to bubble UserTokenExpired");
    let typed = err
        .downcast_ref::<GitHubApiError>()
        .expect("expected GitHubApiError downcast");
    assert_eq!(typed.source, GitHubErrorSource::Unauthenticated);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
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

    let typed = err
        .downcast_ref::<GitHubApiError>()
        .expect("expected GitHubApiError downcast");
    assert_eq!(typed.source, GitHubErrorSource::Unauthenticated);
}
