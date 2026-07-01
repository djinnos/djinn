use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::{
    ActionsJobsResponse, CheckRunsResponse, CiFailureContextBundle, CiFailureContextRequest,
    CiSetupStep, WorkflowRun, WorkflowRunsResponse,
};
use crate::github_api::{ActionsJob, CheckAnnotation, GitHubApiClient, GitHubApiError};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_yaml::Value as YamlValue;

const LOG_TAIL_LINES: usize = 200;
const LOG_TAIL_BYTES: usize = 24_000;

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

    jobs.iter()
        .filter(|job| is_failing_conclusion(job.conclusion.as_deref()))
        .find(|job| check_name_matches_job(&request.required_check_name, job))
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
