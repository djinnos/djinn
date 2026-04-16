use anyhow::{Result, anyhow};

use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::ActionsJobsResponse;
use crate::github_api::{ActionsJob, CheckAnnotation, GitHubApiClient};

impl GitHubApiClient {
    /// Fetch annotations for a check run.
    pub async fn get_check_run_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>> {
        let url = format!(
            "{}/repos/{}/{}/check-runs/{}/annotations",
            self.base_url, owner, repo, check_run_id
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "get_check_run_annotations failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await?)
    }

    /// List jobs for a workflow run.
    pub async fn list_run_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<Vec<ActionsJob>> {
        let url = format!(
            "{}/repos/{}/{}/actions/runs/{}/jobs?per_page=100",
            self.base_url, owner, repo, run_id
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("list_run_jobs failed ({}): {}", status, body));
        }
        let parsed: ActionsJobsResponse = resp.json().await?;
        Ok(parsed.jobs)
    }

    /// Download the raw log text for a specific Actions job.
    pub async fn get_job_logs(&self, owner: &str, repo: &str, job_id: u64) -> Result<String> {
        let url = format!(
            "{}/repos/{}/{}/actions/jobs/{}/logs",
            self.base_url, owner, repo, job_id
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

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_job_logs failed ({}): {}", status, body));
        }
        Ok(resp.text().await?)
    }
}
