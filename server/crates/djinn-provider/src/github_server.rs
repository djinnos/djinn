//! Server-facing GitHub API helpers.
//!
//! These helpers wrap the small set of outbound GitHub REST calls made by the
//! server binary (`server/src/server/auth.rs`, `github_install.rs`, and
//! `org_sync.rs`). They keep `reqwest::Client` construction inside
//! `djinn-provider` so that server modules do not directly import outbound
//! HTTP client types.
//!
//! Operations are intentionally narrow: user identity, org membership,
//! App installation listing, and paginated org member enumeration. They return
//! `String` errors to match the existing server error style without forcing a
//! new error type on callers.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::Deserialize;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "djinn-server/0.1 (+https://github.com/djinnos/server)";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub user identity returned by `GET /user`.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubUser {
    pub id: u64,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// One GitHub App installation as seen by `GET /app/installations`.
#[derive(Debug, Clone, Deserialize)]
pub struct AppInstallation {
    pub id: u64,
    #[serde(default)]
    pub account: Option<InstallationAccount>,
    #[serde(default)]
    pub repository_selection: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

impl AppInstallation {
    /// The repository selection policy, defaulting to `"all"` when GitHub
    /// omits the field.
    pub fn repository_selection(&self) -> &str {
        self.repository_selection.as_deref().unwrap_or("all")
    }

    /// The installation settings page URL, defaulting to an empty string.
    pub fn html_url(&self) -> &str {
        self.html_url.as_deref().unwrap_or("")
    }

    /// Account metadata, falling back to a default empty record when absent.
    pub fn account(&self) -> InstallationAccount {
        self.account.clone().unwrap_or_default()
    }
}

/// Account record attached to a GitHub App installation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InstallationAccount {
    pub id: u64,
    #[serde(default)]
    pub login: String,
    #[serde(rename = "type", default)]
    pub account_type: String,
}

/// One entry from `GET /orgs/{org}/members`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GithubOrgMember {
    pub id: i64,
    pub login: String,
}

/// Small stateless wrapper for the server GitHub API calls.
///
/// The client is pre-configured with a user-agent and timeout. All request
/// construction stays inside this module; callers supply tokens and read
/// results.
#[derive(Debug, Clone, Default)]
pub struct GitHubServerClient;

impl GitHubServerClient {
    /// Create a new client.
    pub fn new() -> Self {
        Self
    }

    /// `GET /user` authenticated with a user-to-server token.
    pub async fn fetch_user(&self, token: &str) -> Result<GithubUser, String> {
        let client = new_client();
        let resp = client
            .get(format!("{GITHUB_API}/user"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GitHub /user returned {status}: {body}"));
        }
        resp.json::<GithubUser>().await.map_err(|e| e.to_string())
    }

    /// Check whether `token` belongs to an active member of `org_login`.
    ///
    /// Returns `Ok(false)` for 404 or 403 (non-member / revoked), and `Ok(true)`
    /// only when GitHub reports `state == "active"`. Any other non-success
    /// status is surfaced as an error.
    pub async fn check_org_membership(&self, token: &str, org_login: &str) -> Result<bool, String> {
        #[derive(Deserialize)]
        struct Membership {
            #[serde(default)]
            state: Option<String>,
        }

        let url = format!(
            "https://api.github.com/user/memberships/orgs/{}",
            urlencode_path_segment(org_login),
        );
        let client = new_client();
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
            return Ok(false);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "GitHub /user/memberships/orgs/{org_login} returned {status}: {body}"
            ));
        }
        let parsed: Membership = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parsed.state.as_deref() == Some("active"))
    }

    /// `GET /app/installations` authenticated with an App JWT.
    pub async fn fetch_app_installations(&self, jwt: &str) -> Result<Vec<AppInstallation>, String> {
        let client = new_client();
        let resp = client
            .get(format!("{GITHUB_API}/app/installations"))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("{status}: {body}"));
        }
        resp.json::<Vec<AppInstallation>>()
            .await
            .map_err(|e| e.to_string())
    }

    /// `GET /app/installations/{id}` authenticated with an App JWT.
    pub async fn fetch_app_installation(
        &self,
        jwt: &str,
        installation_id: u64,
    ) -> Result<AppInstallation, String> {
        let url = format!("{GITHUB_API}/app/installations/{installation_id}");
        let client = new_client();
        let resp = client
            .get(&url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("{status}: {body}"));
        }
        resp.json::<AppInstallation>()
            .await
            .map_err(|e| format!("decode /app/installations/{installation_id}: {e}"))
    }

    /// Fetch every page of `GET /orgs/{org}/members` using the installation
    /// bearer token and the GitHub `Link` header for pagination.
    ///
    /// Returns a 403-specific error message that points operators at the App's
    /// organization permissions.
    pub async fn fetch_org_members(
        &self,
        installation_token: &str,
        org_login: &str,
    ) -> Result<Vec<GithubOrgMember>, String> {
        let client = new_client();
        let mut next = Some(format!(
            "{GITHUB_API}/orgs/{}/members?per_page=100",
            urlencode_path_segment(org_login),
        ));
        let mut out: Vec<GithubOrgMember> = Vec::new();

        // Cap pages so a runaway pagination loop can't hang the background task.
        let mut pages_seen = 0usize;
        const MAX_PAGES: usize = 100; // 100 * 100/page = 10 000 members.

        while let Some(url) = next.take() {
            pages_seen += 1;
            if pages_seen > MAX_PAGES {
                return Err(format!(
                    "org_sync: aborting after {MAX_PAGES} pages — pagination loop?"
                ));
            }

            let resp = client
                .get(&url)
                .bearer_auth(installation_token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await
                .map_err(|e| format!("http: {e}"))?;

            let status = resp.status();
            if status == StatusCode::FORBIDDEN {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "403 Forbidden from /orgs/{org_login}/members — the GitHub App \
                     likely lacks the 'Members: Read' organization permission. \
                     Update it at https://github.com/settings/apps/<slug>/permissions \
                     and re-install. Body: {body}"
                ));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("{status}: {body}"));
            }

            let link_header = resp
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string);

            let page: Vec<GithubOrgMember> =
                resp.json().await.map_err(|e| format!("decode page: {e}"))?;
            out.extend(page);

            next = link_header.as_deref().and_then(parse_next_link);
        }

        Ok(out)
    }
}

fn new_client() -> Client {
    Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build reqwest client")
}

/// Parse a GitHub `Link` header and return the URL whose `rel="next"`, if any.
///
/// Format (per RFC 5988):
///   `<https://api.github.com/…?page=2>; rel="next", <…>; rel="last"`
fn parse_next_link(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let Some((target, params)) = segment.split_once(';') else {
            continue;
        };
        let target = target.trim();
        let target = target.strip_prefix('<')?.strip_suffix('>')?;
        for param in params.split(';') {
            let param = param.trim();
            if let Some(rest) = param.strip_prefix("rel=") {
                let value = rest.trim_matches('"');
                if value == "next" {
                    return Some(target.to_string());
                }
            }
        }
    }
    None
}

/// Minimal percent-encoder for a single URL path segment.
fn urlencode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(c as char);
            }
            _ => out.push_str(&format!("%{c:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_next_link_extracts_rel_next() {
        let header = r#"<https://api.github.com/orgs/acme/members?per_page=100&page=2>; rel="next", <https://api.github.com/orgs/acme/members?per_page=100&page=5>; rel="last""#;
        assert_eq!(
            parse_next_link(header),
            Some("https://api.github.com/orgs/acme/members?per_page=100&page=2".to_string())
        );
    }

    #[test]
    fn parse_next_link_returns_none_when_no_next() {
        let header = r#"<https://api.github.com/orgs/acme/members?per_page=100&page=1>; rel="prev", <https://api.github.com/orgs/acme/members?per_page=100&page=5>; rel="last""#;
        assert_eq!(parse_next_link(header), None);
    }

    #[test]
    fn parse_next_link_accepts_unquoted_rel() {
        let header = "<https://api.github.com/orgs/acme/members?page=2>; rel=next";
        assert_eq!(
            parse_next_link(header),
            Some("https://api.github.com/orgs/acme/members?page=2".to_string())
        );
    }

    #[test]
    fn urlencode_path_segment_encodes_reserved() {
        assert_eq!(urlencode_path_segment("acme org"), "acme%20org");
        assert_eq!(urlencode_path_segment("a/b"), "a%2Fb");
    }

    #[test]
    fn app_installation_defaults_are_sensible() {
        let install = AppInstallation {
            id: 42,
            account: None,
            repository_selection: None,
            html_url: None,
        };
        assert_eq!(install.repository_selection(), "all");
        assert_eq!(install.html_url(), "");
        assert_eq!(install.account().login, "");
    }
}
