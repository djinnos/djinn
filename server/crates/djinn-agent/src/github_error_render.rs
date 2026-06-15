use anyhow::Error;
use djinn_core::tool_error::ToolError;
use serde_json::Value;

/// Render typed GitHub write envelopes for agent/operator-facing text.
///
/// Provider write paths return [`ToolError`] envelopes via `anyhow`.  This
/// adapter preserves the operation-specific prefix owned by the caller while
/// exposing the structured fields agents need instead of flattening the error
/// into an opaque string.
pub(crate) fn render_github_write_error(prefix: &str, err: &Error) -> String {
    match github_write_envelope(err) {
        Some(envelope) => format!("{prefix}: {}", compact_json_like_envelope(envelope)),
        None => format!("{prefix}: {err}"),
    }
}

pub(crate) fn github_write_envelope(err: &Error) -> Option<&ToolError> {
    err.downcast_ref::<ToolError>()
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
    if let Some(body) = &envelope.body {
        value.insert("body".to_string(), Value::String(body.clone()));
    }
    if let Some(hint) = &envelope.hint {
        value.insert("hint".to_string(), Value::String(hint.clone()));
    }
    Value::Object(value).to_string()
}

pub(crate) fn github_write_body_contains(err: &Error, needle: &str) -> bool {
    let Some(envelope) = github_write_envelope(err) else {
        return false;
    };
    let needle = needle.to_ascii_lowercase();
    envelope
        .body
        .as_deref()
        .unwrap_or(&envelope.error)
        .to_ascii_lowercase()
        .contains(&needle)
}

pub(crate) fn github_write_status_is(err: &Error, status: u16) -> bool {
    github_write_envelope(err)
        .and_then(|envelope| envelope.status.as_deref())
        .is_some_and(|actual| actual == status.to_string())
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
}
