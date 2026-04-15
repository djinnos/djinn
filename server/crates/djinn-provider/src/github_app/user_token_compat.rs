//! Minimal read-only shim over the legacy credential-DB row that used to
//! back the retired OAuth App device-code flow.
//!
//! The device-code flow used to write a long-lived user token under
//! `__OAUTH_GITHUB_APP`. That flow is gone: the web-app OAuth (`/auth/github/*`)
//! now stores user tokens in `user_auth_sessions` keyed by cookie, and all
//! repo I/O goes through GitHub App installation tokens.
//!
//! Some MCP tools (the legacy repo-picker and `github_app_installations`)
//! still need a GitHub user token so they can call `GET /user/installations`
//! on behalf of the authenticated caller. Until those tools are wired up to
//! the `AuthenticatedUser` session row, they load the legacy credential DB
//! row if one exists. New code should prefer threading the user token through
//! the HTTP session instead of relying on this shim.
//!
//! This module is intentionally tiny: it only exposes read access to an
//! existing row. No device-code polling, no writers, no legacy constants
//! beyond the one key name we need to locate the row.

use serde::Deserialize;

use crate::repos::CredentialRepository;

/// Credential-DB key the retired device-code flow used to store its token
/// bundle under. Kept as a `const` (rather than `pub const`) because only
/// this module has any business touching the row.
pub(crate) const LEGACY_OAUTH_APP_DB_KEY: &str = "__OAUTH_GITHUB_APP";

/// Minimal projection of the historical `GitHubAppTokens` struct — just the
/// fields the surviving callers actually read. Additional fields in the
/// serialised JSON are ignored so rows written by older versions keep
/// deserialising cleanly.
#[derive(Debug, Clone, Deserialize)]
pub struct UserTokens {
    pub access_token: String,
    #[serde(default)]
    pub user_login: Option<String>,
}

/// Load the legacy user token row from the credential DB, if it still
/// exists. Returns `None` on absent / corrupt rows (with a debug log so
/// operators can trace token loss without spamming the production path).
pub async fn load_user_tokens(repo: &CredentialRepository) -> Option<UserTokens> {
    match repo.get_decrypted(LEGACY_OAUTH_APP_DB_KEY).await {
        Ok(Some(json)) => match serde_json::from_str::<UserTokens>(&json) {
            Ok(tokens) => Some(tokens),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "github_app::user_token_compat: legacy OAuth row present but unparseable"
                );
                None
            }
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the JSON shape we accept is stable across the historical set
    /// of extra fields the old flow used to serialise (pure-JSON test, no
    /// DB — the credential repo itself is exercised in
    /// `repos::credential::tests`).
    #[test]
    fn user_tokens_deserialises_with_extra_historical_fields() {
        let json = r#"{"access_token":"ghu_abc","user_login":"octocat","refresh_token":"x","expires_at":1,"refresh_token_expires_at":2}"#;
        let t: UserTokens = serde_json::from_str(json).unwrap();
        assert_eq!(t.access_token, "ghu_abc");
        assert_eq!(t.user_login.as_deref(), Some("octocat"));
    }

    #[test]
    fn user_tokens_optional_user_login() {
        let json = r#"{"access_token":"ghu_abc"}"#;
        let t: UserTokens = serde_json::from_str(json).unwrap();
        assert_eq!(t.access_token, "ghu_abc");
        assert!(t.user_login.is_none());
    }

    #[test]
    fn legacy_db_key_is_stable() {
        // The key name is observable in migrated credential rows; changing
        // it silently would hide legacy tokens from the compat loader.
        assert_eq!(LEGACY_OAUTH_APP_DB_KEY, "__OAUTH_GITHUB_APP");
    }
}
