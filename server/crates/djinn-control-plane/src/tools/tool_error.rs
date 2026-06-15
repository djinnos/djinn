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
//!   "error":       "model 'openai/ghost' not found in catalog",
//!   "status":      "404",
//!   "method":      "provider_model_lookup",
//!   "path":        "openai/ghost",
//!   "body":        "no matching model id",
//!   "hint":        "Call provider_models_connected to list valid model ids.",
//!   "error_class": "not_found"
//! }
//! ```
//!
//! All fields except `error` are optional — emit only what's actually known at
//! the failing call site. This type is *additive*: tools that return a plain
//! `error: String` today keep working unchanged; new structure is layered in
//! only where a status / method / path / body is genuinely available.
//!
//! # Error classification
//!
//! [`ErrorClass`] is the typed taxonomy used by supervisors and wave planners
//! to branch on failure mode. It is *assigned at construction time* from a
//! typed source (HTTP status, provider error variant) — never recovered later
//! from a stringified error message. The seven classes:
//!
//! - [`ErrorClass::NotFound`] — the targeted resource does not exist (HTTP 404).
//! - [`ErrorClass::ConflictRecoverable`] — the operation hit a recoverable
//!   state conflict and the agent can take a concrete next step (HTTP 409,
//!   HTTP 422 with a known recovery-action substring like "already exists").
//! - [`ErrorClass::Validation`] — the request was malformed/invalid (HTTP 400,
//!   generic HTTP 422).
//! - [`ErrorClass::Permission`] — credentials missing/insufficient
//!   (HTTP 401, 403).
//! - [`ErrorClass::RateLimited`] — upstream quota exhausted (HTTP 429).
//! - [`ErrorClass::Transient`] — a typed retryable network/upstream condition
//!   (HTTP 5xx).
//! - [`ErrorClass::Internal`] — default for status-less / unknown / untyped
//!   errors. **Never** assigned to retryable network blips.

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// Maximum number of bytes of an upstream response body rendered into the
/// serialized `body` field. Bodies longer than this are truncated with a
/// `[truncated: N bytes omitted]` marker; the raw body remains available
/// internally for `tracing::warn!` but is never serialized into the
/// agent-visible envelope.
pub const MAX_BODY_EXCERPT_BYTES: usize = 512;

/// Coarse failure taxonomy for [`ToolError`].
///
/// Used by supervisors and downstream consumers to branch on failure mode
/// without having to re-parse the message string. Every variant maps to a
/// distinct operational decision the agent can make.
///
/// See the [module-level documentation](self) for the full classification
/// table. The serialized form is the snake_case variant name (e.g.
/// `"conflict_recoverable"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The targeted resource does not exist (HTTP 404).
    NotFound,
    /// A recoverable state conflict — the agent can take a concrete next step
    /// (HTTP 409, HTTP 422 with a known recovery-action substring such as
    /// "already exists"). Hints on this class MUST name the recovery action.
    ConflictRecoverable,
    /// The request was malformed or invalid (HTTP 400, generic HTTP 422).
    Validation,
    /// Credentials missing or insufficient (HTTP 401, 403).
    Permission,
    /// Upstream quota exhausted (HTTP 429).
    RateLimited,
    /// A typed retryable network/upstream condition (HTTP 5xx). Only emitted
    /// when the typed source confirms a retryable signature.
    Transient,
    /// Status-less or unknown error. Default for [`ToolError::from_untyped`]
    /// and any unclassified typed source. **Never** used for retryable network
    /// blips — those are [`ErrorClass::Transient`].
    Internal,
}

impl ErrorClass {
    /// Snake_case serialization name (matches the `Serialize` representation).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ConflictRecoverable => "conflict_recoverable",
            Self::Validation => "validation",
            Self::Permission => "permission",
            Self::RateLimited => "rate_limited",
            Self::Transient => "transient",
            Self::Internal => "internal",
        }
    }
}

/// Default human-readable hint for a given class. Callers may override via
/// [`ToolError::with_hint`] when the agent needs a more specific recovery
/// action (e.g. "adopt the existing PR via its URL" for the 422 PR-already-
/// exists case).
fn default_hint_for(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::NotFound => {
            "Resource not found — verify the path/ref/identifier exists and is accessible."
        }
        ErrorClass::ConflictRecoverable => {
            "Upstream reported a recoverable state conflict — inspect `body` and retry with the suggested recovery action."
        }
        ErrorClass::Validation => {
            "Request was rejected as invalid — check the parameter syntax against the tool schema."
        }
        ErrorClass::Permission => {
            "Access denied — the session may lack credentials or permission for this resource."
        }
        ErrorClass::RateLimited => "Rate limited — back off and retry after a short delay.",
        ErrorClass::Transient => {
            "Upstream returned a transient error — safe to retry after a short backoff."
        }
        ErrorClass::Internal => {
            "An internal/unclassified error occurred — inspect `body` and the activity log for details."
        }
    }
}

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
    /// Bounded upstream detail (response body, provider message) when
    /// available. Truncated to [`MAX_BODY_EXCERPT_BYTES`] bytes with a
    /// `[truncated: N bytes omitted]` marker if the raw body was longer. The
    /// raw body is preserved internally for `tracing::warn!` but never
    /// serialized into the agent-visible envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Actionable next step the agent can take to recover or disambiguate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Coarse failure class. Assigned at construction time from a typed
    /// source (HTTP status, provider error variant). Omitted from the wire
    /// when unknown — the legacy envelope shape (without this field) is
    /// still valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
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

    /// Set the coarse failure class.
    #[must_use]
    pub fn with_error_class(mut self, class: ErrorClass) -> Self {
        self.error_class = Some(class);
        self
    }

    /// Build a structured envelope from a typed HTTP status + response body.
    ///
    /// This is the preferred construction API on typed call sites — the
    /// status code drives [`ErrorClass`] classification via the table in the
    /// module docs, the body is bounded to [`MAX_BODY_EXCERPT_BYTES`] bytes
    /// (raw body preserved via `tracing::warn!`), and a default hint matching
    /// the class is set unless the caller overrides it.
    ///
    /// Use [`ToolError::from_untyped`] when no typed status is available.
    pub fn from_http_status(
        method: impl Into<String>,
        path: impl Into<String>,
        status: reqwest::StatusCode,
        body: &str,
    ) -> Self {
        let class = classify_status(status, body);
        let code = status.as_u16();
        // Materialize method/path once: we need them for the human message
        // and the structured fields.
        let method = method.into();
        let path = path.into();
        // When the raw body exceeds the excerpt cap, emit a tracing warning
        // so the full body is recoverable from the activity log / structured
        // tracing sink, even though the agent-visible envelope only carries
        // the bounded excerpt.
        if body.len() > MAX_BODY_EXCERPT_BYTES {
            tracing::warn!(
                method = %method,
                path = %path,
                status = code,
                body_bytes = body.len(),
                "upstream body exceeds excerpt cap; truncated for envelope"
            );
        }
        Self::new(format!("upstream returned {code} for {method}"))
            .with_method(method)
            .with_path(path)
            .with_status(code.to_string())
            .with_body(truncate_body_excerpt(body))
            .with_hint(default_hint_for(class))
            .with_error_class(class)
    }

    /// Build a structured envelope for an untyped / status-less failure.
    ///
    /// This is the only construction API to use when no typed source (HTTP
    /// status, provider error variant) is available. It always emits
    /// [`ErrorClass::Internal`] — status-less / unknown errors are **never**
    /// classified as `Transient` (AC4). The message is set on both `error`
    /// and `body` (truncated to the excerpt) so consumers that look at either
    /// field see the same information.
    pub fn from_untyped(method: impl Into<String>, path: impl Into<String>, message: &str) -> Self {
        // Materialize once so we can both emit the structured fields and (when
        // the body is over the cap) record the raw body length to the activity
        // log. The serialized body is the bounded excerpt either way.
        let method = method.into();
        let path = path.into();
        if message.len() > MAX_BODY_EXCERPT_BYTES {
            tracing::warn!(
                method = %method,
                path = %path,
                body_bytes = message.len(),
                "untyped tool error body exceeds excerpt cap; truncating"
            );
        }
        Self::new(message)
            .with_method(method)
            .with_path(path)
            .with_body(truncate_body_excerpt(message))
            .with_hint(default_hint_for(ErrorClass::Internal))
            .with_error_class(ErrorClass::Internal)
    }

    /// Build a structured envelope from an upstream HTTP/GitHub error string.
    ///
    /// **Deprecated.** Use [`ToolError::from_http_status`] (typed status) or
    /// [`ToolError::from_untyped`] (status-less) directly on call sites where
    /// the typed source is available. This wrapper exists only as a soft
    /// fallback for the two legacy `github_tools.rs` callers that receive an
    /// `anyhow::Error` display string; the migration in T2 removes them.
    ///
    /// When a numeric HTTP status is present in the message this dispatches
    /// to [`from_http_status`](Self::from_http_status); otherwise it
    /// dispatches to [`from_untyped`](Self::from_untyped), which classifies
    /// the failure as [`ErrorClass::Internal`] (never `Transient`).
    #[deprecated(
        since = "0.1.0",
        note = "use from_http_status (typed status) or from_untyped (status-less) on typed call sites"
    )]
    pub fn from_http_error(method: &str, path: &str, raw: &str) -> Self {
        if let Some(status) = extract_http_status(raw) {
            // Parse the salvaged status back into a reqwest::StatusCode. The
            // extraction is already restricted to 100..=599, so this is safe.
            let code: u16 = status
                .parse()
                .expect("extract_http_status yields 3-digit 100-599");
            let parsed = reqwest::StatusCode::from_u16(code)
                .expect("extract_http_status yields a valid StatusCode");
            Self::from_http_status(method, path, parsed, raw)
        } else {
            Self::from_untyped(method, path, raw)
        }
    }
}

/// Either a successful tool payload or a structured [`ToolError`].
///
/// Untagged, like the legacy `ErrorOr<T>`, so a success serializes as the bare
/// payload and a failure as the error envelope object — no wrapper key. This is
/// the G3-aware sibling of `ErrorOr<T>`: the error arm carries the full
/// `{ error, status, method, path, body, hint, error_class }` structure instead
/// of a flat `{ error: String }`, while a plain `ToolError::new("msg")` still
/// serializes to `{ "error": "msg" }` for backward-compatible consumers.
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

/// Classify a typed HTTP status (+ optional body inspection) into an
/// [`ErrorClass`].
///
/// Classification table:
/// - 404 → [`ErrorClass::NotFound`]
/// - 401, 403 → [`ErrorClass::Permission`]
/// - 409 → [`ErrorClass::ConflictRecoverable`]
/// - 422 with a known recovery-action substring ("already exists",
///   "validation failed") → [`ErrorClass::ConflictRecoverable`]
/// - 400, generic 422 → [`ErrorClass::Validation`]
/// - 429 → [`ErrorClass::RateLimited`]
/// - 5xx → [`ErrorClass::Transient`]
/// - anything else → [`ErrorClass::Internal`] (NEVER `Transient`)
fn classify_status(status: reqwest::StatusCode, body: &str) -> ErrorClass {
    let code = status.as_u16();
    match code {
        404 => ErrorClass::NotFound,
        401 | 403 => ErrorClass::Permission,
        409 => ErrorClass::ConflictRecoverable,
        422 => {
            // 422 is a generic "unprocessable entity" — only treat as a
            // recoverable conflict when the body carries a known recovery
            // action (typically a "resource already exists" subtype used by
            // GitHub for PR-create and ref-create, or an explicit
            // "Validation Failed" recovery hint from other providers).
            let lower = body.to_ascii_lowercase();
            if lower.contains("already exists") || lower.contains("validation failed") {
                ErrorClass::ConflictRecoverable
            } else {
                ErrorClass::Validation
            }
        }
        400 => ErrorClass::Validation,
        429 => ErrorClass::RateLimited,
        500..=599 => ErrorClass::Transient,
        _ => ErrorClass::Internal,
    }
}

/// Bound a body string to [`MAX_BODY_EXCERPT_BYTES`], appending a
/// `[truncated: N bytes omitted]` marker when truncation occurred. The raw
/// body is not preserved here — callers that need it for tracing should
/// capture it before calling this.
fn truncate_body_excerpt(body: &str) -> String {
    if body.len() <= MAX_BODY_EXCERPT_BYTES {
        return body.to_string();
    }
    // We must truncate on a UTF-8 char boundary. Walk back from the cap until
    // we land on one so we never produce invalid UTF-8 in the excerpt.
    let mut end = MAX_BODY_EXCERPT_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = body.len() - end;
    let mut excerpt = String::with_capacity(end + 32);
    excerpt.push_str(&body[..end]);
    excerpt.push_str(&format!(" [truncated: {omitted} bytes omitted]"));
    excerpt
}

/// Scan an error message for the first 3-digit HTTP status code in the 100–599
/// range (matching the provider layer's `"(422)"` / `"returned 403"` shapes).
#[allow(dead_code)] // kept as the internal helper for the deprecated from_http_error wrapper
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
    use reqwest::StatusCode;

    // ── Existing-field serialization shape ─────────────────────────────────

    #[test]
    fn serializes_all_fields() {
        let err = ToolError::new("boom")
            .with_http_status(404)
            .with_method("provider_model_lookup")
            .with_path("openai/ghost")
            .with_body("no matching model id")
            .with_hint("call provider_models_connected")
            .with_error_class(ErrorClass::NotFound);
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["error"], "boom");
        assert_eq!(v["status"], "404");
        assert_eq!(v["method"], "provider_model_lookup");
        assert_eq!(v["path"], "openai/ghost");
        assert_eq!(v["body"], "no matching model id");
        assert_eq!(v["hint"], "call provider_models_connected");
        assert_eq!(v["error_class"], "not_found");
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
        assert!(!obj.contains_key("error_class"));
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

    // ── New: error_class serde shape ──────────────────────────────────────

    #[test]
    fn error_class_serializes_when_some() {
        let err = ToolError::new("x").with_error_class(ErrorClass::ConflictRecoverable);
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["error_class"], "conflict_recoverable");
    }

    #[test]
    fn error_class_omitted_when_none() {
        let err = ToolError::new("x");
        let v = serde_json::to_value(&err).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("error_class"));
    }

    #[test]
    fn error_class_serde_round_trip_for_every_variant() {
        for class in [
            ErrorClass::NotFound,
            ErrorClass::ConflictRecoverable,
            ErrorClass::Validation,
            ErrorClass::Permission,
            ErrorClass::RateLimited,
            ErrorClass::Transient,
            ErrorClass::Internal,
        ] {
            let v = serde_json::to_value(class).unwrap();
            let back: ErrorClass = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(back, class, "round-trip failed for {class:?}: json={v}");
            // And the wire form is the snake_case name.
            assert_eq!(
                v,
                serde_json::Value::String(class.as_str().to_string()),
                "wire form mismatch for {class:?}"
            );
        }
    }

    // ── Classification table — one test per ErrorClass variant ────────────

    #[test]
    fn from_http_status_classifies_404_as_not_found() {
        let err = ToolError::from_http_status(
            "github_fetch_file",
            "owner/repo:src/x.rs",
            StatusCode::NOT_FOUND,
            "Not Found",
        );
        assert_eq!(err.status.as_deref(), Some("404"));
        assert_eq!(err.error_class, Some(ErrorClass::NotFound));
        assert_eq!(err.method.as_deref(), Some("github_fetch_file"));
        assert_eq!(err.path.as_deref(), Some("owner/repo:src/x.rs"));
        assert_eq!(err.body.as_deref(), Some("Not Found"));
        assert!(err.hint.as_deref().unwrap().contains("not found"));
    }

    #[test]
    fn from_http_status_classifies_401_as_permission() {
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::UNAUTHORIZED,
            "Bad credentials",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Permission));
        assert!(err.hint.as_deref().unwrap().contains("credentials"));
    }

    #[test]
    fn from_http_status_classifies_403_as_permission() {
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::FORBIDDEN,
            "Resource not accessible by integration",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Permission));
    }

    #[test]
    fn from_http_status_classifies_409_as_conflict_recoverable() {
        let err = ToolError::from_http_status(
            "github_merge_pull_request",
            "owner/repo#42",
            StatusCode::CONFLICT,
            "Merge conflict",
        );
        assert_eq!(err.error_class, Some(ErrorClass::ConflictRecoverable));
    }

    #[test]
    fn from_http_status_classifies_422_already_exists_as_conflict_recoverable() {
        let err = ToolError::from_http_status(
            "create_pull_request",
            "owner/repo:feature-branch",
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"message":"Validation Failed","errors":[{"resource":"PullRequest","code":"custom","field":"base","message":"already_exists"}]}"#,
        );
        assert_eq!(err.error_class, Some(ErrorClass::ConflictRecoverable));
    }

    #[test]
    fn from_http_status_classifies_generic_422_as_validation() {
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::UNPROCESSABLE_ENTITY,
            "GitHub API rejected query: syntax is invalid",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Validation));
    }

    #[test]
    fn from_http_status_classifies_400_as_validation() {
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::BAD_REQUEST,
            "missing required parameter `q`",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Validation));
    }

    #[test]
    fn from_http_status_classifies_429_as_rate_limited() {
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::TOO_MANY_REQUESTS,
            "API rate limit exceeded",
        );
        assert_eq!(err.error_class, Some(ErrorClass::RateLimited));
        assert!(err.hint.as_deref().unwrap().contains("back off"));
    }

    #[test]
    fn from_http_status_classifies_5xx_as_transient() {
        for code in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::from_u16(599).unwrap(),
        ] {
            let err = ToolError::from_http_status(
                "github_fetch_file",
                "owner/repo:src/x.rs",
                code,
                "upstream blip",
            );
            assert_eq!(
                err.error_class,
                Some(ErrorClass::Transient),
                "5xx {code} should be Transient"
            );
        }
    }

    #[test]
    fn from_http_status_classifies_unknown_3xx_as_internal() {
        let err = ToolError::from_http_status(
            "github_fetch_file",
            "owner/repo:src/x.rs",
            StatusCode::from_u16(308).unwrap(),
            "Permanent Redirect",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Internal));
    }

    // ── from_untyped — always Internal, never Transient ────────────────────

    #[test]
    fn from_untyped_always_classifies_internal() {
        let err = ToolError::from_untyped(
            "github_search",
            "owner/repo",
            "error sending request: connection reset by peer",
        );
        assert_eq!(err.error_class, Some(ErrorClass::Internal));
        assert_eq!(err.status, None, "untyped errors must not set a status");
        assert!(!err.hint.as_deref().unwrap().contains("retry"));
        assert_eq!(err.method.as_deref(), Some("github_search"));
        assert_eq!(err.path.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn from_untyped_classifies_empty_message_as_internal() {
        let err = ToolError::from_untyped("github_search", "owner/repo", "");
        assert_eq!(err.error_class, Some(ErrorClass::Internal));
        assert_eq!(err.status, None);
        assert!(!err.hint.as_deref().unwrap().contains("retry"));
    }

    // ── Legacy from_http_error wrapper ────────────────────────────────────
    //
    // These tests intentionally exercise the deprecated wrapper to lock in its
    // dispatch behaviour. The `#[allow(deprecated)]` keeps `-D warnings`
    // green until T2 removes the wrapper entirely.

    #[test]
    #[allow(deprecated)]
    fn from_http_error_extracts_422_and_populates_envelope() {
        // Mirrors the github_api search.rs 422 message shape. The wrapper
        // dispatches to from_http_status, so error_class is set.
        let err = ToolError::from_http_error(
            "github_search",
            "owner/repo",
            "GitHub API rejected query (422): the search is invalid",
        );
        assert_eq!(err.status.as_deref(), Some("422"));
        assert_eq!(err.error_class, Some(ErrorClass::Validation));
        assert_eq!(err.method.as_deref(), Some("github_search"));
        assert_eq!(err.path.as_deref(), Some("owner/repo"));
        assert!(err.body.as_deref().unwrap().contains("rejected query"));
        assert!(err.hint.as_deref().unwrap().contains("rejected as invalid"));
    }

    #[test]
    #[allow(deprecated)]
    fn from_http_error_extracts_404() {
        let err = ToolError::from_http_error(
            "github_fetch_file",
            "owner/repo:src/x.rs",
            "fetch_file failed (404): Not Found",
        );
        assert_eq!(err.status.as_deref(), Some("404"));
        assert_eq!(err.error_class, Some(ErrorClass::NotFound));
        assert!(err.hint.as_deref().unwrap().contains("not found"));
    }

    #[test]
    #[allow(deprecated)]
    fn from_http_error_falls_back_to_internal_not_network() {
        // No HTTP status in the message → untyped path → error_class = Internal,
        // status = None, hint does NOT suggest retry (AC4).
        let err = ToolError::from_http_error(
            "github_search",
            "owner/repo",
            "error sending request: connection reset by peer",
        );
        assert_eq!(
            err.error_class,
            Some(ErrorClass::Internal),
            "status-less messages must classify as Internal, not Transient"
        );
        assert!(
            err.status.is_none(),
            "status-less messages must not synthesize a `status` (no more `\"network\"` fallback)"
        );
        let hint = err.hint.as_deref().unwrap();
        assert!(
            !hint.to_ascii_lowercase().contains("retry"),
            "hint must not suggest retry for status-less errors, got: {hint:?}"
        );
    }

    #[test]
    #[allow(deprecated)]
    fn from_http_error_already_exists_422_is_conflict_recoverable() {
        // 422 with the GitHub "already exists" recovery substring should land
        // on ConflictRecoverable — proves the wrapper dispatches through
        // classify_status, not a hand-rolled hint map.
        let err = ToolError::from_http_error(
            "create_pull_request",
            "owner/repo:branch",
            "create_pull_request failed (422): a PR already exists for this branch",
        );
        assert_eq!(err.status.as_deref(), Some("422"));
        assert_eq!(err.error_class, Some(ErrorClass::ConflictRecoverable));
    }

    #[test]
    #[allow(deprecated)]
    fn from_http_error_5xx_dispatches_to_transient() {
        let err = ToolError::from_http_error(
            "github_fetch_file",
            "owner/repo:src/x.rs",
            "upstream returned (503): Service Unavailable",
        );
        assert_eq!(err.status.as_deref(), Some("503"));
        assert_eq!(err.error_class, Some(ErrorClass::Transient));
    }

    // ── Bounded body excerpt ──────────────────────────────────────────────

    #[test]
    fn body_under_cap_is_preserved_verbatim() {
        let small = "a".repeat(MAX_BODY_EXCERPT_BYTES);
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::NOT_FOUND,
            &small,
        );
        let body = err.body.as_deref().unwrap();
        assert_eq!(body.len(), MAX_BODY_EXCERPT_BYTES);
        assert!(
            !body.contains("truncated"),
            "short body must not be truncated"
        );
        assert_eq!(body, small);
    }

    #[test]
    fn body_over_cap_is_truncated_with_marker() {
        // 2 KB body → only the first MAX_BODY_EXCERPT_BYTES survive, with the
        // marker appended. The marker must name the omitted count.
        let big = "x".repeat(2 * MAX_BODY_EXCERPT_BYTES);
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::INTERNAL_SERVER_ERROR,
            &big,
        );
        let body = err.body.as_deref().unwrap();
        assert!(
            body.len() < big.len(),
            "excerpt must be shorter than the raw body"
        );
        assert!(body.starts_with(&"x".repeat(MAX_BODY_EXCERPT_BYTES)));
        assert!(body.contains("[truncated:"));
        assert!(body.contains("bytes omitted]"));
        // The marker name must carry the actual omitted count.
        let omitted = big.len() - MAX_BODY_EXCERPT_BYTES;
        assert!(
            body.contains(&format!("[truncated: {omitted} bytes omitted]")),
            "marker must carry the omitted count, got body tail: {:?}",
            &body[body.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn body_truncation_respects_utf8_boundaries() {
        // Each emoji is 4 bytes; pack the buffer so a naive byte cap would
        // slice mid-codepoint. The excerpt must remain valid UTF-8.
        let body: String = "🦀".repeat(MAX_BODY_EXCERPT_BYTES / 2); // 2 * cap bytes total
        let err = ToolError::from_http_status(
            "github_search",
            "owner/repo",
            StatusCode::INTERNAL_SERVER_ERROR,
            &body,
        );
        let excerpt = err.body.as_deref().unwrap();
        // Round-tripping through serde_json validates UTF-8 cleanly.
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["body"], excerpt);
        assert!(excerpt.contains("truncated"));
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    #[test]
    fn extract_http_status_ignores_non_status_digit_runs() {
        // A bare number that isn't a 3-digit HTTP code must not be misread.
        assert_eq!(extract_http_status("processed 12345 rows"), None);
        assert_eq!(
            extract_http_status("returned 503 Service Unavailable").as_deref(),
            Some("503")
        );
        assert_eq!(extract_http_status("no status here"), None);
    }

    #[test]
    fn classify_status_table_spot_checks() {
        // Direct table checks (belt-and-braces — the from_http_status tests
        // above already exercise each branch end-to-end, but the helper is
        // the single source of truth for the spec).
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND, ""),
            ErrorClass::NotFound
        );
        assert_eq!(
            classify_status(StatusCode::CONFLICT, ""),
            ErrorClass::ConflictRecoverable
        );
        assert_eq!(
            classify_status(StatusCode::UNPROCESSABLE_ENTITY, "already exists"),
            ErrorClass::ConflictRecoverable
        );
        assert_eq!(
            classify_status(StatusCode::UNPROCESSABLE_ENTITY, "Validation Failed"),
            ErrorClass::ConflictRecoverable
        );
        assert_eq!(
            classify_status(StatusCode::UNPROCESSABLE_ENTITY, "other"),
            ErrorClass::Validation
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, ""),
            ErrorClass::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, ""),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_status(StatusCode::from_u16(418).unwrap(), ""),
            ErrorClass::Internal,
            "418 (I'm a teapot) is not a known typed source → Internal, never Transient"
        );
    }
}
