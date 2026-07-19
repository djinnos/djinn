use djinn_provider::github_api::{GitHubApiClient, GitHubErrorSource};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OWNER: &str = "task-owner";
const REPO: &str = "task-repository";
const RUN_ID: u64 = 42;
const ARTIFACT_ID: u64 = 7;

fn client(server: &MockServer) -> GitHubApiClient {
    GitHubApiClient::for_user_token_with_base_url("token".into(), server.uri())
}

fn artifacts(total_count: u64, count: u64) -> serde_json::Value {
    serde_json::json!({
        "total_count": total_count,
        "artifacts": (0..count).map(|id| serde_json::json!({
            "id": id + 1,
            "name": format!("artifact-{id}"),
            "size_in_bytes": id,
            "expired": false,
            "expires_at": null
        })).collect::<Vec<_>>()
    })
}

#[tokio::test]
async fn artifact_routes_are_bound_to_the_task_repository() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(artifacts(1, 1)))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap();
    assert_eq!(page.artifacts[0].id, 1);
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
    let download = client(&server)
        .download_artifact(OWNER, REPO, RUN_ID, ARTIFACT_ID)
        .await
        .unwrap();
    assert_eq!(download.bytes, b"zip");
}

#[tokio::test]
async fn artifact_listing_empty_page_succeeds_without_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(artifacts(0, 0)))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap();
    assert!(page.artifacts.is_empty());
    assert_eq!(page.total_count, 0);
    assert!(!page.truncated);
}

#[tokio::test]
async fn artifact_listing_exactly_one_hundred_preserves_github_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(artifacts(100, 100)))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap();
    assert_eq!(page.artifacts.len(), 100);
    assert_eq!(page.artifacts.first().unwrap().name, "artifact-0");
    assert_eq!(page.artifacts.last().unwrap().name, "artifact-99");
    assert!(!page.truncated);
}

#[tokio::test]
async fn artifact_listing_over_one_hundred_truncates_without_following_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(artifacts(101, 101)))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap();
    assert_eq!(page.total_count, 101);
    assert_eq!(page.artifacts.len(), 100);
    assert_eq!(page.artifacts.last().unwrap().name, "artifact-99");
    assert!(page.truncated);
}

#[tokio::test]
async fn artifact_http_errors_include_operation_and_run_context() {
    for (status, expected) in [
        (403, "Actions read permission"),
        (404, "missing or expired"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/repos/task-owner/task-repository/actions/artifacts/7/zip",
            ))
            .respond_with(ResponseTemplate::new(status).set_body_string("github response"))
            .mount(&server)
            .await;
        let error = client(&server)
            .download_artifact(OWNER, REPO, RUN_ID, ARTIFACT_ID)
            .await
            .unwrap_err();
        assert_eq!(error.method, "download_artifact");
        assert_eq!(error.status.unwrap().as_u16(), status);
        assert!(error.body.contains(expected));
        assert!(error.body.contains("run 42"));
    }
}

#[tokio::test]
async fn artifact_rate_limit_error_includes_operation_and_run_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", "0")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;
    let error = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap_err();
    assert_eq!(error.source, GitHubErrorSource::RateLimited);
    assert_eq!(error.method, "list_run_artifacts");
    assert!(error.body.contains("run 42"));
}

#[tokio::test]
async fn artifact_malformed_response_includes_run_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/runs/42/artifacts",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    let error = client(&server)
        .list_run_artifacts(OWNER, REPO, RUN_ID)
        .await
        .unwrap_err();
    assert_eq!(error.method, "list_run_artifacts");
    assert!(error.body.contains("malformed"));
    assert!(error.body.contains("run 42"));
}

#[tokio::test]
async fn artifact_download_over_compressed_budget_is_rejected_with_run_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/artifacts/7/zip",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 32 * 1024 * 1024 + 1]))
        .mount(&server)
        .await;
    let error = client(&server)
        .download_artifact(OWNER, REPO, RUN_ID, ARTIFACT_ID)
        .await
        .unwrap_err();
    assert_eq!(error.method, "download_artifact");
    assert!(error.body.contains("run 42"));
    assert!(error.body.contains("32 MiB"));
}

#[tokio::test]
async fn artifact_timeout_includes_operation_and_run_context() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/task-owner/task-repository/actions/artifacts/7/zip",
        ))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(31)))
        .mount(&server)
        .await;
    let error = client(&server)
        .download_artifact(OWNER, REPO, RUN_ID, ARTIFACT_ID)
        .await
        .unwrap_err();
    assert_eq!(error.source, GitHubErrorSource::Transport);
    assert_eq!(error.method, "download_artifact");
    assert!(error.body.contains("run 42"));
    assert!(error.body.to_ascii_lowercase().contains("timeout"));
}
