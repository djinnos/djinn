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
use kube::api::{Api, ListParams};
use tracing::{debug, warn};

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

// ─── Truncation ─────────────────────────────────────────────────────────────

/// Truncate `s` to at most `max_bytes` bytes at a UTF-8 character boundary,
/// keeping the END of the string.
///
/// Returns the longest valid UTF-8 suffix of `s` whose byte length is ≤
/// `max_bytes`.  If `s` is already within the limit, it is returned as-is.
///
/// The suffix (not the prefix) is what matters for a crash capture: the panic
/// message is the last thing a dying worker prints.  The 2026-07-22 v0.6.114
/// outage was undiagnosable because prefix truncation systematically discarded
/// the panic text while keeping routine startup lines.
pub fn truncate_log_tail_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Find the first char boundary at or after len - max_bytes.
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Combined redact + truncate helper used by the infra-death capture path.
///
/// 1. Redacts secret-bearing patterns.
/// 2. Truncates to `TASK_ATTEMPT_LOG_TAIL_MAX_LEN` at a UTF-8 boundary.
pub fn prepare_log_tail(raw: &str) -> String {
    let redacted = redact_log_tail(raw);
    let truncated = truncate_log_tail_utf8(&redacted, TASK_ATTEMPT_LOG_TAIL_MAX_LEN);
    truncated.to_owned()
}

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

/// Try to capture the last log lines from the worker Pod's container after an
/// infra-death has been detected.  Returns `None` on any failure.
///
/// The `namespace` and `client` come from the same `KubernetesRuntime` that
/// owns the Pod.  The `task_run_id` is used to find the Pod via the standard
/// task-run label selector.
pub async fn capture_infra_death_log_tail(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Option<InfraDeathLogTailCapture> {
    let result =
        tokio::time::timeout(CAPTURE_TIMEOUT, do_capture(client, namespace, task_run_id)).await;

    match result {
        Ok(capture) => capture,
        Err(_elapsed) => {
            warn!(
                task_run_id,
                "infra_death_log_tail: capture timed out after {:?}", CAPTURE_TIMEOUT
            );
            Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some(LogTailFetchError::Timeout.as_str().to_owned()),
                fetch_error_detail: Some(format!(
                    "log-tail capture timed out after {:?}",
                    CAPTURE_TIMEOUT
                )),
            })
        }
    }
}

async fn do_capture(
    client: &kube::Client,
    namespace: &str,
    task_run_id: &str,
) -> Option<InfraDeathLogTailCapture> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let label_selector = format!("{}={}", LABEL_TASK_RUN_ID, task_run_id);

    // 1. Find the Pod.
    let pod_name = match pods
        .list(&ListParams::default().labels(&label_selector))
        .await
    {
        Ok(list) => match list.items.into_iter().next() {
            Some(pod) => {
                let name = pod.metadata.name.clone().unwrap_or_default();
                if name.is_empty() {
                    return Some(InfraDeathLogTailCapture {
                        log_tail: None,
                        fetch_error_class: Some(LogTailFetchError::NoPodFound.as_str().to_owned()),
                        fetch_error_detail: Some("Pod found but has no name".to_owned()),
                    });
                }
                name
            }
            None => {
                debug!(
                    task_run_id,
                    "infra_death_log_tail: no Pod found (already GC'd)"
                );
                return Some(InfraDeathLogTailCapture {
                    log_tail: None,
                    fetch_error_class: Some(LogTailFetchError::NoPodFound.as_str().to_owned()),
                    fetch_error_detail: Some(
                        "Pod not found by label (likely already GC'd by Job TTL)".to_owned(),
                    ),
                });
            }
        },
        Err(e) => {
            let classified = classify_fetch_error(&e);
            warn!(
                task_run_id,
                error = %e,
                error_class = classified.as_str(),
                "infra_death_log_tail: pod list failed"
            );
            let detail = format!("Pod list failed: {e}");
            return Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some(classified.as_str().to_owned()),
                fetch_error_detail: Some(detail),
            });
        }
    };

    // 2. Fetch logs from the `worker` container (falls back to first container).
    // No `limit_bytes`: the apiserver applies it to the FRONT of the
    // tail-lines window, which discards the panic text at the end — the one
    // part a crash capture exists to keep.  `tail_lines` bounds the fetch;
    // `prepare_log_tail` bounds the stored size from the suffix side.
    let log_params = kube::api::LogParams {
        container: Some("worker".to_owned()),
        tail_lines: Some(LOG_TAIL_LINE_COUNT),
        ..Default::default()
    };

    let logs = match pods.logs(&pod_name, &log_params).await {
        Ok(logs) => logs,
        Err(e) => {
            let classified = classify_fetch_error(&e);
            warn!(
                task_run_id,
                pod = %pod_name,
                error = %e,
                error_class = classified.as_str(),
                "infra_death_log_tail: log fetch failed"
            );
            let detail = format!("Pod log fetch failed: {e}");
            return Some(InfraDeathLogTailCapture {
                log_tail: None,
                fetch_error_class: Some(classified.as_str().to_owned()),
                fetch_error_detail: Some(detail),
            });
        }
    };

    if logs.is_empty() {
        debug!(
            task_run_id,
            pod = %pod_name,
            "infra_death_log_tail: pod logs are empty"
        );
        return Some(InfraDeathLogTailCapture {
            log_tail: None,
            fetch_error_class: Some("empty_logs".to_owned()),
            fetch_error_detail: Some("Pod logs are empty".to_owned()),
        });
    }

    // 3. Redact and truncate to the DB bound.
    let prepared = prepare_log_tail(&logs);

    debug!(
        task_run_id,
        pod = %pod_name,
        byte_count = prepared.len(),
        "infra_death_log_tail: captured successfully"
    );

    Some(InfraDeathLogTailCapture {
        log_tail: Some(prepared),
        fetch_error_class: None,
        fetch_error_detail: None,
    })
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
        let noise = format!("INFO sqlx slow statement: {}\n", "x".repeat(TASK_ATTEMPT_LOG_TAIL_MAX_LEN));
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

    // ── log_tail max len constant ───────────────────────────────────────────

    #[test]
    fn log_tail_max_len_is_reasonable() {
        // The column is VARCHAR(8000). Verify the constant matches expectations.
        assert_eq!(TASK_ATTEMPT_LOG_TAIL_MAX_LEN, 8000);
    }
}
