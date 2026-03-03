pub mod middleware;

use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

const CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("token expired")]
    TokenExpired,
    #[error("invalid signature")]
    SignatureInvalid,
    #[error("invalid token")]
    InvalidToken,
    #[error("signing key not found")]
    KeyNotFound,
    #[error("failed to fetch JWKS: {0}")]
    JwksFetch(#[from] reqwest::Error),
}

/// JWT claims extracted from a Clerk token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Clerk user ID.
    pub sub: String,
    pub exp: u64,
    pub iat: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
}

struct JwksState {
    key_set: JwkSet,
    fetched_at: Instant,
}

/// Caches Clerk JWKS keys with a 1-hour TTL. Invalidates on signature failure.
#[derive(Clone)]
pub struct JwksCache {
    state: Arc<RwLock<Option<JwksState>>>,
    http: Client,
    jwks_url: String,
}

impl JwksCache {
    pub fn new(jwks_url: impl Into<String>) -> Self {
        Self {
            state: Arc::new(RwLock::new(None)),
            http: Client::new(),
            jwks_url: jwks_url.into(),
        }
    }

    async fn fetch(&self) -> Result<JwkSet, AuthError> {
        let resp = self
            .http
            .get(&self.jwks_url)
            .send()
            .await?
            .json::<JwkSet>()
            .await?;
        Ok(resp)
    }

    async fn get_or_fetch(&self) -> Result<JwkSet, AuthError> {
        // Fast path: cache hit under read lock.
        {
            let guard = self.state.read().await;
            if let Some(s) = guard.as_ref() {
                if s.fetched_at.elapsed() < CACHE_TTL {
                    return Ok(s.key_set.clone());
                }
            }
        }
        // Slow path: write lock, double-check, then fetch.
        let mut guard = self.state.write().await;
        if let Some(s) = guard.as_ref() {
            if s.fetched_at.elapsed() < CACHE_TTL {
                return Ok(s.key_set.clone());
            }
        }
        tracing::debug!(url = %self.jwks_url, "fetching JWKS");
        let key_set = self.fetch().await?;
        *guard = Some(JwksState {
            key_set: key_set.clone(),
            fetched_at: Instant::now(),
        });
        Ok(key_set)
    }

    fn verify(token: &str, key_set: &JwkSet) -> Result<Claims, AuthError> {
        let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let kid = header.kid.ok_or(AuthError::InvalidToken)?;
        let jwk = key_set.find(&kid).ok_or(AuthError::KeyNotFound)?;
        let decoding_key = DecodingKey::from_jwk(jwk).map_err(|_| AuthError::InvalidToken)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_aud = false; // Clerk JWTs omit aud for server-side validation.
        validation.set_required_spec_claims(&["sub", "exp"]);

        decode::<Claims>(token, &decoding_key, &validation)
            .map(|d| d.claims)
            .map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::InvalidSignature => AuthError::SignatureInvalid,
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::TokenExpired,
                _ => AuthError::InvalidToken,
            })
    }

    /// Validate a JWT. On signature failure, invalidates the cache and retries once.
    pub async fn validate(&self, token: &str) -> Result<Claims, AuthError> {
        let key_set = self.get_or_fetch().await?;
        match Self::verify(token, &key_set) {
            Ok(claims) => Ok(claims),
            Err(AuthError::SignatureInvalid) | Err(AuthError::KeyNotFound) => {
                tracing::debug!("JWKS signature mismatch — invalidating cache and retrying");
                {
                    let mut guard = self.state.write().await;
                    *guard = None;
                }
                let refreshed = self.get_or_fetch().await?;
                Self::verify(token, &refreshed)
            }
            Err(e) => Err(e),
        }
    }
}
