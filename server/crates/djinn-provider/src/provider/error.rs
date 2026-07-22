//! Typed provider-error taxonomy + secret redaction.
//!
//! Every provider HTTP failure used to be flattened into an
//! `anyhow::Error` carrying `format!("provider API error {status}: {body}")`,
//! forcing every downstream consumer to re-classify the failure by
//! substring-matching the message. [`ProviderError`] structures the failure
//! once, at the provider-crate boundary, so callers can
//! `downcast_ref::<ProviderError>()` and branch on the typed variant while a
//! readable (and redacted) message rides along as the `anyhow` context.
//!
//! Raw response bodies — which a misbehaving gateway may echo the api-key into
//! — are scrubbed via [`redact_secrets`] before they reach any error message
//! or `tracing` field.

/// Structured classification of a provider failure.
///
/// The variant set is intentionally small: it captures the distinctions
/// downstream retry/breaker logic needs, nothing more.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// Rate-limited / over quota (429, 529, 408, 425). `retry_after_ms` is the
    /// server-supplied Retry-After in milliseconds when known.
    #[error("rate limited{}", match .retry_after_ms {
        Some(ms) => format!(" (retry after {ms}ms)"),
        None => String::new(),
    })]
    RateLimit { retry_after_ms: Option<u64> },
    /// Request exceeded the model's context window (413 or body says so).
    #[error("context overflow")]
    ContextOverflow,
    /// Auth failure (401/403) — bad/expired/insufficient credentials.
    #[error("authentication failed")]
    Authentication,
    /// Other 4xx — malformed request the provider rejected.
    #[error("invalid request")]
    InvalidRequest,
    /// 5xx — provider-side internal error.
    #[error("provider internal error (status {status})")]
    ProviderInternal { status: u16 },
    /// Network/transport failure (connect, timeout, broken stream).
    #[error("transport error")]
    Transport,
    /// Eligible stream-initial failure after ordinary request retries.
    #[error("exhausted transport error ({:?})", .0.category)]
    ExhaustedTransport(crate::provider::transport::ExhaustedTransportDiagnostic),
    /// Provider returned a 200 but no usable completion (empty turn / refusal).
    #[error("empty completion")]
    EmptyCompletion,
    /// Provider returned output that could not be parsed into our shape.
    #[error("invalid output")]
    InvalidOutput,
}

impl ProviderError {
    /// Whether this failure class is worth retrying.
    ///
    /// RateLimit / ProviderInternal{5xx} / Transport / EmptyCompletion are
    /// transient; Authentication / InvalidRequest / ContextOverflow /
    /// InvalidOutput are terminal for the request as sent.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::RateLimit { .. }
            | ProviderError::Transport
            | ProviderError::ExhaustedTransport(_)
            | ProviderError::EmptyCompletion => true,
            ProviderError::ProviderInternal { status } => (500..600).contains(status),
            ProviderError::Authentication
            | ProviderError::InvalidRequest
            | ProviderError::ContextOverflow
            | ProviderError::InvalidOutput => false,
        }
    }

    /// Server-supplied Retry-After in milliseconds, if this is a RateLimit
    /// that carried one.
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimit { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }

    /// Classify an HTTP status + response body into a [`ProviderError`].
    ///
    /// Mirrors the spirit of opencode's `statusReason`. `retry_after_ms` is
    /// left `None` here for rate-limit statuses; the HTTP client supplies the
    /// parsed Retry-After via [`ProviderError::with_retry_after`].
    pub fn from_status(status: u16, body: &str) -> Self {
        match status {
            401 | 403 => ProviderError::Authentication,
            408 | 425 | 429 | 529 => ProviderError::RateLimit {
                retry_after_ms: None,
            },
            413 => ProviderError::ContextOverflow,
            _ if (500..600).contains(&status) => ProviderError::ProviderInternal { status },
            _ if (400..500).contains(&status) => {
                if body_indicates_context_overflow(body) {
                    ProviderError::ContextOverflow
                } else if body.trim().is_empty() {
                    // A 4xx with no body carries no actionable "what was
                    // wrong" detail — it's a structurally-broken / absent
                    // downstream (the Fireworks empty-body-404 signature,
                    // `project_fireworks_worker_404_empty_body`), not a request
                    // the provider meaningfully rejected. Name it honestly as
                    // broken output. Like `InvalidRequest` it is terminal and
                    // feeds the breaker as a `Failure` (→ failover once the
                    // consecutive-failure streak trips), so behaviour is
                    // unchanged; the classification just stops pretending the
                    // *request* was the problem.
                    ProviderError::InvalidOutput
                } else {
                    ProviderError::InvalidRequest
                }
            }
            // Anything else (incl. unexpected non-error codes) — treat as a
            // provider-internal failure carrying the raw status.
            _ => ProviderError::ProviderInternal { status },
        }
    }

    /// Classify an **in-stream** OpenAI error event into a [`ProviderError`].
    ///
    /// Unlike [`from_status`], the HTTP response was `200 OK` — the failure
    /// arrived mid-stream as a `response.failed` / `error` SSE event carrying an
    /// error `code` (e.g. `server_error`, `insufficient_quota`,
    /// `context_length_exceeded`) but no HTTP status to lean on. We map the
    /// `code` string, fall back to a body scan for context-overflow phrasing,
    /// and default an unrecognised/absent code to a transient provider-internal
    /// failure: the failure was real, so it should feed the breaker as a 5xx
    /// `Failure` rather than be dropped as an untyped/legacy string error
    /// (which classifies as `None` and never reaches the breaker).
    pub fn from_stream_error(code: Option<&str>, message: &str) -> Self {
        let code = code.unwrap_or("").to_ascii_lowercase();
        if code.contains("rate_limit") || code.contains("quota") {
            ProviderError::RateLimit {
                retry_after_ms: None,
            }
        } else if code.contains("context_length")
            || code.contains("context_window")
            || body_indicates_context_overflow(message)
        {
            ProviderError::ContextOverflow
        } else if code.contains("invalid_api_key")
            || code.contains("authentication")
            || code.contains("unauthorized")
            || code.contains("permission")
        {
            ProviderError::Authentication
        } else if code.contains("invalid_request") || code.contains("invalid_prompt") {
            ProviderError::InvalidRequest
        } else {
            // `server_error`, `internal_error`, an empty code, or any code we
            // don't specifically recognise: treat as a transient provider-side
            // 5xx so it feeds the gentle consecutive-failure breaker.
            ProviderError::ProviderInternal { status: 500 }
        }
    }

    /// Attach a parsed Retry-After (ms) to a RateLimit variant. No-op for
    /// other variants.
    pub fn with_retry_after(self, retry_after_ms: Option<u64>) -> Self {
        match self {
            ProviderError::RateLimit { .. } => ProviderError::RateLimit { retry_after_ms },
            other => other,
        }
    }
}

/// Heuristic: does this 4xx body describe a context-length / prompt-too-long
/// failure? Providers vary wildly in how they phrase it, so we match the same
/// vocabulary the downstream substring classifier looks for.
fn body_indicates_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length")
        || lower.contains("context length")
        || lower.contains("context limit")
        || lower.contains("too many tokens")
        || lower.contains("maximum context")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("token limit")
}

// ─── Secret redaction (A5) ─────────────────────────────────────────────────

const REDACTED: &str = "***REDACTED***";

/// Scrub secrets out of a provider response body before it reaches an error
/// message or a `tracing` field.
///
/// Two passes:
/// 1. Structural: redact the *value* of any JSON-ish field whose key matches
///    `(?i)authorization|api[-_]?key|access[-_]?token|secret|bearer`.
///    Implemented as a non-regex scan (the crate has no `regex` dep).
/// 2. Literal: replace every non-empty entry in `sent_secrets` (and, for any
///    `Bearer <token>` entry, the bare token after the prefix) with the
///    redaction marker — catches a gateway echoing the credential we sent.
pub fn redact_secrets(body: &str, sent_secrets: &[&str]) -> String {
    let mut out = redact_sensitive_fields(body);

    for secret in sent_secrets {
        if secret.is_empty() {
            continue;
        }
        out = out.replace(secret, REDACTED);
        // If the caller passed a full `Bearer <token>` header value, also
        // redact the bare token in case the body echoed it without the prefix.
        if let Some(bare) = secret.strip_prefix("Bearer ") {
            let bare = bare.trim();
            if !bare.is_empty() {
                out = out.replace(bare, REDACTED);
            }
        }
    }

    out
}

/// Sensitive-key fragments. A key is sensitive if its lowercased,
/// dash-to-underscore-normalised form contains any of these substrings.
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "authorization",
    "apikey",
    "api_key",
    "access_token",
    "accesstoken",
    "secret",
    "bearer",
];

/// Does the given JSON key carry a secret value that should be redacted?
///
/// Normalises exactly as before: lowercasing and mapping `-` → `_`, then
/// checks the fragment table.
fn key_is_sensitive(key: &str) -> bool {
    let normalized: String = key
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .collect();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|frag| normalized.contains(frag))
}

// ── Scanner helpers ──────────────────────────────────────────────────────

/// Given the index just past an opening `"`, return the index of the matching
/// closing `"`, honouring backslash escapes. `None` if unterminated.
fn find_string_end(bytes: &[u8], mut i: usize) -> Option<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped char
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// If `bytes[pos]` is `"`, find the matching closing quote.
/// Returns `(open_quote_index, close_quote_index)` on success.
fn try_parse_quoted_string(bytes: &[u8], pos: usize) -> Option<(usize, usize)> {
    if pos < bytes.len() && bytes[pos] == b'"' {
        find_string_end(bytes, pos + 1).map(|end| (pos, end))
    } else {
        None
    }
}

/// Advance past any ASCII whitespace starting at `pos`.
fn skip_whitespace(bytes: &[u8], pos: usize) -> usize {
    let mut p = pos;
    while p < bytes.len() && (bytes[p] as char).is_whitespace() {
        p += 1;
    }
    p
}

/// After parsing a quoted key that ends at `key_end`, check whether it is the
/// left-hand side of a sensitive `"key": "value"` pair.  If so, emit the
/// redacted form into `out` and return `Some(next_position)`.
///
/// Returns `None` when the key is not sensitive, no colon follows, or the
/// value is not a quoted string — the caller should emit the key verbatim.
fn try_redact_kv(
    body: &str,
    bytes: &[u8],
    key_start: usize,
    key_end: usize,
    out: &mut String,
) -> Option<usize> {
    let key = &body[key_start + 1..key_end];
    let after_key = key_end + 1;
    let colon_pos = skip_whitespace(bytes, after_key);

    if colon_pos >= bytes.len() || bytes[colon_pos] != b':' || !key_is_sensitive(key) {
        return None;
    }

    let val_start = skip_whitespace(bytes, colon_pos + 1);
    let (_, val_end) = try_parse_quoted_string(bytes, val_start)?;

    out.push_str(&body[key_start..after_key]);
    out.push_str(&body[after_key..val_start]);
    out.push('"');
    out.push_str(REDACTED);
    out.push('"');
    Some(val_end + 1)
}

/// Walk the text looking for `"<key>" : "<value>"` JSON pairs and replace the
/// value when the key is sensitive.  Operates on the raw string (not a parsed
/// `serde_json::Value`) so it survives non-JSON and malformed bodies.
fn redact_sensitive_fields(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;

    while i < bytes.len() {
        if let Some((key_start, key_end)) = try_parse_quoted_string(bytes, i) {
            if let Some(next) = try_redact_kv(body, bytes, key_start, key_end, &mut out) {
                i = next;
            } else {
                let after_key = key_end + 1;
                out.push_str(&body[i..after_key]);
                i = after_key;
            }
            continue;
        }
        // Default: copy the byte through.
        let ch = body[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_status_maps_auth() {
        assert_eq!(
            ProviderError::from_status(401, ""),
            ProviderError::Authentication
        );
        assert_eq!(
            ProviderError::from_status(403, ""),
            ProviderError::Authentication
        );
    }

    #[test]
    fn from_status_maps_rate_limit() {
        for code in [408, 425, 429, 529] {
            assert_eq!(
                ProviderError::from_status(code, ""),
                ProviderError::RateLimit {
                    retry_after_ms: None
                },
                "status {code} should be RateLimit"
            );
        }
    }

    #[test]
    fn from_status_maps_context_overflow() {
        assert_eq!(
            ProviderError::from_status(413, ""),
            ProviderError::ContextOverflow
        );
        // 400 with a context-length body also classifies as overflow.
        let body =
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#;
        assert_eq!(
            ProviderError::from_status(400, body),
            ProviderError::ContextOverflow
        );
    }

    #[test]
    fn from_status_maps_invalid_request_and_internal() {
        // A 4xx *with* a body the provider supplied to explain the rejection
        // stays InvalidRequest.
        assert_eq!(
            ProviderError::from_status(400, "bad json"),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            ProviderError::from_status(404, r#"{"error":"model not found"}"#),
            ProviderError::InvalidRequest
        );
        assert_eq!(
            ProviderError::from_status(500, ""),
            ProviderError::ProviderInternal { status: 500 }
        );
        assert_eq!(
            ProviderError::from_status(503, ""),
            ProviderError::ProviderInternal { status: 503 }
        );
    }

    #[test]
    fn from_status_empty_bodied_4xx_is_broken_output_not_invalid_request() {
        // A4: an empty-bodied 4xx is the structurally-broken-downstream
        // signature (Fireworks empty-body-404). Classify it as InvalidOutput,
        // distinct from a request the provider meaningfully rejected. Both are
        // terminal + non-retryable + feed the breaker as a Failure, so the only
        // observable change is the honest label.
        for code in [400, 404, 410] {
            assert_eq!(
                ProviderError::from_status(code, ""),
                ProviderError::InvalidOutput,
                "empty-bodied {code} should be InvalidOutput"
            );
            assert_eq!(
                ProviderError::from_status(code, "   \n  "),
                ProviderError::InvalidOutput,
                "whitespace-only-bodied {code} should be InvalidOutput"
            );
            assert!(!ProviderError::from_status(code, "").retryable());
        }
    }

    #[test]
    fn from_stream_error_classifies_by_code() {
        // The mid-stream failure that was being dropped as an untyped string:
        // a `server_error` code → transient 5xx → breaker `Failure`.
        assert_eq!(
            ProviderError::from_stream_error(Some("server_error"), "An error occurred"),
            ProviderError::ProviderInternal { status: 500 }
        );
        // Quota / rate-limit codes → throttle.
        assert_eq!(
            ProviderError::from_stream_error(Some("insufficient_quota"), "quota exceeded"),
            ProviderError::RateLimit {
                retry_after_ms: None
            }
        );
        assert_eq!(
            ProviderError::from_stream_error(Some("rate_limit_exceeded"), ""),
            ProviderError::RateLimit {
                retry_after_ms: None
            }
        );
        // Context overflow by code or by message phrasing.
        assert_eq!(
            ProviderError::from_stream_error(Some("context_length_exceeded"), "too many tokens"),
            ProviderError::ContextOverflow
        );
        assert_eq!(
            ProviderError::from_stream_error(None, "prompt is too long for this model"),
            ProviderError::ContextOverflow
        );
        // Auth.
        assert_eq!(
            ProviderError::from_stream_error(Some("invalid_api_key"), "bad key"),
            ProviderError::Authentication
        );
        // Unknown / absent code defaults to a transient provider-internal
        // failure (real failure, fed to the breaker) rather than untyped.
        assert_eq!(
            ProviderError::from_stream_error(None, "something opaque"),
            ProviderError::ProviderInternal { status: 500 }
        );
        // The classified server_error is retryable, matching its transient nature.
        assert!(
            ProviderError::from_stream_error(Some("server_error"), "boom").retryable(),
            "server_error must be retryable"
        );
    }

    #[test]
    fn retryable_classification() {
        assert!(
            ProviderError::RateLimit {
                retry_after_ms: None
            }
            .retryable()
        );
        assert!(
            ProviderError::RateLimit {
                retry_after_ms: Some(5)
            }
            .retryable()
        );
        assert!(ProviderError::Transport.retryable());
        assert!(ProviderError::EmptyCompletion.retryable());
        assert!(ProviderError::ProviderInternal { status: 500 }.retryable());
        assert!(ProviderError::ProviderInternal { status: 599 }.retryable());

        assert!(!ProviderError::Authentication.retryable());
        assert!(!ProviderError::InvalidRequest.retryable());
        assert!(!ProviderError::ContextOverflow.retryable());
        assert!(!ProviderError::InvalidOutput.retryable());
    }

    #[test]
    fn retry_after_roundtrip() {
        let e = ProviderError::from_status(429, "").with_retry_after(Some(1234));
        assert_eq!(e.retry_after_ms(), Some(1234));
        assert_eq!(
            e,
            ProviderError::RateLimit {
                retry_after_ms: Some(1234)
            }
        );
        // with_retry_after is a no-op on non-rate-limit variants.
        let e2 = ProviderError::Authentication.with_retry_after(Some(99));
        assert_eq!(e2.retry_after_ms(), None);
        assert_eq!(e2, ProviderError::Authentication);
    }

    #[test]
    fn redact_pass1_structural_fields() {
        let body =
            r#"{"authorization":"Bearer sk-abc123","model":"gpt-5","api_key":"k-9","note":"ok"}"#;
        let out = redact_secrets(body, &[]);
        assert!(out.contains(r#""authorization":"***REDACTED***""#), "{out}");
        assert!(out.contains(r#""api_key":"***REDACTED***""#), "{out}");
        // Non-sensitive fields preserved.
        assert!(out.contains(r#""model":"gpt-5""#), "{out}");
        assert!(out.contains(r#""note":"ok""#), "{out}");
        // The literal secret value no longer appears.
        assert!(!out.contains("sk-abc123"), "{out}");
        assert!(!out.contains("k-9"), "{out}");
    }

    #[test]
    fn redact_pass1_case_and_separator_insensitive() {
        let body = r#"{"X-Api-Key":"secret1","AccessToken":"secret2","SECRET":"secret3"}"#;
        let out = redact_secrets(body, &[]);
        assert!(!out.contains("secret1"), "{out}");
        assert!(!out.contains("secret2"), "{out}");
        assert!(!out.contains("secret3"), "{out}");
    }

    #[test]
    fn redact_pass2_literal_secrets() {
        let body = "upstream said: invalid key sk-LIVE-9999 for project";
        let out = redact_secrets(body, &["sk-LIVE-9999"]);
        assert!(!out.contains("sk-LIVE-9999"), "{out}");
        assert!(out.contains("***REDACTED***"), "{out}");
        // Empty entries are skipped (no spurious replacement).
        let out2 = redact_secrets("hello", &[""]);
        assert_eq!(out2, "hello");
    }

    #[test]
    fn redact_pass2_strips_bearer_prefix() {
        let body = "echoed token: tok-7777 (full header: Bearer tok-7777)";
        let out = redact_secrets(body, &["Bearer tok-7777"]);
        assert!(!out.contains("tok-7777"), "{out}");
    }

    #[test]
    fn redact_preserves_non_json_text() {
        let body = "plain text error with no secrets";
        assert_eq!(redact_secrets(body, &[]), body);
    }
}
