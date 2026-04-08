use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{CheckRunsResponse, CreatePrParams, GitHubApiClient, MergeMethod, PrState};

use super::{make_repo, seed_tokens};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pull_request_success() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(header("Authorization", "Bearer ghu_user"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "number": 42,
            "title": "feat: add feature",
            "state": "open",
            "merged": false,
            "html_url": "https://github.com/djinnos/server/pull/42",
            "head": { "ref": "feature-branch", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" },
            "auto_merge": null,
            "node_id": "PR_abc123"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let pr = client
        .create_pull_request(
            "djinnos",
            "server",
            CreatePrParams {
                title: "feat: add feature".into(),
                body: "Description".into(),
                head: "feature-branch".into(),
                base: "main".into(),
                maintainer_can_modify: None,
                draft: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(pr.number, 42);
    assert_eq!(pr.title, "feat: add feature");
    assert_eq!(pr.state, PrState::Open);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_auto_merge_success() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghu_user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "enablePullRequestAutoMerge": {
                    "pullRequest": {
                        "number": 42,
                        "title": "feat: add feature",
                        "autoMergeRequest": {
                            "enabledAt": "2026-01-01T00:00:00Z",
                            "mergeMethod": "SQUASH"
                        }
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let result = client
        .enable_auto_merge(
            "djinnos",
            "server",
            42,
            MergeMethod::Squash,
            "PR_node123",
            "chore(clbs): Phase 1: split extension params",
        )
        .await
        .unwrap();

    assert!(result["data"]["enablePullRequestAutoMerge"]["pullRequest"]["number"] == 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_pr_ready_for_review_uses_graphql_mutation() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghu_user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "markPullRequestReadyForReview": {
                    "pullRequest": { "number": 8, "isDraft": false }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let result = client.mark_pr_ready_for_review("PR_node456").await.unwrap();

    assert_eq!(
        result["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_pr_ready_for_review_propagates_graphql_error() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Resource not accessible by integration" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let err = client
        .mark_pr_ready_for_review("PR_node456")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("GraphQL error"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pull_request_success() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 42,
            "title": "feat: add feature",
            "state": "open",
            "merged": false,
            "html_url": "https://github.com/djinnos/server/pull/42",
            "head": { "ref": "feature-branch", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" },
            "auto_merge": null,
            "node_id": "PR_abc123"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(
            r"/repos/djinnos/server/commits/abc123/check-runs",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [{
                "id": 1,
                "name": "ci",
                "status": "completed",
                "conclusion": "success",
                "html_url": "https://github.com/checks/1"
            }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let (pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(pr.number, 42);
    assert_eq!(checks.total_count, 1);
    assert_eq!(checks.check_runs[0].conclusion.as_deref(), Some("success"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pr_returns_error_on_422() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "message": "A pull request already exists" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let result = client
        .create_pull_request(
            "djinnos",
            "server",
            CreatePrParams {
                title: "feat: dupe".into(),
                body: "".into(),
                head: "feature".into(),
                base: "main".into(),
                maintainer_can_modify: None,
                draft: None,
            },
        )
        .await;

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("422"), "expected 422 in error: {}", msg);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pulls_by_head_returns_matching_prs() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "djinnos:task/453b"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "number": 99,
                "title": "chore(453b): Move epic tools",
                "state": "open",
                "merged": false,
                "html_url": "https://github.com/djinnos/server/pull/99",
                "head": { "ref": "task/453b", "sha": "aaa111" },
                "base": { "ref": "main", "sha": "bbb222" },
                "auto_merge": null,
                "node_id": "PR_existing"
            }
        ])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let prs = client
        .list_pulls_by_head("djinnos", "server", "djinnos:task/453b")
        .await
        .unwrap();

    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].number, 99);
    assert_eq!(prs[0].html_url, "https://github.com/djinnos/server/pull/99");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pulls_by_head_returns_empty_when_no_match() {
    let server = MockServer::start().await;
    let repo = make_repo();
    seed_tokens(&repo, "ghu_user").await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let prs = client
        .list_pulls_by_head("djinnos", "server", "djinnos:no-such-branch")
        .await
        .unwrap();

    assert!(prs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_creds_returns_error() {
    let server = MockServer::start().await;
    let repo = make_repo();
    let client = GitHubApiClient::with_base_url(repo, server.uri());
    let result = client.get_pull_request("djinnos", "server", 1).await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("No GitHub App tokens"),
        "unexpected error: {}",
        msg
    );
}
