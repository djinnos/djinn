use djinn_provider::github_api::GitHubApiClient;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn artifact_routes_are_bound_to_the_task_repository() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/task-owner/task-repository/actions/runs/42/artifacts"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "artifacts": [{"id": 7, "name": "report", "size_in_bytes": 3, "expired": false, "expires_at": null}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = GitHubApiClient::for_user_token_with_base_url("token".into(), server.uri());
    let page = client
        .list_run_artifacts("task-owner", "task-repository", 42)
        .await
        .unwrap();
    assert_eq!(page.artifacts[0].id, 7);
    assert!(!page.truncated);
}

#[tokio::test]
async fn artifact_transport_exposes_read_only_routes_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/artifacts/7/zip",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"zip"))
        .expect(1)
        .mount(&server)
        .await;
    let client = GitHubApiClient::for_user_token_with_base_url("token".into(), server.uri());
    let download = client
        .download_artifact("task-owner", "task-repository", 42, 7)
        .await
        .unwrap();
    assert_eq!(download.bytes, b"zip");
}
