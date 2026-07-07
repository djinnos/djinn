//! One-time boot token for the self-setup flow.
//!
//! When `DJINN_ENABLE_SELF_SETUP=true` and no usable GitHub App credentials
//! exist, the server generates a single-use token at boot. The raw token is
//! logged once as a URL; only a SHA-256 digest is kept in memory. The token
//! is consumed (atomically marked used) on first valid exchange.

use crate::server::auth::{constant_time_eq, random_token_b64};
use sha2::{Digest, Sha256};

/// Process-wide boot token state: raw token + SHA-256 digest.
///
/// Stored as `Option<BootToken>` behind a `RwLock` on [`AppState`]. `None`
/// means either no token was generated (setup disabled / credentials present)
/// or the token was already consumed.
pub(crate) struct BootToken {
    #[allow(dead_code)]
    raw: String,
    digest: Vec<u8>,
    used: bool,
}

impl BootToken {
    /// Generate a fresh 32-byte (256-bit) random token, returning both the
    /// raw value (for the boot log URL) and the wrapped struct (for storage).
    pub(crate) fn generate() -> (String, Self) {
        let raw = random_token_b64();
        let digest = Self::hash(&raw);
        let token = Self {
            raw: raw.clone(),
            digest,
            used: false,
        };
        (raw, token)
    }

    fn hash(raw: &str) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        hasher.finalize().to_vec()
    }

    /// Verify an incoming raw token against the stored digest using
    /// constant-time comparison. Does NOT mark the token as used — call
    /// [`mark_used`] separately after all preconditions pass.
    pub(crate) fn verify(&self, candidate: &str) -> bool {
        if self.used {
            return false;
        }
        let candidate_digest = Self::hash(candidate);
        constant_time_eq(&self.digest, &candidate_digest)
    }

    /// Atomically mark the token as consumed. Returns `true` on the first
    /// successful call; returns `false` if the token was already used (which
    /// should cause the caller to reject the request).
    pub(crate) fn mark_used(&mut self) -> bool {
        if self.used {
            return false;
        }
        self.used = true;
        true
    }

    /// Whether this token has already been consumed.
    #[allow(dead_code)]
    pub(crate) fn is_used(&self) -> bool {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_creates_256_bit_token() {
        let (raw, token) = BootToken::generate();
        // random_token_b64 produces 32 bytes → 43 URL-safe base64 chars
        assert_eq!(raw.len(), 43);
        assert!(!raw.contains('='));
        assert!(!token.used);
        assert_eq!(token.digest.len(), 32); // SHA-256
    }

    #[test]
    fn verify_accepts_matching_token() {
        let (raw, token) = BootToken::generate();
        assert!(token.verify(&raw));
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let (_raw, token) = BootToken::generate();
        assert!(!token.verify("not-the-right-token"));
    }

    #[test]
    fn verify_rejects_after_used() {
        let (raw, mut token) = BootToken::generate();
        assert!(token.mark_used());
        assert!(!token.verify(&raw));
    }

    #[test]
    fn mark_used_is_single_use() {
        let (_raw, mut token) = BootToken::generate();
        assert!(token.mark_used());
        assert!(!token.mark_used());
        assert!(token.is_used());
    }
}
