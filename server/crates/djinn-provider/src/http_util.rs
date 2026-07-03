//! Lightweight HTTP JSON-fetch helper for non-LLM outbound requests.
//!
//! Wraps [`reqwest::Client`] so that crates outside `djinn-provider` can
//! perform simple GET → JSON operations without constructing or naming reqwest
//! types directly.  This keeps outbound HTTP client construction confined to
//! `djinn-provider` per the capability-boundary guard.

use std::time::Duration;

/// A thin wrapper around [`reqwest::Client`] for simple JSON GET requests.
///
/// Constructed with a timeout; used by control-plane toolchain-version
/// fetching and similar lightweight data-plane needs that don't require the
/// full LLM provider client stack.
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Build a new client with the given request timeout.
    ///
    /// Returns `None` if the underlying client fails to build.
    pub fn new(timeout: Duration) -> Option<Self> {
        let inner = reqwest::Client::builder().timeout(timeout).build().ok()?;
        Some(Self { inner })
    }

    /// Perform a GET request and parse the response as JSON.
    ///
    /// Returns `None` on any transport, status, or deserialization error.
    pub async fn get_json(&self, url: &str) -> Option<serde_json::Value> {
        let resp = self.inner.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<serde_json::Value>().await.ok()
    }
}
