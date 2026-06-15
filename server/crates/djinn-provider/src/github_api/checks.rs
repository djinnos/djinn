use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::{
    ActionsJobsResponse, CheckRunsResponse, WorkflowRun, WorkflowRunsResponse,
};
use crate::github_api::{ActionsJob, CheckAnnotation, GitHubApiClient, GitHubApiError};

impl GitHubApiClient {
    /// List workflow runs for a repo filtered by trigger `event` (e.g.
    /// `"merge_group"`), newest first. Used to find the merge-group run that
    /// rejected a PR so we can surface its real failure — the run (and its
    /// head commit's check runs) persist even after the ephemeral merge-queue
    /// branch is deleted.
    pub async fn list_workflow_runs_for_event(
        &self,
        owner: &str,
        repo: &str,
        event: &str,
        per_page: u32,
    ) -> std::result::Result<Vec<WorkflowRun>, GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/actions/runs?event={}&per_page={}",
            self.base_url, owner, repo, event, per_page
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
            return Err(GitHubApiError::http(
                "list_workflow_runs_for_event",
                format!("/repos/{owner}/{repo}/actions/runs"),
                status,
                body,
            ));
        }
        let parsed: WorkflowRunsResponse = resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_workflow_runs_for_event",
                format!("/repos/{owner}/{repo}/actions/runs"),
                e.to_string(),
            )
        })?;
        Ok(parsed.workflow_runs)
    }

    /// List check runs for an arbitrary git ref or commit SHA.
    ///
    /// Unlike `get_pull_request` (which fetches the PR head's checks), this
    /// targets any ref — notably a merge-queue `gh-readonly-queue/...` branch.
    /// The merge group runs CI against that ref's merge commit, so when the
    /// queue rejects a PR the *real* failing checks live here, not on the PR
    /// head (whose checks passed). Used to surface the actual failure to the
    /// reworking worker instead of GitHub's generic dequeue reason.
    pub async fn list_check_runs_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> std::result::Result<CheckRunsResponse, GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/commits/{}/check-runs?per_page=100",
            self.base_url, owner, repo, git_ref
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
            return Err(GitHubApiError::http(
                "list_check_runs_for_ref",
                format!("/repos/{owner}/{repo}/commits/{git_ref}/check-runs"),
                status,
                body,
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_check_runs_for_ref",
                format!("/repos/{owner}/{repo}/commits/{git_ref}/check-runs"),
                e.to_string(),
            )
        })
    }

    /// Fetch annotations for a check run.
    pub async fn get_check_run_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
    ) -> std::result::Result<Vec<CheckAnnotation>, GitHubApiError> {
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
            return Err(GitHubApiError::http(
                "get_check_run_annotations",
                format!("/repos/{owner}/{repo}/check-runs/{check_run_id}/annotations"),
                status,
                body,
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "get_check_run_annotations",
                format!("/repos/{owner}/{repo}/check-runs/{check_run_id}/annotations"),
                e.to_string(),
            )
        })
    }

    /// List jobs for a workflow run.
    pub async fn list_run_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> std::result::Result<Vec<ActionsJob>, GitHubApiError> {
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
            return Err(GitHubApiError::http(
                "list_run_jobs",
                format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs"),
                status,
                body,
            ));
        }
        let parsed: ActionsJobsResponse = resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_run_jobs",
                format!("/repos/{owner}/{repo}/actions/runs/{run_id}/jobs"),
                e.to_string(),
            )
        })?;
        Ok(parsed.jobs)
    }

    /// Download the raw log text for a specific Actions job.
    pub async fn get_job_logs(
        &self,
        owner: &str,
        repo: &str,
        job_id: u64,
    ) -> std::result::Result<String, GitHubApiError> {
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
            return Err(GitHubApiError::http(
                "get_job_logs",
                format!("/repos/{owner}/{repo}/actions/jobs/{job_id}/logs"),
                status,
                body,
            ));
        }
        resp.text().await.map_err(|e| {
            GitHubApiError::transport(
                "get_job_logs",
                format!("/repos/{owner}/{repo}/actions/jobs/{job_id}/logs"),
                e.to_string(),
            )
        })
    }
}
