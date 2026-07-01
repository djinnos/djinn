use crate::github_api::transport::handle_rate_limit;
use crate::github_api::types::{
    ActionsJobsResponse, CheckRunsResponse, ReproductionJob, ReproductionSetupStep,
    ReproductionStep, RequiredCheckReproduction, RequiredCheckReproductionContext,
    RequiredCheckUnreproducible, RequiredCheckUnreproducibleReason, WorkflowRun,
    WorkflowRunsResponse,
};
use crate::github_api::{
    ActionsJob, ActionsJobStep, CheckAnnotation, CheckRun, GitHubApiClient, GitHubApiError,
};

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
        let Some(job) = jobs
            .iter()
            .find(|job| job_matches_check(job, check_run))
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
