//! Bearer-token validation via the Kubernetes `TokenReview` API.
//!
//! Stub in PR 1. Real implementation lands in PR 2 when the TCP auth
//! handshake flips on.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TokenReviewError {
    #[error("token review not yet implemented")]
    Unimplemented,
}

/// Validate a bearer token against the Kubernetes `TokenReview` API.
///
/// Returns the ServiceAccount username on success; `TokenReviewError` on
/// rejection. Stub; see module docs.
pub async fn validate(_token: &str, _audience: &str) -> Result<String, TokenReviewError> {
    Err(TokenReviewError::Unimplemented)
}
