use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::GitHubApiClient;

use super::make_repo;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_with_retry_returns_reauth_error_on_401() {
    let server = MockServer::start().await;
    let repo = make_repo();

    let json = serde_json::json!({
        "access_token": "ghu_user",
        "user_login": "djinn-test",
    })
    .to_string();
    repo.set("github_app", "__OAUTH_GITHUB_APP", &json)
        .await
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let result = client.get_pull_request("djinnos", "server", 1).await;

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("please re-authenticate")
    );
}
