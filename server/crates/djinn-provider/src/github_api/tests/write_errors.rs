use crate::github_api::{GitHubWriteErrorInput, ToolErrorClass, github_write_error_envelope};

const CAPTURED_CREATE_PR_422_ALREADY_EXISTS: &str = r#"{
  "message": "Validation Failed",
  "errors": [
    {
      "resource": "PullRequest",
      "code": "custom",
      "message": "A pull request already exists for djinnos:feature-branch."
    }
  ]
}"#;

fn envelope(status: Option<u16>, body: &str) -> crate::github_api::GitHubWriteErrorEnvelope {
    github_write_error_envelope(
        GitHubWriteErrorInput::new("POST", "/repos/djinnos/server/pulls")
            .with_status(status)
            .with_body_or_detail(Some(body))
            .with_operation(Some("create_pull_request")),
    )
}

#[test]
fn captured_create_pr_422_already_exists_is_conflict_recoverable() {
    let err = envelope(Some(422), CAPTURED_CREATE_PR_422_ALREADY_EXISTS);

    assert_eq!(err.error_class, ToolErrorClass::ConflictRecoverable);
    assert_eq!(err.error_class.as_str(), "conflict_recoverable");
    assert!(
        err.hint
            .contains("Adopt/use the existing pull request for this branch"),
        "hint was: {}",
        err.hint
    );
}

#[test]
fn github_write_error_classification_matrix() {
    assert_eq!(
        envelope(Some(404), r#"{"message":"Not Found"}"#).error_class,
        ToolErrorClass::NotFound
    );
    assert_eq!(
        envelope(
            Some(403),
            r#"{"message":"Resource not accessible by integration"}"#,
        )
        .error_class,
        ToolErrorClass::Permission
    );
    assert_eq!(
        envelope(Some(401), r#"{"message":"Bad credentials"}"#).error_class,
        ToolErrorClass::Permission
    );
    assert_eq!(
        envelope(Some(422), r#"{"message":"Validation Failed","errors":[]}"#).error_class,
        ToolErrorClass::Validation
    );
    assert_eq!(
        envelope(Some(429), r#"{"message":"API rate limit exceeded"}"#).error_class,
        ToolErrorClass::RateLimited
    );
    assert_eq!(
        envelope(None, "error sending request: connection reset by peer").error_class,
        ToolErrorClass::Internal
    );
}

#[test]
fn rate_limit_response_wins_over_permission_status() {
    let err = envelope(Some(403), r#"{"message":"API rate limit exceeded"}"#);

    assert_eq!(err.error_class, ToolErrorClass::RateLimited);
}

#[test]
fn statusless_unknown_is_internal_never_transient() {
    let err = envelope(None, "opaque provider failure");

    assert_eq!(err.error_class.as_str(), "internal");
    assert_ne!(err.error_class.as_str(), "transient");
    assert!(err.compact().contains("status=none"));
    assert!(err.compact().contains("error_class=internal"));
}

#[test]
fn compact_rendering_includes_fields_and_bounded_body_excerpt() {
    let long_tail = "x".repeat(500);
    let body = format!("{CAPTURED_CREATE_PR_422_ALREADY_EXISTS} {long_tail}");
    let err = envelope(Some(422), &body);
    let rendered = err.compact();

    assert!(rendered.contains("method=POST"));
    assert!(rendered.contains("path=/repos/djinnos/server/pulls"));
    assert!(rendered.contains("status=422"));
    assert!(rendered.contains("error_class=conflict_recoverable"));
    assert!(rendered.contains("hint=Adopt/use the existing pull request for this branch"));
    assert!(rendered.contains("body="));
    assert!(rendered.contains("Validation Failed"));
    assert!(rendered.contains('…'));
    assert!(
        err.body.as_ref().unwrap().chars().count() <= 241,
        "body excerpt was not bounded: {:?}",
        err.body
    );
    assert!(
        !rendered.contains(&"x".repeat(300)),
        "long body tail should be bounded: {rendered}"
    );
}
