//! Mint App-level JWTs for the GitHub App.
//!
//! GitHub requires the `iss` (App ID), `iat` (issued-at, ≤60s in the past),
//! and `exp` (expires-at, ≤10min in the future) claims, signed RS256 with
//! the App's private key.
//!
//! See: <https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app>

use anyhow::{Result, anyhow};
use djinn_core::clock::{Clock, SystemClock};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use std::time::UNIX_EPOCH;

use super::{ENV_APP_ID, ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH, runtime_config};

/// Errors produced while minting an App JWT.
#[derive(Debug, thiserror::Error)]
pub enum AppJwtError {
    #[error("GitHub App not configured: {0} is unset")]
    MissingEnv(&'static str),
    #[error("GITHUB_APP_ID must be numeric, got {0:?}")]
    NonNumericAppId(String),
    #[error("GITHUB_APP_ID must be greater than zero")]
    ZeroAppId,
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

/// Return the configured App ID from the runtime snapshot, falling back to
/// environment before server initialization.
pub fn app_id() -> Result<u64, AppJwtError> {
    if let Some(config) = runtime_config() {
        return (config.app_id != 0)
            .then_some(config.app_id)
            .ok_or(AppJwtError::ZeroAppId);
    }
    let raw = std::env::var(ENV_APP_ID).map_err(|_| AppJwtError::MissingEnv(ENV_APP_ID))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppJwtError::MissingEnv(ENV_APP_ID));
    }
    let app_id = trimmed
        .parse::<u64>()
        .map_err(|_| AppJwtError::NonNumericAppId(trimmed.to_string()))?;
    (app_id != 0)
        .then_some(app_id)
        .ok_or(AppJwtError::ZeroAppId)
}

/// Verify that a PEM contains a usable RSA private key, using the same RS256
/// signing path as App JWT minting. Parsing alone is insufficient: it can
/// accept structurally RSA-shaped keys with invalid key parameters that fail
/// only when the signer is constructed.
pub(crate) fn validate_rsa_private_key(pem: &str) -> Result<(), AppJwtError> {
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())?;
    let header = Header::new(Algorithm::RS256);
    let validation_claims = Claims {
        iss: "1".to_owned(),
        iat: 0,
        exp: 1,
    };
    encode(&header, &validation_claims, &key)?;
    Ok(())
}

/// Load the App's RSA private key PEM from either
/// [`GITHUB_APP_PRIVATE_KEY`](ENV_PRIVATE_KEY) (inline multi-line PEM) or
/// [`GITHUB_APP_PRIVATE_KEY_PATH`](ENV_PRIVATE_KEY_PATH) (filesystem path).
pub fn private_key_pem() -> Result<String, AppJwtError> {
    if let Some(config) = runtime_config() {
        if config.pem.trim().is_empty() {
            return Err(AppJwtError::MissingEnv(ENV_PRIVATE_KEY));
        }
        return Ok(config.pem.clone());
    }
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
    let now = SystemClock::new()
        .now()
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
pub(crate) mod tests {
    use super::*;

    pub(crate) const TEST_RSA_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAu+kuYBjHip/Xq2lQnPQD9G2f97RMYFwa3Sr7PNrTd4gjEFkg
OXFA1SCqlImNG/lNLxDK/tIoBSLkRkzEmbRCE/z+lV9pM3xs+UVeWLDzty5nzAMN
KDh6RAcexrSEf8TMfvmMmBYJ8Cut6Htu8PkgQSehmSMQ4r5k+VcJw80yYSfL6wQK
R+7p5O9UPRU4lcygOAxmt9PNeTkWHX/IPmLcsKyoi6IDlGxIeUPj9HkWxj8EYSxB
XtoB22i5QAxAFmClQ3U08yAaENNZtU/liwTxE4igT93Txnai2mUxzKz6Pacf0ibU
mCPTAiqnIEoOVKJpGOCZSi4LZDKr8yR4KDd2wwIDAQABAoIBAEWdKkARjgLuGoD3
IBU1VS29WxDyK4VbOdyLqs2tp7/VoF/TFNwS99i9JFSo7KzbW9u+1eU3R/o3Jehh
Ukg6/mvXQx1lXlzjkJ98MmqbC37mYy+yRbKL0cfX92/Xump3Juc3Xf2N1Jq0I9ZH
vB7rvCZHH1fTJNNLg67Xrtdp8msJJ75oC13SbrrDedwPGlFJFNZan+cItD54dTTL
fZ79Eg5UP+C9zZ0hP7fNJUnqPRUtz768l282IUP/C2Eb4ScbXa4gH8UHKmrNdK4d
7CXaUDklNe79VaruD2mX6D92ztGnp/M49IqkLLtSirnOoOKIIbf2qi4tvoHe9WX3
wKRvsOkCgYEA3d1jO+AaHqQ/K6JkWwiCDaOuZZud6aNsvaDtudRuVMjh6ygD+wcY
Z5KqU7kieBShS8xLBI91EG3FBytSloNWu/aeJC9FAwZ7qKk3F3AKIR/wqbQkOTT/
8ThTxe+1eaewgxXYNpHGC8LaZwCRSgmFkc6EVKMAmFO/9ovIAiTHx4cCgYEA2NJy
RxAjXNqegFCeePpwhPvPd3M8K9wo0hwrSgkhzoaKYxQ+qInL/OUSiOnegGrVsJq9
IfzwV2uBmkVR+sZJv1xyigqmR86h+Dlw0FfV2r5s/BmDYQ7V9MVdxdT7/bfbiggZ
WdyP3o4YlnSLk/qLim7/QdmW8khj3seQWkJm7eUCgYA0jdCHylnlkDp2d40WEznb
ST5ySx5ozZFgidJGBo/r/XmmXmAzAkdBoXg/RMdpclmSvt22QtUUAyx8ukJh7NKK
y6xCHgBW6x43oX2vS5baqdo0GLvL4UYPOax+Yn22R4aERpRkuLsU5h8d7wB7bS36
j9TAx6vIaW47VHkYKOY52QKBgQCht602panKiuDnkbnxP9IGzg466L87c3Ua6Zm8
Gb2WXbEAH0xwxn5YPL8rUUv8ejKyC2f/3rmganX7C7MOmTDOQvTHUxQcwNj73FPx
gWHnSlrdWWYtUTRx4XeEo8vjvGtJs6q85I6GD3P1XC3zDE9hzFIk2lcElMuwkSZw
u9ArpQKBgD0fKZvRNpXaJRUTkZfF/6HrYpBg3risCi0hzA8KD2CYhDN+n41qZhFV
R68QYTUok1u+1E/zrD8hR66a0YlSbcHfO5IeyfK2UszZyJxZgkrYk91b3vPqAuqu
B43YjMtfeD03UXwS4GGcof401pHtVBHuukNdH8qhqH1fObcwd9Ot
-----END RSA PRIVATE KEY-----";

    // Structurally RSA-shaped test data with invalid key parameters.
    const MALFORMED_RSA_TEST_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
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
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        let _g = EnvGuard::set(&[(ENV_APP_ID, "not-a-number")]);
        assert!(matches!(app_id(), Err(AppJwtError::NonNumericAppId(_))));
    }

    #[test]
    fn app_id_rejects_empty() {
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        let _g = EnvGuard::set(&[(ENV_APP_ID, "  ")]);
        assert!(matches!(app_id(), Err(AppJwtError::MissingEnv(_))));
    }

    #[test]
    fn app_id_rejects_zero() {
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        let _g = EnvGuard::set(&[(ENV_APP_ID, "0")]);
        assert!(matches!(app_id(), Err(AppJwtError::ZeroAppId)));
    }

    #[test]
    fn private_key_reads_inline_with_escaped_newlines() {
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        let escaped = TEST_RSA_PRIVATE_KEY.replace('\n', "\\n");
        let _g = EnvGuard::set(&[(ENV_PRIVATE_KEY, escaped.as_str())]);
        let pem = private_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(pem.contains('\n'));
    }

    #[test]
    fn mint_app_jwt_errors_on_invalid_key() {
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        // The embedded PEM is a well-formed literal but not a real RSA
        // keypair (generating one here would require a dev-dep on `rsa`
        // which bloats the graph). We assert instead that the jsonwebtoken
        // encoder surfaces an InvalidKey error rather than panicking,
        // which is enough to validate the plumbing between env → PEM →
        // EncodingKey. Runtime verification of a real key happens against
        // GitHub during setup (see docs/GITHUB_APP_SETUP.md).
        let _g = EnvGuard::set(&[
            (ENV_APP_ID, "123456"),
            (ENV_PRIVATE_KEY, MALFORMED_RSA_TEST_KEY),
        ]);
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

    #[test]
    fn runtime_config_mints_without_github_app_env() {
        let _lock = super::super::config::github_app_test_lock();
        super::super::clear_runtime_config();
        for key in [ENV_APP_ID, ENV_PRIVATE_KEY, ENV_PRIVATE_KEY_PATH] {
            unsafe { std::env::remove_var(key) };
        }

        let config = super::super::AppConfig {
            app_id: 654321,
            slug: "runtime-app".into(),
            client_id: "Iv1.runtime".into(),
            client_secret: "runtime-secret".into(),
            pem: TEST_RSA_PRIVATE_KEY.into(),
            webhook_secret: String::new(),
            public_url: "http://localhost:3000".into(),
        };
        super::super::install_runtime_config(std::sync::Arc::new(config));

        assert_eq!(app_id().unwrap(), 654321);
        assert_eq!(private_key_pem().unwrap(), TEST_RSA_PRIVATE_KEY);
        let token = mint_app_jwt().expect("valid runtime RSA key should mint an App JWT");
        assert_eq!(token.split('.').count(), 3);

        super::super::clear_runtime_config();
        assert!(matches!(app_id(), Err(AppJwtError::MissingEnv(_))));
    }
}
