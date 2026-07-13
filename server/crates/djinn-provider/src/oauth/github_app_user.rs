//! GitHub App user-to-server OAuth helpers.
//!
//! Wraps the two `POST https://github.com/login/oauth/access_token` calls
//! the server makes against GitHub during a user's lifetime:
//!
//! * [`exchange_code`] — initial code → tokens swap at the end of the
//!   OAuth dance kicked off by `/auth/github/start`.
//! * [`refresh_user_token`] — burns a refresh token for a fresh access
//!   token after the previous one expired (8h by default when the App
//!   has "Expire user authorization tokens" enabled).
//!
//! Both calls return the same payload shape ([`GithubUserTokens`]), so
//! callers can share a single persistence helper. `expires_in` and the
//! refresh-token fields are `Option` because GitHub Apps configured with
//! non-expiring user tokens omit them.
//!
//! These helpers live in `djinn-provider` rather than the web crate so
//! that background coordinator paths (the `pr_poller` 401-refresh path)
//! can reach them without taking a dependency on the HTTP layer.

use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// GitHub's user-to-server OAuth token endpoint. Used for both the
/// initial code exchange and refresh-token rotations.
const GITHUB_TOKEN_ENDPOINT: &str = "https://github.com/login/oauth/access_token";

/// Resolve the GitHub App OAuth client_id + client_secret from the provider's
/// runtime credential snapshot, with environment fallback before server
/// initialization.
///
/// Centralised here so that background paths (`pr_poller`'s refresh
/// hook) and the web layer (`/auth/github/callback`) read the same
/// pair of env vars without duplicating the trim+empty filtering.
pub fn client_credentials() -> Option<(String, String)> {
    crate::github_app::oauth_client_credentials()
}

/// Tokens minted by GitHub for a user-to-server OAuth flow.
///
/// `expires_in` and refresh fields are `Option` because GitHub Apps
/// configured with non-expiring user tokens omit them.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubUserTokens {
    pub access_token: String,
    /// Lifetime of `access_token` in seconds. Absent when the App is
    /// configured with non-expiring user tokens.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Paired refresh credential. Absent when the App is configured
    /// with non-expiring user tokens (in that case there is nothing to
    /// rotate and the access token must be re-minted via `/auth/github/start`
    /// if it ever gets revoked).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Lifetime of `refresh_token` in seconds (typically 6 months).
    #[serde(default)]
    pub refresh_token_expires_in: Option<i64>,
}

/// Exchange an OAuth `code` (delivered to `/auth/github/callback`) for
/// the initial token bundle.
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<GithubUserTokens> {
    #[derive(Serialize)]
    struct Req<'a> {
        client_id: &'a str,
        client_secret: &'a str,
        code: &'a str,
        redirect_uri: &'a str,
    }
    post_token_endpoint(&Req {
        client_id,
        client_secret,
        code,
        redirect_uri,
    })
    .await
}

/// Rotate a refresh token for a fresh access token + refresh token pair.
///
/// On `400 Bad Request` (refresh token expired, revoked, or already
/// consumed) the call surfaces an error rather than silently retrying —
/// callers translate that into "session is dead, force re-login".
pub async fn refresh_user_token(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<GithubUserTokens> {
    #[derive(Serialize)]
    struct Req<'a> {
        client_id: &'a str,
        client_secret: &'a str,
        grant_type: &'static str,
        refresh_token: &'a str,
    }
    post_token_endpoint(&Req {
        client_id,
        client_secret,
        grant_type: "refresh_token",
        refresh_token,
    })
    .await
}

async fn post_token_endpoint<B: Serialize>(body: &B) -> Result<GithubUserTokens> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(flatten)]
        tokens: Option<GithubUserTokens>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
    }

    let resp: Resp = Client::new()
        .post(GITHUB_TOKEN_ENDPOINT)
        .header("Accept", "application/json")
        .header("User-Agent", "djinn-server")
        .json(body)
        .send()
        .await
        .map_err(|e| anyhow!("token endpoint request failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("token endpoint response decode failed: {e}"))?;

    if let Some(err) = resp.error {
        return Err(anyhow!(
            "github oauth error: {err}: {}",
            resp.error_description.unwrap_or_default()
        ));
    }
    resp.tokens
        .ok_or_else(|| anyhow!("token endpoint response missing access_token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_expiring_app_response() {
        let payload = r#"{
            "access_token": "ghu_AAA",
            "expires_in": 28800,
            "refresh_token": "ghr_BBB",
            "refresh_token_expires_in": 15897600,
            "token_type": "bearer",
            "scope": ""
        }"#;
        let parsed: GithubUserTokens = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.access_token, "ghu_AAA");
        assert_eq!(parsed.expires_in, Some(28800));
        assert_eq!(parsed.refresh_token.as_deref(), Some("ghr_BBB"));
        assert_eq!(parsed.refresh_token_expires_in, Some(15897600));
    }

    #[test]
    fn deserialises_non_expiring_app_response() {
        // Apps with "Expire user authorization tokens" disabled emit just
        // `access_token` (and possibly `token_type`/`scope`).
        let payload = r#"{
            "access_token": "ghu_CCC",
            "token_type": "bearer",
            "scope": ""
        }"#;
        let parsed: GithubUserTokens = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.access_token, "ghu_CCC");
        assert!(parsed.expires_in.is_none());
        assert!(parsed.refresh_token.is_none());
    }
}
