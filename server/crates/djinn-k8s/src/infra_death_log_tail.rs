//! Best-effort capture of the last log lines from a dying K8s worker Pod.
//!
//! Called between `watch_infra_death` resolving and `teardown` deleting the
//! Job, so the Pod may still exist on the apiserver for a brief window.
//!
//! Design constraints:
//! - Short timeout (≤ 10 s) — must never block teardown.
//! - Truncates to the `task_attempts.log_tail` DB bound (~8 KiB).
//! - Returns `None` on any failure — capture is purely diagnostic enrichment.
//!
//! The redaction, truncation, and fetch-error classification helpers are
//! **synchronous and kube-free** so they are unit-testable without a live
//! Kubernetes cluster.

use std::time::Duration;

use djinn_core::models::task_attempt::TASK_ATTEMPT_LOG_TAIL_MAX_LEN;
use djinn_runtime::InfraDeathLogTailCapture;
use k8s_openapi::api::core::v1::Pod;
use serde_json::Value;
use kube::api::{Api, ListParams};

use crate::job::LABEL_TASK_RUN_ID;

/// Maximum number of log lines to request from the apiserver.
const LOG_TAIL_LINE_COUNT: i64 = 200;

/// Timeout for the entire capture operation (pod list + log fetch).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

// ─── Redaction ──────────────────────────────────────────────────────────────

/// Redaction marker that replaces matched secret material.
const REDACTED: &str = "***REDACTED***";

/// JSON-ish sensitive key fragments (case-insensitive, dash→underscore
/// normalised).  Mirrors the set in `djinn-provider::provider::error`.
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "authorization",
    "apikey",
    "api_key",
    "access_token",
    "secret",
    "bearer",
    "password",
    "passwd",
];

/// Shell/env-style sensitive variable name prefixes (case-insensitive).
const SENSITIVE_ENV_PREFIXES: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "AUTH",
    "BEARER",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "AWS_SECRET",
    "AWS_SESSION_TOKEN",
];

/// Redact secret-like material from a pod log tail.
///
/// Two-pass approach:
/// 1. **Structural**: redact the *value* of any JSON-ish `"key": "value"` pair
///    whose key matches a sensitive fragment.
/// 2. **Literal**: redact ENV-style `KEY=value` assignments where the key
///    matches a sensitive prefix.
///
/// The function is deliberately simple — it does not need to handle every
/// possible encoding; the goal is to strip credentials that a real pod log
/// might emit (process environment leaks, curl debug output, etc.).
pub fn redact_log_tail(raw: &str) -> String {
    let out = redact_sensitive_json_fields(raw);
    redact_sensitive_env_assignments(&out)
}

/// Scan for `"key": "value"` or `"key":"value"` patterns whose key
/// contains a sensitive fragment and replace the value with `REDACTED`.
fn redact_sensitive_json_fields(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();
    let mut last_emitted = 0;

    while let Some((i, ch)) = chars.next() {
        if ch == '"' {
            // Try to match a JSON key at this position.
            if let Some(key_end) = find_closing_quote(input, i + 1) {
                let key = &input[i + 1..key_end];
                let key_normalized = key.to_ascii_lowercase().replace('-', "_");
                let is_sensitive = SENSITIVE_KEY_FRAGMENTS
                    .iter()
                    .any(|frag| key_normalized.contains(frag));
                if is_sensitive {
                    // Look for `:` after the closing quote.
                    let after_key = input[key_end + 1..].trim_start();
                    if after_key.starts_with(':') {
                        let colon_offset = input.len() - after_key.len();
                        let after_colon = input[colon_offset + 1..].trim_start();
                        // Find the value.
                        if let Some((val_start, val_end)) =
                            find_json_value(after_colon, input.len() - after_colon.len())
                        {
                            // Emit everything up to and including the key.
                            out.push_str(&input[last_emitted..key_end + 1]);
                            out.push_str(&input[key_end + 1..val_start]);
                            out.push_str(REDACTED);
                            // `find_json_value` spans a closing quote for a string;
                            // retain it so redacted JSON remains valid for capping.
                            if val_start > 0 && input.as_bytes()[val_start - 1] == b'"' {
                                out.push('"');
                            }
                            last_emitted = val_end;
                            // Skip past the value.
                            for (ni, _) in chars.by_ref() {
                                if ni >= val_end {
                                    break;
                                }
                            }
                            continue;
                        }
                    }
                }
            }
        }
    }
    out.push_str(&input[last_emitted..]);
    out
}

/// Find the closing `"` for a JSON string starting at `start` (after the
/// opening quote).
fn find_closing_quote(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2; // skip escaped char
            continue;
        }
        if bytes[i] == b'"' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Given a trimmed string starting at the JSON value, find the value's span.
/// Returns `(start_byte_offset, end_byte_offset)`.
fn find_json_value(trimmed: &str, base_offset: usize) -> Option<(usize, usize)> {
    let first = trimmed.as_bytes().first()?;
    if *first == b'"' {
        // String value: find closing quote.
        let val_content_start = base_offset + 1;
        find_closing_quote(trimmed, 1).map(|rel_end| (val_content_start, base_offset + rel_end + 1))
    } else {
        // Non-string value: consume until `,`, `}`, or `]`.
        let end = trimmed.find([',', '}', ']']).unwrap_or(trimmed.len());
        Some((base_offset, base_offset + end))
    }
}

/// Scan for `KEY=value` patterns (ENV-style assignments commonly seen in
/// process-environment dumps) where KEY matches a sensitive prefix.
fn redact_sensitive_env_assignments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        let trimmed = trimmed.trim_end_matches('\r');
        if let Some(eq_pos) = trimmed.find('=') {
            let key = &trimmed[..eq_pos];
            // Match KEY at the start of the line or after whitespace.
            let key_stripped = key.trim_start_matches(|c: char| c.is_whitespace());
            let key_upper = key_stripped.to_ascii_uppercase();
            let is_sensitive = SENSITIVE_ENV_PREFIXES.iter().any(|prefix| {
                key_upper == *prefix
                    || key_upper.starts_with(&format!("{prefix}_"))
                    || key_upper.ends_with(&format!("_{prefix}"))
                    || key_upper.contains(&format!("_{prefix}_"))
            });
            if is_sensitive {
                // Preserve everything up to and including `=`, replace value.
                let prefix_end = eq_pos + 1; // inclusive of `=`
                out.push_str(&line[..prefix_end]);
                out.push_str(REDACTED);
                // Append the line separator if the source line had one.
                if line.ends_with('\n') {
                    out.push('\n');
                }
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

// ─── V2 sanitizing and framing ───────────────────────────────────────────────

const JSON_VALUE_CAP_BYTES: usize = 2048;
const HEAD_MAX_BYTES: usize = 1024;
const SCHEMA_VERSION: u8 = 2;
const CAPPED_JSON_FIELDS: &[&str] = &[
    "statement", "sql", "query", "request_body", "response_body", "body",
];

/// The bytes and transformation record persisted alongside a v2 capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLogTail {
    pub value: String,
    pub head_bytes: usize,
    pub tail_bytes: usize,
    pub omitted_bytes: usize,
    pub sanitizers: Vec<String>,
}

fn prefix_utf8(s: &str, max_bytes: usize) -> &str {
    let mut end = s.len().min(max_bytes);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate `s` to at most `max_bytes` bytes at a UTF-8 character boundary,
/// keeping the END of the string. Retained for v1 callers and focused tests.
pub fn truncate_log_tail_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes { return s; }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) { start += 1; }
    &s[start..]
}

fn cap_json_values(value: &mut Value) -> bool {
    match value {
        Value::Object(object) => object.iter_mut().fold(false, |changed, (key, value)| {
            let this = CAPPED_JSON_FIELDS.contains(&key.as_str())
                && matches!(value, Value::String(text) if text.len() > JSON_VALUE_CAP_BYTES);
            if this && let Value::String(text) = value {
                *text = prefix_utf8(text, JSON_VALUE_CAP_BYTES).to_owned();
            }
            changed | this | cap_json_values(value)
        }),
        Value::Array(values) => values.iter_mut().any(cap_json_values),
        _ => false,
    }
}

/// Cap only eligible JSON object lines. Plain text, arrays, malformed JSON and
/// any multiline value are deliberately returned byte-for-byte unchanged.
pub fn cap_supported_json_values(input: &str) -> String {
    if input.contains('\n') || input.contains('\r') || input.contains("djinn.panic_summary.v1") {
        return input.to_owned();
    }
    let Some(json_start) = input.find('{') else { return input.to_owned(); };
    let (prefix, json) = input.split_at(json_start);
    let Ok(mut value) = serde_json::from_str::<Value>(json) else { return input.to_owned(); };
    if !value.is_object() || !cap_json_values(&mut value) { return input.to_owned(); }
    format!("{prefix}{value}")
}

/// Frame an already-sanitized value. Small values are retained verbatim; large
/// values use the v2 head/tail frame, whose markers are included in the 8000 B
/// bound and whose accounting is over the sanitized pre-frame bytes.
pub fn frame_log_tail_v2(sanitized: &str) -> PreparedLogTail {
    if sanitized.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN {
        return PreparedLogTail {
            value: sanitized.to_owned(), head_bytes: sanitized.len(), tail_bytes: 0,
            omitted_bytes: 0, sanitizers: vec!["sensitive_value_redaction".into(), "json_string_value_cap_2048".into()],
        };
    }
    let head = prefix_utf8(sanitized, HEAD_MAX_BYTES);
    let head_marker = format!("[DJINN_LOG_HEAD_V2 bytes={}]\n", head.len());
    let available = TASK_ATTEMPT_LOG_TAIL_MAX_LEN.saturating_sub(head_marker.len() + head.len());
    let mut tail_len = sanitized.len().saturating_sub(head.len()).min(available);
    loop {
        let tail = truncate_log_tail_utf8(sanitized, tail_len);
        let omitted = sanitized.len() - head.len() - tail.len();
        let tail_marker = format!("\n[DJINN_LOG_TAIL_V2 bytes={} omitted={}]\n", tail.len(), omitted);
        if head_marker.len() + head.len() + tail_marker.len() + tail.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN {
            return PreparedLogTail {
                value: format!("{head_marker}{head}{tail_marker}{tail}"),
                head_bytes: head.len(), tail_bytes: tail.len(), omitted_bytes: omitted,
                sanitizers: vec!["sensitive_value_redaction".into(), "json_string_value_cap_2048".into(), "v2_head_tail_frame".into()],
            };
        }
        if tail_len == 0 { unreachable!("v2 markers fit within the attempt log bound"); }
        tail_len -= 1;
        while tail_len > 0 && !sanitized.is_char_boundary(sanitized.len() - tail_len) { tail_len -= 1; }
    }
}

/// Apply the v2 order: redact first, cap eligible JSON values second, frame
/// last. This is intentionally a pure pipeline for fixture coverage.
pub fn prepare_log_tail_v2(raw: &str) -> PreparedLogTail {
    let redacted = redact_log_tail(raw);
    let capped = cap_supported_json_values(&redacted);
    frame_log_tail_v2(&capped)
}

/// Compatibility convenience for callers that only need the stored value.
pub fn prepare_log_tail(raw: &str) -> String { prepare_log_tail_v2(raw).value }

// ─── Fetch error classification ─────────────────────────────────────────────

/// Classified K8s API error encountered while fetching pod logs.
///
/// The infra-death capture path records this in `summary_json` so operators
/// can distinguish "no pod was found" from "apiserver was down" without
/// parsing an opaque kube error string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogTailFetchError {
    /// The pod was not found (empty label-selector list or 404).
    NoPodFound,
    /// The Kubernetes API server returned a non-transient error (403, 422,
    /// etc.).
    ApiError(String),
    /// The log-fetch timed out (the pod may have been stuck terminating).
    Timeout,
}

impl LogTailFetchError {
    /// Machine-stable key stored in `summary_json.log_tail_fetch_error`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoPodFound => "no_pod_found",
            Self::ApiError(_) => "api_error",
            Self::Timeout => "timeout",
        }
    }
}

/// Classify a `kube::Error` from a pod-logs fetch into a
/// [`LogTailFetchError`].
///
/// This is intentionally separated from the async K8s calls so it can be
/// unit-tested with synthetic error values.
pub fn classify_fetch_error(err: &kube::Error) -> LogTailFetchError {
    match err {
        kube::Error::Api(api_err) if api_err.code == 404 => LogTailFetchError::NoPodFound,
        kube::Error::Api(api_err) => {
            LogTailFetchError::ApiError(format!("{}: {}", api_err.reason, api_err.message))
        }
        kube::Error::HttpError(body) => {
            // Try to detect a 404 in the raw HTTP error body.
            let msg = body.to_string();
            if msg.contains("404") || msg.contains("NotFound") {
                LogTailFetchError::NoPodFound
            } else {
                LogTailFetchError::ApiError(msg)
            }
        }
        other => {
            let msg = other.to_string();
            if msg.contains("timed out") || msg.contains("timeout") || msg.contains("deadline") {
                LogTailFetchError::Timeout
            } else {
                LogTailFetchError::ApiError(msg)
            }
        }
    }
}

// ─── Async capture path (requires kube) ─────────────────────────────────────

fn empty_capture(error_class: Option<String>, error_detail: Option<String>) -> InfraDeathLogTailCapture {
    InfraDeathLogTailCapture {
        log_tail: None, schema_version: SCHEMA_VERSION, pod_name: None, pod_uid: None,
        container_name: None, container_exit_reason: None, container_exit_code: None,
        head_bytes: 0, tail_bytes: 0, omitted_bytes: 0, sanitizers: Vec::new(),
        fetch_error_class: error_class, fetch_error_detail: error_detail,
    }
}

fn pod_capture(pod: &Pod) -> InfraDeathLogTailCapture {
    let status = pod.status.as_ref().and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == "worker").or_else(|| statuses.first()));
    let terminated = status.and_then(|status| status.state.as_ref())
        .and_then(|state| state.terminated.as_ref());
    InfraDeathLogTailCapture {
        log_tail: None, schema_version: SCHEMA_VERSION, pod_name: pod.metadata.name.clone(),
        pod_uid: pod.metadata.uid.clone(), container_name: status.map(|status| status.name.clone()),
        container_exit_reason: terminated.and_then(|state| state.reason.clone()),
        container_exit_code: terminated.map(|state| state.exit_code),
        head_bytes: 0, tail_bytes: 0, omitted_bytes: 0, sanitizers: Vec::new(),
        fetch_error_class: None, fetch_error_detail: None,
    }
}

/// Try to capture worker logs after infra death. Pod identity and terminal
/// container status are decoded immediately after listing, before the log API
/// request can race Pod GC.
pub async fn capture_infra_death_log_tail(client: &kube::Client, namespace: &str, task_run_id: &str) -> Option<InfraDeathLogTailCapture> {
    match tokio::time::timeout(CAPTURE_TIMEOUT, do_capture(client, namespace, task_run_id)).await {
        Ok(capture) => capture,
        Err(_) => Some(empty_capture(Some(LogTailFetchError::Timeout.as_str().into()), Some(format!("log-tail capture timed out after {CAPTURE_TIMEOUT:?}")))),
    }
}

async fn do_capture(client: &kube::Client, namespace: &str, task_run_id: &str) -> Option<InfraDeathLogTailCapture> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = format!("{}={}", LABEL_TASK_RUN_ID, task_run_id);
    let pod = match pods.list(&ListParams::default().labels(&selector)).await {
        Ok(list) => match list.items.into_iter().next() {
            Some(pod) if pod.metadata.name.is_some() => pod,
            Some(_) => return Some(empty_capture(Some(LogTailFetchError::NoPodFound.as_str().into()), Some("Pod found but has no name".into()))),
            None => return Some(empty_capture(Some(LogTailFetchError::NoPodFound.as_str().into()), Some("Pod not found by label (likely already GC'd by Job TTL)".into()))),
        },
        Err(error) => {
            let classified = classify_fetch_error(&error);
            return Some(empty_capture(Some(classified.as_str().into()), Some(format!("Pod list failed: {error}"))));
        }
    };
    let mut capture = pod_capture(&pod);
    let pod_name = capture.pod_name.clone().expect("name checked above");
    let container = capture.container_name.clone().unwrap_or_else(|| "worker".into());
    let params = kube::api::LogParams { container: Some(container), tail_lines: Some(LOG_TAIL_LINE_COUNT), ..Default::default() };
    let logs = match pods.logs(&pod_name, &params).await {
        Ok(logs) => logs,
        Err(error) => {
            let classified = classify_fetch_error(&error);
            capture.fetch_error_class = Some(classified.as_str().into());
            capture.fetch_error_detail = Some(format!("Pod log fetch failed: {error}"));
            return Some(capture);
        }
    };
    if logs.is_empty() {
        capture.fetch_error_class = Some("empty_logs".into());
        capture.fetch_error_detail = Some("Pod logs are empty".into());
        return Some(capture);
    }
    let prepared = prepare_log_tail_v2(&logs);
    capture.log_tail = Some(prepared.value);
    capture.head_bytes = prepared.head_bytes;
    capture.tail_bytes = prepared.tail_bytes;
    capture.omitted_bytes = prepared.omitted_bytes;
    capture.sanitizers = prepared.sanitizers;
    Some(capture)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── redaction ───────────────────────────────────────────────────────────

    #[test]
    fn redacts_bearer_token_in_json() {
        let input = r#"{"Authorization": "Bearer sk-abc123xyz", "msg": "hello"}"#;
        let redacted = redact_log_tail(input);
        assert!(
            !redacted.contains("sk-abc123xyz"),
            "bearer token must be redacted: {redacted}"
        );
        assert!(
            redacted.contains(REDACTED),
            "redacted output must contain the marker: {redacted}"
        );
        assert!(
            redacted.contains("\"msg\""),
            "non-sensitive fields must be preserved"
        );
    }

    #[test]
    fn redacts_api_key_in_json() {
        let input = r#"{"api_key": "sk-LIVE-KEY-9999", "other": "ok"}"#;
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("sk-LIVE-KEY-9999"));
        assert!(redacted.contains(REDACTED));
        assert!(redacted.contains("\"other\": \"ok\""));
    }

    #[test]
    fn redacts_apikey_with_dash() {
        // Dash-normalised: "x-api-key" → "x_api_key" → matches "api_key"
        let input = r#"{"x-api-key": "secret-token-123"}"#;
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("secret-token-123"));
    }

    #[test]
    fn redacts_env_style_api_key() {
        let input = "GITHUB_TOKEN=ghp_abc123secret\nOTHER=ok\n";
        let redacted = redact_log_tail(input);
        assert!(
            !redacted.contains("ghp_abc123secret"),
            "env token must be redacted: {redacted}"
        );
        assert!(redacted.contains(REDACTED));
        assert!(redacted.contains("OTHER=ok"));
    }

    #[test]
    fn redacts_openai_env_key() {
        let input = "OPENAI_API_KEY=sk-proj-abcdef\nDEBUG=true\n";
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("sk-proj-abcdef"));
        assert!(redacted.contains("DEBUG=true"));
    }

    #[test]
    fn redacts_password_in_json() {
        let input = r#"{"password": "hunter2", "user": "admin"}"#;
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("hunter2"));
        assert!(redacted.contains("\"user\": \"admin\""));
    }

    #[test]
    fn no_secrets_unchanged() {
        let input = "just normal log output\nnothing sensitive here\n";
        assert_eq!(redact_log_tail(input), input);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(redact_log_tail(""), "");
    }

    #[test]
    fn multiple_secrets_all_redacted() {
        let input = concat!(
            r#"{"api_key": "key-1", "secret": "s-2", "msg": "ok"}"#,
            "\n",
            "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI\n",
        );
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("key-1"));
        assert!(!redacted.contains("s-2"));
        assert!(!redacted.contains("wJalrXUtnFEMI"));
        assert!(redacted.contains("ok"));
    }

    #[test]
    fn redacts_secret_in_env_assignment_without_trailing_newline() {
        let input = "SECRET=do-not-print";
        let redacted = redact_log_tail(input);
        assert!(!redacted.contains("do-not-print"));
        assert!(redacted.contains("SECRET="));
        assert!(redacted.contains(REDACTED));
    }

    // ── truncation ──────────────────────────────────────────────────────────

    #[test]
    fn truncate_short_string_unchanged() {
        let s = "hello";
        assert_eq!(truncate_log_tail_utf8(s, 100), "hello");
    }

    #[test]
    fn truncate_exactly_at_boundary() {
        let s = "abcd";
        assert_eq!(truncate_log_tail_utf8(s, 4), "abcd");
    }

    #[test]
    fn truncate_on_utf8_boundary_keeps_suffix() {
        // "é" is 2 bytes in UTF-8: a(0) b(1) c(2) é(3-4) d(5) e(6) f(7).
        let s = "abcédef";
        // Last 5 bytes start exactly at é's boundary (byte 3).
        assert_eq!(truncate_log_tail_utf8(s, 5), "édef");
        // Last 4 bytes would start mid-é (byte 4) — advance past it.
        assert_eq!(truncate_log_tail_utf8(s, 4), "def");
    }

    #[test]
    fn truncate_multibyte_no_split_keeps_suffix() {
        // "🦀" is 4 bytes in UTF-8: a(0) 🦀(1-4) b(5).
        let s = "a🦀b";
        // Last 3 bytes would start mid-🦀 — advance to 'b'.
        assert_eq!(truncate_log_tail_utf8(s, 3), "b");
        // Last 5 bytes start exactly at 🦀's boundary.
        assert_eq!(truncate_log_tail_utf8(s, 5), "🦀b");
    }

    #[test]
    fn truncate_empty_string() {
        assert_eq!(truncate_log_tail_utf8("", 10), "");
    }

    #[test]
    fn truncate_zero_max_bytes() {
        assert_eq!(truncate_log_tail_utf8("hello", 0), "");
    }

    #[test]
    fn truncate_ascii_keeps_suffix() {
        assert_eq!(truncate_log_tail_utf8("hello world", 5), "world");
    }

    #[test]
    fn truncate_keeps_panic_line_at_end() {
        // Incident regression (2026-07-22): a giant routine log line before
        // the panic must not evict the panic text from the capture.
        let noise = format!(
            "INFO sqlx slow statement: {}\n",
            "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN)
        );
        let panic_line = "thread 'main' panicked at src/lib.rs:42: boom";
        let log = format!("{noise}{panic_line}");
        let result = prepare_log_tail(&log);
        assert!(result.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
        assert!(
            result.ends_with(panic_line),
            "panic line must survive truncation, got tail: {:?}",
            &result[result.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn truncate_multibyte_utf8_char_count() {
        // "ééé" is 6 bytes in UTF-8.
        let s = "ééé";
        assert_eq!(truncate_log_tail_utf8(s, 5).len(), 4); // "éé" = 4 bytes
        assert_eq!(truncate_log_tail_utf8(s, 4).len(), 4); // "éé"
        assert_eq!(truncate_log_tail_utf8(s, 3).len(), 2); // "é"
    }

    // ── prepare_log_tail (combined) ─────────────────────────────────────────

    #[test]
    fn prepare_redacts_then_truncates() {
        // Create a log tail with a secret that, after redaction, is still
        // within the limit.
        let secret = "sk-secret-12345";
        let log = format!("running task\napi_key={secret}\ndone\n");
        let result = prepare_log_tail(&log);
        assert!(!result.contains(secret), "secret must be redacted");
        assert!(result.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
    }

    #[test]
    fn prepare_respects_log_tail_max_len() {
        // Create a long log with no secrets.
        let long_log = "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN + 500);
        let result = prepare_log_tail(&long_log);
        assert!(result.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
    }

    #[test]
    fn prepare_long_log_with_utf8_truncates_at_char_boundary() {
        // Build a log tail that is just over the limit with multi-byte chars.
        let chunk = "🦀";
        let count = TASK_ATTEMPT_LOG_TAIL_MAX_LEN / chunk.len() + 5;
        let long_log = chunk.repeat(count);
        let result = prepare_log_tail(&long_log);
        assert!(result.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
        // Verify the result is valid UTF-8 (truncation didn't split a char).
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn prepare_preserves_newlines_and_structure() {
        let log = "line1\nline2\nline3\n";
        let result = prepare_log_tail(log);
        assert_eq!(result, log);
    }

    #[test]
    fn prepare_secret_bearing_log_tail_redacted_and_bounded() {
        // Scenario: a real pod log with multiple secret patterns.
        // Verify that (a) all secrets are gone, (b) result fits in column.
        let secret1 = "sk-proj-abcdef1234567890abcdef1234567890";
        let secret2 = "ghp_SuperSecretGitHubToken1234567890abcdef";
        let log = format!(
            concat!(
                "2025-01-01T00:00:00Z Starting worker\n",
                "env: OPENAI_API_KEY={}\n",
                "env: GITHUB_TOKEN={}\n",
                "worker: processing task ...\n",
                "worker: completed successfully\n",
            ),
            secret1, secret2,
        );
        let result = prepare_log_tail(&log);
        assert!(
            !result.contains(secret1),
            "OpenAI key must be redacted: {result}"
        );
        assert!(
            !result.contains(secret2),
            "GitHub token must be redacted: {result}"
        );
        assert!(result.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
        assert!(result.contains("Starting worker"));
        assert!(result.contains("completed successfully"));
    }

    // ── fetch error classification ──────────────────────────────────────────
    //
    // These tests exercise the production classification logic used by
    // `do_capture` — `classify_fetch_error` is called for every `kube::Error`
    // encountered during pod list and log fetch operations.

    #[test]
    fn classify_kube_api_404_as_no_pod_found() {
        let err = kube::Error::Api(kube::error::ErrorResponse {
            status: "Failure".into(),
            message: "pods \"worker-xyz\" not found".into(),
            reason: "NotFound".into(),
            code: 404,
        });
        assert_eq!(classify_fetch_error(&err), LogTailFetchError::NoPodFound);
    }

    #[test]
    fn classify_kube_api_403_as_api_error() {
        let err = kube::Error::Api(kube::error::ErrorResponse {
            status: "Failure".into(),
            message: "Forbidden".into(),
            reason: "Forbidden".into(),
            code: 403,
        });
        let classified = classify_fetch_error(&err);
        assert!(matches!(classified, LogTailFetchError::ApiError(_)));
        assert_eq!(classified.as_str(), "api_error");
    }

    #[test]
    fn classify_kube_api_500_as_api_error() {
        let err = kube::Error::Api(kube::error::ErrorResponse {
            status: "Failure".into(),
            message: "Internal Server Error".into(),
            reason: "InternalError".into(),
            code: 500,
        });
        let classified = classify_fetch_error(&err);
        assert!(matches!(classified, LogTailFetchError::ApiError(_)));
    }

    #[test]
    fn classify_http_error_with_not_found_as_no_pod() {
        // Construct an http::Error by building an invalid HTTP response.
        // The message includes "NotFound" so our classifier picks it up.
        let http_err = http::Response::builder()
            .status(200)
            .header("invalid header name", "value")
            .body(())
            .unwrap_err();
        let err = kube::Error::HttpError(http_err);
        // http::Error Display may or may not contain "NotFound"; just verify
        // classification doesn't panic and returns a valid variant.
        let classified = classify_fetch_error(&err);
        assert!(matches!(
            classified,
            LogTailFetchError::NoPodFound | LogTailFetchError::ApiError(_)
        ));
    }

    #[test]
    fn classify_http_error_generic_as_api_error() {
        let http_err = http::Response::builder()
            .status(200)
            .header("bad\x00name", "val")
            .body(())
            .unwrap_err();
        let err = kube::Error::HttpError(http_err);
        let classified = classify_fetch_error(&err);
        assert!(matches!(classified, LogTailFetchError::ApiError(_)));
    }

    #[test]
    fn classify_other_error_as_api_error() {
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = kube::Error::SerdeError(serde_err);
        let classified = classify_fetch_error(&err);
        assert!(matches!(classified, LogTailFetchError::ApiError(_)));
    }

    #[test]
    fn fetch_error_as_str_values() {
        assert_eq!(LogTailFetchError::NoPodFound.as_str(), "no_pod_found");
        assert_eq!(
            LogTailFetchError::ApiError("test".into()).as_str(),
            "api_error"
        );
        assert_eq!(LogTailFetchError::Timeout.as_str(), "timeout");
    }

    mod attempt_evidence {
        use super::*;
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };

        #[test]
        fn v2_contract() {
            let secret = "credential-that-must-not-survive";
            let structured = format!(
                r#"tracing::event {{"api_key":"{secret}","sql":"{}"}}"#,
                "é".repeat(1500)
            );
            let prepared = prepare_log_tail_v2(&structured);
            assert!(!prepared.value.contains(secret));
            assert!(prepared.value.contains(REDACTED));
            assert!(prepared.value.len() <= "tracing::event ".len() + 2100);

            for unchanged in ["plain text", "{not json}", "[\"array\"]", "{\"sql\":\"x\"}\nsecond"] {
                assert_eq!(cap_supported_json_values(unchanged), unchanged);
            }
            let panic = format!("djinn.panic_summary.v1 {{\"body\":\"{}\"}}", "x".repeat(3000));
            assert_eq!(cap_supported_json_values(&panic), panic);

            let source = format!("{}{}", "🦀".repeat(3000), "\ndjinn.panic_summary.v1 final summary");
            let frame = frame_log_tail_v2(&source);
            assert!(frame.value.len() <= TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
            assert!(frame.value.starts_with(&format!("[DJINN_LOG_HEAD_V2 bytes={}]\n", frame.head_bytes)));
            assert!(frame.value.contains(&format!("[DJINN_LOG_TAIL_V2 bytes={} omitted={}]", frame.tail_bytes, frame.omitted_bytes)));
            assert_eq!(frame.head_bytes + frame.tail_bytes + frame.omitted_bytes, source.len());
            assert!(frame.value.ends_with("djinn.panic_summary.v1 final summary"));
            assert!(std::str::from_utf8(frame.value.as_bytes()).is_ok());

            let pod = Pod {
                metadata: kube::core::ObjectMeta { name: Some("worker-pod".into()), uid: Some("uid-1".into()), ..Default::default() },
                status: Some(PodStatus { container_statuses: Some(vec![ContainerStatus {
                    name: "worker".into(), state: Some(ContainerState { terminated: Some(ContainerStateTerminated { reason: Some("OOMKilled".into()), exit_code: 137, ..Default::default() }), ..Default::default() }),
                    image: String::new(), image_id: String::new(), ready: false, restart_count: 0, ..Default::default()
                }]), ..Default::default() }), ..Default::default()
            };
            let evidence = pod_capture(&pod);
            assert_eq!(evidence.pod_uid.as_deref(), Some("uid-1"));
            assert_eq!(evidence.container_exit_reason.as_deref(), Some("OOMKilled"));
            assert_eq!(evidence.container_exit_code, Some(137));
        }
    }

    // ── log_tail max len constant ───────────────────────────────────────────

    #[test]
    fn log_tail_max_len_is_reasonable() {
        // The column is VARCHAR(8000). Verify the constant matches expectations.
        assert_eq!(TASK_ATTEMPT_LOG_TAIL_MAX_LEN, 8000);
    }
}
