use super::*;
use djinn_provider::github_api::{ActionsJob, WorkflowRun};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// How many workflow runs to request when scanning a PR head for the failing
/// run. The relevant run is always the newest one, so a small page is plenty;
/// the extra entries only cover the (rare) case where the newest run is still
/// in-progress and an older one already concluded failed.
const RUN_SCAN_PER_PAGE: u32 = 20;
/// The `merge_group` event needs a wider window: many PRs share the queue, and
/// the run for *this* PR may be several entries back. Mirrors the PR poller's
/// dequeue-enrichment page size (`pr_commands.rs`).
const MERGE_GROUP_SCAN_PER_PAGE: u32 = 50;
/// Artifact discovery is deliberately subordinate to a successful log fetch.
const ARTIFACT_HINT_TIMEOUT: Duration = Duration::from_secs(2);

/// Fetch the failing GitHub Actions CI log for a task's PR.
///
/// Discovery is snapshot/project-driven — owner/repo come from the task's
/// project, and the failing jobs are discovered from the task's recorded CI
/// state (PR-head lane, then the merge-queue lane) plus a live GitHub read. The
/// legacy activity-scan path (which broke for planner escalation tasks and went
/// stale on every push) has been removed.
///
/// Resolution:
///   1. `job_id` given → fetch that job's log directly (repo-scoped API, so a
///      foreign job id simply 404s — no activity-based authorization needed).
///   2. no `job_id` → discover the currently-failing jobs for the target PR
///      (`pr_number` param, else `task.ci_pr_number`) across two lanes:
///        - PR-head lane: newest failed workflow run for the PR head SHA.
///        - merge-queue lane: `task.ci_mq_run_id`, else the newest failed
///          `merge_group` run whose branch carries this PR's marker.
///
/// The raw log is cleaned (timestamps stripped, group markers removed) and
/// returned as a plain string. When the result exceeds the tool-result size
/// limit, the reply-loop automatically stashes the full output and the worker
/// can paginate with `output_view` / `output_grep`.
pub(crate) async fn call_ci_job_log(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: CiJobLogParams = parse_args(arguments)?;

    let task_id = session_task_id.ok_or("ci_job_log requires a task context (session_task_id)")?;

    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let project_repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());

    let task = task_repo
        .get(task_id)
        .await
        .map_err(|e| format!("failed to load task {task_id}: {e}"))?
        .ok_or_else(|| format!("ci_job_log: task {task_id} not found"))?;

    // Owner/repo come from the task's project — durable and correct even for
    // planner escalation tasks whose CI metadata lives on a *source* task.
    let (owner, repo) = project_repo
        .get_github_coords(&task.project_id)
        .await
        .map_err(|e| format!("failed to resolve project GitHub coordinates: {e}"))?
        .ok_or_else(|| {
            format!(
                "ci_job_log: project {} has no GitHub owner/repo coordinates recorded. \
                 Re-register the project through the GitHub App flow.",
                task.project_id
            )
        })?;

    let gh_client = match resolve_installation_client_for_task(
        &project_repo,
        &task.project_id,
        &owner,
        &repo,
    )
    .await
    {
        Some(c) => c,
        None => {
            return Err(format!(
                "ci_job_log: no GitHub App installation found for task {task_id} (owner={owner}, repo={repo}). \
                     Re-register the project through the GitHub App flow to enable background log fetches."
            ));
        }
    };

    // ── Escape hatch: explicit job id ──────────────────────────────────────
    // The GitHub logs endpoint is repo-scoped, so a foreign job id 404s here;
    // no activity-based authorization is needed.
    if let Some(job_id) = p.job_id {
        let raw_log = gh_client
            .get_job_logs(&owner, &repo, job_id)
            .await
            .map_err(|e| format!("failed to fetch job log for job_id={job_id}: {e}"))?;
        let output = render_log(&raw_log, p.step.as_deref());
        return Ok(serde_json::Value::String(
            append_artifact_hint(
                &gh_client,
                &owner,
                &repo,
                task_id,
                Some(job_id),
                None,
                output,
            )
            .await,
        ));
    }

    // ── Discovery mode ─────────────────────────────────────────────────────
    let pr_number = p
        .pr_number
        .or_else(|| task.ci_pr_number.and_then(|n| u64::try_from(n).ok()))
        .ok_or(
            "ci_job_log: this task has no recorded PR number to discover CI from. \
             Pass `pr_number` naming the PR whose failing CI you want to read.",
        )?;

    let resolved = resolve_workflow_run(
        &gh_client,
        &owner,
        &repo,
        WorkflowRunResolutionRequest {
            explicit_run_id: None,
            pr_number: Some(pr_number),
            recorded_head_sha: (task.ci_pr_number == Some(pr_number as i64))
                .then(|| task.ci_head_sha.clone())
                .flatten(),
            recorded_merge_queue_run_id: task.ci_mq_run_id.and_then(|id| u64::try_from(id).ok()),
        },
    )
    .await
    .map_err(|error| format!("ci_job_log: {error}"))?;
    let jobs = gh_client
        .list_run_jobs(&owner, &repo, resolved.run_id)
        .await
        .map_err(|e| format!("failed to list jobs for run {}: {e}", resolved.run_id))?;
    let failing_jobs: Vec<ActionsJob> = select_failing_jobs(&jobs).into_iter().cloned().collect();
    debug_assert!(!failing_jobs.is_empty());
    let lane = match resolved.lane {
        WorkflowRunLane::PrHead => DiscoveryLane::PrHead,
        WorkflowRunLane::RecordedMergeQueue | WorkflowRunLane::LiveMergeGroup => {
            DiscoveryLane::MergeQueue
        }
        WorkflowRunLane::Explicit => DiscoveryLane::None,
    };

    // If a step was given and it uniquely identifies one of the failing jobs,
    // treat it as an unambiguous single-job fetch.
    if let Some(step) = p.step.as_deref()
        && let Some(job) = select_job_for_step(&failing_jobs, step)
    {
        let raw_log = gh_client
            .get_job_logs(&owner, &repo, job.id)
            .await
            .map_err(|e| format!("failed to fetch job log for job_id={}: {e}", job.id))?;
        let output = render_log(&raw_log, Some(step));
        return Ok(serde_json::Value::String(
            append_artifact_hint(
                &gh_client,
                &owner,
                &repo,
                task_id,
                Some(job.id),
                Some(resolved.run_id),
                output,
            )
            .await,
        ));
    }

    // Fetch the FIRST failing job's log. When several jobs failed, prepend a
    // header enumerating all of them so the agent can request the others by id.
    let first = &failing_jobs[0];
    let raw_log = gh_client
        .get_job_logs(&owner, &repo, first.id)
        .await
        .map_err(|e| format!("failed to fetch job log for job_id={}: {e}", first.id))?;
    let body = render_log(&raw_log, p.step.as_deref());

    let output = if failing_jobs.len() > 1 {
        format!(
            "{}\n\n{}",
            format_failing_jobs_header(&failing_jobs, lane),
            body
        )
    } else {
        body
    };

    Ok(serde_json::Value::String(
        append_artifact_hint(
            &gh_client,
            &owner,
            &repo,
            task_id,
            Some(first.id),
            Some(resolved.run_id),
            output,
        )
        .await,
    ))
}

/// A suppressed hint failure retains the concrete run once direct-job lookup
/// has resolved it, so operational telemetry remains actionable.
struct ArtifactHintFailure {
    run_id: Option<u64>,
    error: String,
}

/// Carries a direct-job run ID out of the timed hint future. The outer timeout
/// cancels that future while the artifact list request is pending, so a shared
/// context is necessary to retain a run ID obtained before that request.
#[derive(Clone)]
struct ArtifactHintRunContext(Arc<Mutex<Option<u64>>>);

impl ArtifactHintRunContext {
    fn new(run_id: Option<u64>) -> Self {
        Self(Arc::new(Mutex::new(run_id)))
    }

    fn run_id(&self) -> Option<u64> {
        *self
            .0
            .lock()
            .expect("artifact hint run context lock poisoned")
    }

    fn set_run_id(&self, run_id: u64) {
        *self
            .0
            .lock()
            .expect("artifact hint run context lock poisoned") = Some(run_id);
    }
}

/// Append a bounded, read-only artifact hint without changing successful log
/// semantics. Discovery already owns a verified run ID; direct-job mode obtains
/// it from GitHub's repository-scoped job detail endpoint.
async fn append_artifact_hint(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    task_id: &str,
    job_id: Option<u64>,
    known_run_id: Option<u64>,
    output: String,
) -> String {
    let run_context = ArtifactHintRunContext::new(known_run_id);
    match tokio::time::timeout(
        ARTIFACT_HINT_TIMEOUT,
        artifact_hint(
            client,
            owner,
            repo,
            job_id,
            known_run_id,
            run_context.clone(),
        ),
    )
    .await
    {
        Ok(Ok(Some(hint))) => format!("{output}\n\n{hint}"),
        Ok(Ok(None)) => output,
        Ok(Err(failure)) => {
            tracing::warn!(
                operation = "ci_job_log_artifact_hint",
                outcome = "suppressed_provider_error",
                task_id,
                job_id = ?job_id,
                run_id = ?failure.run_id.or(run_context.run_id()),
                error = %failure.error,
                "ci_job_log artifact hint lookup failed; returning successful log unchanged"
            );
            output
        }
        Err(_) => {
            tracing::warn!(
                operation = "ci_job_log_artifact_hint",
                outcome = "suppressed_timeout",
                task_id,
                job_id = ?job_id,
                run_id = ?run_context.run_id(),
                timeout_ms = ARTIFACT_HINT_TIMEOUT.as_millis(),
                "ci_job_log artifact hint lookup timed out; returning successful log unchanged"
            );
            output
        }
    }
}

/// Make at most one bounded list request. This intentionally never downloads
/// an artifact: the public `ci_artifact` tool remains responsible for fetches.
async fn artifact_hint(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    job_id: Option<u64>,
    known_run_id: Option<u64>,
    run_context: ArtifactHintRunContext,
) -> Result<Option<String>, ArtifactHintFailure> {
    let run_id = match known_run_id {
        Some(run_id) => run_id,
        None => {
            let job_id = job_id.ok_or_else(|| ArtifactHintFailure {
                run_id: None,
                error: "no job or workflow run was available for artifact hint".to_string(),
            })?;
            client
                .get_job(owner, repo, job_id)
                .await
                .map_err(|error| ArtifactHintFailure {
                    run_id: None,
                    error: format!("failed to fetch job detail for job_id={job_id}: {error}"),
                })?
                .run_id
                .ok_or_else(|| ArtifactHintFailure {
                    run_id: None,
                    error: format!(
                        "job detail for job_id={job_id} did not include a workflow run ID"
                    ),
                })?
        }
    };
    // Preserve the resolved direct-job run before the list await. If the outer
    // hint deadline cancels this future while listing, telemetry still has it.
    run_context.set_run_id(run_id);
    let artifacts = client
        .list_run_artifacts(owner, repo, run_id)
        .await
        .map_err(|error| ArtifactHintFailure {
            run_id: Some(run_id),
            error: format!("failed to list artifacts for run {run_id}: {error}"),
        })?;
    if artifacts.artifacts.is_empty() {
        return Ok(None);
    }

    // Provider order is GitHub's order. Keep it intact so the user can copy an
    // exact name into the concrete fetch example below.
    let names = artifacts
        .artifacts
        .iter()
        .map(|artifact| format!("`{}`", artifact.name))
        .collect::<Vec<_>>()
        .join(", ");
    let first_name = &artifacts.artifacts[0].name;
    Ok(Some(format!(
        "Workflow run {run_id} has artifacts: {names}. List them with `ci_artifact(action=\"list\", run_id={run_id})`. Fetch an exact artifact name with `ci_artifact(action=\"fetch\", run_id={run_id}, artifact=\"{first_name}\")`."
    )))
}

/// An implicitly discovered run must itself have completed with a failing
/// conclusion. A failed job alone is not sufficient: GitHub can expose a
/// completed failed job while its enclosing workflow remains in progress.
fn is_implicit_failing_run(run: &WorkflowRun) -> bool {
    is_failing_conclusion(run.conclusion.as_deref())
}

/// Which lane discovery resolved the failing jobs from — surfaced in the
/// multi-job header so the agent knows whether it is looking at the PR head or
/// the merge-queue run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryLane {
    None,
    PrHead,
    MergeQueue,
}

impl DiscoveryLane {
    fn label(self) -> &'static str {
        match self {
            DiscoveryLane::None => "unknown",
            DiscoveryLane::PrHead => "PR-head",
            DiscoveryLane::MergeQueue => "merge-queue",
        }
    }
}

/// A concrete repository-scoped workflow run retained by CI discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedWorkflowRun {
    pub(crate) run_id: u64,
    pub(crate) lane: WorkflowRunLane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowRunLane {
    Explicit,
    PrHead,
    RecordedMergeQueue,
    LiveMergeGroup,
}

impl WorkflowRunLane {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::PrHead => "pr_head",
            Self::RecordedMergeQueue => "recorded_merge_queue",
            Self::LiveMergeGroup => "merge_group",
        }
    }
}

/// True when a workflow-run / job / step conclusion is a failure flavor the
/// worker must act on. GitHub reports `failure`, `timed_out`, and `cancelled`
/// as distinct conclusions; all three keep a required gate red.
fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("failure") | Some("timed_out") | Some("cancelled")
    )
}

/// Select the newest failed workflow run from a newest-first list.
#[cfg(test)]
fn select_failing_run(runs: &[WorkflowRun]) -> Option<&WorkflowRun> {
    runs.iter()
        .find(|r| is_failing_conclusion(r.conclusion.as_deref()))
}

/// Select the newest failed `merge_group` run belonging to a specific PR.
///
/// Runs arrive newest-first. The merge-queue branch is ephemeral, but the run
/// persists with `head_branch = gh-readonly-queue/.../pr-<number>-<sha>`, so we
/// match on the `pr-<number>-` marker. Mirrors the PR poller's dequeue
/// enrichment in `pr_commands.rs` (which matches conclusion `"failure"` only;
/// here we accept the broader failure set for robustness).
#[cfg(test)]
fn select_merge_group_run(runs: &[WorkflowRun], pr_number: u64) -> Option<&WorkflowRun> {
    let marker = format!("pr-{pr_number}-");
    runs.iter().find(|r| {
        is_failing_conclusion(r.conclusion.as_deref())
            && r.head_branch
                .as_deref()
                .is_some_and(|b| b.contains(&marker))
    })
}

/// Select the failing jobs of a workflow run, preserving input order.
fn select_failing_jobs(jobs: &[ActionsJob]) -> Vec<&ActionsJob> {
    jobs.iter()
        .filter(|j| is_failing_conclusion(j.conclusion.as_deref()))
        .collect()
}

/// When a `step` filter is given, narrow the failing jobs to those that contain
/// a failing step whose name matches (case-insensitive substring). Returns
/// `Some(job)` only when exactly one job matches — an unambiguous single-job
/// fetch — so an ambiguous `step` falls through to the multi-job header path.
fn select_job_for_step<'a>(jobs: &'a [ActionsJob], step: &str) -> Option<&'a ActionsJob> {
    let needle = step.to_lowercase();
    let mut matched: Option<&ActionsJob> = None;
    for job in jobs {
        let hit = job.steps.iter().any(|s| {
            s.name.to_lowercase().contains(&needle)
                && is_failing_conclusion(s.conclusion.as_deref())
        });
        if hit {
            if matched.is_some() {
                return None; // ambiguous
            }
            matched = Some(job);
        }
    }
    matched
}

/// Header prepended to the first failing job's log when several jobs failed, so
/// the agent can request the others by `job_id`.
fn format_failing_jobs_header(jobs: &[ActionsJob], lane: DiscoveryLane) -> String {
    let mut lines = vec![format!(
        "{} failing jobs on the {} lane for this PR. Showing the log for the first (**{}**, job_id={}). \
         Request another with `ci_job_log(job_id=…)`:",
        jobs.len(),
        lane.label(),
        jobs.first().map(|j| j.name.as_str()).unwrap_or("unknown"),
        jobs.first().map(|j| j.id).unwrap_or(0),
    )];
    for job in jobs {
        let conclusion = job.conclusion.as_deref().unwrap_or("unknown");
        lines.push(format!(
            "- {} (job_id={}) — {}",
            job.name, job.id, conclusion
        ));
    }
    lines.join("\n")
}

/// Clean a raw job log and optionally narrow it to a single step.
fn render_log(raw_log: &str, step: Option<&str>) -> String {
    let cleaned = clean_actions_log(raw_log);
    match step {
        Some(step_name) => extract_step_log(&cleaned, step_name).unwrap_or_else(|| {
            format!(
                "Step '{step_name}' not found in the job log. Returning full cleaned log.\n\n{cleaned}"
            )
        }),
        None => cleaned,
    }
}

/// Build an installation-scoped GitHub API client for a `ci_job_log` call.
///
/// `ci_job_log` runs in background (worker) scope, so it cannot read the
/// session-user task-local. Resolution order:
///   1. The task's `project_id` → `projects.installation_id`.
///   2. Fallback: look up a project by the project-derived `owner/repo` pair
///      and read its `installation_id` (covers legacy rows where the task's
///      `project_id` has a NULL `installation_id` but a sibling row for the
///      same repo carries one).
///
/// Returns `None` if neither path yields an installation.
pub(crate) async fn resolve_installation_client_for_task(
    project_repo: &djinn_db::ProjectRepository,
    project_id: &str,
    owner: &str,
    repo: &str,
) -> Option<GitHubApiClient> {
    if let Ok(Some(id)) = project_repo.get_installation_id(project_id).await {
        return Some(GitHubApiClient::for_installation(id));
    }

    if let Ok(Some(project)) = project_repo.get_by_github(owner, repo).await
        && let Ok(Some(id)) = project_repo.get_installation_id(&project.id).await
    {
        return Some(GitHubApiClient::for_installation(id));
    }

    None
}

/// Strip GitHub Actions noise from a raw job log.
///
/// Removes ISO-8601 timestamp prefixes and `##[group]`/`##[endgroup]`
/// markers while preserving `##[error]` and `##[warning]` content.
fn clean_actions_log(raw_log: &str) -> String {
    raw_log
        .lines()
        .map(|line| {
            // Strip leading ISO-8601 timestamp prefix (29 chars like "2026-03-24T17:10:50.0448487Z ")
            line.get(..29)
                .filter(|prefix| {
                    prefix.len() >= 20
                        && prefix.as_bytes().first() == Some(&b'2')
                        && prefix.contains('T')
                        && prefix.ends_with(' ')
                })
                .map(|_| &line[29..])
                .unwrap_or(line)
        })
        .filter(|line| !line.starts_with("##[endgroup]"))
        .map(|line| {
            line.strip_prefix("##[group]")
                .or_else(|| line.strip_prefix("##[error]"))
                .or_else(|| line.strip_prefix("##[warning]"))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the log section for a specific step name.
///
/// GitHub Actions logs use `##[group]Run <step>` / `##[endgroup]` to delimit
/// steps. After cleaning (which strips `##[group]` prefixes), the step
/// boundaries become plain text lines starting with `Run ...` or the step
/// name itself. We look for the step name in these boundary lines and return
/// everything between the start and the next boundary (or end of log).
fn extract_step_log(cleaned_log: &str, step_name: &str) -> Option<String> {
    let lines: Vec<&str> = cleaned_log.lines().collect();
    let step_lower = step_name.to_lowercase();

    // Find the start of the target step section.
    // After cleaning, step headers look like:
    //   "Run cd server && cargo test ..." or just the step name
    // We search for lines that contain the step name (case-insensitive).
    let mut start_idx = None;
    let mut end_idx = lines.len();

    // Track step boundaries — lines that look like GitHub Actions step headers.
    // These typically start with "Run " after group marker removal, or match
    // known step patterns. We use a heuristic: if a line exactly matches one
    // of the step names from the job, it's a boundary.
    //
    // Simpler approach: scan for the step name, then collect until the next
    // recognizable boundary or end of log.
    for (i, line) in lines.iter().enumerate() {
        if line.to_lowercase().contains(&step_lower) && start_idx.is_none() {
            start_idx = Some(i);
        }
    }

    let start = start_idx?;

    // Look for the next step boundary after our start.
    // Step boundaries in cleaned logs are hard to detect generically.
    // Use a practical heuristic: "Post Run " lines mark cleanup steps,
    // and "Complete job" marks the end.
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if trimmed.starts_with("Post Run ") || trimmed == "Complete job" {
            end_idx = i;
            break;
        }
    }

    let section: Vec<&str> = lines[start..end_idx].to_vec();
    if section.is_empty() {
        None
    } else {
        Some(section.join("\n"))
    }
}

#[cfg(test)]
#[path = "ci_tests.rs"]
mod tests;

/// Inputs captured from a task before resolving an artifact or CI-log run.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowRunResolutionRequest {
    pub(crate) explicit_run_id: Option<u64>,
    pub(crate) pr_number: Option<u64>,
    pub(crate) recorded_head_sha: Option<String>,
    pub(crate) recorded_merge_queue_run_id: Option<u64>,
}

/// Resolve a repository-scoped run. Passing and in-progress runs are never
/// selected implicitly; an explicit run is repository-verified and may have any conclusion.
pub(crate) async fn resolve_workflow_run(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    request: WorkflowRunResolutionRequest,
) -> Result<ResolvedWorkflowRun, String> {
    if let Some(run_id) = request.explicit_run_id {
        if run_id == 0 {
            return Err("explicit workflow run ID must be positive".to_string());
        }
        client
            .get_workflow_run(owner, repo, run_id)
            .await
            .map_err(|e| {
                format!("explicit run {run_id} is not accessible in repository {owner}/{repo}: {e}")
            })?;
        return Ok(ResolvedWorkflowRun {
            run_id,
            lane: WorkflowRunLane::Explicit,
        });
    }
    let pr = request
        .pr_number
        .ok_or("no explicit workflow run ID or PR number was available to resolve CI artifacts")?;
    let sha = match request.recorded_head_sha.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            client
                .get_pull_request(owner, repo, pr)
                .await
                .map_err(|e| format!("failed to fetch PR #{pr}: {e}"))?
                .0
                .head
                .sha
        }
    };
    let runs = client
        .list_workflow_runs_for_head_sha(owner, repo, &sha, RUN_SCAN_PER_PAGE)
        .await
        .map_err(|e| format!("failed to list workflow runs for head {sha}: {e}"))?;
    for run in runs.iter().filter(|run| is_implicit_failing_run(run)) {
        if run_has_failing_job(client, owner, repo, run.id).await? {
            return Ok(ResolvedWorkflowRun {
                run_id: run.id,
                lane: WorkflowRunLane::PrHead,
            });
        }
    }
    if let Some(id) = request.recorded_merge_queue_run_id {
        let run = client
            .get_workflow_run(owner, repo, id)
            .await
            .map_err(|e| format!("failed to verify recorded merge-queue run {id}: {e}"))?;
        if is_implicit_failing_run(&run) && run_has_failing_job(client, owner, repo, id).await? {
            return Ok(ResolvedWorkflowRun {
                run_id: id,
                lane: WorkflowRunLane::RecordedMergeQueue,
            });
        }
    }
    let runs = client
        .list_workflow_runs_for_event(owner, repo, "merge_group", MERGE_GROUP_SCAN_PER_PAGE)
        .await
        .map_err(|e| format!("failed to list merge_group runs: {e}"))?;
    let marker = format!("pr-{pr}-");
    for run in runs.iter().filter(|run| {
        is_implicit_failing_run(run)
            && run
                .head_branch
                .as_deref()
                .is_some_and(|branch| branch.contains(&marker))
    }) {
        if run_has_failing_job(client, owner, repo, run.id).await? {
            return Ok(ResolvedWorkflowRun {
                run_id: run.id,
                lane: WorkflowRunLane::LiveMergeGroup,
            });
        }
    }
    Err(format!(
        "no failing workflow run with failing jobs found for PR #{pr}; CI may be passing or still in progress"
    ))
}
async fn run_has_failing_job(
    client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    run_id: u64,
) -> Result<bool, String> {
    let jobs = client
        .list_run_jobs(owner, repo, run_id)
        .await
        .map_err(|e| format!("failed to list jobs for run {run_id}: {e}"))?;
    Ok(!select_failing_jobs(&jobs).is_empty())
}
