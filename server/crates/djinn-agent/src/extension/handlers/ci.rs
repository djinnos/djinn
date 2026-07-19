use super::*;
use djinn_provider::github_api::{ActionsJob, WorkflowRun};

/// How many workflow runs to request when scanning a PR head for the failing
/// run. The relevant run is always the newest one, so a small page is plenty;
/// the extra entries only cover the (rare) case where the newest run is still
/// in-progress and an older one already concluded failed.
const RUN_SCAN_PER_PAGE: u32 = 20;
/// The `merge_group` event needs a wider window: many PRs share the queue, and
/// the run for *this* PR may be several entries back. Mirrors the PR poller's
/// dequeue-enrichment page size (`pr_commands.rs`).
const MERGE_GROUP_SCAN_PER_PAGE: u32 = 50;

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
        return Ok(serde_json::Value::String(render_log(
            &raw_log,
            p.step.as_deref(),
        )));
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
        return Ok(serde_json::Value::String(render_log(&raw_log, Some(step))));
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

    Ok(serde_json::Value::String(output))
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
async fn resolve_installation_client_for_task(
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
mod tests {
    use super::*;
    use djinn_provider::github_api::{ActionsJob, ActionsJobStep, WorkflowRun};

    fn run(id: u64, conclusion: Option<&str>, head_branch: Option<&str>) -> WorkflowRun {
        WorkflowRun {
            id,
            workflow_id: None,
            name: None,
            path: None,
            head_branch: head_branch.map(str::to_string),
            head_sha: format!("sha-{id}"),
            status: Some("completed".to_string()),
            conclusion: conclusion.map(str::to_string),
        }
    }

    fn job(id: u64, name: &str, conclusion: Option<&str>) -> ActionsJob {
        ActionsJob {
            id,
            run_id: Some(1),
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: conclusion.map(str::to_string),
            html_url: format!("https://example.test/job/{id}"),
            workflow_name: None,
            steps: Vec::new(),
        }
    }

    fn step(name: &str, conclusion: Option<&str>) -> ActionsJobStep {
        ActionsJobStep {
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: conclusion.map(str::to_string),
            number: 1,
        }
    }

    // ── select_failing_run ────────────────────────────────────────────────
    #[test]
    fn select_failing_run_returns_newest_failure() {
        // Newest-first: a passing run in front must not shadow the failing one.
        let runs = vec![
            run(30, Some("success"), None),
            run(20, Some("failure"), None),
            run(10, Some("failure"), None),
        ];
        assert_eq!(select_failing_run(&runs).map(|r| r.id), Some(20));
    }

    #[test]
    fn select_failing_run_none_when_all_pass() {
        let runs = vec![run(2, Some("success"), None), run(1, None, None)];
        assert!(select_failing_run(&runs).is_none());
    }

    #[test]
    fn select_failing_run_includes_timed_out_and_cancelled() {
        assert_eq!(
            select_failing_run(&[run(5, Some("timed_out"), None)]).map(|r| r.id),
            Some(5)
        );
        assert_eq!(
            select_failing_run(&[run(6, Some("cancelled"), None)]).map(|r| r.id),
            Some(6)
        );
    }

    #[test]
    fn implicit_run_requires_a_failing_workflow_conclusion() {
        // A recorded merge-queue run can have a failed completed job while the
        // enclosing workflow is still running. It must not be selected until
        // GitHub reports a failure-flavor workflow conclusion.
        assert!(!is_implicit_failing_run(&run(1, None, None)));
        assert!(!is_implicit_failing_run(&run(2, Some("success"), None)));
        assert!(is_implicit_failing_run(&run(3, Some("failure"), None)));
        assert!(is_implicit_failing_run(&run(4, Some("timed_out"), None)));
        assert!(is_implicit_failing_run(&run(5, Some("cancelled"), None)));
    }

    // ── select_merge_group_run ────────────────────────────────────────────
    #[test]
    fn select_merge_group_run_matches_pr_marker() {
        let runs = vec![
            run(3, Some("failure"), Some("gh-readonly-queue/main/pr-99-abc")),
            run(2, Some("failure"), Some("gh-readonly-queue/main/pr-42-def")),
            run(1, Some("failure"), Some("gh-readonly-queue/main/pr-7-xyz")),
        ];
        assert_eq!(select_merge_group_run(&runs, 42).map(|r| r.id), Some(2));
    }

    #[test]
    fn select_merge_group_run_ignores_passing_and_foreign_prs() {
        let runs = vec![
            run(3, Some("success"), Some("gh-readonly-queue/main/pr-42-abc")),
            run(
                2,
                Some("failure"),
                Some("gh-readonly-queue/main/pr-100-def"),
            ),
        ];
        assert!(select_merge_group_run(&runs, 42).is_none());
    }

    #[test]
    fn select_merge_group_run_does_not_confuse_pr_prefixes() {
        // `pr-4-` must not match PR 42's `pr-42-` branch.
        let runs = vec![run(
            1,
            Some("failure"),
            Some("gh-readonly-queue/main/pr-42-abc"),
        )];
        assert!(select_merge_group_run(&runs, 4).is_none());
    }

    // ── select_failing_jobs ───────────────────────────────────────────────
    #[test]
    fn select_failing_jobs_filters_and_preserves_order() {
        let jobs = vec![
            job(1, "clippy", Some("success")),
            job(2, "tests", Some("failure")),
            job(3, "sqlx", Some("timed_out")),
            job(4, "fmt", Some("cancelled")),
            job(5, "docs", None),
        ];
        let selected = select_failing_jobs(&jobs);
        assert_eq!(
            selected.iter().map(|j| j.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn select_failing_jobs_empty_when_all_green() {
        let jobs = vec![job(1, "a", Some("success")), job(2, "b", None)];
        assert!(select_failing_jobs(&jobs).is_empty());
    }

    // ── select_job_for_step ───────────────────────────────────────────────
    #[test]
    fn select_job_for_step_unique_match() {
        let mut a = job(1, "quality", Some("failure"));
        a.steps = vec![
            step("Clippy", Some("success")),
            step("Run tests", Some("failure")),
        ];
        let mut b = job(2, "build", Some("failure"));
        b.steps = vec![step("compile", Some("failure"))];
        let jobs = vec![a, b];
        assert_eq!(select_job_for_step(&jobs, "tests").map(|j| j.id), Some(1));
    }

    #[test]
    fn select_job_for_step_ambiguous_returns_none() {
        let mut a = job(1, "a", Some("failure"));
        a.steps = vec![step("Run tests", Some("failure"))];
        let mut b = job(2, "b", Some("failure"));
        b.steps = vec![step("More tests", Some("failure"))];
        let jobs = vec![a, b];
        assert!(select_job_for_step(&jobs, "tests").is_none());
    }

    #[test]
    fn select_job_for_step_ignores_passing_steps() {
        let mut a = job(1, "a", Some("failure"));
        a.steps = vec![step("Tests", Some("success"))];
        let jobs = vec![a];
        assert!(select_job_for_step(&jobs, "tests").is_none());
    }

    // ── format_failing_jobs_header ─────────────────────────────────────────
    #[test]
    fn format_failing_jobs_header_lists_all_jobs() {
        let jobs = vec![
            job(11, "Server Clippy", Some("failure")),
            job(22, "Server Tests", Some("timed_out")),
        ];
        let header = format_failing_jobs_header(&jobs, DiscoveryLane::MergeQueue);
        assert!(header.contains("2 failing jobs"));
        assert!(header.contains("merge-queue"));
        assert!(header.contains("**Server Clippy**, job_id=11"));
        assert!(header.contains("- Server Clippy (job_id=11) — failure"));
        assert!(header.contains("- Server Tests (job_id=22) — timed_out"));
    }

    // ── clean_actions_log ─────────────────────────────────────────────────
    #[test]
    fn clean_actions_log_strips_timestamps_and_group_markers() {
        let raw = "2026-03-24T17:10:50.0448487Z ##[group]Run cargo test\n\
                   2026-03-24T17:10:51.0000000Z ##[error]boom\n\
                   2026-03-24T17:10:52.0000000Z ##[endgroup]\n\
                   2026-03-24T17:10:53.0000000Z plain line";
        let cleaned = clean_actions_log(raw);
        assert_eq!(cleaned, "Run cargo test\nboom\nplain line");
    }

    #[test]
    fn clean_actions_log_preserves_non_timestamped_lines() {
        let raw = "no timestamp here\n##[warning]watch out";
        assert_eq!(clean_actions_log(raw), "no timestamp here\nwatch out");
    }

    // ── extract_step_log ──────────────────────────────────────────────────
    #[test]
    fn extract_step_log_returns_section_until_boundary() {
        let cleaned = "Run cargo build\nbuilding...\nRun cargo test\ntest output\nFAILED\nPost Run actions/checkout\ncleanup";
        let section = extract_step_log(cleaned, "cargo test").expect("step found");
        assert!(section.contains("Run cargo test"));
        assert!(section.contains("test output"));
        assert!(section.contains("FAILED"));
        assert!(!section.contains("cleanup"));
        assert!(!section.contains("building..."));
    }

    #[test]
    fn extract_step_log_none_when_step_absent() {
        let cleaned = "Run cargo build\nbuilding...";
        assert!(extract_step_log(cleaned, "nonexistent step").is_none());
    }

    #[test]
    fn extract_step_log_runs_to_end_without_boundary() {
        let cleaned = "Run cargo test\nline1\nline2";
        let section = extract_step_log(cleaned, "cargo test").expect("found");
        assert_eq!(section, "Run cargo test\nline1\nline2");
    }

    // ── render_log ────────────────────────────────────────────────────────
    #[test]
    fn render_log_without_step_returns_full_clean() {
        let raw = "2026-03-24T17:10:50.0448487Z hello";
        assert_eq!(render_log(raw, None), "hello");
    }

    #[test]
    fn render_log_missing_step_falls_back_to_full_log() {
        let raw = "Run a\nout";
        let out = render_log(raw, Some("no such step"));
        assert!(out.contains("not found in the job log"));
        assert!(out.contains("Run a"));
    }

    // ── param deserialization ─────────────────────────────────────────────
    #[test]
    fn ci_job_log_params_all_optional() {
        let empty: CiJobLogParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(empty.job_id.is_none());
        assert!(empty.pr_number.is_none());
        assert!(empty.step.is_none());
    }

    #[test]
    fn ci_job_log_params_parses_all_fields() {
        let p: CiJobLogParams = serde_json::from_value(serde_json::json!({
            "job_id": 12345,
            "pr_number": 42,
            "step": "Tests"
        }))
        .unwrap();
        assert_eq!(p.job_id, Some(12345));
        assert_eq!(p.pr_number, Some(42));
        assert_eq!(p.step.as_deref(), Some("Tests"));
    }
}

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
