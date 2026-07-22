use anyhow::anyhow;
use async_stream::stream;
use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

use super::AuthMethod;
use super::error::{ProviderError, redact_secrets};
use super::first_event::first_event_budget;
use super::transport::{
    TransportClassificationInput, classify_exhausted_transport, initial_request_transport_category,
};
use crate::rate_limit::{activate_suppression_window, clear_suppression_window};

/// Collect the secret values an `AuthMethod` puts on the wire so a misbehaving
/// gateway echoing them back in the response body gets them redacted.
fn auth_secrets(auth: &AuthMethod) -> Vec<String> {
    match auth {
        AuthMethod::BearerToken(token) => vec![token.clone()],
        AuthMethod::ApiKeyHeader { key, .. } => vec![key.clone()],
        AuthMethod::NoAuth => Vec::new(),
    }
}

/// Env var that gates the outbound provider-request debug logger. Truthy
/// (`1`/`true`/`yes`/`on`, case-insensitive) turns it on. Default OFF — no log
/// spam and no behaviour change when unset.
const DEBUG_PROVIDER_REQUEST_ENV: &str = "DJINN_DEBUG_PROVIDER_REQUEST";

/// `tracing` target the outbound-request log uses. Grep
/// `djinn_provider::outbound_request` to find the captured lines.
const OUTBOUND_REQUEST_TARGET: &str = "djinn_provider::outbound_request";

fn debug_provider_request_enabled() -> bool {
    std::env::var(DEBUG_PROVIDER_REQUEST_ENV)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Best-effort extraction of the model id from a request body for log fields.
fn body_model_id(body: &serde_json::Value) -> &str {
    body.get("model").and_then(|m| m.as_str()).unwrap_or("")
}

/// Parse one SSE line, returning the trimmed `data:` payload to yield, or `None`
/// for non-data lines (`event:`, `id:`, comments, blanks) and the empty/`[DONE]`
/// sentinels.
///
/// Per the SSE spec the single space after `data:` is OPTIONAL. Anthropic and
/// OpenAI emit `data: {...}` (with space), but some Anthropic-compatible vendors
/// — notably the Kimi for Coding endpoint (`api.kimi.com/coding`) — emit
/// `data:{...}` with NO space. We strip only the `data:` field name and let
/// `trim()` drop the optional leading space so both forms parse. Matching the
/// literal `"data: "` (with space) silently dropped every Kimi event, which
/// surfaced as a "stream ended without events" empty turn.
fn parse_sse_data_line(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    Some(data)
}

/// A single parsed SSE frame from the transport layer.
///
/// Distinguishes the three meaningful outcomes of a `data:` line so that
/// format adapters can observe the provider-specific terminal sentinel
/// (e.g. OpenAI `[DONE]`) rather than relying on raw EOF as a proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrame {
    /// A data payload — the trimmed JSON string from `data: <json>`.
    Data(String),
    /// The `[DONE]` terminal sentinel (`data: [DONE]`).
    Done,
}

/// Classify one SSE line into an [`SseFrame`], or `None` for non-data lines
/// (`event:`, `id:`, comments, blanks) and empty `data:` payloads.
///
/// Unlike [`parse_sse_data_line`] which swallows `[DONE]`, this function
/// surfaces it as [`SseFrame::Done`] so callers can distinguish a clean
/// provider terminal from a raw EOF.
fn classify_sse_line(line: &str) -> Option<SseFrame> {
    let data = line.strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(SseFrame::Done);
    }
    if data.is_empty() {
        return None;
    }
    Some(SseFrame::Data(data.to_string()))
}

/// Render the request headers (auth + extras) into a single redacted string for
/// the debug log. Auth header VALUES are always redacted; any other header that
/// looks credential-bearing is scrubbed via [`redact_secrets`] using the same
/// key/value heuristics as response bodies.
fn render_redacted_headers(auth: &AuthMethod, extra_headers: &HeaderMap) -> String {
    let mut lines: Vec<String> = Vec::new();
    match auth {
        AuthMethod::BearerToken(_) => {
            lines.push("Authorization: Bearer ***REDACTED***".to_string());
        }
        AuthMethod::ApiKeyHeader { header, .. } => {
            lines.push(format!("{header}: ***REDACTED***"));
        }
        AuthMethod::NoAuth => {}
    }
    for (name, value) in extra_headers {
        let raw = value.to_str().unwrap_or("<non-utf8>");
        // Reuse the body redactor on a synthetic `"name":"value"` pair so the
        // existing sensitive-key heuristics (authorization/api-key/secret/bearer)
        // scrub credential-bearing extra headers (e.g. Helicone-Auth).
        let scrubbed = redact_secrets(&format!("\"{name}\":\"{raw}\""), &[]);
        let rendered = scrubbed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(|inner| inner.replacen("\":\"", ": ", 1))
            .unwrap_or_else(|| format!("{name}: {raw}"));
        lines.push(rendered);
    }
    lines.join("\n")
}

/// When `DJINN_DEBUG_PROVIDER_REQUEST` is truthy, log the EXACT outbound LLM
/// HTTP request (method, effective URL, all headers, full JSON body) at INFO,
/// with secrets redacted, just before it is sent. Used to diagnose failures we
/// cannot reproduce offline (the kimi-for-coding/k2p7 empty-stream incident: the
/// literal runtime request — full injected MCP tool set + exact serialization —
/// differs from a known-good curl in some way we can only see by capturing it).
fn log_outbound_request(
    method: &str,
    url: &str,
    body: &serde_json::Value,
    auth: &AuthMethod,
    extra_headers: &HeaderMap,
) {
    if !debug_provider_request_enabled() {
        return;
    }
    let secrets = auth_secrets(auth);
    let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
    // The actual serialized JSON that goes on the wire, then redacted so a
    // gateway-echoed api-key or any sensitive field never lands in the log.
    let body_json = serde_json::to_string(body).unwrap_or_else(|_| "<unserializable>".to_string());
    let body_redacted = redact_secrets(&body_json, &secret_refs);
    let headers_redacted = render_redacted_headers(auth, extra_headers);
    tracing::info!(
        target: OUTBOUND_REQUEST_TARGET,
        provider_model = %body_model_id(body),
        method = %method,
        url = %url,
        headers = %headers_redacted,
        body = %body_redacted,
        "outbound provider request (DJINN_DEBUG_PROVIDER_REQUEST)"
    );
}

/// Build the boundary `anyhow::Error` for a non-success HTTP status: a typed
/// [`ProviderError`] source (so callers can `downcast_ref`) plus a readable,
/// secret-redacted context message.
fn provider_status_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
    secrets: &[&str],
) -> anyhow::Error {
    let redacted = redact_secrets(body, secrets);
    let typed =
        ProviderError::from_status(status.as_u16(), &redacted).with_retry_after(retry_after_ms);
    anyhow::Error::new(typed).context(format!("provider API error {status}: {redacted}"))
}

/// Preserve a pre-body eligible observation through request retry wrappers, but
/// only expose it once the ordinary initial-request retry budget is exhausted.
fn exhausted_transport_error(
    retries_exhausted: bool,
    observed: Option<super::transport::ExhaustedTransportCategory>,
    input: TransportClassificationInput,
    estimated_payload_chars: usize,
    context: String,
) -> anyhow::Error {
    match classify_exhausted_transport(retries_exhausted, observed, input, estimated_payload_chars)
    {
        Some(diagnostic) => anyhow::Error::new(ProviderError::ExhaustedTransport(diagnostic)),
        None => anyhow::Error::new(ProviderError::Transport),
    }
    .context(context)
}

fn initial_stream_read_category(
    error: &std::io::Error,
) -> Option<super::transport::ExhaustedTransportCategory> {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("connection reset") || message.contains("reset by peer") {
        Some(super::transport::ExhaustedTransportCategory::ConnectionReset)
    } else if message.contains("unexpected eof") || message.contains("connection closed") {
        Some(super::transport::ExhaustedTransportCategory::UnexpectedEof)
    } else {
        None
    }
}

// ─── Retry configuration ────────────────────────────────────────────────────

/// Maximum number of retries for transient HTTP errors.
const MAX_RETRIES: u32 = 3;
/// Initial backoff interval in milliseconds.
const INITIAL_BACKOFF_MS: u64 = 1000;
/// Backoff multiplier (exponential).
const BACKOFF_MULTIPLIER: f64 = 2.0;
/// Maximum backoff interval in milliseconds.
const MAX_BACKOFF_MS: u64 = 30_000;

/// Overall HTTP request timeout (covers connect + full response).
/// OpenCode uses 300s; we use 600s since LLM generations
/// with tool use can legitimately take minutes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Timeout for reading the next SSE chunk from a streaming response.
/// If the provider stops sending data for this long, we consider the stream
/// dead. This catches the "hung connection" scenario that the overall request
/// timeout might not catch once headers have already been received.
///
/// Sized to at least the longest reasoning-family first-event floor
/// (`super::first_event::REASONING_FLOOR_TIMEOUT`, 600s) so the per-chunk read
/// timeout is never lower than the first-event (TTFT) detector threshold.
/// Reasoning streams can also have long inter-chunk gaps during extended
/// thinking, so we use 600s across the board. The default first-event budget
/// for non-reasoning models is 90s (well below this), so non-reasoning streams
/// are not penalized.
const STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(600);

/// HTTP client for streaming SSE requests to LLM provider APIs.
pub struct ApiClient {
    inner: reqwest::Client,
    /// Optional ceiling on the first-event (TTFT) detector budget for this
    /// client. When `Some`, `stream_sse` caps the derived budget at this value
    /// instead of [`STREAM_CHUNK_TIMEOUT`]. Always `None` for production
    /// clients; exposed via [`Self::new_with_first_event_budget_override`]
    /// so tests can fire the TTFT guard in milliseconds rather than minutes.
    first_event_budget_override: Option<Duration>,
}

impl ApiClient {
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(10)
            .build()
            .expect("failed to build reqwest client");
        Self {
            inner,
            first_event_budget_override: None,
        }
    }

    /// Build a client whose `stream_sse` first-event guard uses `override_` as
    /// the maximum detector budget instead of [`STREAM_CHUNK_TIMEOUT`].
    ///
    /// `#[doc(hidden)]` — production callers use [`Self::new`]. This is a
    /// test-only escape hatch so the TTFT guard can fire in milliseconds
    /// without waiting on real production budgets (90s default / 600s floor).
    #[doc(hidden)]
    pub fn new_with_first_event_budget_override(override_: Duration) -> Self {
        let inner = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(10)
            .build()
            .expect("failed to build reqwest client");
        Self {
            inner,
            first_event_budget_override: Some(override_),
        }
    }

    /// POST to `url` with `body`, stream the response as SSE data lines.
    ///
    /// Yields raw JSON strings from `data: <json>` SSE lines.
    /// Skips empty lines, comment lines, and `[DONE]` sentinel.
    ///
    /// The initial HTTP request is retried with exponential backoff on
    /// transient errors (429, 5xx, network errors).
    pub fn stream_sse(
        &self,
        url: &str,
        body: serde_json::Value,
        auth: &AuthMethod,
        extra_headers: HeaderMap,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>> {
        let request_body = serde_json::to_vec(&body).expect("serde_json::Value serializes");
        let estimated_payload_chars = request_body.len();
        let client = self.inner.clone();
        let url = url.to_string();
        let auth = auth.clone();
        let secrets = auth_secrets(&auth);
        let first_event_budget_override = self.first_event_budget_override;

        Box::pin(stream! {
            let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
            // Env-gated capture of the literal outbound request (default OFF).
            log_outbound_request("POST", &url, &body, &auth, &extra_headers);
            // Retry loop for the initial HTTP request.
            let response = 'retry: {
                let mut attempt = 0u32;
                let mut observed_transport_category = None;
                loop {
                    let mut req = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").body(request_body.clone());

                    // Apply authentication
                    req = match &auth {
                        AuthMethod::BearerToken(token) => {
                            req.header("Authorization", format!("Bearer {}", token))
                        }
                        AuthMethod::ApiKeyHeader { header, key } => {
                            req.header(header.as_str(), key.as_str())
                        }
                        AuthMethod::NoAuth => req,
                    };

                    // Apply extra headers (e.g. Helicone-Auth, anthropic-version)
                    for (name, value) in &extra_headers {
                        req = req.header(name, value);
                    }

                    match req.send().await {
                        Ok(resp) => {
                            let status = resp.status();
                            if status.is_success() {
                                clear_suppression_window();
                                break 'retry resp;
                            }

                            let is_retryable = is_retryable_status(status);

                            if should_retry(attempt, is_retryable) {
                                // Check for Retry-After header.
                                let retry_after_ms = retry_after_ms(resp.headers());

                                let body_text = resp.text().await.unwrap_or_default();
                                attempt += 1;
                                let delay_ms = retry_after_ms.unwrap_or_else(|| backoff_delay_ms(attempt));
                                if is_rate_limit_status(status) {
                                    activate_suppression_window(Duration::from_millis(delay_ms));
                                }
                                tracing::warn!(
                                    attempt,
                                    status = %status,
                                    delay_ms,
                                    "SSE request failed with retryable status; retrying"
                                );
                                tracing::debug!(body = %redact_secrets(&body_text, &secret_refs), "retryable error body");
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                continue;
                            }

                            // Non-retryable or exhausted retries.
                            let retry_after_ms = retry_after_ms(resp.headers());
                            let body_text = resp.text().await.unwrap_or_default();
                            if let Some(diagnostic) = classify_exhausted_transport(
                                attempt >= MAX_RETRIES,
                                observed_transport_category,
                                TransportClassificationInput::ProviderBody,
                                estimated_payload_chars,
                            ) {
                                yield Err(anyhow::Error::new(ProviderError::ExhaustedTransport(diagnostic)).context(format!("provider API error {status}: {}", redact_secrets(&body_text, &secret_refs))));
                                return;
                            }
                            yield Err(provider_status_error(status, &body_text, retry_after_ms, &secret_refs));
                            return;
                        }
                        Err(e) => {
                            let current_category = initial_request_transport_category(&e);
                            observed_transport_category = observed_transport_category.or(current_category);
                            let is_retryable = e.is_connect()
                                || e.is_timeout()
                                || e.is_request();

                            if should_retry(attempt, is_retryable) {
                                attempt += 1;
                                let delay_ms = backoff_delay_ms(attempt);
                                tracing::warn!(
                                    attempt,
                                    error = %e,
                                    delay_ms,
                                    "SSE request failed with network error; retrying"
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                continue;
                            }

                            let input = current_category.map(TransportClassificationInput::Eligible).unwrap_or(TransportClassificationInput::UnknownTransport);
                            yield Err(exhausted_transport_error(is_retryable && attempt >= MAX_RETRIES, observed_transport_category, input, estimated_payload_chars, format!("failed to send SSE request after {} attempts: {}", attempt + 1, e)));
                            return;
                        }
                    }
                }
            };

            // Convert the byte stream to an async reader we can read lines from
            let byte_stream = response.bytes_stream().map(|r| {
                r.map_err(std::io::Error::other)
            });
            let stream_reader = StreamReader::new(byte_stream);
            let mut lines = BufReader::new(stream_reader).lines();

            // ── First-event (TTFT) guard ─────────────────────────────────────
            //
            // After HTTP headers arrive, a stream that never delivers its first
            // usable SSE `data:` event is wedged. We derive a per-model budget
            // from the request body's model id and apply it until the first
            // data event lands. After that, the normal STREAM_CHUNK_TIMEOUT
            // governs inter-chunk gaps.
            //
            // The transport read timeout must be at least as high as the
            // derived detector threshold, otherwise the first-event guard would
            // be redundant with (or masked by) the chunk timeout. STREAM_CHUNK_TIMEOUT
            // is sized to at least the reasoning floor (600s) precisely for this
            // reason; the `min` here is a defensive belt should the chunk timeout
            // be reduced below a future reasoning floor.
            let model_id = body_model_id(&body).to_string();
            let derived_first_event_budget = first_event_budget(&model_id, None);
            let first_event_timeout = derived_first_event_budget
                .min(first_event_budget_override.unwrap_or(STREAM_CHUNK_TIMEOUT));
            let mut first_event_received = false;

            loop {
                let timeout_dur = if first_event_received {
                    STREAM_CHUNK_TIMEOUT
                } else {
                    first_event_timeout
                };
                match tokio::time::timeout(timeout_dur, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        // Yield payloads from SSE `data:` lines; skip event:, id:,
                        // comment, blank lines and the `[DONE]` sentinel.
                        if let Some(data) = parse_sse_data_line(&line) {
                            first_event_received = true;
                            yield Ok(data.to_string());
                        }
                    }
                    Ok(Ok(None)) => {
                        if !first_event_received {
                            yield Err(exhausted_transport_error(
                                true,
                                None,
                                TransportClassificationInput::Eligible(super::transport::ExhaustedTransportCategory::UnexpectedEof),
                                estimated_payload_chars,
                                "SSE stream ended before the first data event".to_string(),
                            ));
                        }
                        break;
                    }
                    Ok(Err(e)) => {
                        let input = if first_event_received {
                            TransportClassificationInput::PostFirstEvent
                        } else {
                            initial_stream_read_category(&e)
                                .map(TransportClassificationInput::Eligible)
                                .unwrap_or(TransportClassificationInput::UnknownTransport)
                        };
                        yield Err(exhausted_transport_error(
                            !first_event_received,
                            None,
                            input,
                            estimated_payload_chars,
                            format!("SSE read error: {}", e),
                        ));
                        break;
                    }
                    Err(_) => {
                        if !first_event_received {
                            // First-event / TTFT timeout: no usable SSE event
                            // arrived within the derived budget. Emit a typed
                            // retryable Transport error so downstream failover
                            // can classify it (not an empty turn).
                            yield Err(exhausted_transport_error(
                                true,
                                None,
                                TransportClassificationInput::Eligible(super::transport::ExhaustedTransportCategory::SseFirstEventTimeout),
                                estimated_payload_chars,
                                format!(
                                    "SSE first-event (TTFT) timeout: no data event received within {}s for model {:?}",
                                    first_event_timeout.as_secs(),
                                    model_id
                                ),
                            ));
                        } else {
                            yield Err(anyhow::Error::new(ProviderError::Transport).context(format!(
                                "SSE stream timed out: no data received for {}s",
                                STREAM_CHUNK_TIMEOUT.as_secs()
                            )));
                        }
                        break;
                    }
                }
            }
        })
    }

    /// Like [`stream_sse`] but yields [`SseFrame`] instead of raw `String`,
    /// surfacing the `[DONE]` terminal sentinel to format adapters.
    ///
    /// Non-data SSE lines (`event:`, `id:`, comments, blanks) and empty `data:`
    /// payloads are silently skipped. The `[DONE]` sentinel is yielded as
    /// [`SseFrame::Done`] so adapters can enforce terminal-frame invariants
    /// (e.g. failing typed on raw EOF before `[DONE]`).
    pub fn stream_sse_frames(
        &self,
        url: &str,
        body: serde_json::Value,
        auth: &AuthMethod,
        extra_headers: HeaderMap,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<SseFrame>> + Send>> {
        let request_body = serde_json::to_vec(&body).expect("serde_json::Value serializes");
        let estimated_payload_chars = request_body.len();
        let client = self.inner.clone();
        let url = url.to_string();
        let auth = auth.clone();
        let secrets = auth_secrets(&auth);
        let first_event_budget_override = self.first_event_budget_override;

        Box::pin(stream! {
            let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
            log_outbound_request("POST", &url, &body, &auth, &extra_headers);
            let response = 'retry: {
                let mut attempt = 0u32;
                let mut observed_transport_category = None;
                loop {
                    let mut req = client.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").body(request_body.clone());

                    req = match &auth {
                        AuthMethod::BearerToken(token) => {
                            req.header("Authorization", format!("Bearer {}", token))
                        }
                        AuthMethod::ApiKeyHeader { header, key } => {
                            req.header(header.as_str(), key.as_str())
                        }
                        AuthMethod::NoAuth => req,
                    };

                    for (name, value) in &extra_headers {
                        req = req.header(name, value);
                    }

                    match req.send().await {
                        Ok(resp) => {
                            let status = resp.status();
                            if status.is_success() {
                                clear_suppression_window();
                                break 'retry resp;
                            }

                            let is_retryable = is_retryable_status(status);

                            if should_retry(attempt, is_retryable) {
                                let retry_after_ms = retry_after_ms(resp.headers());
                                let body_text = resp.text().await.unwrap_or_default();
                                attempt += 1;
                                let delay_ms = retry_after_ms.unwrap_or_else(|| backoff_delay_ms(attempt));
                                if is_rate_limit_status(status) {
                                    activate_suppression_window(Duration::from_millis(delay_ms));
                                }
                                tracing::warn!(
                                    attempt,
                                    status = %status,
                                    delay_ms,
                                    "SSE request failed with retryable status; retrying"
                                );
                                tracing::debug!(body = %redact_secrets(&body_text, &secret_refs), "retryable error body");
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                continue;
                            }

                            let retry_after_ms = retry_after_ms(resp.headers());
                            let body_text = resp.text().await.unwrap_or_default();
                            if let Some(diagnostic) = classify_exhausted_transport(
                                attempt >= MAX_RETRIES,
                                observed_transport_category,
                                TransportClassificationInput::ProviderBody,
                                estimated_payload_chars,
                            ) {
                                yield Err(anyhow::Error::new(ProviderError::ExhaustedTransport(diagnostic)).context(format!("provider API error {status}: {}", redact_secrets(&body_text, &secret_refs))));
                                return;
                            }
                            yield Err(provider_status_error(status, &body_text, retry_after_ms, &secret_refs));
                            return;
                        }
                        Err(e) => {
                            let current_category = initial_request_transport_category(&e);
                            observed_transport_category = observed_transport_category.or(current_category);
                            let is_retryable = e.is_connect()
                                || e.is_timeout()
                                || e.is_request();

                            if should_retry(attempt, is_retryable) {
                                attempt += 1;
                                let delay_ms = backoff_delay_ms(attempt);
                                tracing::warn!(
                                    attempt,
                                    error = %e,
                                    delay_ms,
                                    "SSE request failed with network error; retrying"
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                continue;
                            }

                            let input = current_category.map(TransportClassificationInput::Eligible).unwrap_or(TransportClassificationInput::UnknownTransport);
                            yield Err(exhausted_transport_error(is_retryable && attempt >= MAX_RETRIES, observed_transport_category, input, estimated_payload_chars, format!("failed to send SSE request after {} attempts: {}", attempt + 1, e)));
                            return;
                        }
                    }
                }
            };

            let byte_stream = response.bytes_stream().map(|r| {
                r.map_err(std::io::Error::other)
            });
            let stream_reader = StreamReader::new(byte_stream);
            let mut lines = BufReader::new(stream_reader).lines();

            let model_id = body_model_id(&body).to_string();
            let derived_first_event_budget = first_event_budget(&model_id, None);
            let first_event_timeout = derived_first_event_budget
                .min(first_event_budget_override.unwrap_or(STREAM_CHUNK_TIMEOUT));
            let mut first_event_received = false;

            loop {
                let timeout_dur = if first_event_received {
                    STREAM_CHUNK_TIMEOUT
                } else {
                    first_event_timeout
                };
                match tokio::time::timeout(timeout_dur, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        if let Some(frame) = classify_sse_line(&line) {
                            // Only real data events reset the TTFT guard;
                            // `[DONE]` is a terminal signal, not content.
                            if matches!(frame, SseFrame::Data(_)) {
                                first_event_received = true;
                            }
                            yield Ok(frame);
                        }
                    }
                    Ok(Ok(None)) => {
                        if !first_event_received {
                            yield Err(exhausted_transport_error(
                                true,
                                None,
                                TransportClassificationInput::Eligible(super::transport::ExhaustedTransportCategory::UnexpectedEof),
                                estimated_payload_chars,
                                "SSE stream ended before the first data event".to_string(),
                            ));
                        }
                        break;
                    }
                    Ok(Err(e)) => {
                        let input = if first_event_received {
                            TransportClassificationInput::PostFirstEvent
                        } else {
                            initial_stream_read_category(&e)
                                .map(TransportClassificationInput::Eligible)
                                .unwrap_or(TransportClassificationInput::UnknownTransport)
                        };
                        yield Err(exhausted_transport_error(
                            !first_event_received,
                            None,
                            input,
                            estimated_payload_chars,
                            format!("SSE read error: {}", e),
                        ));
                        break;
                    }
                    Err(_) => {
                        if !first_event_received {
                            yield Err(exhausted_transport_error(
                                true,
                                None,
                                TransportClassificationInput::Eligible(super::transport::ExhaustedTransportCategory::SseFirstEventTimeout),
                                estimated_payload_chars,
                                format!(
                                    "SSE first-event (TTFT) timeout: no data event received within {}s for model {:?}",
                                    first_event_timeout.as_secs(),
                                    model_id
                                ),
                            ));
                        } else {
                            yield Err(anyhow::Error::new(ProviderError::Transport).context(format!(
                                "SSE stream timed out: no data received for {}s",
                                STREAM_CHUNK_TIMEOUT.as_secs()
                            )));
                        }
                        break;
                    }
                }
            }
        })
    }

    /// POST to `url` with `body`, return the complete JSON response body.
    ///
    /// Used for non-streaming provider requests. Applies the same retry logic
    /// as `stream_sse` for transient errors.
    pub async fn post_json(
        &self,
        url: &str,
        body: serde_json::Value,
        auth: &AuthMethod,
        extra_headers: HeaderMap,
    ) -> anyhow::Result<String> {
        let request_body = serde_json::to_vec(&body).expect("serde_json::Value serializes");
        let estimated_payload_chars = request_body.len();
        let secrets = auth_secrets(auth);
        let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        // Env-gated capture of the literal outbound request (default OFF).
        log_outbound_request("POST", url, &body, auth, &extra_headers);
        let mut attempt = 0u32;
        let mut observed_transport_category = None;
        loop {
            let mut req = self
                .inner
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body.clone());

            req = match auth {
                AuthMethod::BearerToken(token) => {
                    req.header("Authorization", format!("Bearer {}", token))
                }
                AuthMethod::ApiKeyHeader { header, key } => {
                    req.header(header.as_str(), key.as_str())
                }
                AuthMethod::NoAuth => req,
            };

            for (name, value) in &extra_headers {
                req = req.header(name, value);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        clear_suppression_window();
                        let text = resp
                            .text()
                            .await
                            .map_err(|e| anyhow!("failed to read response body: {e}"))?;
                        // A4: a success status with an empty body is a
                        // structurally-broken downstream, not a usable response.
                        // Surface it as a typed InvalidOutput (terminal, breaker-
                        // feeding Failure → failover) rather than handing an empty
                        // string downstream where it would masquerade as a
                        // retryable empty completion the worker hammers.
                        if text.trim().is_empty() {
                            return Err(anyhow::Error::new(ProviderError::InvalidOutput)
                                .context("provider returned a success status with an empty body"));
                        }
                        return Ok(text);
                    }

                    let is_retryable = is_retryable_status(status);
                    if should_retry(attempt, is_retryable) {
                        let retry_after_ms = retry_after_ms(resp.headers());

                        let body_text = resp.text().await.unwrap_or_default();
                        attempt += 1;
                        let delay_ms = retry_after_ms.unwrap_or_else(|| backoff_delay_ms(attempt));
                        if is_rate_limit_status(status) {
                            activate_suppression_window(Duration::from_millis(delay_ms));
                        }
                        tracing::warn!(attempt, status = %status, delay_ms, "POST request failed; retrying");
                        tracing::debug!(body = %redact_secrets(&body_text, &secret_refs), "retryable error body");
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }

                    let retry_after_ms = retry_after_ms(resp.headers());
                    let body_text = resp.text().await.unwrap_or_default();
                    // Preserve an eligible request transport failure observed
                    // before this provider body once the ordinary initial
                    // request retry budget has been exhausted. This mirrors
                    // the streaming paths: the later HTTP status must not
                    // erase a concrete pre-body connection failure.
                    if let Some(diagnostic) = classify_exhausted_transport(
                        attempt >= MAX_RETRIES,
                        observed_transport_category,
                        TransportClassificationInput::ProviderBody,
                        estimated_payload_chars,
                    ) {
                        return Err(anyhow::Error::new(ProviderError::ExhaustedTransport(
                            diagnostic,
                        ))
                        .context(format!(
                            "provider API error {status}: {}",
                            redact_secrets(&body_text, &secret_refs)
                        )));
                    }
                    return Err(provider_status_error(
                        status,
                        &body_text,
                        retry_after_ms,
                        &secret_refs,
                    ));
                }
                Err(e) => {
                    let current_category = initial_request_transport_category(&e);
                    observed_transport_category = observed_transport_category.or(current_category);
                    let is_retryable = e.is_connect() || e.is_timeout() || e.is_request();
                    if should_retry(attempt, is_retryable) {
                        attempt += 1;
                        let delay_ms = backoff_delay_ms(attempt);
                        tracing::warn!(attempt, error = %e, delay_ms, "POST request failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    let input = current_category
                        .map(TransportClassificationInput::Eligible)
                        .unwrap_or(TransportClassificationInput::UnknownTransport);
                    return Err(exhausted_transport_error(
                        is_retryable && attempt >= MAX_RETRIES,
                        observed_transport_category,
                        input,
                        estimated_payload_chars,
                        format!("failed to send POST after {} attempts: {}", attempt + 1, e),
                    ));
                }
            }
        }
    }
}

/// Calculate exponential backoff delay with jitter for a given attempt (1-based).
fn backoff_delay_ms(attempt: u32) -> u64 {
    let base = INITIAL_BACKOFF_MS as f64 * BACKOFF_MULTIPLIER.powi(attempt as i32 - 1);
    let capped = base.min(MAX_BACKOFF_MS as f64);
    // Jitter: 0.8x to 1.2x
    let jitter = 0.8 + (pseudo_random_f64() * 0.4);
    (capped * jitter) as u64
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 429 || status.is_server_error()
}

fn is_rate_limit_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 529)
}

fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    let retry_after =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822)
            .ok()
            .or_else(|| {
                time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
                    .ok()
            })?;
    let now = time::OffsetDateTime::now_utc();
    let delta = retry_after - now;
    let millis = delta.whole_milliseconds();
    (millis > 0).then_some(millis as u64)
}

fn should_retry(attempt: u32, is_retryable: bool) -> bool {
    is_retryable && attempt < MAX_RETRIES
}

/// Simple pseudo-random f64 in [0, 1) using system time nanoseconds.
/// Good enough for jitter — no need for a full RNG crate.
fn pseudo_random_f64() -> f64 {
    let nanos = SystemClockTrait::new()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos % 1000) as f64 / 1000.0
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
