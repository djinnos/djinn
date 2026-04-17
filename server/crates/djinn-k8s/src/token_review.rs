//! Typed wrapper around `authentication.k8s.io/v1/TokenReview`.
//!
//! The djinn-server side of the transport uses this to validate the bearer
//! token a worker Pod sends over the wire. The kubelet hands each worker
//! Pod a projected ServiceAccount token with audience `djinn`; the server
//! posts that token at the `TokenReview` subresource, which returns
//! `authenticated: true` plus the token's user identity and audiences.
//!
//! PR 1 only lands the typed shell. PR 2 flips the TCP listener over to
//! calling [`review_token`] on the first frame of every connection.

use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use kube::api::{Api, PostParams};
use thiserror::Error;

/// Outcome of a [`review_token`] call.
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

/// POST a `TokenReview` for `token` with the expected `audience` and return
/// a decoded [`TokenReviewResult`].
///
/// Intended call site is the TCP listener's auth handshake: the worker
/// sends the token it read from `/var/run/secrets/tokens/djinn` and the
/// server rejects the connection if `authenticated` is false or if the
/// task-run id embedded in the following `AuthHello` frame does not match
/// the user the token belongs to.
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
