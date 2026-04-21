//! GitHub Contents + Git Refs helpers — the minimal surface needed to
//! commit a single file on a fresh branch and open a PR from the server
//! without a local worktree.
//!
//! # Operations
//! - [`GitHubApiClient::get_ref`] — resolve a ref to its target commit SHA
//! - [`GitHubApiClient::create_ref`] — create a branch/tag pointing at a SHA
//!   (treats `422 Reference already exists` as a no-op so the caller can
//!   be idempotent)
//! - [`GitHubApiClient::get_file_sha`] — fetch the blob SHA for a file on a
//!   branch, or `None` when the file does not exist yet
//! - [`GitHubApiClient::put_file`] — create or update a single file via
//!   `PUT /repos/{owner}/{repo}/contents/{path}`, commit attributed to the
//!   authenticated principal (installation App → `djinn-bot[bot]`)

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::github_api::GitHubApiClient;
use crate::github_api::transport::handle_rate_limit;

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Deserialize)]
struct RefResponse {
    object: RefObject,
}

#[derive(Deserialize)]
struct ContentFile {
    sha: String,
}

impl GitHubApiClient {
    /// Resolve `ref_name` (e.g. `"heads/main"`) to the SHA it points at.
    /// Returns `Ok(None)` on `404 Not Found` so callers can branch on
    /// ref absence without catching error strings.
    pub async fn get_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/repos/{}/{}/git/ref/{}",
            self.base_url, owner, repo, ref_name
        );

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

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_ref failed ({}): {}", status, body));
        }
        let parsed: RefResponse = resp.json().await?;
        Ok(Some(parsed.object.sha))
    }

    /// Create a ref `ref_name` (full form, e.g. `"refs/heads/djinn/setup"`)
    /// pointing at `sha`. Treats `422 Reference already exists` as success
    /// so this is safe to call in a retry loop.
    pub async fn create_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
        sha: &str,
    ) -> Result<()> {
        let url = format!("{}/repos/{}/{}/git/refs", self.base_url, owner, repo);
        let body = serde_json::json!({ "ref": ref_name, "sha": sha });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if resp.status().is_success() {
            return Ok(());
        }
        if resp.status() == StatusCode::UNPROCESSABLE_ENTITY {
            // "Reference already exists" — caller is driving an idempotent
            // branch-create path, so swallow and let them proceed.
            let body = resp.text().await.unwrap_or_default();
            if body.contains("already exists") {
                return Ok(());
            }
            return Err(anyhow!("create_ref failed (422): {}", body));
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("create_ref failed ({}): {}", status, body))
    }

    /// Fetch the blob SHA for `path` on `ref_or_branch`, or `Ok(None)` if
    /// the file does not exist on that ref. Callers thread the returned
    /// SHA into [`put_file`] when updating (GitHub requires it for
    /// non-create writes).
    pub async fn get_file_sha(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_or_branch: &str,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/repos/{}/{}/contents/{}?ref={}",
            self.base_url, owner, repo, path, ref_or_branch
        );

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

        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_file_sha failed ({}): {}", status, body));
        }
        let parsed: ContentFile = resp.json().await?;
        Ok(Some(parsed.sha))
    }

    /// Create or update a single file on `branch` via the Contents API.
    /// Pass `prev_sha = None` for a create and `Some(sha)` for an update.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        branch: &str,
        message: &str,
        content: &[u8],
        prev_sha: Option<&str>,
    ) -> Result<()> {
        let url = format!(
            "{}/repos/{}/{}/contents/{}",
            self.base_url, owner, repo, path
        );
        let encoded = BASE64.encode(content);
        let mut body = serde_json::json!({
            "message": message,
            "content": encoded,
            "branch": branch,
        });
        if let Some(sha) = prev_sha
            && let Some(map) = body.as_object_mut()
        {
            map.insert("sha".into(), serde_json::Value::String(sha.to_string()));
        }

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .put(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("put_file failed ({}): {}", status, body));
        }
        Ok(())
    }
}
