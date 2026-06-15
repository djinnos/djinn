//! Structured GitHub write-error envelopes.
//!
//! Write-side GitHub helpers historically flattened upstream failures into
//! strings such as `create_pull_request failed (422): ...`.  This module keeps
//! the construction-time classification in one place so individual write paths
//! can return a typed envelope with the same `error_class` taxonomy agents use
//! for tool errors, while still offering compact text for legacy surfaces.

use std::fmt;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const EXCERPT_LIMIT: usize = 240;

/// `ToolError.error_class` taxonomy for GitHub write failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorClass {
    ConflictRecoverable,
    NotFound,
    Permission,
    Validation,
    RateLimited,
    Internal,
}

impl ToolErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConflictRecoverable => "conflict_recoverable",
            Self::NotFound => "not_found",
            Self::Permission => "permission",
            Self::Validation => "validation",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ToolErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed envelope for a failed GitHub write operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubWriteErrorEnvelope {
    pub error: String,
    pub error_class: ToolErrorClass,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub hint: String,
}

impl GitHubWriteErrorEnvelope {
    /// Compact deterministic rendering for agent/operator-facing strings.
    pub fn compact(&self) -> String {
        let status = self
            .status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".to_string());
        let mut rendered = format!(
            "GitHub write failed method={} path={} status={} error_class={} hint={}",
            self.method, self.path, status, self.error_class, self.hint
        );
        if let Some(body) = &self.body {
            rendered.push_str(" body=");
            rendered.push_str(body);
        }
        rendered
    }
}

impl fmt::Display for GitHubWriteErrorEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.compact())
    }
}

impl std::error::Error for GitHubWriteErrorEnvelope {}

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
    let body = bounded_excerpt(detail).filter(|s| !s.is_empty());

    GitHubWriteErrorEnvelope {
        error: format!(
            "{operation} failed: method={} path={} status={} error_class={}",
            input.method, input.path, status, error_class
        ),
        error_class,
        method: input.method.to_string(),
        path: input.path.to_string(),
        status: input.status,
        body,
        hint,
    }
}

fn classify_github_write_error(
    status: Option<u16>,
    detail: &str,
    rate_limited: bool,
) -> ToolErrorClass {
    if rate_limited || status == Some(429) || detail_indicates_rate_limit(detail) {
        return ToolErrorClass::RateLimited;
    }

    match status {
        Some(404) => ToolErrorClass::NotFound,
        Some(401 | 403) => ToolErrorClass::Permission,
        Some(422) if detail_indicates_existing_pull_request(detail) => {
            ToolErrorClass::ConflictRecoverable
        }
        Some(code) if (400..500).contains(&code) => {
            if detail_indicates_permission(detail) {
                ToolErrorClass::Permission
            } else {
                ToolErrorClass::Validation
            }
        }
        _ => {
            if detail_indicates_permission(detail) {
                ToolErrorClass::Permission
            } else {
                ToolErrorClass::Internal
            }
        }
    }
}

fn default_hint(error_class: ToolErrorClass, detail: &str) -> String {
    match error_class {
        ToolErrorClass::ConflictRecoverable if detail_indicates_existing_pull_request(detail) => {
            "Adopt/use the existing pull request for this branch instead of creating a new PR."
                .to_string()
        }
        ToolErrorClass::ConflictRecoverable => {
            "Use the existing GitHub resource or reconcile the conflict before retrying."
                .to_string()
        }
        ToolErrorClass::NotFound => {
            "Verify the repository, ref, pull request, or endpoint exists and is accessible."
                .to_string()
        }
        ToolErrorClass::Permission => {
            "Check that the GitHub token or App installation has permission for this write."
                .to_string()
        }
        ToolErrorClass::Validation => {
            "Fix the rejected GitHub request parameters before retrying.".to_string()
        }
        ToolErrorClass::RateLimited => {
            "Back off until the GitHub rate limit resets before retrying.".to_string()
        }
        ToolErrorClass::Internal => {
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
    fn classification_matrix_for_github_write_failures() {
        assert_eq!(
            envelope(Some(404), r#"{"message":"Not Found"}"#).error_class,
            ToolErrorClass::NotFound
        );
        assert_eq!(
            envelope(
                Some(403),
                r#"{"message":"Resource not accessible by integration"}"#
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
    fn rate_limit_body_overrides_permission_status() {
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
