use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::{
    ActionsArtifactsResponse, ActionsJobsResponse, CheckRunsResponse, CiFailureContextBundle,
    CiFailureContextRequest, CiSetupStep, ReproductionJob, ReproductionSetupStep, ReproductionStep,
    RequiredCheckReproduction, RequiredCheckReproductionContext, RequiredCheckUnreproducible,
    RequiredCheckUnreproducibleReason, WorkflowRun, WorkflowRunsResponse,
};
use crate::github_api::{
    ActionsJob, ActionsJobStep, CheckAnnotation, CheckRun, GitHubApiClient, GitHubApiError,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures::StreamExt;
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_yaml::Value as YamlValue;

const LOG_TAIL_LINES: usize = 200;
const LOG_TAIL_BYTES: usize = 24_000;
/// Maximum compressed artifact response size (32 MiB).
pub const MAX_ARTIFACT_DOWNLOAD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ContentsFileResponse {
    content: String,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkflowStep {
    name: String,
    run: Option<String>,
    uses: Option<String>,
}

impl GitHubApiClient {
    /// Fetch one repository-scoped job, including its owning workflow run id.
    pub async fn get_job(
        &self,
        owner: &str,
        repo: &str,
        job_id: u64,
    ) -> std::result::Result<ActionsJob, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/actions/jobs/{job_id}");
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    handle_rate_limit(
                        http.get(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .send()
                            .await?,
                    )
                    .await
                }
            })
            .await
            .map_err(|err| artifact_context(err, "get_job", &path, None))?;
        if !resp.status().is_success() {
            return Err(artifact_http("get_job", path, resp, None).await);
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "get_job",
                format!("/repos/{owner}/{repo}/actions/jobs/{job_id}"),
                format!("malformed GitHub Actions job response: {e}"),
            )
        })
    }

    /// Return GitHub first artifact page in original order without pagination.
    pub async fn list_run_artifacts(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> std::result::Result<crate::github_api::RunArtifactsPage, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/actions/runs/{run_id}/artifacts");
        let url = format!("{}{}?per_page=100", self.base_url, path);
        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    handle_rate_limit(
                        http.get(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .send()
                            .await?,
                    )
                    .await
                }
            })
            .await
            .map_err(|err| artifact_context(err, "list_run_artifacts", &path, Some(run_id)))?;
        if !resp.status().is_success() {
            return Err(artifact_http("list_run_artifacts", path, resp, Some(run_id)).await);
        }
        let parsed: ActionsArtifactsResponse = resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_run_artifacts",
                format!("/repos/{owner}/{repo}/actions/runs/{run_id}/artifacts"),
                format!("malformed GitHub Actions artifact response for run {run_id}: {e}"),
            )
        })?;
        let returned = parsed.artifacts.len();
        let mut artifacts = parsed.artifacts;
        artifacts.truncate(100);
        Ok(crate::github_api::RunArtifactsPage {
            total_count: parsed.total_count,
            truncated: parsed.total_count > artifacts.len() as u64 || returned > artifacts.len(),
            artifacts,
        })
    }

    /// Download archive bytes without extraction, enforcing a 32 MiB compressed cap.
    pub async fn download_artifact(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
        artifact_id: u64,
    ) -> std::result::Result<crate::github_api::DownloadedArtifact, GitHubApiError> {
        let path = format!("/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip");
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    handle_rate_limit(
                        http.get(&url)
                            .bearer_auth(&token)
                            .header("Accept", "application/vnd.github+json")
                            .header("X-GitHub-Api-Version", "2022-11-28")
                            .send()
                            .await?,
                    )
                    .await
                }
            })
            .await
            .map_err(|err| artifact_context(err, "download_artifact", &path, Some(run_id)))?;
        if !resp.status().is_success() {
            return Err(artifact_http("download_artifact", path, resp, Some(run_id)).await);
        }
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let content_length = match resp.headers().get(header::CONTENT_LENGTH) { Some(value) => Some(value.to_str().ok().and_then(|v| v.parse::<u64>().ok()).ok_or_else(|| GitHubApiError::transport("download_artifact", format!("/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip"), format!("malformed Content-Length while downloading artifact {artifact_id} for run {run_id}")))?), None => None };
        if content_length.is_some_and(|n| n > MAX_ARTIFACT_DOWNLOAD_BYTES as u64) {
            return Err(artifact_budget_error(owner, repo, run_id, artifact_id));
        }
        let mut bytes = Vec::with_capacity(
            content_length
                .unwrap_or_default()
                .min(MAX_ARTIFACT_DOWNLOAD_BYTES as u64) as usize,
        );
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                GitHubApiError::transport(
                    "download_artifact",
                    format!("/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip"),
                    format!("artifact download stream failed for run {run_id}: {e}"),
                )
            })?;
            if bytes.len().saturating_add(chunk.len()) > MAX_ARTIFACT_DOWNLOAD_BYTES {
                return Err(artifact_budget_error(owner, repo, run_id, artifact_id));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(crate::github_api::DownloadedArtifact {
            bytes,
            content_type,
            content_length,
        })
    }
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

    /// Fetch a single workflow run by id.
    pub async fn get_workflow_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> std::result::Result<WorkflowRun, GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/actions/runs/{}",
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
                "get_workflow_run",
                format!("/repos/{owner}/{repo}/actions/runs/{run_id}"),
                status,
                body,
            ));
        }
        resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "get_workflow_run",
                format!("/repos/{owner}/{repo}/actions/runs/{run_id}"),
                e.to_string(),
            )
        })
    }

    /// List workflow runs for a specific head SHA, newest first.
    pub async fn list_workflow_runs_for_head_sha(
        &self,
        owner: &str,
        repo: &str,
        head_sha: &str,
        per_page: u32,
    ) -> std::result::Result<Vec<WorkflowRun>, GitHubApiError> {
        let url = format!(
            "{}/repos/{}/{}/actions/runs?head_sha={}&per_page={}",
            self.base_url, owner, repo, head_sha, per_page
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
                "list_workflow_runs_for_head_sha",
                format!("/repos/{owner}/{repo}/actions/runs"),
                status,
                body,
            ));
        }
        let parsed: WorkflowRunsResponse = resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "list_workflow_runs_for_head_sha",
                format!("/repos/{owner}/{repo}/actions/runs"),
                e.to_string(),
            )
        })?;
        Ok(parsed.workflow_runs)
    }
}

impl GitHubApiClient {
    /// Read a workflow file from the repository at a specific ref/SHA.
    pub async fn get_workflow_file_at_ref(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        ref_or_sha: &str,
    ) -> std::result::Result<Option<String>, GitHubApiError> {
        let encoded_ref = serde_urlencoded::to_string([("ref", ref_or_sha)]).map_err(|e| {
            GitHubApiError::transport(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                e.to_string(),
            )
        })?;
        let url = format!(
            "{}/repos/{}/{}/contents/{}?{}",
            self.base_url, owner, repo, path, encoded_ref
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
            return Err(GitHubApiError::http(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                status,
                body,
            ));
        }

        let parsed: ContentsFileResponse = resp.json().await.map_err(|e| {
            GitHubApiError::transport(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                e.to_string(),
            )
        })?;
        if parsed.encoding.as_deref() != Some("base64") {
            return Err(GitHubApiError::transport(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                "workflow file response was not base64 encoded".to_string(),
            ));
        }
        let compact = parsed.content.replace(['\n', '\r'], "");
        let decoded = BASE64.decode(compact).map_err(|e| {
            GitHubApiError::transport(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                e.to_string(),
            )
        })?;
        String::from_utf8(decoded).map(Some).map_err(|e| {
            GitHubApiError::transport(
                "get_workflow_file_at_ref",
                format!("/repos/{owner}/{repo}/contents/{path}"),
                e.to_string(),
            )
        })
    }

    /// Build a repo-agnostic context bundle for a failing required GitHub
    /// Actions check/job. The command and setup context are derived from the
    /// target repository's workflow YAML for the observed run head SHA.
    pub async fn build_ci_failure_context_bundle(
        &self,
        request: CiFailureContextRequest,
    ) -> std::result::Result<CiFailureContextBundle, GitHubApiError> {
        let run = match request.workflow_run_id {
            Some(run_id) => {
                self.get_workflow_run(&request.owner, &request.repo, run_id)
                    .await?
            }
            None => self
                .list_workflow_runs_for_head_sha(
                    &request.owner,
                    &request.repo,
                    &request.head_sha,
                    50,
                )
                .await?
                .into_iter()
                .find(|run| {
                    request
                        .workflow_id
                        .is_none_or(|id| run.workflow_id == Some(id))
                        && run.head_sha == request.head_sha
                })
                .ok_or_else(|| {
                    GitHubApiError::transport(
                        "build_ci_failure_context_bundle",
                        format!("{}/{}@{}", request.owner, request.repo, request.head_sha),
                        "no workflow run matched the requested head SHA".to_string(),
                    )
                })?,
        };

        let jobs = self
            .list_run_jobs(&request.owner, &request.repo, run.id)
            .await?;
        let job = select_actions_job(&jobs, &request).ok_or_else(|| {
            GitHubApiError::transport(
                "build_ci_failure_context_bundle",
                format!(
                    "/repos/{}/{}/actions/runs/{}/jobs",
                    request.owner, request.repo, run.id
                ),
                "no matching failing Actions job found".to_string(),
            )
        })?;
        let failed_step = select_failed_step(job).ok_or_else(|| {
            GitHubApiError::transport(
                "build_ci_failure_context_bundle",
                format!(
                    "/repos/{}/{}/actions/jobs/{}",
                    request.owner, request.repo, job.id
                ),
                "matching Actions job did not include a failing step".to_string(),
            )
        })?;

        let workflow_path = request
            .workflow_path
            .clone()
            .or_else(|| run.path.clone())
            .or_else(|| {
                job.workflow_name
                    .as_ref()
                    .map(|name| workflow_path_from_name(name))
            })
            .ok_or_else(|| {
                GitHubApiError::transport(
                    "build_ci_failure_context_bundle",
                    format!(
                        "/repos/{}/{}/actions/runs/{}",
                        request.owner, request.repo, run.id
                    ),
                    "workflow path unavailable for run".to_string(),
                )
            })?;

        let workflow_yaml = self
            .get_workflow_file_at_ref(&request.owner, &request.repo, &workflow_path, &run.head_sha)
            .await?
            .ok_or_else(|| {
                GitHubApiError::transport(
                    "build_ci_failure_context_bundle",
                    format!(
                        "/repos/{}/{}/contents/{}",
                        request.owner, request.repo, workflow_path
                    ),
                    "workflow file was not found at run head SHA".to_string(),
                )
            })?;
        let workflow_steps = extract_workflow_steps(&workflow_yaml, &job.name, &failed_step.name)?;
        let failed_index = workflow_steps
            .iter()
            .position(|step| {
                step.name == failed_step.name || step.run.as_deref() == Some(&failed_step.name)
            })
            .ok_or_else(|| {
                GitHubApiError::transport(
                    "build_ci_failure_context_bundle",
                    workflow_path.clone(),
                    "failed step was not found in workflow definition".to_string(),
                )
            })?;
        let step_script = workflow_steps[failed_index].run.clone().ok_or_else(|| {
            GitHubApiError::transport(
                "build_ci_failure_context_bundle",
                workflow_path.clone(),
                "failed workflow step does not define a run script".to_string(),
            )
        })?;
        let setup_steps = workflow_steps[..failed_index]
            .iter()
            .map(|step| CiSetupStep {
                name: step.name.clone(),
                command: step.run.clone(),
                uses: step.uses.clone(),
            })
            .collect();

        let logs = self
            .get_job_logs(&request.owner, &request.repo, job.id)
            .await?;
        let log_tail = relevant_log_tail(&logs, &failed_step.name);

        Ok(CiFailureContextBundle {
            owner: request.owner,
            repo: request.repo,
            required_check_name: request.required_check_name,
            workflow_run_id: run.id,
            workflow_id: run.workflow_id.or(request.workflow_id),
            workflow_name: job.workflow_name.clone().or(run.name),
            workflow_path: Some(workflow_path),
            job_id: job.id,
            job_name: job.name.clone(),
            failing_step_name: failed_step.name.clone(),
            failing_step_number: failed_step.number,
            step_script,
            setup_steps,
            log_tail,
            observed_head_sha: run.head_sha,
        })
    }
}

fn select_actions_job<'a>(
    jobs: &'a [ActionsJob],
    request: &CiFailureContextRequest,
) -> Option<&'a ActionsJob> {
    if let Some(job_id) = request.job_id
        && let Some(job) = jobs.iter().find(|job| job.id == job_id)
    {
        return Some(job);
    }

    // Prefer the job matching the required check only when it carries a real
    // failing step. Aggregate-gate setups (a required "gate" job that `needs:`
    // every real job) combined with fail-fast run cancellation leave the gate
    // job `cancelled` with no steps — the diagnosable failure lives in the
    // sibling job that hard-failed before the cancel.
    jobs.iter()
        .filter(|job| is_failing_conclusion(job.conclusion.as_deref()))
        .find(|job| {
            check_name_matches_job(&request.required_check_name, job) && has_failing_step(job)
        })
        .or_else(|| jobs.iter().find(|job| has_hard_failed_step(job)))
        .or_else(|| {
            jobs.iter()
                .find(|job| check_name_matches_job(&request.required_check_name, job))
        })
        .or_else(|| {
            jobs.iter()
                .find(|job| is_failing_conclusion(job.conclusion.as_deref()))
        })
}

fn check_name_matches_job(check_name: &str, job: &ActionsJob) -> bool {
    check_name == job.name
        || job.workflow_name.as_ref().is_some_and(|workflow| {
            check_name == workflow || check_name == format!("{} / {}", workflow, job.name)
        })
}

fn select_failed_step(job: &ActionsJob) -> Option<&crate::github_api::ActionsJobStep> {
    job.steps
        .iter()
        .find(|step| is_failing_conclusion(step.conclusion.as_deref()))
        .or_else(|| job.steps.iter().find(|step| step.status == "completed"))
}

fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("failure") | Some("timed_out") | Some("cancelled") | Some("action_required")
    )
}

/// A *hard* failure — the work itself broke, as opposed to `cancelled`, which
/// under fail-fast semantics usually means a sibling job failed first.
fn is_hard_failed_conclusion(conclusion: Option<&str>) -> bool {
    matches!(conclusion, Some("failure") | Some("timed_out"))
}

fn has_failing_step(job: &ActionsJob) -> bool {
    job.steps
        .iter()
        .any(|step| is_failing_conclusion(step.conclusion.as_deref()))
}

/// True when the job contains a step that hard-failed. Detected at step level
/// on purpose: a job that cancels its own run on failure can end up with a
/// job-level conclusion of `cancelled` (the cancel signal races its post
/// steps), but the failed step's conclusion is sealed before that — so this
/// identifies the real source of failure in a fail-fast-cancelled run
/// regardless of how the job-level race resolved.
fn has_hard_failed_step(job: &ActionsJob) -> bool {
    job.steps
        .iter()
        .any(|step| is_hard_failed_conclusion(step.conclusion.as_deref()))
}

fn workflow_path_from_name(name: &str) -> String {
    if name.starts_with(".github/workflows/") {
        name.to_string()
    } else {
        format!(".github/workflows/{name}")
    }
}

fn extract_workflow_steps(
    workflow_yaml: &str,
    job_name: &str,
    failed_step_name: &str,
) -> std::result::Result<Vec<WorkflowStep>, GitHubApiError> {
    let root: YamlValue = serde_yaml::from_str(workflow_yaml).map_err(|e| {
        GitHubApiError::transport(
            "extract_workflow_steps",
            "workflow yaml".to_string(),
            e.to_string(),
        )
    })?;

    let jobs = root
        .get("jobs")
        .and_then(YamlValue::as_mapping)
        .ok_or_else(|| {
            GitHubApiError::transport(
                "extract_workflow_steps",
                "workflow yaml".to_string(),
                "workflow has no jobs mapping".to_string(),
            )
        })?;

    let mut fallback_with_step = None;
    for (key, value) in jobs {
        let key = key.as_str().unwrap_or_default();
        let display_name = value.get("name").and_then(YamlValue::as_str).unwrap_or(key);
        let steps = parse_workflow_steps(value);
        if key == job_name || display_name == job_name {
            return Ok(steps);
        }
        if fallback_with_step.is_none()
            && steps.iter().any(|step| {
                step.name == failed_step_name || step.run.as_deref() == Some(failed_step_name)
            })
        {
            fallback_with_step = Some(steps);
        }
    }

    fallback_with_step.ok_or_else(|| {
        GitHubApiError::transport(
            "extract_workflow_steps",
            "workflow yaml".to_string(),
            "no workflow job matched the Actions job".to_string(),
        )
    })
}

fn parse_workflow_steps(job: &YamlValue) -> Vec<WorkflowStep> {
    job.get("steps")
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|step| {
            let map = step.as_mapping()?;
            let run = map
                .get(YamlValue::String("run".to_string()))
                .and_then(YamlValue::as_str)
                .map(ToOwned::to_owned);
            let uses = map
                .get(YamlValue::String("uses".to_string()))
                .and_then(YamlValue::as_str)
                .map(ToOwned::to_owned);
            let name = map
                .get(YamlValue::String("name".to_string()))
                .and_then(YamlValue::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| run.clone())
                .or_else(|| uses.clone())?;
            Some(WorkflowStep { name, run, uses })
        })
        .collect()
}

impl GitHubApiClient {
    /// Build a repo-derived reproduction bundle for a failing required check.
    ///
    /// The bundle is intentionally sourced only from GitHub's own check-run,
    /// workflow-run, job, step, and log data. If that data cannot identify an
    /// Actions shell command, this returns a typed `Unreproducible` result
    /// instead of guessing or defaulting a command.
    pub async fn required_check_reproduction_context(
        &self,
        owner: &str,
        repo: &str,
        observed_head_sha: &str,
        required_check_name: &str,
    ) -> std::result::Result<RequiredCheckReproduction, GitHubApiError> {
        let checks = self
            .list_check_runs_for_ref(owner, repo, observed_head_sha)
            .await?;
        let Some(check_run) = checks
            .check_runs
            .iter()
            .find(|check| check.name == required_check_name)
        else {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::CheckRunNotFound,
                None,
            ));
        };

        if !is_failed_conclusion(check_run.conclusion.as_deref()) {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::CheckRunNotFailed,
                check_run.conclusion.clone(),
            ));
        }

        let Some(run) = self
            .find_workflow_run_for_head(owner, repo, observed_head_sha)
            .await?
        else {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::WorkflowRunNotFound,
                None,
            ));
        };

        self.required_check_reproduction_context_for_run(
            owner,
            repo,
            observed_head_sha,
            required_check_name,
            check_run,
            &run,
        )
        .await
    }

    async fn required_check_reproduction_context_for_run(
        &self,
        owner: &str,
        repo: &str,
        observed_head_sha: &str,
        required_check_name: &str,
        check_run: &CheckRun,
        run: &WorkflowRun,
    ) -> std::result::Result<RequiredCheckReproduction, GitHubApiError> {
        let jobs = self.list_run_jobs(owner, repo, run.id).await?;
        // Same fail-fast-aware preference as `select_actions_job`: an
        // aggregate required gate cancelled by a fail-fast watchdog has no
        // failing step, so a check-name match only wins when it can actually
        // name a failing step; otherwise take the hard-failed sibling job that
        // caused the cancellation. Without this, every bundle for such a run
        // comes back Unreproducible (FailingStepNotFound on the gate job).
        let Some(job) = jobs
            .iter()
            .find(|job| job_matches_check(job, check_run) && has_failing_step(job))
            .or_else(|| jobs.iter().find(|job| has_hard_failed_step(job)))
            .or_else(|| jobs.iter().find(|job| job_matches_check(job, check_run)))
            .or_else(|| {
                jobs.iter()
                    .find(|job| is_failed_conclusion(job.conclusion.as_deref()))
            })
        else {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::JobNotFound,
                Some(format!("workflow_run_id={}", run.id)),
            ));
        };

        let Some(failing_step) = job.steps.iter().find(|step| {
            is_failed_conclusion(step.conclusion.as_deref())
                || step.conclusion.as_deref() == Some("timed_out")
        }) else {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::FailingStepNotFound,
                Some(format!("job_id={}", job.id)),
            ));
        };

        let logs = self.get_job_logs(owner, repo, job.id).await?;
        let parsed_steps = parse_actions_run_commands(&logs);
        let Some(command_index) = select_command_index(&parsed_steps, failing_step) else {
            return Ok(unreproducible(
                required_check_name,
                observed_head_sha,
                RequiredCheckUnreproducibleReason::CommandNotFound,
                Some(format!(
                    "job_id={}, step_number={}, step_name={}",
                    job.id, failing_step.number, failing_step.name
                )),
            ));
        };

        let parsed_command = &parsed_steps[command_index];
        let setup_steps = parsed_steps[..command_index]
            .iter()
            .map(|step| ReproductionSetupStep {
                number: step.ordinal,
                name: step.name.clone(),
                command: step.command.clone(),
            })
            .collect();

        Ok(RequiredCheckReproduction::Reproducible(
            RequiredCheckReproductionContext {
                required_check_name: required_check_name.to_owned(),
                observed_head_sha: observed_head_sha.to_owned(),
                check_run_id: check_run.id,
                workflow_run_id: run.id,
                workflow_name: job.workflow_name.clone().or_else(|| run.name.clone()),
                job: ReproductionJob {
                    id: job.id,
                    name: job.name.clone(),
                    html_url: job.html_url.clone(),
                },
                failing_step: ReproductionStep {
                    number: failing_step.number,
                    name: failing_step.name.clone(),
                },
                command: parsed_command.command.clone(),
                setup_steps,
                log_tail: log_tail_from(&logs, parsed_command.line_index),
            },
        ))
    }

    async fn find_workflow_run_for_head(
        &self,
        owner: &str,
        repo: &str,
        observed_head_sha: &str,
    ) -> std::result::Result<Option<WorkflowRun>, GitHubApiError> {
        const EVENTS: &[&str] = &["pull_request", "push", "merge_group", "workflow_dispatch"];
        for event in EVENTS {
            let runs = self
                .list_workflow_runs_for_event(owner, repo, event, 50)
                .await?;
            if let Some(run) = runs
                .into_iter()
                .find(|run| run.head_sha == observed_head_sha)
            {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }
}

fn unreproducible(
    required_check_name: &str,
    observed_head_sha: &str,
    reason: RequiredCheckUnreproducibleReason,
    details: Option<String>,
) -> RequiredCheckReproduction {
    RequiredCheckReproduction::Unreproducible(RequiredCheckUnreproducible {
        required_check_name: required_check_name.to_owned(),
        observed_head_sha: observed_head_sha.to_owned(),
        reason,
        details,
    })
}

fn is_failed_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("failure") | Some("timed_out") | Some("cancelled")
    )
}

fn job_matches_check(job: &ActionsJob, check_run: &CheckRun) -> bool {
    job.name == check_run.name
        || check_run.name.ends_with(&format!(" / {}", job.name))
        || check_run.name.ends_with(&format!("/{}", job.name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRunCommand {
    ordinal: u64,
    name: String,
    command: String,
    line_index: usize,
}

fn parse_actions_run_commands(logs: &str) -> Vec<ParsedRunCommand> {
    logs.lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let normalized = strip_log_prefix(line);
            let command = normalized
                .strip_prefix("##[group]Run ")
                .or_else(|| normalized.strip_prefix("Run "))?
                .trim();
            if command.is_empty() {
                return None;
            }
            Some((line_index, command.to_owned()))
        })
        .enumerate()
        .map(|(index, (line_index, command))| ParsedRunCommand {
            ordinal: index as u64 + 1,
            name: command.lines().next().unwrap_or(&command).to_owned(),
            command,
            line_index,
        })
        .collect()
}

fn relevant_log_tail(logs: &str, failed_step_name: &str) -> String {
    let lines: Vec<&str> = logs.lines().collect();
    let start = lines
        .iter()
        .rposition(|line| line.contains(failed_step_name))
        .unwrap_or_else(|| lines.len().saturating_sub(LOG_TAIL_LINES));
    let mut tail = lines[start..].join("\n");
    if tail.len() > LOG_TAIL_BYTES {
        let split_at = tail.len() - LOG_TAIL_BYTES;
        tail = tail[split_at..].to_string();
        if let Some(newline) = tail.find('\n') {
            tail = tail[newline + 1..].to_string();
        }
    }
    tail
}

fn strip_log_prefix(line: &str) -> &str {
    if let Some((prefix, rest)) = line.split_once(' ')
        && prefix.contains('T')
        && prefix.ends_with('Z')
    {
        return rest;
    }
    line
}

fn select_command_index(
    parsed_steps: &[ParsedRunCommand],
    failing_step: &ActionsJobStep,
) -> Option<usize> {
    parsed_steps
        .iter()
        .position(|step| step_matches_name(step, &failing_step.name))
        .or_else(|| {
            let step_number = failing_step.number.saturating_sub(1) as usize;
            (step_number < parsed_steps.len()).then_some(step_number)
        })
        .or_else(|| parsed_steps.len().checked_sub(1))
}

fn step_matches_name(parsed: &ParsedRunCommand, step_name: &str) -> bool {
    let step_name = step_name.trim();
    parsed.name == step_name
        || parsed.command == step_name
        || parsed.name.contains(step_name)
        || step_name.contains(&parsed.name)
}

fn log_tail_from(logs: &str, line_index: usize) -> String {
    const MAX_LINES: usize = 80;
    let lines: Vec<&str> = logs.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = line_index.min(lines.len() - 1);
    let end = (start + MAX_LINES).min(lines.len());
    lines[start..end].join("\n")
}

fn artifact_context(
    mut error: GitHubApiError,
    operation: &'static str,
    path: &str,
    run_id: Option<u64>,
) -> GitHubApiError {
    error.method = operation;
    error.path = path.to_owned();
    let run_context = run_id
        .map(|id| format!(" for run {id}"))
        .unwrap_or_default();
    let failure_context = if error.source == crate::github_api::GitHubErrorSource::Transport {
        " transport failure (including timeout)"
    } else {
        ""
    };
    error.body = format!(
        "GitHub Actions operation {operation}{run_context}{failure_context}: {}",
        error.body
    );
    error
}

async fn artifact_http(
    operation: &'static str,
    path: String,
    response: reqwest::Response,
    run_id: Option<u64>,
) -> GitHubApiError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let guidance = match status {
        StatusCode::FORBIDDEN => "GitHub Actions read permission is required",
        StatusCode::NOT_FOUND => "artifact or workflow run is missing or expired",
        _ => "GitHub Actions request failed",
    };
    let run_context = run_id
        .map(|id| format!(" for run {id}"))
        .unwrap_or_default();
    GitHubApiError::http(
        operation,
        path,
        status,
        format!("{guidance}{run_context}: {body}"),
    )
}

fn artifact_budget_error(owner: &str, repo: &str, run_id: u64, artifact_id: u64) -> GitHubApiError {
    GitHubApiError::transport(
        "download_artifact",
        format!("/repos/{owner}/{repo}/actions/artifacts/{artifact_id}/zip"),
        format!(
            "compressed artifact {artifact_id} for run {run_id} exceeds the 32 MiB download limit"
        ),
    )
}
