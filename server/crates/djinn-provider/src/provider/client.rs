use anyhow::anyhow;
use async_stream::stream;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

use super::AuthMethod;

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
/// 300s matches OpenCode's default — reasoning models can be slow to start.
const STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(300);

/// HTTP client for streaming SSE requests to LLM provider APIs.
pub struct ApiClient {
    inner: reqwest::Client,
}

impl ApiClient {
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .pool_max_idle_per_host(10)
            .build()
            .expect("failed to build reqwest client");
        Self { inner }
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
        let client = self.inner.clone();
        let url = url.to_string();
        let auth = auth.clone();

        Box::pin(stream! {
            // Retry loop for the initial HTTP request.
            let response = 'retry: {
                let mut attempt = 0u32;
                loop {
                    let mut req = client.post(&url).json(&body);

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
                                break 'retry resp;
                            }

                            let is_retryable = is_retryable_status(status);

                            if should_retry(attempt, is_retryable) {
                                // Check for Retry-After header.
                                let retry_after_ms = resp
                                    .headers()
                                    .get("retry-after")
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .map(|secs| secs * 1000);

                                let body_text = resp.text().await.unwrap_or_default();
                                attempt += 1;
                                let delay_ms = retry_after_ms.unwrap_or_else(|| backoff_delay_ms(attempt));
                                tracing::warn!(
                                    attempt,
                                    status = %status,
                                    delay_ms,
                                    "SSE request failed with retryable status; retrying"
                                );
                                tracing::debug!(body = %body_text, "retryable error body");
                                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                                continue;
                            }

                            // Non-retryable or exhausted retries.
                            let body_text = resp.text().await.unwrap_or_default();
                            yield Err(anyhow!("provider API error {}: {}", status, body_text));
                            return;
                        }
                        Err(e) => {
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

                            yield Err(anyhow!("failed to send SSE request after {} attempts: {}", attempt + 1, e));
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

            loop {
                match tokio::time::timeout(STREAM_CHUNK_TIMEOUT, lines.next_line()).await {
                    Ok(Ok(Some(line))) => {
                        // SSE lines starting with "data: "
                        if let Some(data) = line.strip_prefix("data: ") {
                            let data = data.trim();
                            if data.is_empty() || data == "[DONE]" {
                                continue;
                            }
                            yield Ok(data.to_string());
                        }
                        // Skip event:, id:, comment lines, and blank lines
                    }
                    Ok(Ok(None)) => break, // end of stream
                    Ok(Err(e)) => {
                        yield Err(anyhow!("SSE read error: {}", e));
                        break;
                    }
                    Err(_) => {
                        yield Err(anyhow!(
                            "SSE stream timed out: no data received for {}s",
                            STREAM_CHUNK_TIMEOUT.as_secs()
                        ));
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
        let mut attempt = 0u32;
        loop {
            let mut req = self.inner.post(url).json(&body);

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
                        return resp
                            .text()
                            .await
                            .map_err(|e| anyhow!("failed to read response body: {e}"));
                    }

                    let is_retryable = is_retryable_status(status);
                    if should_retry(attempt, is_retryable) {
                        let retry_after_ms = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(|secs| secs * 1000);

                        let body_text = resp.text().await.unwrap_or_default();
                        attempt += 1;
                        let delay_ms = retry_after_ms.unwrap_or_else(|| backoff_delay_ms(attempt));
                        tracing::warn!(attempt, status = %status, delay_ms, "POST request failed; retrying");
                        tracing::debug!(body = %body_text, "retryable error body");
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }

                    let body_text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("provider API error {}: {}", status, body_text));
                }
                Err(e) => {
                    let is_retryable = e.is_connect() || e.is_timeout() || e.is_request();
                    if should_retry(attempt, is_retryable) {
                        attempt += 1;
                        let delay_ms = backoff_delay_ms(attempt);
                        tracing::warn!(attempt, error = %e, delay_ms, "POST request failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    return Err(anyhow!(
                        "failed to send POST after {} attempts: {}",
                        attempt + 1,
                        e
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

fn should_retry(attempt: u32, is_retryable: bool) -> bool {
    is_retryable && attempt < MAX_RETRIES
}

/// Simple pseudo-random f64 in [0, 1) using system time nanoseconds.
/// Good enough for jitter — no need for a full RNG crate.
fn pseudo_random_f64() -> f64 {
    let nanos = std::time::SystemTime::now()
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
mod tests {
    use super::*;

    #[test]
    fn request_timeout_is_10_minutes() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(600));
    }

    #[test]
    fn stream_chunk_timeout_is_5_minutes() {
        assert_eq!(STREAM_CHUNK_TIMEOUT, Duration::from_secs(300));
    }

    #[test]
    fn backoff_delay_first_attempt() {
        let delay = backoff_delay_ms(1);
        // First attempt: 1000ms * 2^0 = 1000ms, with 0.8-1.2x jitter
        assert!((800..=1200).contains(&delay), "delay was {delay}");
    }

    #[test]
    fn backoff_delay_representative_attempts_stay_within_jitter_window() {
        for attempt in [2, 3, 5] {
            let delay = backoff_delay_ms(attempt);
            let base =
                (INITIAL_BACKOFF_MS as f64 * BACKOFF_MULTIPLIER.powi(attempt as i32 - 1)) as u64;
            let min = (base as f64 * 0.8) as u64;
            let max = (base as f64 * 1.2) as u64;
            assert!(
                (min..=max).contains(&delay),
                "attempt {attempt} delay was {delay}"
            );
        }
    }

    #[test]
    fn backoff_delay_capped_at_max() {
        let delay = backoff_delay_ms(100);
        // Should be capped at MAX_BACKOFF_MS (30s) * 1.2x jitter max
        assert!(
            delay <= ((MAX_BACKOFF_MS as f64 * 1.2) as u64),
            "delay was {delay}"
        );
    }

    #[test]
    fn retryable_status_policy_matches_expectations() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));

        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn retry_budget_allows_only_max_retries_after_initial_attempt() {
        assert!(should_retry(0, true));
        assert!(should_retry(1, true));
        assert!(should_retry(2, true));
        assert!(!should_retry(MAX_RETRIES, true));
        assert!(!should_retry(MAX_RETRIES + 1, true));
        assert!(!should_retry(0, false));
    }

    #[test]
    fn client_builds_successfully() {
        let _client = ApiClient::new();
    }
}
