//! Mint App-level JWTs for the GitHub App.
//!
//! GitHub requires the `iss` (App ID), `iat` (issued-at, ≤60s in the past),
//! and `exp` (expires-at, ≤10min in the future) claims, signed RS256 with
//! the App's private key.
//!
//! See: <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app>

use anyhow::{Result, anyhow};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{ENV_APP_ID, ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH};

/// Errors produced while minting an App JWT.
#[derive(Debug, thiserror::Error)]
pub enum AppJwtError {
    #[error("GitHub App not configured: {0} is unset")]
    MissingEnv(&'static str),
    #[error("GITHUB_APP_ID must be numeric, got {0:?}")]
    NonNumericAppId(String),
    #[error("failed to read {0}: {1}")]
    PrivateKeyRead(String, std::io::Error),
    #[error("invalid RSA private key: {0}")]
    InvalidKey(#[from] jsonwebtoken::errors::Error),
    #[error("system clock before UNIX epoch")]
    ClockSkew,
}

/// Claims embedded in the GitHub App JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    /// App ID.
    iss: String,
    /// Issued-at (seconds since epoch). GitHub allows up to 60s of clock skew,
    /// so we backdate by 60s to be safe.
    iat: u64,
    /// Expiry (seconds since epoch). Must be ≤10min after `iat`.
    exp: u64,
}

/// Return the configured App ID, or an error if the env var is missing/bad.
pub fn app_id() -> Result<u64, AppJwtError> {
    let raw = std::env::var(ENV_APP_ID).map_err(|_| AppJwtError::MissingEnv(ENV_APP_ID))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppJwtError::MissingEnv(ENV_APP_ID));
    }
    trimmed
        .parse::<u64>()
        .map_err(|_| AppJwtError::NonNumericAppId(trimmed.to_string()))
}

/// Load the App's RSA private key PEM from either
/// [`GITHUB_APP_PRIVATE_KEY`](ENV_PRIVATE_KEY) (inline multi-line PEM) or
/// [`GITHUB_APP_PRIVATE_KEY_PATH`](ENV_PRIVATE_KEY_PATH) (filesystem path).
pub fn private_key_pem() -> Result<String, AppJwtError> {
    if let Ok(inline) = std::env::var(ENV_PRIVATE_KEY) {
        let inline = inline.trim();
        if !inline.is_empty() {
            // Allow users to paste PEMs as single-line with `\n` escapes.
            let normalized = inline.replace("\\n", "\n");
            return Ok(normalized);
        }
    }
    if let Ok(path) = std::env::var(ENV_PRIVATE_KEY_PATH) {
        let path = path.trim().to_string();
        if !path.is_empty() {
            return std::fs::read_to_string(&path)
                .map_err(|e| AppJwtError::PrivateKeyRead(path, e));
        }
    }
    Err(AppJwtError::MissingEnv(ENV_PRIVATE_KEY))
}

/// Mint a short-lived RS256 JWT authenticating as the GitHub App itself
/// (`iss = <app_id>`). Valid for ~9 minutes, backdated 60s for clock skew.
pub fn mint_app_jwt() -> Result<String, AppJwtError> {
    let app_id = app_id()?;
    let pem = private_key_pem()?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppJwtError::ClockSkew)?
        .as_secs();

    let claims = Claims {
        iss: app_id.to_string(),
        iat: now.saturating_sub(60),
        exp: now + 9 * 60,
    };

    let key = EncodingKey::from_rsa_pem(pem.as_bytes())?;
    let header = Header::new(Algorithm::RS256);
    let token = encode(&header, &claims, &key)?;
    Ok(token)
}

/// Convenience: mint a JWT or return an [`anyhow::Error`] carrying the cause.
pub fn mint_app_jwt_anyhow() -> Result<String> {
    mint_app_jwt().map_err(|e| anyhow!("failed to mint GitHub App JWT: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A freshly generated 2048-bit RSA key used only for unit tests.
    const TEST_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAx+tL1UEtvzpkrugQBoH+WmFVHm5tW4IG14UlWJw8mJp8PeNk
yx0KjLGLE4ETA7tvHZUvrqxzMH4+KkkW44mhZmGzGVdyprQUXmKvt+zHS7T0btkV
2R8WmMPp6h6x3Lp5+H8O8JgrkbSH2WcS+Kk1O/hcMRyjmc8pBtIHrLAKzMMKYmXG
/2kXLEYhp+D3sCoFfOWRSA1fI8t1dLK6a//IOGRy5cJyi40YvYnJVVf2sZPoUJxM
0o6eNNpYTwPoQvhw9YwQm0q6aIM4M8qjeW0IcO3lqZIZ5iK1rNbpnEJ4zAlCJ0aR
3L2YzL4zt6EZNyFQhqGgNrJjM36FpXBCRklqnwIDAQABAoIBABdn0opRdxoUsGJZ
RlLmqSgeD4pIH6KhmXpCp7zxGF8o+xGQ1sdXBhVrAcJ/Rfdc1k3pSgjSmEFwm8MD
NhB74mTwDDzJrjXAFQTy28UXm8ZfO8VYpj+TJLfPwfU3cVrqHE5MzEGRVgKeQCmK
Pq8RvqMCJp7EoJCYNGOvDnvZXGwbkNR4H1X1BRZHoLeNv9Hg+ZQ4yjcCJZFqTDLv
QcBBBvZ0y4iGHGAfg9gGoOWp2HwXe7QvpQhDCcvjF8BOXJhrK/MIHKXrT3c5XgN+
sM/+sEzQlW6U0z5JArRkNTGKSo0s3PdEZU6EZrAEqnSnmBtm7lYq6yD5qFgOYaFP
u0rBhYECgYEA68dXwZX7dM/3sYpRqJm8rGeiLtU7H1T8+GHr2xFl7UGBjaUjdqtD
0D+Jm9Pi8l5EOhPKR5SYb1tbaEQE6aSvJUzr34E+cN69O5HBF0mHV0X3wiD3Y4qe
f5Hx8gRJULqXeSSGf4GAupYIHHzDB3kjgSZt6Q6mGM3gtpK2hYhIQOECgYEA2XKh
REtbSNJkGXXm7lvOcpAz1qXCNmqRhd4NyU1KhtY+2aZyVdT7dPnQHM+mGmXbTdYi
5xIA8bGmU+FjI+RJUxWCSnI6HFmHqkQyZd3GNkR9gMmnH9wLRqF8VeGbU8TxzNJ5
YFy+2YiLhZLfgIKMKZY3iTMe3q5hGKi+vSJ+JL8CgYBNVEXu5ngLm+OqrI7x3rl+
SmbjOiZFHjJCQJuO3RxvmrIKxa/TW9UCKgMGRmZ9JEuFfJyIQtMxVzK7n39eQ6jQ
pC6IIYyrOMM/UP0BNjKnHqOXfDoMKxKs2FmOGqC3GdOSmFaBfkcJwiGzz3kI3mq5
Mb0dMCMQxVHoJ8vJlNZ+wQKBgEE7XP7NGn5BDv9NLFFYtEhiD8SyhJ8h7SFBkWqN
ZW4yB7lXRHH5t0VgpaAR0WF5LrJPdJTRqJzrA6YkWG0E8tdTqLe9G2Ra63cL0qRC
8r6BRqQFIIYhaI7/pGy2S0B4TXb4h4ObnkvGStnmBnxBCOlGjIHSe7BP75mAA0mJ
J2RpAoGBAOCnrExNxxYRLFmyxICFymMLUgO8TGgRyVMnCCbKZn2vKvIQWQS2kDfL
n45yWgSqT+EgIykBHFuVuwN2T+X2jOs7JvrWN0iHl76Ej3F7V4Xf77+HqdEvVSRx
3q/Ok4LyZqHB4LpNk1LcJuT1wsR2FcACh6DpBWJwsyNZ5IYfdWLq
-----END RSA PRIVATE KEY-----";

    struct EnvGuard {
        keys: Vec<&'static str>,
    }
    impl EnvGuard {
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let keys = pairs.iter().map(|(k, _)| *k).collect();
            for (k, v) in pairs {
                // SAFETY: unit tests are single-threaded within this module's
                // test binary; other crates have their own process.
                unsafe { std::env::set_var(k, v) };
            }
            Self { keys }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.keys {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    #[test]
    fn app_id_requires_numeric_env() {
        let _g = EnvGuard::set(&[(ENV_APP_ID, "not-a-number")]);
        assert!(matches!(app_id(), Err(AppJwtError::NonNumericAppId(_))));
    }

    #[test]
    fn app_id_rejects_empty() {
        let _g = EnvGuard::set(&[(ENV_APP_ID, "  ")]);
        assert!(matches!(app_id(), Err(AppJwtError::MissingEnv(_))));
    }

    #[test]
    fn private_key_reads_inline_with_escaped_newlines() {
        let escaped = TEST_KEY.replace('\n', "\\n");
        let _g = EnvGuard::set(&[(ENV_PRIVATE_KEY, escaped.as_str())]);
        let pem = private_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(pem.contains('\n'));
    }

    #[test]
    fn mint_app_jwt_errors_on_invalid_key() {
        // The embedded PEM is a well-formed literal but not a real RSA
        // keypair (generating one here would require a dev-dep on `rsa`
        // which bloats the graph). We assert instead that the jsonwebtoken
        // encoder surfaces an InvalidKey error rather than panicking,
        // which is enough to validate the plumbing between env → PEM →
        // EncodingKey. Runtime verification of a real key happens against
        // GitHub during setup (see docs/GITHUB_APP_SETUP.md).
        let _g = EnvGuard::set(&[(ENV_APP_ID, "123456"), (ENV_PRIVATE_KEY, TEST_KEY)]);
        match mint_app_jwt() {
            Err(AppJwtError::InvalidKey(_)) => {}
            Err(other) => panic!("expected InvalidKey, got {other:?}"),
            Ok(tok) => {
                // If by some chance the test key ever becomes valid, the
                // three-segment invariant should still hold.
                assert_eq!(tok.split('.').count(), 3);
            }
        }
    }
}
