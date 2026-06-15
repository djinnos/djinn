use reqwest::StatusCode;

use crate::github_api::{GitHubApiError, GitHubErrorSource};

const PR_ALREADY_EXISTS: &str = r#"{
  "message": "Validation Failed",
  "errors": [{
    "resource": "PullRequest",
    "code": "custom",
    "message": "A pull request already exists for djinnos:feature-branch."
  }]
}"#;

const NOT_FOUND: &str =
    r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest"}"#;
const MERGE_QUEUE_405: &str = r#"{"message":"Pull Request is in the merge queue.","status":"405"}"#;
const CONVERSATION_405: &str = "{\"message\":\"Repository rule violations found\\n\\nA conversation must be resolved before this pull request can be merged.\\n\\n\",\"status\":\"405\"}";
const CONVERSATION_409: &str =
    "A conversation must be resolved before this pull request can be merged.";
const ENQUEUE_ALREADY_QUEUED: &str = r#"[{"type":"UNPROCESSABLE","path":["enqueuePullRequest"],"message":"Pull request is already in the queue"}]"#;
const ENQUEUE_NOT_MERGEABLE: &str =
    r#"[{"type":"UNPROCESSABLE","message":"Pull request is not mergeable"}]"#;

#[test]
fn constructors_preserve_status_source_and_body() {
    let http = GitHubApiError::http(
        "create_pull_request",
        "/repos/djinnos/server/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        PR_ALREADY_EXISTS.to_string(),
    );
    assert_eq!(http.status, Some(StatusCode::UNPROCESSABLE_ENTITY));
    assert_eq!(http.source, GitHubErrorSource::Http);
    assert_eq!(http.body, PR_ALREADY_EXISTS);

    let not_found = GitHubApiError::http(
        "get_check_run_annotations",
        "/repos/djinnos/server/check-runs/555/annotations".to_string(),
        StatusCode::NOT_FOUND,
        NOT_FOUND.to_string(),
    );
    assert_eq!(not_found.status, Some(StatusCode::NOT_FOUND));
    assert_eq!(not_found.source, GitHubErrorSource::Http);
    assert_eq!(not_found.body, NOT_FOUND);

    let merge_queue = GitHubApiError::http(
        "merge_pull_request",
        "/repos/djinnos/server/pulls/1/merge".to_string(),
        StatusCode::METHOD_NOT_ALLOWED,
        MERGE_QUEUE_405.to_string(),
    );
    assert_eq!(merge_queue.status, Some(StatusCode::METHOD_NOT_ALLOWED));
    assert_eq!(merge_queue.source, GitHubErrorSource::Http);
    assert_eq!(merge_queue.body, MERGE_QUEUE_405);

    let conversation_405 = GitHubApiError::http(
        "merge_pull_request",
        "/repos/djinnos/server/pulls/1/merge".to_string(),
        StatusCode::METHOD_NOT_ALLOWED,
        CONVERSATION_405.to_string(),
    );
    assert_eq!(
        conversation_405.status,
        Some(StatusCode::METHOD_NOT_ALLOWED)
    );
    assert_eq!(conversation_405.source, GitHubErrorSource::Http);
    assert_eq!(conversation_405.body, CONVERSATION_405);

    let conversation_409 = GitHubApiError::http(
        "merge_pull_request",
        "/repos/djinnos/server/pulls/1/merge".to_string(),
        StatusCode::CONFLICT,
        CONVERSATION_409.to_string(),
    );
    assert_eq!(conversation_409.status, Some(StatusCode::CONFLICT));
    assert_eq!(conversation_409.source, GitHubErrorSource::Http);
    assert_eq!(conversation_409.body, CONVERSATION_409);

    let graphql = GitHubApiError::graphql(
        "enqueue_pull_request",
        "/graphql".to_string(),
        ENQUEUE_ALREADY_QUEUED.to_string(),
    );
    assert_eq!(graphql.status, None);
    assert_eq!(graphql.source, GitHubErrorSource::GraphQL);
    assert_eq!(graphql.body, ENQUEUE_ALREADY_QUEUED);

    let graphql_other = GitHubApiError::graphql(
        "enqueue_pull_request",
        "/graphql".to_string(),
        ENQUEUE_NOT_MERGEABLE.to_string(),
    );
    assert_eq!(graphql_other.status, None);
    assert_eq!(graphql_other.source, GitHubErrorSource::GraphQL);
    assert_eq!(graphql_other.body, ENQUEUE_NOT_MERGEABLE);

    let rate_limited = GitHubApiError::rate_limited(
        "handle_rate_limit",
        "<request>".to_string(),
        "X-RateLimit-Remaining: 0".to_string(),
    );
    assert_eq!(rate_limited.status, Some(StatusCode::TOO_MANY_REQUESTS));
    assert_eq!(rate_limited.source, GitHubErrorSource::RateLimited);
    assert_eq!(rate_limited.body, "X-RateLimit-Remaining: 0");

    let transport = GitHubApiError::transport(
        "send_with_retry",
        "<request>".to_string(),
        "connection reset".to_string(),
    );
    assert_eq!(transport.status, None);
    assert_eq!(transport.source, GitHubErrorSource::Transport);
    assert_eq!(transport.body, "connection reset");

    let unauthenticated = GitHubApiError::unauthenticated(
        "send_with_retry",
        "<request>".to_string(),
        "token revoked".to_string(),
    );
    assert_eq!(unauthenticated.status, Some(StatusCode::UNAUTHORIZED));
    assert_eq!(unauthenticated.source, GitHubErrorSource::Unauthenticated);
    assert_eq!(unauthenticated.body, "token revoked");
}

#[test]
fn is_pr_already_exists_matches_only_422_marker() {
    let already_exists = GitHubApiError::http(
        "create_pull_request",
        "/repos/djinnos/server/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        PR_ALREADY_EXISTS.to_string(),
    );
    assert!(already_exists.is_pr_already_exists());

    let other_422 = GitHubApiError::http(
        "create_pull_request",
        "/repos/djinnos/server/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "Validation Failed".to_string(),
    );
    assert!(!other_422.is_pr_already_exists());

    let non_422 = GitHubApiError::http(
        "create_pull_request",
        "/repos/djinnos/server/pulls".to_string(),
        StatusCode::CONFLICT,
        "already exists".to_string(),
    );
    assert!(!non_422.is_pr_already_exists());
}

#[test]
fn converts_to_anyhow_via_error_blanket() {
    let typed = GitHubApiError::transport(
        "send_with_retry",
        "<request>".to_string(),
        "connection reset".to_string(),
    );
    let err: anyhow::Error = typed.into();
    let round_trip = err.downcast_ref::<GitHubApiError>().unwrap();
    assert_eq!(round_trip.source, GitHubErrorSource::Transport);
    assert_eq!(round_trip.body, "connection reset");
}
