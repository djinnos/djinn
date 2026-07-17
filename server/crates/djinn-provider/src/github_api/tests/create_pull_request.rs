//! `create_pull_request` tests: plain creation plus the 422-already-exists
//! adoption path (single-shot and retried). Split from `pull_requests.rs`
//! to keep that module under the size guard.

use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{CreatePrParams, GitHubApiClient, PrState};

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
async fn create_pull_request_adopts_existing_on_422_already_exists() {
    // GitHub returns 422 "A pull request already exists for <owner>:<head>" when
    // a PR for the head branch is already open. create_pull_request must adopt it
    // (idempotent) instead of erroring — otherwise the supervisor loops
    // reopen→create→422 forever (task ps1q).
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "PullRequest",
                "code": "custom",
                "message": "A pull request already exists for djinnos:feature-branch."
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "djinnos:feature-branch"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "number": 7,
                "title": "feat: existing",
                "state": "open",
                "merged": false,
                "html_url": "https://github.com/djinnos/server/pull/7",
                "head": { "ref": "feature-branch", "sha": "abc123" },
                "base": { "ref": "main", "sha": "def456" },
                "auto_merge": null,
                "node_id": "PR_existing"
            }])),
        )
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
                maintainer_can_modify: Some(true),
                draft: Some(true),
            },
        )
        .await
        .expect("422-already-exists must resolve to the existing PR, not an error");

    assert_eq!(pr.number, 7, "should adopt the existing PR");
    assert_eq!(pr.state, PrState::Open);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_pull_request_adoption_retries_transient_list_failure() {
    // During GitHub degraded-API incidents (and plain read-after-write lag on
    // the list endpoint) the POST's 422 "already exists" gets through while
    // the adoption list 5xxs or comes back empty. A single-shot list then
    // leaks the 422 and fails the whole PR-open flow for a PR that exists
    // (task mbfw, 2026-07-16). The adoption list must retry.
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "PullRequest",
                "code": "custom",
                "message": "A pull request already exists for djinnos:feature-branch."
            }]
        })))
        .mount(&server)
        .await;

    // First list attempt: GitHub 503s (mounted first + scoped to one match so
    // the retry falls through to the success mock below).
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Unicorn!"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "djinnos:feature-branch"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "number": 7,
                "title": "feat: existing",
                "state": "open",
                "merged": false,
                "html_url": "https://github.com/djinnos/server/pull/7",
                "head": { "ref": "feature-branch", "sha": "abc123" },
                "base": { "ref": "main", "sha": "def456" },
                "auto_merge": null,
                "node_id": "PR_existing"
            }])),
        )
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
                maintainer_can_modify: Some(true),
                draft: Some(true),
            },
        )
        .await
        .expect("adoption must survive one transient list failure");

    assert_eq!(pr.number, 7, "should adopt the existing PR on retry");
    assert_eq!(pr.state, PrState::Open);
}
