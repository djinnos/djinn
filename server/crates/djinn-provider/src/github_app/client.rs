//! Thin reqwest wrapper that injects installation access tokens into
//! outbound GitHub REST calls.
//!
//! Unlike the [`crate::github_api::GitHubApiClient`] (which reads the user
//! OAuth token from the credential DB), this client is stateless and is
//! parameterised by an `installation_id`. It lazily fetches and caches
//! installation tokens via [`super::installations::get_installation_token`].

use anyhow::{Result, anyhow};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::Deserialize;

use super::installations::{
    InstallationToken, get_installation_token, invalidate_cache, list_installations_for_user,
};
use super::{ENV_APP_SLUG, app_id};

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "djinn-server/0.1 (+https://github.com/djinnos/server)";

/// Build the "install this app" URL for the given slug.
///
/// Returns `None` if [`GITHUB_APP_SLUG`](ENV_APP_SLUG) is unset.
pub fn install_url() -> Option<String> {
    let slug = std::env::var(ENV_APP_SLUG).ok()?;
    let slug = slug.trim();
    if slug.is_empty() {
        return None;
    }
    Some(format!("https://github.com/apps/{slug}/installations/new"))
}

/// Reqwest wrapper scoped to one installation.
///
/// Every outbound request pulls a cached installation token and attaches it
/// as `Authorization: Bearer <token>`. On a `401`, the cache is invalidated
/// and a single retry with a fresh token is attempted.
#[derive(Clone)]
pub struct GitHubAppClient {
    http: Client,
    installation_id: u64,
}

impl GitHubAppClient {
    pub fn new(installation_id: u64) -> Self {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            installation_id,
        }
    }

    pub fn installation_id(&self) -> u64 {
        self.installation_id
    }

    /// Mint (or reuse) an installation token for this installation.
    pub async fn token(&self) -> Result<InstallationToken> {
        get_installation_token(self.installation_id).await
    }

    /// Convenience: return just the bearer string.
    pub async fn bearer(&self) -> Result<String> {
        Ok(self.token().await?.token)
    }

    /// GET `path` on `api.github.com`, inserting the installation token.
    /// Caller supplies the path starting with `/`.
    pub async fn get(&self, path: &str) -> Result<reqwest::Response> {
        self.send(|http, tok| {
            http.get(format!("{GITHUB_API}{path}"))
                .bearer_auth(tok)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
        })
        .await
    }

    /// Generic: let the caller build the request via `f(http, token)`.
    /// Retries once with a fresh installation token on 401.
    pub async fn send<F>(&self, build: F) -> Result<reqwest::Response>
    where
        F: Fn(&Client, &str) -> RequestBuilder,
    {
        let tok = self.bearer().await?;
        let resp = build(&self.http, &tok)
            .send()
            .await
            .map_err(|e| anyhow!("github-app request failed: {e}"))?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        // Token may have been revoked mid-flight. Invalidate and retry once.
        tracing::warn!(
            installation_id = self.installation_id,
            "github-app: 401 — refreshing installation token and retrying"
        );
        invalidate_cache(self.installation_id);
        let tok = self.bearer().await?;
        build(&self.http, &tok)
            .send()
            .await
            .map_err(|e| anyhow!("github-app retry failed: {e}"))
    }

    /// List every repository available to this installation.
    ///
    /// `GET /installation/repositories` is page-indexed at 1 and caps at
    /// 100 items per page. GitHub returns `total_count` on page 1, so we
    /// compute the remaining page count up front and fetch them in order.
    /// Orgs with hundreds of repos therefore get fully listed instead of
    /// silently truncated at 100. `per_page` tunes the page size (and
    /// therefore request count); the returned list is always the full set.
    ///
    /// To keep a pathological org from hammering us, we bail at
    /// [`MAX_PAGES`] and log a warning — callers needing true unlimited
    /// pagination can extend from there.
    pub async fn list_repositories(
        &self,
        per_page: Option<usize>,
    ) -> Result<Vec<InstallationRepo>> {
        const MAX_PAGES: u32 = 50; // 50 * 100 = 5000 repos — more than any real org we care about.

        let per_page = per_page.unwrap_or(100).clamp(1, 100);

        #[derive(Deserialize)]
        struct RepoPage {
            #[serde(default)]
            total_count: u32,
            repositories: Vec<RawRepo>,
        }
        #[derive(Deserialize)]
        struct RawRepo {
            name: String,
            full_name: String,
            #[serde(default)]
            default_branch: Option<String>,
            #[serde(default)]
            private: bool,
            #[serde(default)]
            description: Option<String>,
            owner: RawOwner,
        }
        #[derive(Deserialize)]
        struct RawOwner {
            login: String,
        }

        let fetch_page = |page: u32| async move {
            let resp = self
                .get(&format!(
                    "/installation/repositories?per_page={per_page}&page={page}"
                ))
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "/installation/repositories failed ({status}): {body}"
                ));
            }
            resp.json::<RepoPage>()
                .await
                .map_err(|e| anyhow!("failed to decode repo page: {e}"))
        };

        let first = fetch_page(1).await?;
        let total = first.total_count as usize;
        let total_pages = total.div_ceil(per_page).max(1) as u32;

        let mut out: Vec<InstallationRepo> = Vec::with_capacity(total);
        out.extend(first.repositories.into_iter().map(raw_to_installation));

        if total_pages > 1 {
            let fetch_upto = total_pages.min(MAX_PAGES);
            if total_pages > MAX_PAGES {
                tracing::warn!(
                    installation_id = self.installation_id,
                    total_repos = total,
                    total_pages,
                    max_pages = MAX_PAGES,
                    "list_repositories: truncating — installation has more repos than MAX_PAGES can cover",
                );
            }
            for page in 2..=fetch_upto {
                let p = fetch_page(page).await?;
                out.extend(p.repositories.into_iter().map(raw_to_installation));
            }
        }

        return Ok(out);

        fn raw_to_installation(r: RawRepo) -> InstallationRepo {
            InstallationRepo {
                owner: r.owner.login,
                repo: r.name,
                full_name: r.full_name,
                default_branch: r.default_branch.unwrap_or_else(|| "main".into()),
                private: r.private,
                description: r.description,
            }
        }
    }
}

/// One repository visible to an installation.
#[derive(Debug, Clone)]
pub struct InstallationRepo {
    pub owner: String,
    pub repo: String,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub description: Option<String>,
}

/// Given a user token, find an installation that contains `owner/repo`,
/// returning its id. Queries `/installation/repositories` for each
/// installation until a match is found.
pub async fn find_installation_for_repo(user_token: &str, owner: &str, repo: &str) -> Result<u64> {
    // App config must be present to mint the per-installation tokens.
    let _ = app_id().map_err(|e| anyhow!("GitHub App not configured: {e}"))?;

    let installs = list_installations_for_user(user_token).await?;
    let target = format!("{}/{}", owner.to_lowercase(), repo.to_lowercase());
    for install in installs {
        let client = GitHubAppClient::new(install.id);
        let repos = match client.list_repositories(Some(100)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    installation_id = install.id,
                    error = %e,
                    "find_installation_for_repo: skipping installation"
                );
                continue;
            }
        };
        if repos.iter().any(|r| r.full_name.to_lowercase() == target) {
            return Ok(install.id);
        }
    }
    Err(anyhow!(
        "no installation of the Djinn GitHub App contains {owner}/{repo} — \
         install the app at {}",
        install_url().unwrap_or_else(|| "https://github.com/settings/installations".into())
    ))
}

/// Re-export for callers that want the raw Installation struct.
pub use super::installations::Installation as _Installation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_url_respects_env() {
        unsafe { std::env::set_var(ENV_APP_SLUG, "djinn-bot") };
        assert_eq!(
            install_url().as_deref(),
            Some("https://github.com/apps/djinn-bot/installations/new")
        );
        unsafe { std::env::remove_var(ENV_APP_SLUG) };
        assert_eq!(install_url(), None);
    }
}
