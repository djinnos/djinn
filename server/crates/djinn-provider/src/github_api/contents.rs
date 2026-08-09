//! GitHub Contents + Git Refs helpers — the minimal surface needed to
//! commit a single file on a fresh branch and open a PR from the server
//! without a local worktree.

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::StatusCode;
use serde::Deserialize;

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::{
    ExactRefObservation, ExpectedAbsentRefResult, ExpectedOldShaRefUpdateResult, GitHubApiClient,
    GitHubApiError,
};

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
    /// Observe a ref without collapsing not-found and provider failure.
    pub async fn observe_exact_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> ExactRefObservation {
        match self.get_ref(owner, repo, ref_name).await {
            Ok(Some(sha)) => ExactRefObservation::Found { sha },
            Ok(None) => ExactRefObservation::NotFound,
            Err(error) => ExactRefObservation::ProviderFailure(GitHubApiError::transport(
                "observe_exact_ref",
                format!("/repos/{owner}/{repo}/git/ref/{ref_name}"),
                error.to_string(),
            )),
        }
    }

    /// Create an absent ref, adopting a create race only at the exact SHA.
    pub async fn create_ref_expected_absent(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
        expected_sha: &str,
    ) -> ExpectedAbsentRefResult {
        let path = format!("/repos/{owner}/{repo}/git/refs");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::json!({ "ref": ref_name, "sha": expected_sha });
        let response = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    handle_rate_limit(
                        http.post(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .json(&body)
                            .send()
                            .await?,
                    )
                    .await
                }
            })
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return ExpectedAbsentRefResult::ProviderFailure(error),
        };
        if response.status().is_success() {
            return ExpectedAbsentRefResult::Created;
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status != StatusCode::UNPROCESSABLE_ENTITY || !body.contains("already exists") {
            return ExpectedAbsentRefResult::ProviderFailure(GitHubApiError::http(
                "create_ref_expected_absent",
                path,
                status,
                body,
            ));
        }
        match self.observe_exact_ref(owner, repo, ref_name).await {
            ExactRefObservation::Found { sha } if sha == expected_sha => {
                ExpectedAbsentRefResult::AdoptedExact { sha }
            }
            ExactRefObservation::Found { sha } => {
                ExpectedAbsentRefResult::BranchIdentityMismatch { observed_sha: sha }
            }
            ExactRefObservation::NotFound => {
                ExpectedAbsentRefResult::ProviderFailure(GitHubApiError::http(
                    "create_ref_expected_absent",
                    format!("/repos/{owner}/{repo}/git/ref/{ref_name}"),
                    StatusCode::CONFLICT,
                    "ref absent after already-exists response".into(),
                ))
            }
            ExactRefObservation::ProviderFailure(error) => {
                ExpectedAbsentRefResult::ProviderFailure(error)
            }
        }
    }

    /// Advance a ref only after observing the expected SHA; never force-push.
    pub async fn update_ref_expected_old_sha(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
        expected_old_sha: &str,
        new_sha: &str,
    ) -> ExpectedOldShaRefUpdateResult {
        match self.observe_exact_ref(owner, repo, ref_name).await {
            ExactRefObservation::Found { sha } if sha != expected_old_sha => {
                return ExpectedOldShaRefUpdateResult::StaleObservedHead {
                    observed_sha: Some(sha),
                };
            }
            ExactRefObservation::NotFound => {
                return ExpectedOldShaRefUpdateResult::StaleObservedHead { observed_sha: None };
            }
            ExactRefObservation::ProviderFailure(error) => {
                return ExpectedOldShaRefUpdateResult::ProviderFailure(error);
            }
            ExactRefObservation::Found { .. } => {}
        }
        let path = format!("/repos/{owner}/{repo}/git/refs/{ref_name}");
        let url = format!("{}{}", self.base_url, path);
        let body = serde_json::json!({ "sha": new_sha, "force": false });
        let response = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    handle_rate_limit(
                        http.patch(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .json(&body)
                            .send()
                            .await?,
                    )
                    .await
                }
            })
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                ExpectedOldShaRefUpdateResult::Updated {
                    sha: new_sha.into(),
                }
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                match self.observe_exact_ref(owner, repo, ref_name).await {
                    // A failed conditional write is stale only when the
                    // follow-up observation proves another writer moved (or
                    // removed) the ref. Do not mask a 403 while the ref is
                    // still at the expected old SHA.
                    ExactRefObservation::Found { sha } if sha != expected_old_sha => {
                        ExpectedOldShaRefUpdateResult::StaleObservedHead {
                            observed_sha: Some(sha),
                        }
                    }
                    ExactRefObservation::Found { .. } => {
                        ExpectedOldShaRefUpdateResult::ProviderFailure(GitHubApiError::http(
                            "update_ref_expected_old_sha",
                            path,
                            status,
                            body,
                        ))
                    }
                    ExactRefObservation::NotFound => {
                        ExpectedOldShaRefUpdateResult::StaleObservedHead { observed_sha: None }
                    }
                    ExactRefObservation::ProviderFailure(_) => {
                        ExpectedOldShaRefUpdateResult::ProviderFailure(GitHubApiError::http(
                            "update_ref_expected_old_sha",
                            path,
                            status,
                            body,
                        ))
                    }
                }
            }
            Err(error) => ExpectedOldShaRefUpdateResult::ProviderFailure(error),
        }
    }

    pub async fn get_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<Option<String>> {
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

    pub async fn create_ref(
        &self,
        owner: &str,
        repo: &str,
        ref_name: &str,
        sha: &str,
    ) -> std::result::Result<(), GitHubApiError> {
        let url = format!("{}/repos/{}/{}/git/refs", self.base_url, owner, repo);
        let api_path = format!("/repos/{owner}/{repo}/git/refs");
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
            let body = resp.text().await.unwrap_or_default();
            if body.contains("already exists") {
                return Ok(());
            }
            return Err(GitHubApiError::http(
                "create_ref",
                api_path,
                StatusCode::UNPROCESSABLE_ENTITY,
                body,
            ));
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(GitHubApiError::http("create_ref", api_path, status, body))
    }

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
    ) -> std::result::Result<(), GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/contents/{}",
            self.base_url, owner, repo, path
        );
        let encoded = BASE64.encode(content);
        let mut body =
            serde_json::json!({ "message": message, "content": encoded, "branch": branch });
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
            return Err(GitHubApiError::http(
                "put_file",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                status,
                body,
            ));
        }
        Ok(())
    }
}
