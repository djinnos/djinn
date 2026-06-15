//! Structured GitHub write-error envelopes.
//!
//! Write-side GitHub helpers historically flattened upstream failures into
//! strings such as `create_pull_request failed (422): ...`.  This module keeps
//! construction-time classification in one place so individual write paths can
//! return the shared typed [`ToolError`] envelope with the dfk7
//! `error_class` taxonomy, while still offering compact text for legacy
//! surfaces.

use djinn_core::tool_error::{ErrorClass, ToolError};
use reqwest::StatusCode;

const EXCERPT_LIMIT: usize = 240;

/// Shared typed envelope for a failed GitHub write operation.
pub type GitHubWriteErrorEnvelope = ToolError;

/// Inputs for constructing a [`GitHubWriteErrorEnvelope`].
#[derive(Debug, Clone)]
pub struct GitHubWriteErrorInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub status: Option<u16>,
    pub body_or_detail: Option<&'a str>,
    pub operation: Option<&'a str>,
    pub hint: Option<&'a str>,
    /// Set when the caller observed rate-limit headers (for example
    /// `X-RateLimit-Remaining: 0`) even if the HTTP status is not 429.
    pub rate_limited: bool,
}

impl<'a> GitHubWriteErrorInput<'a> {
    pub fn new(method: &'a str, path: &'a str) -> Self {
        Self {
            method,
            path,
            status: None,
            body_or_detail: None,
            operation: None,
            hint: None,
            rate_limited: false,
        }
    }

    pub fn with_status(mut self, status: impl Into<Option<u16>>) -> Self {
        self.status = status.into();
        self
    }

    pub fn with_reqwest_status(mut self, status: Option<StatusCode>) -> Self {
        self.status = status.map(|s| s.as_u16());
        self
    }

    pub fn with_body_or_detail(mut self, body_or_detail: impl Into<Option<&'a str>>) -> Self {
        self.body_or_detail = body_or_detail.into();
        self
    }

    pub fn with_operation(mut self, operation: impl Into<Option<&'a str>>) -> Self {
        self.operation = operation.into();
        self
    }

    pub fn with_hint(mut self, hint: impl Into<Option<&'a str>>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn with_rate_limited(mut self, rate_limited: bool) -> Self {
        self.rate_limited = rate_limited;
        self
    }
}

/// Construct a typed GitHub write-error envelope without parsing a flattened
/// `anyhow` string.
pub fn github_write_error_envelope(input: GitHubWriteErrorInput<'_>) -> GitHubWriteErrorEnvelope {
    let detail = input.body_or_detail.unwrap_or_default();
    let error_class = classify_github_write_error(input.status, detail, input.rate_limited);
    let hint = input
        .hint
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_hint(error_class, detail));
    let operation = input.operation.unwrap_or("GitHub write");
    let status = input
        .status
        .map(|s| s.to_string())
        .unwrap_or_else(|| "status-less".to_string());
    let body = bounded_excerpt(detail);

    let mut envelope = ToolError::new(format!(
        "{operation} failed: method={} path={} status={} error_class={}",
        input.method, input.path, status, error_class
    ))
    .with_error_class(error_class)
    .with_method(input.method)
    .with_path(input.path)
    .with_hint(hint);

    if let Some(status) = input.status {
        envelope = envelope.with_http_status(status);
    }
    if let Some(body) = body {
        envelope = envelope.with_body(body);
    }

    envelope
}

fn classify_github_write_error(
    status: Option<u16>,
    detail: &str,
    rate_limited: bool,
) -> ErrorClass {
    if rate_limited || status == Some(429) || detail_indicates_rate_limit(detail) {
        return ErrorClass::RateLimited;
    }

    match status {
        Some(404) => ErrorClass::NotFound,
        Some(401 | 403) => ErrorClass::Permission,
        Some(422) if detail_indicates_existing_pull_request(detail) => {
            ErrorClass::ConflictRecoverable
        }
        Some(code) if (400..500).contains(&code) => {
            if detail_indicates_permission(detail) {
                ErrorClass::Permission
            } else {
                ErrorClass::Validation
            }
        }
        _ => {
            if detail_indicates_permission(detail) {
                ErrorClass::Permission
            } else {
                ErrorClass::Internal
            }
        }
    }
}

fn default_hint(error_class: ErrorClass, detail: &str) -> String {
    match error_class {
        ErrorClass::ConflictRecoverable if detail_indicates_existing_pull_request(detail) => {
            "Adopt/use the existing pull request for this branch instead of creating a new PR."
                .to_string()
        }
        ErrorClass::ConflictRecoverable => {
            "Use the existing GitHub resource or reconcile the conflict before retrying."
                .to_string()
        }
        ErrorClass::NotFound => {
            "Verify the repository, ref, pull request, or endpoint exists and is accessible."
                .to_string()
        }
        ErrorClass::Permission => {
            "Check that the GitHub token or App installation has permission for this write."
                .to_string()
        }
        ErrorClass::Validation => {
            "Fix the rejected GitHub request parameters before retrying.".to_string()
        }
        ErrorClass::RateLimited => {
            "Back off until the GitHub rate limit resets before retrying.".to_string()
        }
        ErrorClass::Transient => {
            "Retry after a short delay if GitHub reports a transient upstream failure.".to_string()
        }
        ErrorClass::Internal => {
            "Treat as an internal/provider failure; inspect the bounded detail before retrying."
                .to_string()
        }
    }
}

fn detail_indicates_existing_pull_request(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("pull request") && lower.contains("already exists")
}

fn detail_indicates_permission(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("bad credentials")
        || lower.contains("requires authentication")
        || lower.contains("must authenticate")
        || lower.contains("unauthorized")
        || lower.contains("authorization")
        || lower.contains("not authorized")
        || lower.contains("access denied")
        || lower.contains("resource not accessible")
        || lower.contains("not accessible by integration")
        || lower.contains("permission")
        || lower.contains("forbidden")
}

fn detail_indicates_rate_limit(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("secondary rate")
        || lower.contains("api rate limit exceeded")
}

fn bounded_excerpt(detail: &str) -> Option<String> {
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut out = String::new();
    for ch in normalized.chars().take(EXCERPT_LIMIT) {
        out.push(ch);
    }
    if normalized.chars().count() > EXCERPT_LIMIT {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR_ALREADY_EXISTS: &str = r#"{
        "message": "Validation Failed",
        "errors": [{
            "resource": "PullRequest",
            "code": "custom",
            "message": "A pull request already exists for djinnos:feature-branch."
        }]
    }"#;

    fn envelope(status: Option<u16>, body: &str) -> GitHubWriteErrorEnvelope {
        github_write_error_envelope(
            GitHubWriteErrorInput::new("POST", "/repos/djinnos/server/pulls")
                .with_status(status)
                .with_body_or_detail(Some(body))
                .with_operation(Some("create_pull_request")),
        )
    }

    #[test]
    fn classifies_captured_422_already_exists_as_conflict_recoverable() {
        let err = envelope(Some(422), PR_ALREADY_EXISTS);

        assert_eq!(err.error_class, Some(ErrorClass::ConflictRecoverable));
        assert_eq!(err.error_class.unwrap().as_str(), "conflict_recoverable");
        assert!(
            err.hint
                .as_deref()
                .unwrap()
                .contains("Adopt/use the existing pull request for this branch"),
            "hint was: {:?}",
            err.hint
        );
    }

    #[test]
    fn classification_matrix_for_github_write_failures() {
        assert_eq!(
            envelope(Some(404), r#"{"message":"Not Found"}"#).error_class,
            Some(ErrorClass::NotFound)
        );
        assert_eq!(
            envelope(
                Some(403),
                r#"{"message":"Resource not accessible by integration"}"#
            )
            .error_class,
            Some(ErrorClass::Permission)
        );
        assert_eq!(
            envelope(Some(401), r#"{"message":"Bad credentials"}"#).error_class,
            Some(ErrorClass::Permission)
        );
        assert_eq!(
            envelope(Some(422), r#"{"message":"Validation Failed","errors":[]}"#).error_class,
            Some(ErrorClass::Validation)
        );
        assert_eq!(
            envelope(Some(429), r#"{"message":"API rate limit exceeded"}"#).error_class,
            Some(ErrorClass::RateLimited)
        );
        assert_eq!(
            envelope(None, "error sending request: connection reset by peer").error_class,
            Some(ErrorClass::Internal)
        );
    }

    #[test]
    fn rate_limit_body_overrides_permission_status() {
        let err = envelope(Some(403), r#"{"message":"API rate limit exceeded"}"#);

        assert_eq!(err.error_class, Some(ErrorClass::RateLimited));
    }

    #[test]
    fn statusless_unknown_is_internal_never_transient() {
        let err = envelope(None, "opaque provider failure");

        assert_eq!(err.error_class.unwrap().as_str(), "internal");
        assert_ne!(err.error_class.unwrap().as_str(), "transient");
        assert!(err.compact().contains("status=none"));
        assert!(err.compact().contains("error_class=internal"));
    }

    #[test]
    fn compact_rendering_includes_required_fields_and_bounded_excerpt() {
        let long_tail = "x".repeat(500);
        let body = format!("{PR_ALREADY_EXISTS} {long_tail}");
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
        assert!(err.body.as_ref().unwrap().chars().count() <= EXCERPT_LIMIT + 1);
        assert!(
            !rendered.contains(&"x".repeat(300)),
            "long body tail should be bounded: {rendered}"
        );
    }
}
