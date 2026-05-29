use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{CheckRunsResponse, CreatePrParams, GitHubApiClient, MergeMethod, PrState};

use super::seed_installation_token;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pull_request_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(header("Authorization", "Bearer ghs_test_install"))
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

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
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

    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("422"), "expected 422 in error: {}", msg);
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

    let client = GitHubApiClient::for_user_token_with_base_url(
        "ghu_user_xyz".to_string(),
        server.uri(),
    );
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

    let client = GitHubApiClient::for_user_token_with_base_url(
        "ghu_self".to_string(),
        server.uri(),
    );
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
    use crate::github_api::UserTokenExpired;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls/503/reviews"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_user_token_with_base_url(
        "ghu_expired".to_string(),
        server.uri(),
    );
    let err = client
        .approve_pull_request("djinnos", "server", 503, "abc")
        .await
        .expect_err("expected 401 to fail");
    assert!(
        err.downcast_ref::<UserTokenExpired>().is_some(),
        "expected UserTokenExpired downcast, got: {err:?}"
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
                        "mergeQueueEntry": null,
                        "timelineItems": {
                            "nodes": [
                                {
                                    "__typename": "RemovedFromMergeQueueEvent",
                                    "reason": "CHECKS_FAILED",
                                    "createdAt": "2026-05-21T22:30:00Z"
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
        .and(path_regex(r"/repos/.+/branches/.+/protection/required_status_checks"))
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
    assert_eq!(contexts, None, "404 (no protection) maps to None for fallback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_required_status_checks_403_errors() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path_regex(r"/repos/.+/branches/.+/protection/required_status_checks"))
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
