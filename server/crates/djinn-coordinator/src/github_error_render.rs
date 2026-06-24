use std::borrow::Cow;

use anyhow::Error;
use djinn_core::tool_error::{ErrorClass, ToolError};
use djinn_provider::github_api::{GitHubApiError, GitHubErrorSource};
use serde_json::Value;

const ENVELOPE_DETAIL_LIMIT: usize = 240;

/// Render typed GitHub write envelopes for agent/operator-facing text.
///
/// Provider write paths return [`ToolError`] envelopes via `anyhow`.  This
/// adapter preserves the operation-specific prefix owned by the caller while
/// exposing the structured fields agents need instead of flattening the error
/// into an opaque string.
pub(crate) fn render_github_write_error(
    prefix: &str,
    err: &(impl GithubWriteError + ?Sized),
) -> String {
    match err.github_write_envelope() {
        Some(envelope) => format!("{prefix}: {}", compact_json_like_envelope(&envelope)),
        None => format!("{prefix}: {}", err.display_string()),
    }
}

pub(crate) trait GithubWriteError {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>>;
    fn github_write_body(&self) -> Option<&str>;
    fn github_write_status(&self) -> Option<u16>;
    fn display_string(&self) -> String;
}

impl GithubWriteError for Error {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>> {
        if let Some(envelope) = self.downcast_ref::<ToolError>() {
            return Some(Cow::Borrowed(envelope));
        }
        self.downcast_ref::<GitHubApiError>()
            .map(|err| Cow::Owned(github_api_error_envelope(err)))
    }

    fn github_write_body(&self) -> Option<&str> {
        if let Some(envelope) = self.downcast_ref::<ToolError>() {
            return Some(envelope.body.as_deref().unwrap_or(&envelope.error));
        }
        self.downcast_ref::<GitHubApiError>()
            .map(|err| err.body.as_str())
    }

    fn github_write_status(&self) -> Option<u16> {
        if let Some(envelope) = self.downcast_ref::<ToolError>() {
            return envelope
                .status
                .as_deref()
                .and_then(|status| status.parse().ok());
        }
        self.downcast_ref::<GitHubApiError>()
            .and_then(GithubWriteError::github_write_status)
    }

    fn display_string(&self) -> String {
        self.to_string()
    }
}

impl GithubWriteError for ToolError {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>> {
        Some(Cow::Borrowed(self))
    }

    fn github_write_body(&self) -> Option<&str> {
        Some(self.body.as_deref().unwrap_or(&self.error))
    }

    fn github_write_status(&self) -> Option<u16> {
        self.status
            .as_deref()
            .and_then(|status| status.parse().ok())
    }

    fn display_string(&self) -> String {
        self.to_string()
    }
}

impl GithubWriteError for GitHubApiError {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>> {
        Some(Cow::Owned(github_api_error_envelope(self)))
    }

    fn github_write_body(&self) -> Option<&str> {
        Some(&self.body)
    }

    fn github_write_status(&self) -> Option<u16> {
        self.status.map(|status| status.as_u16())
    }

    fn display_string(&self) -> String {
        self.to_string()
    }
}

fn github_api_error_envelope(err: &GitHubApiError) -> ToolError {
    let error_class = classify_github_api_error(err);
    let mut envelope = ToolError::new(github_api_error_detail(err))
        .with_error_class(error_class)
        .with_method(err.method)
        .with_path(err.path.clone())
        .with_body(err.body.clone())
        .with_hint(github_api_error_hint(err, error_class));

    if let Some(status) = err.status {
        envelope = envelope.with_http_status(status.as_u16());
    }

    envelope
}

fn github_api_error_detail(err: &GitHubApiError) -> String {
    match err.status {
        Some(status) => format!(
            "GitHub write failed (source={:?}, status={})",
            err.source,
            status.as_u16()
        ),
        None => format!("GitHub write failed (source={:?}, status=none)", err.source),
    }
}

fn classify_github_api_error(err: &GitHubApiError) -> ErrorClass {
    match err.source {
        GitHubErrorSource::Unauthenticated => return ErrorClass::Permission,
        GitHubErrorSource::RateLimited => return ErrorClass::RateLimited,
        GitHubErrorSource::Transport if is_rate_limit_body(err) => return ErrorClass::RateLimited,
        GitHubErrorSource::Transport => return ErrorClass::Internal,
        GitHubErrorSource::GraphQL => return classify_graphql_github_error(err),
        GitHubErrorSource::Http => {}
    }

    match err.status.map(|status| status.as_u16()) {
        Some(401 | 403) => ErrorClass::Permission,
        Some(404) => ErrorClass::NotFound,
        Some(409) => ErrorClass::ConflictRecoverable,
        Some(429) => ErrorClass::RateLimited,
        Some(422) if err.is_pr_already_exists() || is_retryable_branch_conflict(err) => {
            ErrorClass::ConflictRecoverable
        }
        Some(422) => ErrorClass::Validation,
        Some(405) if is_merge_queue_block(err) => ErrorClass::ConflictRecoverable,
        Some(400 | 405) => ErrorClass::Validation,
        Some(code) if (500..600).contains(&code) => ErrorClass::Transient,
        _ => ErrorClass::Internal,
    }
}

fn classify_graphql_github_error(err: &GitHubApiError) -> ErrorClass {
    if is_rate_limit_body(err) {
        return ErrorClass::RateLimited;
    }
    if github_api_body_contains(err, "forbidden")
        || github_api_body_contains(err, "unauthorized")
        || github_api_body_contains(err, "not authorized")
    {
        return ErrorClass::Permission;
    }
    if is_already_queued(err) || is_retryable_branch_conflict(err) {
        return ErrorClass::ConflictRecoverable;
    }
    if github_api_body_contains(err, "not found") {
        return ErrorClass::NotFound;
    }
    if github_api_body_contains(err, "unprocessable")
        || github_api_body_contains(err, "validation")
        || github_api_body_contains(err, "auto merge is not allowed")
        || github_api_body_contains(err, "not mergeable")
    {
        return ErrorClass::Validation;
    }
    ErrorClass::Internal
}

fn github_api_error_hint(err: &GitHubApiError, error_class: ErrorClass) -> &'static str {
    if err.source == GitHubErrorSource::Transport {
        return "Inspect GitHub transport logs, network connectivity, and token refresh state before retrying.";
    }
    if err.source == GitHubErrorSource::GraphQL && error_class == ErrorClass::Internal {
        return "Inspect the GraphQL errors array and GitHub mutation logs before retrying.";
    }
    if is_already_queued(err) {
        return "Adopt the existing merge queue entry instead of enqueueing the pull request again.";
    }
    if is_merge_queue_block(err) {
        return "Treat the pull request as already delegated to GitHub's merge queue.";
    }
    if is_conversation_resolution_block(err) {
        return "Resolve or acknowledge blocking review conversations before retrying the merge.";
    }
    if err.is_pr_already_exists() {
        return "Find and reuse the existing pull request for this branch instead of creating another.";
    }
    if is_retryable_branch_conflict(err) {
        return "Refresh the pull request head/base state, then retry the branch update or merge operation.";
    }

    match error_class {
        ErrorClass::Permission => {
            "Check GitHub authentication and app installation permissions for this repository."
        }
        ErrorClass::RateLimited => "Back off until GitHub rate limits reset before retrying.",
        ErrorClass::NotFound => {
            "Verify the GitHub repository, pull request, branch, and refs still exist."
        }
        ErrorClass::ConflictRecoverable => {
            "Refresh GitHub pull request state and retry only if the operation is still needed."
        }
        ErrorClass::Validation => {
            "Fix the rejected GitHub write inputs or branch protection requirements."
        }
        ErrorClass::Transient => {
            "Retry after a short delay; GitHub returned a status-bearing 5xx failure."
        }
        ErrorClass::Internal => "Inspect GitHub write logs for the unclassified provider failure.",
    }
}

fn is_retryable_branch_conflict(err: &GitHubApiError) -> bool {
    github_api_body_contains(err, "expected head sha")
        || github_api_body_contains(err, "head sha did not match")
        || github_api_body_contains(err, "base branch was modified")
        || github_api_body_contains(err, "update branch")
        || github_api_body_contains(err, "merge conflict")
}

fn is_merge_queue_block(err: &GitHubApiError) -> bool {
    err.status.map(|status| status.as_u16()) == Some(405)
        && github_api_body_contains(err, "merge queue")
}

fn is_already_queued(err: &GitHubApiError) -> bool {
    github_api_body_contains(err, "already in the queue")
}

fn is_conversation_resolution_block(err: &GitHubApiError) -> bool {
    github_api_body_contains(err, "conversation must be resolved")
        || (github_api_body_contains(err, "repository rule violations")
            && github_api_body_contains(err, "conversation"))
}

fn is_rate_limit_body(err: &GitHubApiError) -> bool {
    github_api_body_contains(err, "rate limit")
        || github_api_body_contains(err, "rate_limit")
        || github_api_body_contains(err, "ratelimit")
        || github_api_body_contains(err, "x-ratelimit-remaining: 0")
}

fn github_api_body_contains(err: &GitHubApiError, needle: &str) -> bool {
    err.body
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn bounded_excerpt(detail: &str) -> String {
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in normalized.chars().take(ENVELOPE_DETAIL_LIMIT) {
        out.push(ch);
    }
    if normalized.chars().count() > ENVELOPE_DETAIL_LIMIT {
        out.push('…');
    }
    out
}

pub(crate) fn compact_json_like_envelope(envelope: &ToolError) -> String {
    let error_class = envelope
        .error_class
        .map(|class| class.as_str().to_string())
        .unwrap_or_else(|| "unclassified".to_string());
    let mut value = serde_json::Map::new();
    value.insert("error_class".to_string(), Value::String(error_class));
    if let Some(method) = &envelope.method {
        value.insert("method".to_string(), Value::String(method.clone()));
    }
    if let Some(path) = &envelope.path {
        value.insert("path".to_string(), Value::String(path.clone()));
    }
    if let Some(status) = &envelope.status {
        value.insert("status".to_string(), Value::String(status.clone()));
    }
    match envelope.body.as_deref() {
        Some(body) => {
            value.insert("body".to_string(), Value::String(bounded_excerpt(body)));
        }
        None => {
            value.insert(
                "detail".to_string(),
                Value::String(bounded_excerpt(&envelope.error)),
            );
        }
    }
    if let Some(hint) = &envelope.hint {
        value.insert("hint".to_string(), Value::String(hint.clone()));
    }
    Value::Object(value).to_string()
}

pub(crate) fn github_write_body_contains(
    err: &(impl GithubWriteError + ?Sized),
    needle: &str,
) -> bool {
    let Some(body) = err.github_write_body() else {
        return false;
    };
    let needle = needle.to_ascii_lowercase();
    body.to_ascii_lowercase().contains(&needle)
}

pub(crate) fn github_write_status_is(err: &(impl GithubWriteError + ?Sized), status: u16) -> bool {
    err.github_write_status()
        .is_some_and(|actual| actual == status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::tool_error::{ErrorClass, ToolError};
    use reqwest::StatusCode;

    #[test]
    fn renders_compact_json_like_github_write_envelope() {
        let err: anyhow::Error = ToolError::new("merge_pull_request failed")
            .with_error_class(ErrorClass::Validation)
            .with_method("PUT")
            .with_path("/repos/o/r/pulls/7/merge")
            .with_http_status(405)
            .with_body("Repository rule violations found: conversation must be resolved")
            .with_hint("Resolve review conversations before retrying.")
            .into();

        let rendered = render_github_write_error("GitHub merge failed", &err);

        assert!(rendered.starts_with("GitHub merge failed: {"));
        assert!(rendered.contains(r#"error_class":"validation"#));
        assert!(rendered.contains(r#"method":"PUT"#));
        assert!(rendered.contains(r#"path":"/repos/o/r/pulls/7/merge"#));
        assert!(rendered.contains(r#"status":"405"#));
        assert!(rendered.contains("conversation must be resolved"));
        assert!(rendered.contains("Resolve review conversations"));
    }

    #[test]
    fn rendering_bounds_body_excerpt_even_when_envelope_body_is_raw() {
        let raw = "x".repeat(400);
        let err: anyhow::Error = ToolError::new("create_pull_request failed")
            .with_error_class(ErrorClass::Validation)
            .with_method("POST")
            .with_path("/repos/o/r/pulls")
            .with_http_status(422)
            .with_body(raw)
            .into();

        let rendered = render_github_write_error("GitHub PR creation failed", &err);

        assert!(rendered.contains(r#"error_class":"validation"#));
        assert!(rendered.contains(r#"body":"#));
        assert!(rendered.contains('…'));
        assert!(
            !rendered.contains(&"x".repeat(300)),
            "agent-facing envelope body must stay compact"
        );
    }

    #[test]
    fn rendering_uses_detail_excerpt_when_body_is_absent() {
        let err: anyhow::Error = ToolError::new("status-less GitHub transport failure")
            .with_error_class(ErrorClass::Internal)
            .with_method("POST")
            .with_path("/graphql")
            .with_hint("Inspect transport logs before retrying.")
            .into();

        let rendered = render_github_write_error("GitHub auto-merge enable failed", &err);

        assert!(rendered.contains(r#"error_class":"internal"#));
        assert!(rendered.contains(r#"detail":"status-less GitHub transport failure"#));
        assert!(rendered.contains("Inspect transport logs"));
    }

    #[test]
    fn direct_github_api_http_error_renders_structured_envelope() {
        let err = GitHubApiError::http(
            "PUT",
            "/repos/o/r/pulls/7/update-branch".to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Expected head SHA did not match current branch head. ".repeat(8),
        );

        let rendered = render_github_write_error("GitHub update-branch failed", &err);

        assert!(rendered.starts_with("GitHub update-branch failed: {"));
        assert!(rendered.contains(r#"error_class":"conflict_recoverable"#));
        assert!(rendered.contains(r#"method":"PUT"#));
        assert!(rendered.contains(r#"path":"/repos/o/r/pulls/7/update-branch"#));
        assert!(rendered.contains(r#"status":"422"#));
        assert!(rendered.contains(r#"body":"Expected head SHA did not match"#));
        assert!(rendered.contains('…'));
        assert!(rendered.contains("Refresh the pull request head/base state"));
        assert!(!rendered.contains("github PUT /repos/o/r/pulls/7/update-branch failed:"));
    }

    #[test]
    fn status_less_transport_error_is_internal_not_transient() {
        let err = GitHubApiError::transport(
            "POST",
            "/graphql".to_string(),
            "connection reset by peer while enabling auto-merge".to_string(),
        );

        let rendered = render_github_write_error("GitHub auto-merge enable failed", &err);

        assert!(rendered.contains(r#"error_class":"internal"#));
        assert!(rendered.contains(r#"method":"POST"#));
        assert!(rendered.contains(r#"path":"/graphql"#));
        assert!(!rendered.contains(r#"status":"#));
        assert!(rendered.contains(r#"body":"connection reset by peer"#));
        assert!(rendered.contains("Inspect GitHub transport logs"));
        assert!(!rendered.contains(r#"error_class":"transient"#));
    }

    #[test]
    fn unclassified_status_less_graphql_error_is_internal() {
        let err = GitHubApiError::graphql(
            "enqueue_pull_request",
            "/graphql".to_string(),
            r#"[{"type":"UNKNOWN","message":"Something unexpected happened"}]"#.to_string(),
        );

        let rendered = render_github_write_error("GitHub enqueue PR failed", &err);

        assert!(rendered.contains(r#"error_class":"internal"#));
        assert!(rendered.contains(r#"method":"enqueue_pull_request"#));
        assert!(rendered.contains(r#"path":"/graphql"#));
        assert!(rendered.contains("Inspect the GraphQL errors array"));
        assert!(!rendered.contains(r#"error_class":"transient"#));
    }

    #[test]
    fn anyhow_wrapped_github_api_error_also_uses_typed_fields() {
        let err: anyhow::Error = GitHubApiError::http(
            "POST",
            "/repos/o/r/pulls".to_string(),
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by integration"}"#.to_string(),
        )
        .into();

        let rendered = render_github_write_error("GitHub PR creation failed", &err);

        assert!(rendered.contains(r#"error_class":"permission"#));
        assert!(rendered.contains(r#"method":"POST"#));
        assert!(rendered.contains(r#"path":"/repos/o/r/pulls"#));
        assert!(rendered.contains(r#"status":"403"#));
        assert!(!rendered.contains("github POST /repos/o/r/pulls failed:"));
    }
}
