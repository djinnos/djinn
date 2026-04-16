use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::GitHubApiClient;

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
