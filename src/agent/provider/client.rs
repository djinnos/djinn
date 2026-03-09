use anyhow::{anyhow, Context};
use async_stream::stream;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

use super::AuthMethod;

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

            let response = req
                .send()
                .await
                .context("failed to send SSE request")?;

            if !response.status().is_success() {
                let status = response.status();
                let body_text = response.text().await.unwrap_or_default();
                yield Err(anyhow!("provider API error {}: {}", status, body_text));
                return;
            }

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
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}
