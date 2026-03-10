use anyhow::anyhow;
use async_stream::stream;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use std::pin::Pin;
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

/// HTTP client for streaming SSE requests to LLM provider APIs.
pub struct ApiClient {
    inner: reqwest::Client,
}

impl ApiClient {
    pub fn new() -> Self {
        let inner = reqwest::Client::builder()
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

                            let is_retryable = status.as_u16() == 429
                                || status.is_server_error();

                            if is_retryable && attempt < MAX_RETRIES {
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

                            if is_retryable && attempt < MAX_RETRIES {
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
                match lines.next_line().await {
                    Ok(Some(line)) => {
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
                    Ok(None) => break, // end of stream
                    Err(e) => {
                        yield Err(anyhow!("SSE read error: {}", e));
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
                        return resp.text().await.map_err(|e| anyhow!("failed to read response body: {e}"));
                    }

                    let is_retryable = status.as_u16() == 429 || status.is_server_error();
                    if is_retryable && attempt < MAX_RETRIES {
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
                    if is_retryable && attempt < MAX_RETRIES {
                        attempt += 1;
                        let delay_ms = backoff_delay_ms(attempt);
                        tracing::warn!(attempt, error = %e, delay_ms, "POST request failed; retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    return Err(anyhow!("failed to send POST after {} attempts: {}", attempt + 1, e));
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
