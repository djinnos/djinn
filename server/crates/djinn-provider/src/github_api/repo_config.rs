use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::{GitHubApiClient, RepoMergeConfig};

impl GitHubApiClient {
    /// Read the repository's allowed merge methods (`allow_squash_merge`,
    /// `allow_merge_commit`, `allow_rebase_merge`) from `GET /repos`.
    ///
    /// Used by the PR poller to pick a merge strategy the repo actually
    /// permits instead of blindly attempting squash (which 405-loops forever
    /// on repos that disable squash merges). Missing fields default to `true`
    /// (see [`RepoMergeConfig`]), so a partial payload degrades to the
    /// permissive legacy assumption.
    pub async fn get_repo_merge_config(&self, owner: &str, repo: &str) -> Result<RepoMergeConfig> {
        let url = format!("{}/repos/{}/{}", self.base_url, owner, repo);

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "get_repo_merge_config failed ({}): {}",
                status,
                body
            ));
        }
        let cfg: RepoMergeConfig = resp.json().await?;
        Ok(cfg)
    }
}
