//! List installations and exchange App JWTs for installation access tokens.
//!
//! Installation tokens are 1-hour credentials scoped to one installation.
//! We cache them in-memory keyed by installation id and refresh 5 minutes
//! before expiry.

use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::jwt::mint_app_jwt_anyhow;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "djinn-server/0.1 (+https://github.com/djinnos/server)";
/// Margin before actual expiry when we consider a token stale.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

// ─── Types ────────────────────────────────────────────────────────────────────

/// A GitHub App installation discoverable by the authenticated user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub id: u64,
    pub account_login: String,
    /// Either "User" or "Organization".
    pub account_type: String,
    /// Equivalent to `account_type` for most cases; GitHub calls it
    /// `target_type` on the installation resource.
    pub target_type: String,
}

/// An installation access token with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationToken {
    pub token: String,
    /// RFC3339 timestamp (GitHub format).
    pub expires_at: String,
    /// Map of permission name → `"read" | "write" | "admin"`.
    #[serde(default)]
    pub permissions: HashMap<String, String>,
    /// Optional URL for paginating through the installation's repositories.
    #[serde(default)]
    pub repositories_url: Option<String>,
}

// ─── In-memory token cache ────────────────────────────────────────────────────

#[derive(Clone)]
struct CacheEntry {
    token: InstallationToken,
    fetched_at: Instant,
    ttl: Duration,
}

fn cache() -> &'static Mutex<HashMap<u64, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(id: u64) -> Option<InstallationToken> {
    let guard = cache().lock().ok()?;
    let entry = guard.get(&id)?;
    if entry.fetched_at.elapsed() + REFRESH_MARGIN >= entry.ttl {
        return None;
    }
    Some(entry.token.clone())
}

fn cache_put(id: u64, token: InstallationToken) {
    // Parse expires_at → TTL. Fall back to 1h (GitHub's documented lifetime).
    let ttl = rfc3339_duration_until(&token.expires_at).unwrap_or(Duration::from_secs(55 * 60));
    if let Ok(mut guard) = cache().lock() {
        guard.insert(
            id,
            CacheEntry {
                token,
                fetched_at: Instant::now(),
                ttl,
            },
        );
    }
}

/// Parse an RFC3339 timestamp and return the duration between now and it.
fn rfc3339_duration_until(ts: &str) -> Option<Duration> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    let when = OffsetDateTime::parse(ts, &Rfc3339).ok()?;
    let now = OffsetDateTime::now_utc();
    let d = when - now;
    if d.is_negative() {
        return None;
    }
    Some(Duration::from_secs(d.whole_seconds().max(0) as u64))
}

/// Test-only: prime the installation-token cache with a fake token. Lets
/// tests exercise the installation auth path without hitting GitHub.
#[doc(hidden)]
pub fn prime_cache_for_tests(installation_id: u64, token_value: &str) {
    let token = InstallationToken {
        token: token_value.to_string(),
        expires_at: "2099-01-01T00:00:00Z".into(),
        permissions: Default::default(),
        repositories_url: None,
    };
    cache_put(installation_id, token);
}

/// Clear a cached installation token (e.g. after a 401 response).
pub fn invalidate_cache(installation_id: u64) {
    if let Ok(mut guard) = cache().lock() {
        guard.remove(&installation_id);
    }
}

// ─── API calls ────────────────────────────────────────────────────────────────

/// List all GitHub App installations the user can see via their user token.
///
/// `GET /user/installations` — requires a user-to-server token with access
/// to the App. The response shape is `{ total_count, installations: [...] }`.
pub async fn list_installations_for_user(user_token: &str) -> Result<Vec<Installation>> {
    #[derive(Deserialize)]
    struct ListResponse {
        installations: Vec<RawInstallation>,
    }
    #[derive(Deserialize)]
    struct RawInstallation {
        id: u64,
        account: Option<RawAccount>,
        #[serde(default)]
        target_type: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawAccount {
        login: Option<String>,
        #[serde(rename = "type", default)]
        account_type: Option<String>,
    }

    let client = Client::new();
    let resp = client
        .get(format!("{GITHUB_API}/user/installations"))
        .bearer_auth(user_token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| anyhow!("list installations request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("/user/installations failed ({status}): {body}"));
    }

    let parsed: ListResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to decode /user/installations: {e}"))?;

    Ok(parsed
        .installations
        .into_iter()
        .map(|raw| {
            let (login, acct_type) = match raw.account {
                Some(a) => (
                    a.login.unwrap_or_default(),
                    a.account_type.unwrap_or_else(|| "User".into()),
                ),
                None => (String::new(), "User".into()),
            };
            let target_type = raw.target_type.clone().unwrap_or_else(|| acct_type.clone());
            Installation {
                id: raw.id,
                account_login: login,
                account_type: acct_type,
                target_type,
            }
        })
        .collect())
}

/// List all installations of this App, globally (as the App itself).
///
/// `GET /app/installations` — authenticated with a short-lived App JWT,
/// no user session required. Returns every account/org that has installed
/// this App. Appropriate for single-operator self-hosted Djinn, where the
/// App is the source of truth for reachable targets.
pub async fn list_installations_for_app() -> Result<Vec<Installation>> {
    #[derive(Deserialize)]
    struct RawInstallation {
        id: u64,
        account: Option<RawAccount>,
        #[serde(default)]
        target_type: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawAccount {
        login: Option<String>,
        #[serde(rename = "type", default)]
        account_type: Option<String>,
    }

    let jwt = mint_app_jwt_anyhow()?;
    let client = Client::new();
    let resp = client
        .get(format!("{GITHUB_API}/app/installations"))
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| anyhow!("list app installations request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("/app/installations failed ({status}): {body}"));
    }

    let parsed: Vec<RawInstallation> = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to decode /app/installations: {e}"))?;

    Ok(parsed
        .into_iter()
        .map(|raw| {
            let (login, acct_type) = match raw.account {
                Some(a) => (
                    a.login.unwrap_or_default(),
                    a.account_type.unwrap_or_else(|| "User".into()),
                ),
                None => (String::new(), "User".into()),
            };
            let target_type = raw.target_type.clone().unwrap_or_else(|| acct_type.clone());
            Installation {
                id: raw.id,
                account_login: login,
                account_type: acct_type,
                target_type,
            }
        })
        .collect())
}

/// Fetch a single installation by numeric id, authenticated as the App.
///
/// `GET /app/installations/{installation_id}` — used when the deployment is
/// bound to exactly one installation (via `org_config`) and we want to
/// return just that installation's metadata instead of listing all of them.
pub async fn get_installation_by_id(installation_id: u64) -> Result<Installation> {
    #[derive(Deserialize)]
    struct RawInstallation {
        id: u64,
        account: Option<RawAccount>,
        #[serde(default)]
        target_type: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawAccount {
        login: Option<String>,
        #[serde(rename = "type", default)]
        account_type: Option<String>,
    }

    let jwt = mint_app_jwt_anyhow()?;
    let client = Client::new();
    let resp = client
        .get(format!("{GITHUB_API}/app/installations/{installation_id}"))
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| anyhow!("get installation request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "/app/installations/{installation_id} failed ({status}): {body}"
        ));
    }

    let raw: RawInstallation = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to decode /app/installations/{installation_id}: {e}"))?;

    let (login, acct_type) = match raw.account {
        Some(a) => (
            a.login.unwrap_or_default(),
            a.account_type.unwrap_or_else(|| "User".into()),
        ),
        None => (String::new(), "User".into()),
    };
    let target_type = raw.target_type.clone().unwrap_or_else(|| acct_type.clone());
    Ok(Installation {
        id: raw.id,
        account_login: login,
        account_type: acct_type,
        target_type,
    })
}

/// Exchange an App JWT for an installation access token.
///
/// `POST /app/installations/{installation_id}/access_tokens`.
/// Results are cached in-process keyed by `installation_id` until T-5min
/// before the reported `expires_at`.
pub async fn get_installation_token(installation_id: u64) -> Result<InstallationToken> {
    if let Some(cached) = cache_get(installation_id) {
        tracing::debug!(installation_id, "github_app: reusing cached installation token");
        return Ok(cached);
    }

    let jwt = mint_app_jwt_anyhow()?;
    let client = Client::new();
    let resp = client
        .post(format!(
            "{GITHUB_API}/app/installations/{installation_id}/access_tokens"
        ))
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| anyhow!("installation token request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "installation token exchange failed ({status}): {body}"
        ));
    }

    let token: InstallationToken = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to decode installation token response: {e}"))?;

    cache_put(installation_id, token.clone());
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hit_returns_token_until_margin() {
        let id = 12345u64;
        // Manually insert a cache entry that expires in 10 minutes.
        let token = InstallationToken {
            token: "ghs_fake".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            permissions: Default::default(),
            repositories_url: None,
        };
        cache_put(id, token.clone());
        let got = cache_get(id).expect("cached token");
        assert_eq!(got.token, "ghs_fake");
        invalidate_cache(id);
        assert!(cache_get(id).is_none());
    }

    #[test]
    fn rfc3339_duration_until_rejects_past() {
        assert!(rfc3339_duration_until("2000-01-01T00:00:00Z").is_none());
        assert!(rfc3339_duration_until("2099-01-01T00:00:00Z").is_some());
    }
}
