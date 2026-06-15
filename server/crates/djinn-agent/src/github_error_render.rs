use anyhow::Error;
use djinn_core::tool_error::ToolError;
use djinn_provider::github_api::GitHubApiError;
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
        Some(envelope) => format!("{prefix}: {}", compact_json_like_envelope(envelope)),
        None => format!("{prefix}: {}", err.display_string()),
    }
}

pub(crate) trait GithubWriteError {
    fn github_write_envelope(&self) -> Option<&ToolError>;
    fn github_write_body(&self) -> Option<&str>;
    fn github_write_status(&self) -> Option<u16>;
    fn display_string(&self) -> String;
}

impl GithubWriteError for Error {
    fn github_write_envelope(&self) -> Option<&ToolError> {
        self.downcast_ref::<ToolError>()
    }

    fn github_write_body(&self) -> Option<&str> {
        self.github_write_envelope()
            .map(|envelope| envelope.body.as_deref().unwrap_or(&envelope.error))
    }

    fn github_write_status(&self) -> Option<u16> {
        self.github_write_envelope()
            .and_then(|envelope| envelope.status.as_deref())
            .and_then(|status| status.parse().ok())
    }

    fn display_string(&self) -> String {
        self.to_string()
    }
}

impl GithubWriteError for GitHubApiError {
    fn github_write_envelope(&self) -> Option<&ToolError> {
        None
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
}
