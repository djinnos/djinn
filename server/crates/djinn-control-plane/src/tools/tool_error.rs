//! G3 — structured tool-error envelope.
//!
//! When an MCP tool fails, agents historically received a flat `error: String`.
//! That left them unable to branch on *what kind* of failure occurred — a 404
//! model-lookup, a 422 PR-create, or a transient network blip all looked the
//! same. [`ToolError`] is a reusable, serializable envelope that carries the
//! structure an agent needs to decide its next move:
//!
//! ```json
//! {
//!   "error":  "model 'openai/ghost' not found in catalog",
//!   "status": "404",
//!   "method": "provider_model_lookup",
//!   "path":   "openai/ghost",
//!   "body":   "no matching model id",
//!   "hint":   "Call provider_models_connected to list valid model ids."
//! }
//! ```
//!
//! All fields except `error` are optional — emit only what's actually known at
//! the failing call site. This type is *additive*: tools that return a plain
//! `error: String` today keep working unchanged; new structure is layered in
//! only where a status / method / path / body is genuinely available.

use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;

/// Structured error envelope returned to the agent in place of (or alongside) a
/// flat error string. Serializes to a JSON object; absent fields are omitted so
/// the wire shape stays compact.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Default)]
pub struct ToolError {
    /// Human-readable message describing what went wrong. Always present.
    pub error: String,
    /// Status as a numeric HTTP code (e.g. "404", "422") or a coarse category
    /// (e.g. "not_found", "rate_limited", "network"). Stored as a string so a
    /// numeric HTTP status and a symbolic category share one field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The logical operation that failed — typically the MCP tool name (e.g.
    /// "provider_model_lookup") or the upstream method ("GET /search/code").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The resource the call targeted — a model id, repo path, PR ref, URL, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Raw upstream detail (response body, provider message) when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Actionable next step the agent can take to recover or disambiguate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ToolError {
    /// Build an envelope with only a human message. Equivalent in payload to the
    /// legacy flat `error: String`, but typed so call sites can enrich it.
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            ..Default::default()
        }
    }

    /// Set the status (numeric HTTP code or symbolic category).
    #[must_use]
    pub fn with_status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }

    /// Set the status from a numeric HTTP code (e.g. 404, 422).
    #[must_use]
    pub fn with_http_status(mut self, code: u16) -> Self {
        self.status = Some(code.to_string());
        self
    }

    /// Set the logical method / tool name that failed.
    #[must_use]
    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    /// Set the targeted resource (model id, repo path, URL, …).
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the raw upstream detail / response body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Set the actionable recovery hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Build a structured envelope from an upstream HTTP/GitHub error string.
    ///
    /// The provider layer returns these as `anyhow::Error` whose message embeds
    /// the HTTP status and response body (e.g. `"GitHub API rejected query
    /// (422): ..."`, `"GitHub API returned 403 Forbidden : ..."`, `"fetch_file
    /// failed (404): ..."`). We salvage the numeric status when one is present
    /// so the agent can branch on `status`, attach the full message as `body`,
    /// and record `method`/`path` from the call site. When no status is found we
    /// fall back to a coarse `"network"` category — a transient blip the agent
    /// can safely retry.
    pub fn from_http_error(method: &str, path: &str, raw: &str) -> Self {
        let status = extract_http_status(raw);
        let hint = match status.as_deref() {
            Some("404") => "Resource not found — verify the repo/path/ref exists and is accessible.",
            Some("422") => "Request was rejected as invalid — check the query/parameter syntax.",
            Some("401") | Some("403") => {
                "Access denied — the session may lack a GitHub token or permission for this resource."
            }
            Some("429") => "Rate limited — back off and retry after a short delay.",
            Some(_) => "Upstream returned an error — inspect `body` for details.",
            None => "Transient network/upstream failure — safe to retry.",
        };
        let mut err = Self::new(raw.to_string())
            .with_method(method)
            .with_path(path)
            .with_body(raw.to_string())
            .with_hint(hint);
        err.status = Some(status.unwrap_or_else(|| "network".to_string()));
        err
    }
}

/// Either a successful tool payload or a structured [`ToolError`].
///
/// Untagged, like the legacy `ErrorOr<T>`, so a success serializes as the bare
/// payload and a failure as the error envelope object — no wrapper key. This is
/// the G3-aware sibling of `ErrorOr<T>`: the error arm carries the full
/// `{ error, status, method, path, body, hint }` structure instead of a flat
/// `{ error: String }`, while a plain `ToolError::new("msg")` still serializes
/// to `{ "error": "msg" }` for backward-compatible consumers.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolOutcome<T> {
    Ok(T),
    Err(ToolError),
}

impl<T> JsonSchema for ToolOutcome<T>
where
    T: JsonSchema,
{
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("ToolOutcome{}", T::schema_name()).into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

/// Scan an error message for the first 3-digit HTTP status code in the 100–599
/// range (matching the provider layer's `"(422)"` / `"returned 403"` shapes).
fn extract_http_status(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let window = &bytes[i..i + 3];
        if window.iter().all(u8::is_ascii_digit) {
            // Must not be part of a longer run of digits (e.g. a line number).
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = i + 3 < bytes.len() && bytes[i + 3].is_ascii_digit();
            if !prev_digit && !next_digit {
                let code: u16 = std::str::from_utf8(window).ok()?.parse().ok()?;
                if (100..=599).contains(&code) {
                    return Some(code.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_all_fields() {
        let err = ToolError::new("boom")
            .with_http_status(404)
            .with_method("provider_model_lookup")
            .with_path("openai/ghost")
            .with_body("no matching model id")
            .with_hint("call provider_models_connected");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["error"], "boom");
        assert_eq!(v["status"], "404");
        assert_eq!(v["method"], "provider_model_lookup");
        assert_eq!(v["path"], "openai/ghost");
        assert_eq!(v["body"], "no matching model id");
        assert_eq!(v["hint"], "call provider_models_connected");
    }

    #[test]
    fn omits_absent_optional_fields() {
        // A plain message must still produce a valid envelope that carries only
        // `error` — keeps backward compatibility with flat error consumers.
        let err = ToolError::new("just a message");
        let v = serde_json::to_value(&err).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 1, "only `error` should be serialized: {obj:?}");
        assert_eq!(obj["error"], "just a message");
        assert!(!obj.contains_key("status"));
        assert!(!obj.contains_key("hint"));
    }

    #[test]
    fn http_status_and_category_share_the_field() {
        assert_eq!(
            serde_json::to_value(ToolError::new("x").with_http_status(422)).unwrap()["status"],
            "422"
        );
        assert_eq!(
            serde_json::to_value(ToolError::new("x").with_status("network")).unwrap()["status"],
            "network"
        );
    }

    #[test]
    fn from_http_error_extracts_422_and_populates_envelope() {
        // Mirrors the github_api search.rs 422 message shape.
        let err = ToolError::from_http_error(
            "github_search",
            "owner/repo",
            "GitHub API rejected query (422): the search is invalid",
        );
        assert_eq!(err.status.as_deref(), Some("422"));
        assert_eq!(err.method.as_deref(), Some("github_search"));
        assert_eq!(err.path.as_deref(), Some("owner/repo"));
        assert!(err.body.as_deref().unwrap().contains("rejected query"));
        assert!(err.hint.as_deref().unwrap().contains("rejected as invalid"));
    }

    #[test]
    fn from_http_error_extracts_404() {
        let err = ToolError::from_http_error(
            "github_fetch_file",
            "owner/repo:src/x.rs",
            "fetch_file failed (404): Not Found",
        );
        assert_eq!(err.status.as_deref(), Some("404"));
        assert!(err.hint.as_deref().unwrap().contains("not found"));
    }

    #[test]
    fn from_http_error_falls_back_to_network_category() {
        // No HTTP status in the message → coarse, retryable "network" category.
        let err = ToolError::from_http_error(
            "github_search",
            "owner/repo",
            "error sending request: connection reset by peer",
        );
        assert_eq!(err.status.as_deref(), Some("network"));
        assert!(err.hint.as_deref().unwrap().contains("retry"));
    }

    #[test]
    fn extract_http_status_ignores_non_status_digit_runs() {
        // A bare number that isn't a 3-digit HTTP code must not be misread.
        assert_eq!(extract_http_status("processed 12345 rows"), None);
        assert_eq!(extract_http_status("returned 503 Service Unavailable").as_deref(), Some("503"));
        assert_eq!(extract_http_status("no status here"), None);
    }
}
