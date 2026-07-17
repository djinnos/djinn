use djinn_core::tool_error::ErrorClass;
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{
    CheckRunsResponse, CreatePrParams, GitHubApiClient, GitHubApiError, MergeMethod, PrFile,
    PrState,
};

use super::seed_installation_token;

fn github_write_envelope(err: &GitHubApiError) -> &GitHubApiError {
    err
}

fn assert_github_write_envelope(
    err: &GitHubApiError,
    operation: &str,
    path: &str,
    status: &str,
    _error_class: ErrorClass,
    body_contains: &str,
) {
    let envelope = github_write_envelope(err);
    let _ = operation;
    assert_eq!(envelope.path, path);
    assert_eq!(
        envelope.status.map(|s| s.as_u16().to_string()).as_deref(),
        Some(status)
    );
    assert!(envelope.body.contains(body_contains));
    assert!(
        envelope.body.chars().count() <= 1000,
        "body should be present and reasonably sized: {:?}",
        envelope.body
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_auto_merge_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
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

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
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
async fn enqueue_pull_request_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "enqueuePullRequest": {
                    "mergeQueueEntry": { "id": "MQE_test", "state": "QUEUED" }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    client
        .enqueue_pull_request("PR_node789", "abc123def456")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_pull_request_propagates_graphql_error() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Pull request is not in a mergeable state" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .enqueue_pull_request("PR_node789", "abc123def456")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("GraphQL error"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_pr_ready_for_review_uses_graphql_mutation() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "markPullRequestReadyForReview": {
                    "pullRequest": { "number": 8, "isDraft": false }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let result = client.mark_pr_ready_for_review("PR_node456").await.unwrap();

    assert_eq!(
        result["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mark_pr_ready_for_review_propagates_graphql_error() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Resource not accessible by integration" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .mark_pr_ready_for_review("PR_node456")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("GraphQL error"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pull_request_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

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
        .and(query_param("per_page", "100"))
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

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(pr.number, 42);
    assert_eq!(checks.total_count, 1);
    assert_eq!(checks.check_runs[0].conclusion.as_deref(), Some("success"));
}

/// A merged PR exposes the landed `merge_commit_sha` so the PR poller can
/// persist it on the task (the board's Merged column gates on it). Regression
/// for the field silently never being read — closed tasks then never showed as
/// merged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pull_request_exposes_merge_commit_sha_when_merged() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 42,
            "title": "feat: add feature",
            "state": "closed",
            "merged": true,
            "merge_commit_sha": "0123456789abcdef0123456789abcdef01234567",
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
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 0,
            "check_runs": []
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (pr, _checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(pr.merged, Some(true));
    assert_eq!(
        pr.merge_commit_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}

/// A PR with more than one page of check runs must be fully aggregated:
/// the client requests `per_page=100` and pages through `page=1`, `page=2`,
/// ... until a short page signals the end — instead of silently dropping
/// everything past GitHub's default first page.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pull_request_paginates_check_runs() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

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

    // Page 1: a full page of 100 runs (forces a follow-up request).
    let page1_runs: Vec<serde_json::Value> = (0..100)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "name": format!("ci-{i}"),
                "status": "completed",
                "conclusion": "success",
                "html_url": format!("https://github.com/checks/{i}")
            })
        })
        .collect();

    Mock::given(method("GET"))
        .and(path_regex(
            r"/repos/djinnos/server/commits/abc123/check-runs",
        ))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 142,
            "check_runs": page1_runs,
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Page 2: a short page of 42 runs (signals the end).
    let page2_runs: Vec<serde_json::Value> = (100..142)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "name": format!("ci-{i}"),
                "status": "completed",
                "conclusion": "failure",
                "html_url": format!("https://github.com/checks/{i}")
            })
        })
        .collect();

    Mock::given(method("GET"))
        .and(path_regex(
            r"/repos/djinnos/server/commits/abc123/check-runs",
        ))
        .and(query_param("per_page", "100"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 142,
            "check_runs": page2_runs,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    // All 142 runs across both pages must be aggregated, not just the first 100.
    assert_eq!(checks.check_runs.len(), 142);
    assert_eq!(checks.total_count, 142);
    // Run from the second page made it through.
    assert!(checks.check_runs.iter().any(|r| r.name == "ci-141"));
    assert_eq!(
        checks
            .check_runs
            .iter()
            .filter(|r| r.conclusion.as_deref() == Some("failure"))
            .count(),
        42
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pr_returns_error_on_422() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "message": "A pull request already exists" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
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

    let err = result.expect_err("unadopted 422 should return typed envelope");
    assert_github_write_envelope(
        &err,
        "POST",
        "/repos/djinnos/server/pulls",
        "422",
        ErrorClass::ConflictRecoverable,
        "already exists",
    );
    assert!(err.is_pr_already_exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pull_request_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("PATCH"))
        .and(path("/repos/djinnos/server/pulls/42"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 42,
            "title": "feat: add feature",
            "state": "closed",
            "merged": false,
            "html_url": "https://github.com/djinnos/server/pull/42",
            "head": { "ref": "feature-branch", "sha": "abc123" },
            "base": { "ref": "main", "sha": "def456" },
            "auto_merge": null,
            "node_id": "PR_abc123"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let pr = client
        .close_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(pr.number, 42);
    assert_eq!(pr.state, PrState::Closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_pull_request_failure_returns_typed_envelope_with_captured_404_body() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("PATCH"))
        .and(path("/repos/djinnos/server/pulls/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com/rest/pulls/pulls#update-a-pull-request"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .close_pull_request("djinnos", "server", 404)
        .await
        .expect_err("404 should return typed envelope")
        .downcast::<GitHubApiError>()
        .expect("close_pull_request should wrap GitHubApiError");

    assert_github_write_envelope(
        &err,
        "PATCH",
        "/repos/djinnos/server/pulls/404",
        "404",
        ErrorClass::NotFound,
        "Not Found",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pr_comment_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/issues/42/comments"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 123456,
            "body": "Closing stale PR: backing task is closed.",
            "user": { "login": "djinn-bot" },
            "html_url": "https://github.com/djinnos/server/issues/42#issuecomment-123456"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let comment = client
        .create_pr_comment(
            "djinnos",
            "server",
            42,
            "Closing stale PR: backing task is closed.",
        )
        .await
        .unwrap();

    assert_eq!(comment["id"], 123456);
    assert_eq!(comment["body"], "Closing stale PR: backing task is closed.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pr_comment_failure_returns_typed_envelope() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/issues/42/comments"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by integration"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .create_pr_comment("djinnos", "server", 42, "test body")
        .await
        .expect_err("403 should return typed envelope")
        .downcast::<GitHubApiError>()
        .expect("create_pr_comment should wrap GitHubApiError");

    assert_github_write_envelope(
        &err,
        "POST",
        "/repos/djinnos/server/issues/42/comments",
        "403",
        ErrorClass::Permission,
        "Resource not accessible",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_pull_request_failure_returns_typed_envelope_with_captured_404_body() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("PATCH"))
        .and(path("/repos/djinnos/server/pulls/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com/rest/pulls/pulls#update-a-pull-request"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .reopen_pull_request("djinnos", "server", 404)
        .await
        .expect_err("404 should return typed envelope")
        .downcast::<GitHubApiError>()
        .expect("reopen_pull_request should wrap GitHubApiError");

    assert_github_write_envelope(
        &err,
        "PATCH",
        "/repos/djinnos/server/pulls/404",
        "404",
        ErrorClass::NotFound,
        "Not Found",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_auto_merge_failure_returns_typed_envelope() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by integration"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .enable_auto_merge(
            "djinnos",
            "server",
            42,
            MergeMethod::Squash,
            "PR_node123",
            "feat: merge when ready",
        )
        .await
        .expect_err("403 should return typed envelope");

    assert_github_write_envelope(
        &err,
        "POST",
        "/graphql",
        "403",
        ErrorClass::Permission,
        "Resource not accessible",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_pull_request_branch_failure_returns_typed_bounded_envelope() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    let long_tail = "x".repeat(500);

    Mock::given(method("PUT"))
        .and(path("/repos/djinnos/server/pulls/42/update-branch"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": format!("Validation Failed: expected_head_sha does not match {long_tail}")
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .update_pull_request_branch("djinnos", "server", 42, "abc123")
        .await
        .expect_err("422 should return typed envelope");

    assert_github_write_envelope(
        &err,
        "PUT",
        "/repos/djinnos/server/pulls/42/update-branch",
        "422",
        ErrorClass::Validation,
        "expected_head_sha",
    );
    assert!(err.body.contains(&"x".repeat(300)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_pull_request_failure_returns_typed_envelope_for_repository_rules() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("PUT"))
        .and(path("/repos/djinnos/server/pulls/42/merge"))
        .respond_with(ResponseTemplate::new(405).set_body_json(serde_json::json!({
            "message": "Pull Request is not mergeable: repository rules require merge queue"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .merge_pull_request("djinnos", "server", 42, MergeMethod::Squash, "feat: merge")
        .await
        .expect_err("405 should return typed envelope");

    assert_github_write_envelope(
        &err,
        "PUT",
        "/repos/djinnos/server/pulls/42/merge",
        "405",
        ErrorClass::Validation,
        "repository rules require merge queue",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_pulls_by_head_returns_matching_prs() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

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

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
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
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let prs = client
        .list_pulls_by_head("djinnos", "server", "djinnos:no-such-branch")
        .await
        .unwrap();

    assert!(prs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_pull_request_success_pins_commit_id() {
    // Auto-approve path. The matcher asserts BOTH that the bearer is the
    // literal user token (not an installation mint) AND that the body pins
    // `commit_id` so a subsequent push invalidates the approval.
    use wiremock::matchers::body_json;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls/501/reviews"))
        .and(header("Authorization", "Bearer ghu_user_xyz"))
        .and(body_json(serde_json::json!({
            "commit_id": "deadbeef1234",
            "event": "APPROVE",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 999,
            "state": "APPROVED",
            "commit_id": "deadbeef1234"
        })))
        .mount(&server)
        .await;

    let client =
        GitHubApiClient::for_user_token_with_base_url("ghu_user_xyz".to_string(), server.uri());
    client
        .approve_pull_request("djinnos", "server", 501, "deadbeef1234")
        .await
        .expect("approve_pull_request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_pull_request_422_self_approval_surfaces_error() {
    // GitHub returns 422 with body `{"message":"Can not approve your own
    // pull request"}` when the approver authored a commit on the PR.
    // Caller (pr_poller) suppresses retries on this SHA; this test just
    // verifies the error surfaces with the status + body intact.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls/502/reviews"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Can not approve your own pull request",
        })))
        .mount(&server)
        .await;

    let client =
        GitHubApiClient::for_user_token_with_base_url("ghu_self".to_string(), server.uri());
    let err = client
        .approve_pull_request("djinnos", "server", 502, "abc")
        .await
        .expect_err("expected 422 to fail");
    let msg = err.to_string();
    assert!(msg.contains("422"), "error should mention 422: {msg}");
    assert!(
        msg.contains("Can not approve your own pull request"),
        "error should preserve body: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_pull_request_401_surfaces_user_token_expired() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls/503/reviews"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client =
        GitHubApiClient::for_user_token_with_base_url("ghu_expired".to_string(), server.uri());
    let err = client
        .approve_pull_request("djinnos", "server", 503, "abc")
        .await
        .expect_err("expected 401 to fail");
    let typed = err
        .downcast_ref::<GitHubApiError>()
        .expect("expected GitHubApiError downcast");
    assert_eq!(
        typed.source,
        crate::github_api::GitHubErrorSource::Unauthenticated
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disable_auto_merge_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "disablePullRequestAutoMerge": {
                    "pullRequest": { "number": 42, "title": "feat: add feature" }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    client
        .disable_auto_merge("PR_node123")
        .await
        .expect("disable_auto_merge should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disable_auto_merge_propagates_graphql_error() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{
                "type": "FORBIDDEN",
                "message": "Pull request Auto merge is not allowed on this repository"
            }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .disable_auto_merge("PR_node123")
        .await
        .expect_err("GraphQL error should propagate");
    assert!(format!("{err}").contains("not allowed on this repository"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dequeue_pull_request_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "dequeuePullRequest": {
                    "mergeQueueEntry": null
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    client
        .dequeue_pull_request("PR_node123")
        .await
        .expect("dequeue_pull_request should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pr_merge_queue_state_returns_queued_entry() {
    use crate::github_api::MergeQueueEntryState;

    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "mergeStateStatus": "CLEAN",
                        "autoMergeRequest": {
                            "enabledAt": "2026-05-21T22:00:00Z",
                            "mergeMethod": "SQUASH"
                        },
                        "mergeQueueEntry": {
                            "id": "MQE_abc",
                            "state": "QUEUED",
                            "position": 2,
                            "estimatedTimeToMerge": 180,
                            "solo": false
                        },
                        "timelineItems": { "nodes": [] }
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let state = client
        .get_pr_merge_queue_state("djinnos", "server", 42)
        .await
        .expect("state fetch should succeed");

    assert_eq!(state.merge_state_status.as_deref(), Some("CLEAN"));
    let entry = state.merge_queue_entry.expect("queue entry present");
    assert_eq!(entry.id, "MQE_abc");
    assert_eq!(entry.state, MergeQueueEntryState::Queued);
    assert_eq!(entry.position, Some(2));
    assert_eq!(entry.estimated_time_to_merge, Some(180));
    assert!(state.auto_merge_request.is_some());
    assert!(state.last_dequeue.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pr_merge_queue_state_returns_dequeue_event_when_kicked() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "mergeStateStatus": "BLOCKED",
                        "autoMergeRequest": null,
                        "commits": {
                            "nodes": [
                                { "commit": { "committedDate": "2026-05-21T22:10:00Z", "pushedDate": "2026-05-21T22:12:00Z" } }
                            ]
                        },
                        "mergeQueueEntry": null,
                        "timelineItems": {
                            "nodes": [
                                {
                                    "__typename": "RemovedFromMergeQueueEvent",
                                    "reason": "CHECKS_FAILED",
                                    "createdAt": "2026-05-21T22:30:00Z",
                                    "beforeCommit": { "oid": "abc123def456" }
                                }
                            ]
                        }
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let state = client
        .get_pr_merge_queue_state("djinnos", "server", 42)
        .await
        .expect("state fetch should succeed");

    assert!(state.merge_queue_entry.is_none());
    assert!(state.auto_merge_request.is_none());
    let dequeue = state.last_dequeue.expect("dequeue event present");
    assert_eq!(dequeue.reason.as_deref(), Some("CHECKS_FAILED"));
    assert_eq!(dequeue.created_at.as_deref(), Some("2026-05-21T22:30:00Z"));
    assert_eq!(dequeue.before_commit_sha.as_deref(), Some("abc123def456"));
    // pushedDate preferred over committedDate when both are present.
    assert_eq!(
        state.head_committed_at.as_deref(),
        Some("2026-05-21T22:12:00Z")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pr_merge_queue_state_handles_missing_pr() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "repository": { "pullRequest": null } }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .get_pr_merge_queue_state("djinnos", "server", 42)
        .await
        .expect_err("missing PR should error");
    assert!(format!("{err}").contains("pullRequest not found"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_required_status_checks_returns_contexts() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path(
            "/repos/getalternative/alt-front-end/branches/main/protection/required_status_checks",
        ))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "strict": true,
            "contexts": ["unit tests", "Sentinel"],
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let contexts = client
        .list_required_status_checks("getalternative", "alt-front-end", "main")
        .await
        .unwrap();
    assert_eq!(
        contexts,
        Some(vec!["unit tests".to_string(), "Sentinel".to_string()])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_required_status_checks_404_is_none() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path_regex(
            r"/repos/.+/branches/.+/protection/required_status_checks",
        ))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "message": "Branch not protected"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let contexts = client
        .list_required_status_checks("o", "r", "main")
        .await
        .unwrap();
    assert_eq!(
        contexts, None,
        "404 (no protection) maps to None for fallback"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_required_status_checks_403_errors() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path_regex(
            r"/repos/.+/branches/.+/protection/required_status_checks",
        ))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by integration"
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    // 403 (no admin perm) must Err so the caller falls back to the heuristic,
    // not silently treat every check as non-blocking.
    let err = client
        .list_required_status_checks("o", "r", "main")
        .await
        .expect_err("403 must surface as an error");
    assert!(format!("{err}").contains("403"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compare_commits_ahead_by_parses_count() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/o/r/compare/main...58f2d2b75e6d"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ahead_by": 0,
            "behind_by": 3,
            "status": "behind",
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let ahead = client
        .compare_commits_ahead_by("o", "r", "main", "58f2d2b75e6d")
        .await
        .unwrap();
    assert_eq!(ahead, 0, "diff-empty branch reports ahead_by == 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_unresolved_review_thread_ids_returns_only_unresolved() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                { "id": "RT_unresolved_1", "isResolved": false },
                                { "id": "RT_resolved", "isResolved": true },
                                { "id": "RT_unresolved_2", "isResolved": false }
                            ]
                        }
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let ids = client
        .list_unresolved_review_thread_ids("djinnos", "server", 42)
        .await
        .expect("thread list should parse");
    assert_eq!(
        ids,
        vec!["RT_unresolved_1".to_string(), "RT_unresolved_2".to_string()],
        "only isResolved==false threads returned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_unresolved_review_thread_ids_propagates_graphql_error() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Could not resolve to a Repository" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .list_unresolved_review_thread_ids("djinnos", "server", 42)
        .await
        .expect_err("GraphQL error should propagate");
    assert!(err.to_string().contains("GraphQL error"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_review_thread_issues_mutation() {
    use wiremock::matchers::body_json;

    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .and(body_json(serde_json::json!({
            "query": "mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{isResolved}}}",
            "variables": { "threadId": "RT_node123" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "resolveReviewThread": { "thread": { "isResolved": true } }
            }
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    client
        .resolve_review_thread("RT_node123")
        .await
        .expect("resolve mutation should succeed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_review_thread_propagates_graphql_error() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "Could not resolve to a node with the global id" }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let err = client
        .resolve_review_thread("RT_bad")
        .await
        .expect_err("GraphQL error should propagate");
    assert!(err.to_string().contains("GraphQL error"), "got: {err}");
}

// ── get_pr_files tests ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pr_files_parses_response() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path_regex(r"/repos/djinnos/server/pulls/42/files"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "sha": "abc123",
                "filename": "server/crates/djinn-agent/src/foo.rs",
                "status": "modified",
                "additions": 10,
                "deletions": 2,
                "changes": 12
            },
            {
                "sha": "def456",
                "filename": "server/crates/djinn-db/src/bar.rs",
                "status": "added",
                "additions": 50,
                "deletions": 0,
                "changes": 50
            }
        ])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let files: Vec<PrFile> = client
        .get_pr_files("djinnos", "server", 42)
        .await
        .expect("get_pr_files should succeed");

    assert_eq!(files.len(), 2, "should parse two files");
    assert_eq!(files[0].filename, "server/crates/djinn-agent/src/foo.rs");
    assert_eq!(files[0].status, "modified");
    assert_eq!(files[0].additions, 10);
    assert_eq!(files[0].deletions, 2);
    assert_eq!(files[0].changes, 12);
    assert_eq!(files[1].filename, "server/crates/djinn-db/src/bar.rs");
    assert_eq!(files[1].status, "added");
    assert_eq!(files[1].additions, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_pr_files_returns_empty_for_no_changes() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path_regex(r"/repos/djinnos/server/pulls/99/files"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let files: Vec<PrFile> = client
        .get_pr_files("djinnos", "server", 99)
        .await
        .expect("get_pr_files should succeed for empty response");

    assert!(
        files.is_empty(),
        "a PR with no changes returns an empty vec"
    );
}
