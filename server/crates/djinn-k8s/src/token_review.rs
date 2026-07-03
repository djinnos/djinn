//! Typed wrapper around `authentication.k8s.io/v1/TokenReview`.
//!
//! The djinn-server side of the transport uses this to validate the bearer
//! token a worker Pod sends over the wire. The kubelet hands each worker
//! Pod a projected ServiceAccount token with audience `djinn`; the server
//! posts that token at the `TokenReview` subresource, which returns
//! `authenticated: true` plus the token's user identity and audiences.
//!
//! PR 1 only lands the typed shell. PR 2 flips the TCP listener over to
//! calling [`TokenReviewer::review`] on the first frame of every connection.

use std::fmt;

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};
use thiserror::Error;

/// Outcome of a token review call.
#[derive(Debug, Clone)]
pub struct TokenReviewResult {
    /// Whether the cluster authenticated the token.
    pub authenticated: bool,
    /// ServiceAccount username (e.g. `system:serviceaccount:djinn:djinn-taskrun`)
    /// when authenticated; `None` otherwise.
    pub username: Option<String>,
    /// Audiences the cluster confirmed the token carries.
    pub audiences: Vec<String>,
    /// Optional error message surfaced in `status.error` by the apiserver.
    pub error: Option<String>,
}

/// Failures calling the apiserver's `TokenReview` endpoint.
#[derive(Debug, Error)]
pub enum TokenReviewError {
    /// Underlying kube-rs client surfaced a transport or API error.
    #[error("kube client: {0}")]
    Kube(#[from] kube::Error),
}

/// Owner-crate wrapper around a Kubernetes client that performs token review.
///
/// Non-owner crates should use this type instead of directly holding or
/// constructing `kube::Client` for token-review validation.
#[derive(Clone)]
pub struct TokenReviewer {
    client: kube::Client,
    audience: String,
}

impl fmt::Debug for TokenReviewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenReviewer")
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl TokenReviewer {
    /// Build a reviewer from an existing kube client and expected token audience.
    pub fn new(client: kube::Client, audience: impl Into<String>) -> Self {
        Self {
            client,
            audience: audience.into(),
        }
    }

    /// Build a reviewer using the default Kubernetes client (in-cluster config
    /// or kubeconfig) for the current process.
    pub async fn try_default(audience: impl Into<String>) -> Result<Self, TokenReviewError> {
        let client = kube::Client::try_default().await?;
        Ok(Self::new(client, audience))
    }

    /// POST the presented token at the `TokenReview` endpoint and return the
    /// decoded result.
    pub async fn review(&self, token: &str) -> Result<TokenReviewResult, TokenReviewError> {
        review_token(&self.client, token, &self.audience).await
    }
}

/// POST a `TokenReview` for `token` with the expected `audience` and return
/// a decoded [`TokenReviewResult`].
///
/// Intended call site is the TCP listener's auth handshake: the worker
/// sends the token it read from `/var/run/secrets/tokens/djinn` and the
/// server rejects the connection if `authenticated` is false or if the
/// task-run id embedded in the following `AuthHello` frame does not match
/// the user the token belongs to.
///
/// Prefer [`TokenReviewer`] for owner-crate construction; this function is
/// exposed for callers that already own a `kube::Client` and only need the
/// review operation.
pub async fn review_token(
    client: &kube::Client,
    token: &str,
    audience: &str,
) -> Result<TokenReviewResult, TokenReviewError> {
    let api: Api<TokenReview> = Api::all(client.clone());
    let review = TokenReview {
        spec: TokenReviewSpec {
            token: Some(token.to_string()),
            audiences: Some(vec![audience.to_string()]),
        },
        ..TokenReview::default()
    };

    let resp = api.create(&PostParams::default(), &review).await?;
    let status = resp.status.unwrap_or_default();

    Ok(TokenReviewResult {
        authenticated: status.authenticated.unwrap_or(false),
        username: status.user.and_then(|u| u.username),
        audiences: status.audiences.unwrap_or_default(),
        error: status.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // This crate's TokenReview path requires a live cluster (or a kube mock);
    // PR 3 adds the integration test. For PR 1 we only guarantee the types
    // compile.
    #[test]
    fn type_constructs() {
        let _ = TokenReviewResult {
            authenticated: false,
            username: None,
            audiences: Vec::new(),
            error: None,
        };
    }
}
