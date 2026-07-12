//! Lightweight HTTP JSON-fetch helper for non-LLM outbound requests.
//!
//! Wraps [`reqwest::Client`] so that crates outside `djinn-provider` can
//! perform simple GET → JSON operations without constructing or naming reqwest
//! types directly.  This keeps outbound HTTP client construction confined to
//! `djinn-provider` per the capability-boundary guard.

use std::time::Duration;

use bytes::Bytes;

/// Errors that can occur when using the lightweight HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP client build failed: {message}")]
    Build { message: String },
    #[error("HTTP transport failure during {operation}: {message}")]
    Transport { operation: String, message: String },
    #[error("HTTP status {status} during {operation}")]
    Status { operation: String, status: u16 },
    #[error("HTTP body read failure during {operation}: {message}")]
    Body { operation: String, message: String },
    #[error("HTTP body parse failure: {message}")]
    Parse { message: String },
}

/// A thin wrapper around [`reqwest::Client`] for simple JSON GET requests.
///
/// Constructed with a timeout; used by control-plane toolchain-version
/// fetching and similar lightweight data-plane needs that don't require the
/// full LLM provider client stack.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    /// Build a new client with the given request timeout.
    ///
    /// Returns an error if the underlying client fails to build.
    pub fn new(timeout: Duration) -> Result<Self, HttpError> {
        let inner = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| HttpError::Build {
                message: e.to_string(),
            })?;
        Ok(Self { inner })
    }

    /// Start a GET request to the given URL.
    pub fn get(&self, url: &str) -> HttpRequestBuilder {
        HttpRequestBuilder {
            inner: self.inner.get(url),
        }
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

/// Builder for a lightweight HTTP request.
///
/// This is the only way to set per-request headers and authentication before
/// sending the request.
pub struct HttpRequestBuilder {
    inner: reqwest::RequestBuilder,
}

impl HttpRequestBuilder {
    /// Set an HTTP header.
    pub fn header(self, name: &str, value: &str) -> Self {
        Self {
            inner: self.inner.header(name, value),
        }
    }

    /// Set HTTP Basic authentication.
    pub fn basic_auth(self, username: &str, password: &str) -> Self {
        Self {
            inner: self.inner.basic_auth(username, Some(password)),
        }
    }

    /// Set HTTP Bearer authentication.
    pub fn bearer_auth(self, token: &str) -> Self {
        Self {
            inner: self.inner.bearer_auth(token),
        }
    }

    /// Send the request and return a lightweight response wrapper.
    pub async fn send(self, operation: &str) -> Result<HttpResponse, HttpError> {
        let response = self.inner.send().await.map_err(|e| HttpError::Transport {
            operation: operation.to_string(),
            message: e.to_string(),
        })?;
        if !response.status().is_success() {
            return Err(HttpError::Status {
                operation: operation.to_string(),
                status: response.status().as_u16(),
            });
        }
        Ok(HttpResponse { inner: response })
    }
}

/// A lightweight HTTP response wrapper.
pub struct HttpResponse {
    inner: reqwest::Response,
}

impl HttpResponse {
    /// HTTP status code.
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }

    /// Return the first header value for `name`, if it is valid UTF-8.
    pub fn header(&self, name: &str) -> Option<String> {
        self.inner
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    /// Parse the response body as JSON.
    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, HttpError> {
        self.inner.json().await.map_err(|e| HttpError::Parse {
            message: e.to_string(),
        })
    }

    /// Read the response body as raw bytes.
    pub async fn bytes(self, operation: &str) -> Result<Bytes, HttpError> {
        self.inner.bytes().await.map_err(|e| HttpError::Body {
            operation: operation.to_string(),
            message: e.to_string(),
        })
    }
}
