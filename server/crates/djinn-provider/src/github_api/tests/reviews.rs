use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::GitHubApiClient;

use super::seed_installation_token;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pull_request_reviews_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();


    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 1,
                "user": { "login": "reviewer", "id": 999 },
                "body": "LGTM!",
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z",
                "html_url": "https://github.com/djinnos/server/pull/42#comment-1",
                "pull_request_review_id": 100,
                "path": "src/lib.rs",
                "line": 42
            }
        ])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let comments = client
        .list_pull_request_reviews("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "LGTM!");
    assert_eq!(comments[0].user.as_ref().unwrap().login, "reviewer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_pr_review_feedback_aggregates_change_requests_and_comments() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();


    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 1,
                "user": { "login": "reviewer-a", "id": 11 },
                "state": "CHANGES_REQUESTED",
                "submitted_at": "2024-01-01T00:00:00Z",
                "html_url": "https://github.com/reviews/1",
                "body": "Please fix this"
            },
            {
                "id": 2,
                "user": { "login": "reviewer-b", "id": 12 },
                "state": "APPROVED",
                "submitted_at": "2024-01-01T01:00:00Z",
                "html_url": "https://github.com/reviews/2",
                "body": "Looks good"
            }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 3,
                "user": { "login": "reviewer-a", "id": 11 },
                "body": "Nit: rename this",
                "created_at": "2024-01-01T00:00:00Z",
                "updated_at": "2024-01-01T00:00:00Z",
                "html_url": "https://github.com/comments/3",
                "pull_request_review_id": 1,
                "path": "src/lib.rs",
                "line": 10
            }
        ])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let feedback = client
        .fetch_pr_review_feedback(
            "djinnos",
            "server",
            42,
            "https://github.com/djinnos/server/pull/42",
        )
        .await
        .unwrap();

    assert_eq!(feedback.change_request_reviews.len(), 1);
    assert_eq!(
        feedback.change_request_reviews[0].state,
        "CHANGES_REQUESTED"
    );
    assert_eq!(feedback.inline_comments.len(), 1);
    assert_eq!(feedback.inline_comments[0].body, "Nit: rename this");
}
