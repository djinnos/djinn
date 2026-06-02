//! Refresh policy for `AuthMode::UserToken` clients.
//!
//! When the transport sees a `401 Unauthorized` on a user-token request,
//! it consults the [`UserTokenRefresh`] hook to decide whether to (a)
//! attempt a refresh-token rotation and retry once, or (b) surface
//! [`crate::github_api::UserTokenExpired`] so the caller can bounce the
//! user back to `/auth/github/start`.
//!
//! Two implementations ship out of the box:
//!
//! * [`DbBackedRefresher`] — production. Reads the session row that
//!   the cookie points at, calls
//!   [`crate::oauth::github_app_user::refresh_user_token`], writes the
//!   new pair back via
//!   [`djinn_db::repositories::session_auth::SessionAuthRepository::update_github_tokens`].
//!   On refresh failure the row is hard-deleted so the next UI request
//!   misses the cookie lookup and lands on the sign-in screen.
//! * [`NoRefresh`] — test/legacy. Always returns "no refresh available";
//!   the transport treats 401 as terminal.
//!
//! Callers attach one to a client via
//! [`crate::github_api::GitHubApiClient::for_user_session`] (production)
//! or [`crate::github_api::GitHubApiClient::for_user_token`] (legacy).

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use djinn_db::Database;
use djinn_db::repositories::session_auth::{SessionAuthRepository, UpdateGithubTokens};

use crate::oauth::github_app_user::{self, GithubUserTokens};

/// Per-call hook the transport consults on `401` for `UserToken` clients.
///
/// Implementations must be cheap to construct — they're cloned per
/// outbound request via `Arc<dyn UserTokenRefresh>`.
#[async_trait]
pub trait UserTokenRefresh: Send + Sync {
    /// Attempt to mint a fresh access token. On success the transport
    /// retries the original request once with the returned token.
    ///
    /// Returning `Err` is the signal that the user must re-authenticate;
    /// the transport will translate that into
    /// [`crate::github_api::UserTokenExpired`].
    async fn refresh(&self) -> Result<String>;
}

/// Test/legacy refresher that always refuses — used when a caller
/// constructs a client from a raw access-token string without backing
/// session state.
#[derive(Debug, Clone, Default)]
pub struct NoRefresh;

#[async_trait]
impl UserTokenRefresh for NoRefresh {
    async fn refresh(&self) -> Result<String> {
        Err(anyhow!("user token has no refresh credential attached"))
    }
}

/// Database-backed refresher that rotates the session row's access /
/// refresh token pair via GitHub's `/login/oauth/access_token` endpoint.
#[derive(Clone)]
pub struct DbBackedRefresher {
    db: Database,
    /// `user_auth_sessions.token` — the browser session token in the
    /// `djinn_session` cookie, used to locate the row to rotate.
    session_token: String,
    client_id: String,
    client_secret: String,
}

impl DbBackedRefresher {
    pub fn new(
        db: Database,
        session_token: String,
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            db,
            session_token,
            client_id,
            client_secret,
        }
    }

    /// Convenience constructor that boxes into the trait object the
    /// transport expects.
    pub fn into_arc(self) -> Arc<dyn UserTokenRefresh> {
        Arc::new(self)
    }
}

#[async_trait]
impl UserTokenRefresh for DbBackedRefresher {
    async fn refresh(&self) -> Result<String> {
        let repo = SessionAuthRepository::new(self.db.clone());

        let row = repo
            .get_by_token(&self.session_token)
            .await
            .map_err(|e| anyhow!("session lookup failed during refresh: {e}"))?
            .ok_or_else(|| anyhow!("session row vanished before refresh"))?;

        let Some(refresh_token) = row.github_refresh_token.as_deref() else {
            // App is configured without expiring tokens, or the row pre-dates
            // migration 23 — there is nothing to rotate, so the user must
            // sign in again. Hard-evict the row so the next UI request
            // bounces to /auth/github/start.
            let _ = repo.delete_by_token(&self.session_token).await;
            return Err(anyhow!(
                "session has no refresh token on file; user must re-authenticate"
            ));
        };

        match github_app_user::refresh_user_token(
            &self.client_id,
            &self.client_secret,
            refresh_token,
        )
        .await
        {
            Ok(new_tokens) => {
                persist_rotated_tokens(&repo, &self.session_token, &new_tokens).await?;
                Ok(new_tokens.access_token)
            }
            Err(e) => {
                // Refresh credential itself is dead (expired, revoked, or
                // already burned by a parallel rotation). Drop the row so
                // the next request lands at the sign-in screen.
                let _ = repo.delete_by_token(&self.session_token).await;
                Err(anyhow!("github refresh failed: {e}"))
            }
        }
    }
}

async fn persist_rotated_tokens(
    repo: &SessionAuthRepository,
    session_token: &str,
    tokens: &GithubUserTokens,
) -> Result<()> {
    let access_expires_at = tokens.expires_in.map(rfc3339_seconds_from_now);
    let refresh_expires_at = tokens
        .refresh_token_expires_in
        .map(rfc3339_seconds_from_now);

    repo.update_github_tokens(
        session_token,
        UpdateGithubTokens {
            github_access_token: &tokens.access_token,
            github_access_token_expires_at: access_expires_at.as_deref(),
            github_refresh_token: tokens.refresh_token.as_deref(),
            github_refresh_token_expires_at: refresh_expires_at.as_deref(),
        },
    )
    .await
    .map_err(|e| anyhow!("persist rotated tokens: {e}"))?;
    Ok(())
}

fn rfc3339_seconds_from_now(secs: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(secs);
    t.format(&Rfc3339).unwrap_or_default()
}
