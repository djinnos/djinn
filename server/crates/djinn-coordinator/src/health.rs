// djinn:allow-oversize — reconciliation sweep + health module over size-guard threshold; split when touched substantively.
use super::*;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use djinn_core::clock::{Clock, SystemClock};
use djinn_provider::github_api::GitHubApiClient;
use regex::Regex;

/// How long a `task_run` may stay in `running` without an `ended_at` before
/// the periodic sweep flips it to `interrupted`. Must comfortably exceed the
/// K8s Job `activeDeadlineSeconds` (10800s) + `terminationGracePeriodSeconds`
/// (60s) + `ttlSecondsAfterFinished` (300s) so we never reap a still-live run
/// whose pod is mid-termination. Bumped to 4h alongside the deadline raise
/// (3600→10800): a 3h-budget run that legitimately uses most of its window
/// must not be reaped out from under itself.
const STALE_TASK_RUN_THRESHOLD_SECS: i64 = 4 * 60 * 60;

/// Startup-only threshold. Any `task_run` whose `started_at` is older than
/// this at process boot is from a previous host instance — the worker Pod
/// has lost its RPC channel and can't flush a terminal status. Tighter than
/// the periodic threshold because there's no live-run ambiguity at cold
/// start (we know our previous self is gone).
const STARTUP_TASK_RUN_THRESHOLD_SECS: i64 = 10;

/// How long a `task_attempts` row may stay `pending` — with no live
/// (`starting`/`running`) `task_run` and no `running` session for its task —
/// before the orphaned-attempt reaper finalizes it to `crashed`.
///
/// Conservative but responsive: dispatch-start → session/run creation is
/// seconds, and even across a server rolling restart the worker's connect
/// budget (`CONNECT_BACKOFF_MS` in djinn-supervisor, ~90s) means a
/// legitimately-starting attempt registers a live `task_run`/session well
/// within 2 minutes. The reaper is already gated on there being no live
/// (`starting`/`running`) `task_run` and no `running` session for the task,
/// so a 5-minute-old `pending` attempt with nothing executing behind it can
/// never be advanced by the normal lifecycle paths. Left un-reaped it
/// hard-blocks the respawn guard for its (task, role) pair (the guard defers
/// any dispatch while a `pending`/`submitted` attempt exists), so a 5-minute
/// threshold cuts the worst-case post-deploy dispatch wedge from 15 min to 5
/// while staying safely clear of any real starting attempt.
const ORPHANED_PENDING_ATTEMPT_THRESHOLD_SECS: i64 = 5 * 60;

const CARGO_TARGET_RUNS_ROOT: &str = djinn_supervisor::CARGO_TARGET_RUNS_ROOT;

/// Default durable output-stash retention window for coordinator maintenance.
///
/// Terminal-session stash pointers are eligible for GC after 30 days by
/// default. Operators may override this with
/// `DJINN_OUTPUT_STASH_GC_RETENTION_DAYS` (whole days, minimum 1).
///
/// rdx6 did not change the durable stash wire format: turn-budget
/// externalization is still transcript text pointing at the existing
/// `tool_use_id` stash entry. Coordinator GC therefore continues to classify and
/// reap the same v1/legacy pointer records without agent-helper mirroring.
const OUTPUT_STASH_GC_DEFAULT_RETENTION_DAYS: u64 = 30;
const OUTPUT_STASH_GC_RETENTION_ENV: &str = "DJINN_OUTPUT_STASH_GC_RETENTION_DAYS";

/// Minimum elapsed time (in seconds) after a task is closed before a still-
/// `running` session on that task is considered an orphan. The grace period
/// avoids racing with inline session teardown that fires during the task-close
/// transition. Five minutes comfortably exceeds the normal close → interrupt
/// round-trip.
const ORPHAN_SESSION_GRACE_SECS: i64 = 5 * 60;

// ─── Stale-resource sweep ────────────────────────────────────────────────────

pub(super) async fn sweep_stale_resources(
    db: &djinn_db::Database,
    app_state: &crate::context::CoordinatorContext,
) {
    reap_stale_task_runs(db).await;
    reap_orphaned_pending_attempts(db).await;
    reap_orphaned_taskrun_jobs(db, app_state, "periodic").await;
    sweep_orphan_worker_sessions(db).await;
    sweep_orphaned_cargo_target_run_dirs(
        db,
        app_state.cargo_target_runs_root.as_deref(),
        &app_state.cache_cleanup,
    )
    .await;
    sweep_durable_output_stash(db).await;
    sweep_cargo_health().await;
    sweep_sccache_guard(&app_state.cache_cleanup).await;
    sweep_cargo_warm_base_guard(
        db,
        &app_state.cache_cleanup,
        app_state.warm_job_guard.clone(),
    )
    .await;

    let project_repo = ProjectRepository::new(db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(db.clone(), app_state.event_bus.clone());
    let session_repo = djinn_db::SessionRepository::new(db.clone(), app_state.event_bus.clone());

    let projects = match project_repo.list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error=%e, "CoordinatorActor: stale sweep failed to list projects");
            return;
        }
    };

    // Task #8: worktree GC removed — the supervisor-driven dispatch path no
    // longer creates `.djinn/worktrees/<short_id>` directories, and the
    // session record's `worktree_path` column will be dropped in task #13.
    // We still walk per-project local branches to prune `task/<short_id>`
    // refs for closed, Djinn-authored tasks.
    let _ = session_repo;
    for project in projects {
        let project_dir =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);

        if let Ok(git) = app_state.git_actor(&project_dir).await
            && let Ok(out) = git
                .run_command(vec!["branch".into(), "--format=%(refname:short)".into()])
                .await
        {
            for line in out.stdout.lines() {
                let Some(short_id) = line.strip_prefix("task/") else {
                    continue;
                };
                let should_delete = match task_repo.get_by_short_id(short_id).await {
                    // Only delete branches for closed tasks that Djinn created a PR for.
                    // Branches for tasks without a pr_url were not managed by Djinn
                    // and must not be touched.
                    Ok(Some(task)) => task.status == "closed" && task.pr_url.is_some(),
                    // Unknown task — do NOT delete; the branch may belong to
                    // another project or have been created outside Djinn.
                    Ok(None) => false,
                    Err(_) => false,
                };
                if should_delete {
                    tracing::info!(project_id=%project.id, branch=%line, "CoordinatorActor: deleting stale task branch");
                    let _ = git.delete_branch(line).await;
                }
            }
        }

        // Mirror-side task-ref prune. `fetch_mirror` excludes
        // `refs/heads/task/*` from its `--prune` (those are djinn-owned
        // durability refs the worker pods push — see the negative refspec in
        // `MirrorManager::fetch_mirror`), so without this sweep the bare
        // mirror's ref set grows one branch per task forever. Same deletion
        // policy as the project-clone prune above: closed + Djinn-opened PR.
        if let Some(mirror) = app_state.mirror.as_ref() {
            let mirror_dir = mirror.mirror_path(&project.id);
            if let Ok(git) = app_state.git_actor(&mirror_dir).await
                && let Ok(out) = git
                    .run_command(vec!["branch".into(), "--format=%(refname:short)".into()])
                    .await
            {
                for line in out.stdout.lines() {
                    let Some(short_id) = line.strip_prefix("task/") else {
                        continue;
                    };
                    let should_delete = matches!(
                        task_repo.get_by_short_id(short_id).await,
                        Ok(Some(task)) if task.status == "closed" && task.pr_url.is_some()
                    );
                    if should_delete {
                        tracing::info!(
                            project_id=%project.id,
                            branch=%line,
                            "CoordinatorActor: deleting closed task branch from mirror"
                        );
                        let _ = mirror.delete_branch(&project.id, line).await;
                    }
                }
            }
        }
    }

    // Remote PR/branch reconciliation sweep — enumerate open bot-authored PRs
    // on task/* and chore/* branches whose backing task is closed, and reap
    // them (close PR + delete branch) with full guardrails and audit trail.
    sweep_stale_prs(db, app_state).await;
}

// ─── Stale-PR / branch reconciliation sweep ─────────────────────────────

/// Branch prefixes the sweep considers for stale-PR cleanup.
const STALE_PR_BRANCH_PREFIXES: &[&str] = &["task/", "chore/"];

/// Extract the task `short_id` from a branch name like `task/abc123` or
/// `chore/xyz`. Returns `None` if the branch doesn't match a known prefix.
fn extract_short_id_from_branch(branch: &str) -> Option<&str> {
    for prefix in STALE_PR_BRANCH_PREFIXES {
        if let Some(short_id) = branch.strip_prefix(prefix) {
            // Only return non-empty short_ids.
            if !short_id.is_empty() {
                return Some(short_id);
            }
        }
    }
    None
}

/// PR/branch reconciliation sweep stats for structured logging.
#[derive(Debug, Default)]
struct StalePrSweepStats {
    projects_scanned: usize,
    prs_scanned: usize,
    prs_reaped: usize,
    branches_reaped: usize,
    prs_skipped: usize,
    errors: usize,
    dry_run: bool,
}

/// Enumerate open bot-authored PRs on `task/*` and `chore/*` branches across
/// all projects, look up their backing tasks, and reap stale ones (close PR +
/// delete branch) with full guardrails and audit trail.
///
/// Called from [`sweep_stale_resources`] on every periodic tick after the
/// local branch prune.
async fn sweep_stale_prs(db: &djinn_db::Database, app_state: &crate::context::CoordinatorContext) {
    let sweep_config = &app_state.reconciliation_sweep;

    if !sweep_config.enabled {
        tracing::debug!("CoordinatorActor: reconciliation sweep disabled; skipping stale PR sweep");
        return;
    }

    let project_repo = ProjectRepository::new(db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(db.clone(), app_state.event_bus.clone());

    let projects = match project_repo.list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "CoordinatorActor: stale PR sweep failed to list projects");
            return;
        }
    };

    let mut stats = StalePrSweepStats {
        dry_run: sweep_config.dry_run,
        ..StalePrSweepStats::default()
    };

    for project in &projects {
        stats.projects_scanned += 1;

        // Resolve the GitHub App installation for this project.
        let installation_id = match project_repo.get_installation_id(&project.id).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::debug!(
                    project_id = %project.id,
                    slug = %project.slug(),
                    "CoordinatorActor: stale PR sweep skipping project with no installation_id"
                );
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_NO_INSTALLATION,
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_id = %project.id,
                    "CoordinatorActor: stale PR sweep failed to read installation_id"
                );
                stats.errors += 1;
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_API_ERROR,
                );
                continue;
            }
        };

        let github = GitHubApiClient::for_installation(installation_id);

        // Fetch all open PRs for this repository.
        let open_prs = match github
            .list_open_pulls(&project.github_owner, &project.github_repo)
            .await
        {
            Ok(prs) => prs,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_id = %project.id,
                    slug = %project.slug(),
                    "CoordinatorActor: stale PR sweep failed to list open PRs"
                );
                stats.errors += 1;
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_API_ERROR,
                );
                continue;
            }
        };

        // Build the PrCleanupPolicy for guardrail checks, using the
        // reconciliation sweep config for enabled/dry_run/grace_period.
        let cleanup_config = super::pr_poller::pr_cleanup::PrCleanupPolicyConfig {
            enabled: sweep_config.enabled,
            dry_run: sweep_config.dry_run,
            grace_period: sweep_config.grace_period,
            owner: project.github_owner.clone(),
            repo: project.github_repo.clone(),
            ..Default::default()
        };
        let cleanup_policy =
            super::pr_poller::pr_cleanup::PrCleanupPolicy::new(github.clone(), cleanup_config);

        // Filter to bot-authored PRs on task/* or chore/* branches.
        for pr in &open_prs {
            let head_branch = &pr.head.ref_name;

            // Only consider task/* and chore/* branches.
            let Some(short_id) = extract_short_id_from_branch(head_branch) else {
                continue;
            };
            stats.prs_scanned += 1;

            // Skip already-merged PRs (shouldn't be "open" but guard defensively).
            if pr.merged == Some(true) {
                tracing::debug!(
                    project_id = %project.id,
                    pr = pr.number,
                    head = %head_branch,
                    "CoordinatorActor: stale PR sweep skipping already-merged PR"
                );
                stats.prs_skipped += 1;
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_PR_MERGED,
                );
                continue;
            }

            // Look up the backing task.
            let task = match task_repo.get_by_short_id(short_id).await {
                Ok(Some(task)) => Some(task),
                Ok(None) => {
                    // Task not found. For the sweep, a missing task with an open
                    // PR is considered stale — the PR is orphaned. We create a
                    // synthetic minimal task record for the guardrail check.
                    // Use epoch as timestamps so the grace period check passes
                    // (the task is considered long-closed).
                    tracing::info!(
                        project_id = %project.id,
                        pr = pr.number,
                        head = %head_branch,
                        short_id,
                        "CoordinatorActor: stale PR sweep found PR with missing backing task"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        project_id = %project.id,
                        pr = pr.number,
                        short_id,
                        "CoordinatorActor: stale PR sweep failed to look up task"
                    );
                    stats.errors += 1;
                    djinn_telemetry::stale_sweep::increment_pr_skipped(
                        djinn_telemetry::stale_sweep::REASON_API_ERROR,
                    );
                    continue;
                }
            };

            // Only reap PRs for tasks that are closed (or we created a synthetic
            // closed one above).
            if task.as_ref().is_some_and(|task| task.status != "closed") {
                tracing::debug!(
                    project_id = %project.id,
                    pr = pr.number,
                    head = %head_branch,
                    task_status = %task.as_ref().expect("present task was checked").status,
                    "CoordinatorActor: stale PR sweep skipping PR whose task is still open"
                );
                stats.prs_skipped += 1;
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_TASK_OPEN,
                );
                continue;
            }

            // Run guardrail checks via PrCleanupPolicy.
            let cleanup_target = task.as_ref().map(super::pr_poller::pr_cleanup::PrCleanupTarget::from).unwrap_or_else(|| super::pr_poller::pr_cleanup::PrCleanupTarget { short_id: short_id.to_owned(), closed_at: Some("1970-01-01T00:00:00Z".to_owned()), updated_at: "1970-01-01T00:00:00Z".to_owned() });
            match cleanup_policy.should_cleanup_pr(&cleanup_target, pr).await {
                Ok(true) => {
                    // Guardrails passed — proceed with cleanup.
                }
                Ok(false) => {
                    stats.prs_skipped += 1;
                    djinn_telemetry::stale_sweep::increment_pr_skipped(
                        djinn_telemetry::stale_sweep::REASON_GRACE_PERIOD,
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        project_id = %project.id,
                        pr = pr.number,
                        "CoordinatorActor: stale PR sweep guardrail check failed"
                    );
                    stats.errors += 1;
                    djinn_telemetry::stale_sweep::increment_pr_skipped(
                        djinn_telemetry::stale_sweep::REASON_API_ERROR,
                    );
                    continue;
                }
            }

            let is_dry_run = cleanup_policy.config().dry_run;

            if is_dry_run {
                tracing::info!(
                    project_id = %project.id,
                    pr = pr.number,
                    head = %head_branch,
                    short_id,
                    pr_url = %pr.html_url,
                    "CoordinatorActor: stale PR sweep dry-run: would close PR and delete branch"
                );
                stats.prs_reaped += 1;
                stats.branches_reaped += 1;
                stats.dry_run = true;
                djinn_telemetry::stale_sweep::increment_pr_reaped();
                djinn_telemetry::stale_sweep::increment_branch_reaped();
                continue;
            }

            // ── Close the PR ────────────────────────────────────────────
            if let Err(e) = github
                .close_pull_request(&project.github_owner, &project.github_repo, pr.number)
                .await
            {
                tracing::warn!(
                    error = %e,
                    project_id = %project.id,
                    pr = pr.number,
                    "CoordinatorActor: stale PR sweep failed to close PR; continuing"
                );
                stats.errors += 1;
                continue;
            }
            stats.prs_reaped += 1;
            djinn_telemetry::stale_sweep::increment_pr_reaped();

            tracing::info!(
                project_id = %project.id,
                pr = pr.number,
                head = %head_branch,
                short_id,
                pr_url = %pr.html_url,
                "CoordinatorActor: stale PR sweep closed stale PR"
            );

            // ── Delete the remote branch ────────────────────────────────
            match cleanup_policy
                .delete_branch_if_allowed(&cleanup_target, head_branch)
                .await
            {
                Ok(super::pr_poller::pr_cleanup::BranchCleanupOutcome::Deleted) => {
                    stats.branches_reaped += 1;
                    djinn_telemetry::stale_sweep::increment_branch_reaped();
                    tracing::info!(
                        project_id = %project.id,
                        head = %head_branch,
                        "CoordinatorActor: stale PR sweep deleted remote branch"
                    );
                }
                Ok(super::pr_poller::pr_cleanup::BranchCleanupOutcome::DryRunWouldDelete) => {
                    // Already counted in dry-run path above.
                }
                Ok(super::pr_poller::pr_cleanup::BranchCleanupOutcome::Skipped) => {
                    tracing::info!(
                        project_id = %project.id,
                        head = %head_branch,
                        "CoordinatorActor: stale PR sweep skipped branch deletion (guardrail)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        project_id = %project.id,
                        head = %head_branch,
                        "CoordinatorActor: stale PR sweep failed to delete branch; continuing"
                    );
                    stats.errors += 1;
                }
            }

            // ── Audit comment on the closed PR ──────────────────────────
            let audit_comment = format!(
                "**Automated cleanup by Djinn reconciliation sweep.**\n\n\
                 This PR's backing task (`{short_id}`) is closed. \
                 The PR and branch have been reaped by the periodic stale-resource sweep.\n\n\
                 - PR: #{pr_number}\n\
                 - Branch: `{head}`\n\
                 - Task status: closed",
                short_id = short_id,
                pr_number = pr.number,
                head = head_branch,
            );
            if let Err(e) = github
                .create_pr_comment(
                    &project.github_owner,
                    &project.github_repo,
                    pr.number,
                    &audit_comment,
                )
                .await
            {
                tracing::warn!(
                    error = %e,
                    project_id = %project.id,
                    pr = pr.number,
                    "CoordinatorActor: stale PR sweep failed to post audit comment; continuing"
                );
                // Non-fatal — the PR is already closed.
            }

            // ── Task activity log entry (orphaned PRs have no task row).
            if let Some(task) = task.as_ref() {
                let activity_payload = serde_json::json!({ "event": "stale_pr_swept", "pr_number": pr.number, "pr_url": pr.html_url, "branch": head_branch, "project_id": project.id });
                if let Err(e) = task_repo.log_activity(Some(&task.id), "system", "system", "stale_pr_swept", &activity_payload.to_string()).await {
                    tracing::warn!(error = %e, project_id = %project.id, task_id = %task.id, pr = pr.number, "CoordinatorActor: stale PR sweep failed to write activity log; continuing");
                }
            }
        }
    }

    // ── Structured summary log ──────────────────────────────────────────
    tracing::info!(
        projects_scanned = stats.projects_scanned,
        prs_scanned = stats.prs_scanned,
        prs_reaped = stats.prs_reaped,
        branches_reaped = stats.branches_reaped,
        prs_skipped = stats.prs_skipped,
        errors = stats.errors,
        dry_run = stats.dry_run,
        "CoordinatorActor: stale PR/branch reconciliation sweep completed"
    );
}

// ─── Orphan worker-session detection ─────────────────────────────────────────

/// Detect worker sessions that are still `running` but whose backing task has
/// been closed (or deleted) and interrupt them. This acts as a lightweight
/// detection and logging backstop — it does NOT duplicate the full
/// zombie-session reaper logic. The zombie reaper in
/// [`session_recovery::reap_zombie_sessions`] handles the general stall case;
/// this sweep catches the specific scenario where a task was closed but the
/// session was never interrupted by the inline close path.
///
/// Called from [`sweep_stale_resources`] on every periodic tick after the
/// stale-task-run reaping.
async fn sweep_orphan_worker_sessions(db: &djinn_db::Database) {
    let session_repo =
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    // Find running sessions whose backing task is closed (or missing).
    //
    // The grace-period filter is applied at the Rust level (not SQL) so we
    // can reuse `parse_iso_elapsed` with the same ISO-8601 parsing the rest
    // of the codebase uses.
    let rows = match session_repo.orphan_session_candidates().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "CoordinatorActor: orphan worker-session sweep query failed"
            );
            return;
        }
    };

    if rows.is_empty() {
        return;
    }

    let now = time::OffsetDateTime::now_utc();
    let mut reaped = 0usize;

    for row in &rows {
        // Grace-period check: only reap sessions that have been running past
        // the task's close timestamp by at least ORPHAN_SESSION_GRACE_SECS.
        // This avoids racing with inline session teardown that fires during
        // the task-close transition.
        if let Some(closed_at) = &row.task_closed_at {
            if let Some(elapsed_since_close) = parse_iso_elapsed_with(closed_at, now) {
                if elapsed_since_close < ORPHAN_SESSION_GRACE_SECS as u64 {
                    tracing::debug!(
                        session_id = %row.session_id,
                        task_id = ?row.task_id,
                        closed_at = %closed_at,
                        elapsed_since_task_close = elapsed_since_close,
                        "CoordinatorActor: skipping orphan session within grace period"
                    );
                    continue;
                }
            } else {
                // Couldn't parse closed_at — skip rather than act on bad data.
                continue;
            }
        }

        let elapsed_since_task_close = row
            .task_closed_at
            .as_deref()
            .and_then(|ts| parse_iso_elapsed_with(ts, now));

        tracing::warn!(
            session_id = %row.session_id,
            task_id = ?row.task_id,
            started_at = %row.started_at,
            task_status = ?row.task_status,
            elapsed_since_task_close = ?elapsed_since_task_close,
            "CoordinatorActor: orphan worker session detected (task closed/missing)"
        );

        // Interrupt the session via djinn-db helper. We don't go through the
        // full SessionRepository::update path because we don't have an
        // EventBus reference here and the token counts are unknown (the
        // session was never properly finalized).
        match session_repo.interrupt_by_id(&row.session_id).await {
            Ok(true) => {
                reaped += 1;
                djinn_telemetry::stale_sweep::increment_orphan_session_reaped();
                tracing::info!(
                    session_id = %row.session_id,
                    task_id = ?row.task_id,
                    "CoordinatorActor: interrupted orphan worker session"
                );
            }
            Ok(false) => {
                // Session was already interrupted or completed between our
                // SELECT and UPDATE — benign race, nothing to do.
                tracing::debug!(
                    session_id = %row.session_id,
                    "CoordinatorActor: orphan session already finalized; skip"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %row.session_id,
                    task_id = ?row.task_id,
                    error = %e,
                    "CoordinatorActor: failed to interrupt orphan worker session"
                );
            }
        }
    }

    if reaped > 0 {
        tracing::warn!(
            reaped = reaped,
            scanned = rows.len(),
            "CoordinatorActor: orphan worker-session sweep completed"
        );
    }
}

/// Parse an ISO-8601 timestamp and return seconds elapsed since it, relative
/// to a pre-computed `now` timestamp. Returns `None` if the input cannot be
/// parsed.
fn parse_iso_elapsed_with(ts: &str, now: time::OffsetDateTime) -> Option<u64> {
    use time::format_description::well_known::Iso8601;
    let parsed = time::OffsetDateTime::parse(ts, &Iso8601::DEFAULT).ok()?;
    let elapsed = (now - parsed).whole_seconds();
    Some(if elapsed < 0 { 0 } else { elapsed as u64 })
}

// ─── Durable output-stash GC ────────────────────────────────────────────────

async fn sweep_durable_output_stash(db: &djinn_db::Database) {
    let Some(root) = crate::output_stash::durable_root_for_gc() else {
        tracing::debug!("CoordinatorActor: output-stash GC skipped; durable root unavailable");
        return;
    };

    let retention_days = output_stash_gc_retention_days();
    let retention_cutoff_unix_secs = output_stash_retention_cutoff_unix_secs(
        time::OffsetDateTime::now_utc().unix_timestamp(),
        retention_days,
    );
    let sessions = match load_output_stash_gc_sessions(db).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                retention_days,
                "CoordinatorActor: output-stash GC failed to load sessions; will retry on next sweep"
            );
            return;
        }
    };

    let gc_root = root.clone();
    let report = match tokio::task::spawn_blocking(move || {
        crate::output_stash::gc_durable_output_stash(
            &gc_root,
            retention_cutoff_unix_secs,
            |session_id| Ok(sessions.get(session_id).cloned()),
        )
    })
    .await
    {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                retention_days,
                "CoordinatorActor: output-stash GC worker failed; will retry on next sweep"
            );
            return;
        }
    };

    if report.is_success() {
        tracing::info!(
            root = %root.display(),
            retention_days,
            retention_cutoff_unix_secs,
            pointers_scanned = report.pointers_scanned,
            pointers_deleted = report.pointers_deleted,
            pointers_retained = report.pointers_retained,
            blobs_scanned = report.blobs_scanned,
            blobs_deleted = report.blobs_deleted,
            blobs_retained = report.blobs_retained,
            error_count = 0_u64,
            cleanup_outcome = "completed",
            "CoordinatorActor: output-stash GC completed"
        );
    } else {
        tracing::warn!(
            root = %root.display(),
            retention_days,
            retention_cutoff_unix_secs,
            pointers_scanned = report.pointers_scanned,
            pointers_deleted = report.pointers_deleted,
            pointers_retained = report.pointers_retained,
            blobs_scanned = report.blobs_scanned,
            blobs_deleted = report.blobs_deleted,
            blobs_retained = report.blobs_retained,
            error_count = report.errors.len(),
            errors = ?report.errors,
            cleanup_outcome = "completed_with_errors",
            "CoordinatorActor: output-stash GC completed with errors; will retry failed work on next sweep"
        );
    }
}

async fn load_output_stash_gc_sessions(
    db: &djinn_db::Database,
) -> djinn_db::Result<HashMap<String, crate::output_stash::OutputStashGcSession>> {
    let session_repo =
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let rows = session_repo.list_all_status_ended_at().await?;

    Ok(rows
        .into_iter()
        .filter_map(|snap| {
            let status = session_status_from_db(&snap.status)?;
            Some((
                snap.id,
                crate::output_stash::OutputStashGcSession {
                    status,
                    ended_at_unix_secs: parse_session_timestamp_unix_secs(snap.ended_at.as_deref()),
                },
            ))
        })
        .collect())
}

fn session_status_from_db(raw: &str) -> Option<djinn_core::models::SessionStatus> {
    match raw {
        "running" => Some(djinn_core::models::SessionStatus::Running),
        "completed" => Some(djinn_core::models::SessionStatus::Completed),
        "interrupted" => Some(djinn_core::models::SessionStatus::Interrupted),
        "failed" => Some(djinn_core::models::SessionStatus::Failed),
        "paused" => Some(djinn_core::models::SessionStatus::Paused),
        _ => None,
    }
}

fn parse_session_timestamp_unix_secs(raw: Option<&str>) -> Option<u64> {
    let raw = raw?;
    use time::format_description::well_known::{Iso8601, Rfc3339};
    time::OffsetDateTime::parse(raw, &Iso8601::DEFAULT)
        .or_else(|_| time::OffsetDateTime::parse(raw, &Rfc3339))
        .ok()
        .and_then(|ts| u64::try_from(ts.unix_timestamp()).ok())
}

fn output_stash_gc_retention_days() -> u64 {
    std::env::var(OUTPUT_STASH_GC_RETENTION_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(OUTPUT_STASH_GC_DEFAULT_RETENTION_DAYS)
}

fn output_stash_retention_cutoff_unix_secs(now_unix_secs: i64, retention_days: u64) -> u64 {
    let retention_secs = retention_days.saturating_mul(24 * 60 * 60);
    u64::try_from(now_unix_secs)
        .unwrap_or(0)
        .saturating_sub(retention_secs)
}

#[cfg(test)]
mod output_stash_gc_tests {
    use super::*;

    #[test]
    fn output_stash_retention_cutoff_uses_configured_day_window() {
        let now = 1_700_000_000_i64;

        assert_eq!(
            output_stash_retention_cutoff_unix_secs(now, 7),
            1_700_000_000_u64 - (7 * 24 * 60 * 60)
        );
    }

    #[test]
    fn output_stash_retention_cutoff_saturates_before_epoch() {
        assert_eq!(output_stash_retention_cutoff_unix_secs(-1, 30), 0);
        assert_eq!(output_stash_retention_cutoff_unix_secs(10, 30), 0);
    }
}

// ─── Cargo cache health sweep ──────────────────────────────────────────────

/// Filesystem convention for the warm, per-project Cargo target base.
/// Matches `djinn_agent_worker::cargo_target_seed::WARM_BASE_ROOT`.
const WARM_BASE_ROOT: &str = "/cache/cargo-target";

/// Per-project cargo cache health summary extracted from the current
/// Prometheus metrics and warm-base filesystem state.
#[derive(Debug, Clone)]
struct CargoCacheProjectHealth {
    project_id: String,
    seed_hit_count: u64,
    cold_fallback_count: u64,
    warm_base_age_seconds: Option<u64>,
}

/// Read warm-base directories and seed metrics from Prometheus counters,
/// then log a structured health line per project.
///
/// Called from [`sweep_stale_resources`] on every periodic tick.
async fn sweep_cargo_health() {
    sweep_cargo_health_under(Path::new(WARM_BASE_ROOT)).await;
}

/// Testable implementation that accepts an explicit warm-base root.
async fn sweep_cargo_health_under(warm_base_root: &Path) {
    let now_unix_secs = time::OffsetDateTime::now_utc().unix_timestamp();
    let now_u64 = u64::try_from(now_unix_secs).unwrap_or(0);

    // Parse seed metrics from Prometheus text output.
    let seed_metrics = match djinn_telemetry::render() {
        Ok(text) => parse_seed_metrics_from_text(&text),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "CoordinatorActor: cargo cache health sweep skipped metrics; render failed"
            );
            Vec::new()
        }
    };

    let mut project_healths: HashMap<String, CargoCacheProjectHealth> = HashMap::new();

    // Populate from Prometheus counter metrics.
    for (project_id, hits, colds) in seed_metrics {
        project_healths
            .entry(project_id.clone())
            .and_modify(|h| {
                h.seed_hit_count = hits;
                h.cold_fallback_count = colds;
            })
            .or_insert(CargoCacheProjectHealth {
                project_id,
                seed_hit_count: hits,
                cold_fallback_count: colds,
                warm_base_age_seconds: None,
            });
    }

    // Scan warm-base directories for freshness.
    match tokio::fs::read_dir(warm_base_root).await {
        Ok(mut entries) => {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let Some(project_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                    continue;
                };
                let age = compute_warm_base_age_secs(&entry.path(), now_u64).await;
                project_healths
                    .entry(project_id.clone())
                    .and_modify(|h| h.warm_base_age_seconds = age)
                    .or_insert(CargoCacheProjectHealth {
                        project_id,
                        seed_hit_count: 0,
                        cold_fallback_count: 0,
                        warm_base_age_seconds: age,
                    });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                root = %warm_base_root.display(),
                "CoordinatorActor: cargo cache health sweep skipped; warm-base root does not exist"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %warm_base_root.display(),
                "CoordinatorActor: cargo cache health sweep failed to enumerate warm-base root"
            );
        }
    }

    // Log structured health line per project.
    for health in project_healths.values() {
        let seed_hit_rate =
            compute_seed_hit_rate(health.seed_hit_count, health.cold_fallback_count);
        tracing::info!(
            project_id = %health.project_id,
            seed_hit_rate,
            cold_fallback_count = health.cold_fallback_count,
            warm_base_age_seconds = ?health.warm_base_age_seconds,
            "cargo cache health"
        );
    }
}

/// Parse `djinn_cargo_seed_hit_total` and `djinn_cargo_seed_cold_total` counter
/// values from a Prometheus text exposition. Returns `(project_id, hit_count,
/// cold_count)` tuples aggregated per project.
fn parse_seed_metrics_from_text(rendered: &str) -> Vec<(String, u64, u64)> {
    let hit_re = match Regex::new(r#"djinn_cargo_seed_hit_total\{([^}]*)\}\s+(\d+)"#) {
        Ok(re) => re,
        Err(e) => {
            tracing::warn!(error = %e, "cargo cache health: invalid seed-hit regex");
            return Vec::new();
        }
    };
    let cold_re = match Regex::new(r#"djinn_cargo_seed_cold_total\{([^}]*)\}\s+(\d+)"#) {
        Ok(re) => re,
        Err(e) => {
            tracing::warn!(error = %e, "cargo cache health: invalid cold-fallback regex");
            return Vec::new();
        }
    };
    let project_re = match Regex::new(r#"project_id="([^"]+)""#) {
        Ok(re) => re,
        Err(e) => {
            tracing::warn!(error = %e, "cargo cache health: invalid project-label regex");
            return Vec::new();
        }
    };

    let mut hits: HashMap<String, u64> = HashMap::new();
    let mut colds: HashMap<String, u64> = HashMap::new();

    for cap in hit_re.captures_iter(rendered) {
        let labels = &cap[1];
        let value: u64 = cap[2].parse().unwrap_or(0);
        if let Some(pc) = project_re.captures(labels) {
            *hits.entry(pc[1].to_string()).or_insert(0) += value;
        }
    }

    for cap in cold_re.captures_iter(rendered) {
        let labels = &cap[1];
        let value: u64 = cap[2].parse().unwrap_or(0);
        if let Some(pc) = project_re.captures(labels) {
            *colds.entry(pc[1].to_string()).or_insert(0) += value;
        }
    }

    let mut all_projects: HashSet<String> = HashSet::new();
    all_projects.extend(hits.keys().cloned());
    all_projects.extend(colds.keys().cloned());

    let mut result: Vec<(String, u64, u64)> = all_projects
        .into_iter()
        .map(|pid| {
            let h = hits.get(&pid).copied().unwrap_or(0);
            let c = colds.get(&pid).copied().unwrap_or(0);
            (pid, h, c)
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Compute the age (in seconds) of a warm-base directory from its mtime.
/// Returns `None` if the directory metadata cannot be read.
async fn compute_warm_base_age_secs(dir: &Path, now_unix_secs: u64) -> Option<u64> {
    let metadata = tokio::fs::metadata(dir).await.ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now_unix_secs.saturating_sub(mtime_secs))
}

/// Compute the seed hit-rate as `hits / (hits + colds)`.
///
/// Returns `1.0` when no seed outcomes have been recorded yet (cold-start of
/// the health sweep itself), `0.0` when every seed was a cold fallback, and a
/// `(0.0, 1.0]` ratio otherwise.
fn compute_seed_hit_rate(hits: u64, colds: u64) -> f64 {
    let total = hits + colds;
    if total == 0 {
        1.0
    } else {
        hits as f64 / total as f64
    }
}

#[cfg(test)]
mod cargo_cache_health_tests {
    use super::*;
    use std::time::SystemTime;

    // ── Freshness computation ───────────────────────────────────────────

    #[tokio::test]
    async fn freshness_computation_from_mock_mtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("project-abc");
        std::fs::create_dir(&dir).unwrap();

        // The directory was just created, so its mtime ≈ now.
        // This test compares a freshly-created directory's filesystem mtime
        // against the current wall-clock — a real-clock read is the
        // intentionally non-deterministic assertion under test.
        #[allow(clippy::disallowed_methods)] // test-only: asserts real mtime ≈ real now
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let age = compute_warm_base_age_secs(&dir, now).await;
        // Age should be 0 or 1 second depending on clock jitter.
        assert!(age.unwrap() <= 1, "freshly-created dir age should be <= 1s");

        // Simulate an old directory by using a far-future "now".
        let future_now = now + 3600;
        let age = compute_warm_base_age_secs(&dir, future_now).await;
        assert_eq!(age.unwrap(), 3600);
    }

    #[tokio::test]
    async fn freshness_returns_none_for_missing_dir() {
        let age =
            compute_warm_base_age_secs(Path::new("/nonexistent/path/xyz"), 1_700_000_000).await;
        assert!(age.is_none());
    }

    // ── Hit-rate aggregation ────────────────────────────────────────────

    #[test]
    fn hit_rate_no_runs_returns_one() {
        // 0 hits / 0 total = 1.0 (no runs yet — benign default).
        assert_eq!(compute_seed_hit_rate(0, 0), 1.0);
    }

    #[test]
    fn hit_rate_three_hits_one_cold() {
        // 3 hits / 4 total = 0.75.
        let rate = compute_seed_hit_rate(3, 1);
        assert!(
            (rate - 0.75).abs() < f64::EPSILON,
            "expected 0.75, got {rate}"
        );
    }

    #[test]
    fn hit_rate_all_cold() {
        // 0 hits / 1 total = 0.0.
        assert_eq!(compute_seed_hit_rate(0, 1), 0.0);
    }

    #[test]
    fn hit_rate_all_hits() {
        // 5 hits / 5 total = 1.0.
        assert_eq!(compute_seed_hit_rate(5, 0), 1.0);
    }

    // ── Prometheus text parsing ─────────────────────────────────────────

    #[test]
    fn parse_seed_metrics_extracts_per_project() {
        let rendered = concat!(
            "# HELP djinn_cargo_seed_hit_total ...\n",
            "# TYPE djinn_cargo_seed_hit_total counter\n",
            "djinn_cargo_seed_hit_total{project_id=\"abc123\"} 5\n",
            "djinn_cargo_seed_hit_total{project_id=\"def456\"} 3\n",
            "# HELP djinn_cargo_seed_cold_total ...\n",
            "# TYPE djinn_cargo_seed_cold_total counter\n",
            "djinn_cargo_seed_cold_total{fallback_reason=\"base_missing\",project_id=\"abc123\"} 1\n",
            "djinn_cargo_seed_cold_total{fallback_reason=\"scan_failed\",project_id=\"abc123\"} 2\n",
            "djinn_cargo_seed_cold_total{fallback_reason=\"base_missing\",project_id=\"ghi789\"} 4\n",
        );

        let mut metrics = parse_seed_metrics_from_text(rendered);
        metrics.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(metrics.len(), 3);
        // abc123: 5 hits, 3 colds (1 + 2 aggregated)
        assert_eq!(metrics[0], ("abc123".into(), 5, 3));
        // def456: 3 hits, 0 colds
        assert_eq!(metrics[1], ("def456".into(), 3, 0));
        // ghi789: 0 hits, 4 colds
        assert_eq!(metrics[2], ("ghi789".into(), 0, 4));
    }

    #[test]
    fn parse_seed_metrics_empty_input() {
        let metrics = parse_seed_metrics_from_text("");
        assert!(metrics.is_empty());
    }

    #[test]
    fn parse_seed_metrics_no_cargo_metrics() {
        let rendered = "some_other_metric{label=\"x\"} 42\n";
        let metrics = parse_seed_metrics_from_text(rendered);
        assert!(metrics.is_empty());
    }

    // ── End-to-end health sweep with mock filesystem ────────────────────

    #[tokio::test]
    async fn sweep_cargo_health_under_logs_per_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        // Create two warm-base dirs.
        std::fs::create_dir(root.join("proj-alpha")).unwrap();
        std::fs::create_dir(root.join("proj-beta")).unwrap();

        // Run the sweep (metrics portion will be empty in test context
        // unless telemetry was initialized; the filesystem scan is the
        // primary assertion here).
        sweep_cargo_health_under(root).await;

        // No assertion on log output — tracing subscriber is not captured
        // in unit tests. The function's contract is that it doesn't panic
        // and runs to completion. Structured logging is validated by the
        // acceptance criteria review.
    }
}

// ─── /cache/sccache guard ──────────────────────────────────────────────────

/// Default path for the sccache directory on the shared PVC.
/// This matches the `SCCACHE_DIR` convention used by warm/worker pods
/// (namespaced per project), but the guard sweeps the parent `/cache/sccache`
/// directory as a whole.
const SCCACHE_ROOT: &str = "/cache/sccache";

/// Distinct structured outcomes for the sccache sweep guard.
///
/// Each variant maps to a stable log/metric label so operators can build
/// dashboards and alerts on the outcome stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SccacheSweepOutcome {
    /// `/cache/sccache` does not exist — no-op, non-error.
    Missing,
    /// Dry-run mode: candidate bytes and latest mtime were computed and logged;
    /// nothing was deleted.
    DryRunReported,
    /// Directory was older than the configured threshold and was successfully
    /// deleted.
    Deleted,
    /// Directory exists but its latest mtime is *younger* than the configured
    /// threshold.  It was retained and a warning was emitted so accidental new
    /// writers are visible.
    RetainedFresh,
    /// `DJINN_CACHE_CLEANUP_MODE` is not `delete` — directory would be
    /// deletable but the global mode prevents actual removal.
    NotEnabled,
    /// Deletion was attempted but `tokio::fs::remove_dir_all` failed.
    DeletionError,
}

/// Production entry point: sweep the global `/cache/sccache` directory
/// using the coordinator's cleanup config.
///
/// Called from [`sweep_stale_resources`] on every periodic tick.
async fn sweep_sccache_guard(config: &crate::context::CacheCleanupConfig) {
    sweep_sccache_guard_under(config, Path::new(SCCACHE_ROOT)).await;
}

/// Testable implementation that accepts an explicit sccache root path.
///
/// Returns the outcome so tests can assert it.
async fn sweep_sccache_guard_under(
    config: &crate::context::CacheCleanupConfig,
    sccache_root: &Path,
) -> SccacheSweepOutcome {
    use djinn_telemetry::cache_cleanup as cleanup_metrics;
    use djinn_telemetry::cache_cleanup::{
        COMPONENT_SCCACHE, MODE_DELETE, MODE_DRY_RUN, OUTCOME_DELETED, OUTCOME_DRY_RUN,
        OUTCOME_ERROR, OUTCOME_RETAINED, OUTCOME_SKIPPED,
    };

    if !config.sccache_enabled {
        tracing::debug!("CoordinatorActor: sccache cleanup disabled; skipping guard");
        return SccacheSweepOutcome::NotEnabled;
    }

    let mode_label = if config.mode.is_delete() {
        MODE_DELETE
    } else {
        MODE_DRY_RUN
    };

    // Stat the sccache root — missing path is a non-error skip.
    match tokio::fs::metadata(sccache_root).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                path = %sccache_root.display(),
                "CoordinatorActor: sccache guard skipped; path does not exist"
            );
            cleanup_metrics::increment_cleanup_total(
                COMPONENT_SCCACHE,
                OUTCOME_SKIPPED,
                mode_label,
            );
            return SccacheSweepOutcome::Missing;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %sccache_root.display(),
                "CoordinatorActor: sccache guard failed to stat path; skipping"
            );
            cleanup_metrics::increment_cleanup_total(COMPONENT_SCCACHE, OUTCOME_ERROR, mode_label);
            return SccacheSweepOutcome::DeletionError;
        }
    };

    // Compute latest mtime (recursive walk for the most recent file).
    let now_unix_secs = time::OffsetDateTime::now_utc()
        .unix_timestamp()
        .try_into()
        .unwrap_or(0_u64);

    let latest_mtime_secs = latest_mtime_unix_secs(sccache_root).await;
    let age_hours = latest_mtime_secs.map(|mtime| now_unix_secs.saturating_sub(mtime) / 3600);

    let size_bytes = dir_size_recursive(sccache_root).await;

    tracing::info!(
        path = %sccache_root.display(),
        size_bytes,
        latest_mtime_secs = ?latest_mtime_secs,
        age_hours = ?age_hours,
        mode = %config.mode.as_metric_label(),
        "CoordinatorActor: sccache guard candidate"
    );

    cleanup_metrics::increment_candidates(COMPONENT_SCCACHE, mode_label, 1);

    // Determine staleness: only stale candidates are eligible for deletion.
    let threshold_secs = config.sccache_max_age_hours * 3600;
    // For staleness, fall back to the directory's own mtime when the recursive
    // walk finds no files (empty directory).  An empty freshly-created directory
    // should be considered fresh.
    let effective_mtime = latest_mtime_secs.or_else(|| {
        // Synchronous fallback: the directory's own mtime from the metadata
        // we already validated exists above.
        std::fs::metadata(sccache_root)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    });
    let is_stale = match effective_mtime {
        Some(mtime) => now_unix_secs.saturating_sub(mtime) >= threshold_secs,
        // Metadata genuinely unreadable — treat as stale to be conservative
        // toward cleanup (avoids leaving genuinely old directories behind).
        None => true,
    };

    if !is_stale {
        tracing::warn!(
            path = %sccache_root.display(),
            age_hours = ?age_hours,
            size_bytes,
            threshold_hours = config.sccache_max_age_hours,
            "CoordinatorActor: sccache directory is fresh — retaining. \
             An accidental new writer may have recreated it."
        );
        cleanup_metrics::increment_cleanup_total(COMPONENT_SCCACHE, OUTCOME_RETAINED, mode_label);
        return SccacheSweepOutcome::RetainedFresh;
    }

    // Directory is stale — check mode.
    if !config.mode.is_delete() {
        tracing::info!(
            path = %sccache_root.display(),
            age_hours = ?age_hours,
            size_bytes,
            projected_bytes = size_bytes,
            mode = "dry_run",
            cleanup_outcome = "dry_run",
            "CoordinatorActor: sccache guard dry-run — would delete stale directory"
        );
        cleanup_metrics::increment_cleanup_total(COMPONENT_SCCACHE, OUTCOME_DRY_RUN, MODE_DRY_RUN);
        return SccacheSweepOutcome::DryRunReported;
    }

    // Destructive mode: actually delete.
    match tokio::fs::remove_dir_all(sccache_root).await {
        Ok(()) => {
            tracing::info!(
                path = %sccache_root.display(),
                age_hours = ?age_hours,
                size_bytes,
                reclaimed_bytes = size_bytes,
                cleanup_outcome = "deleted",
                "CoordinatorActor: sccache guard deleted stale directory"
            );
            cleanup_metrics::increment_cleanup_total(
                COMPONENT_SCCACHE,
                OUTCOME_DELETED,
                MODE_DELETE,
            );
            cleanup_metrics::record_reclaimed_bytes(COMPONENT_SCCACHE, MODE_DELETE, size_bytes);
            SccacheSweepOutcome::Deleted
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %sccache_root.display(),
                "CoordinatorActor: sccache guard failed to delete stale directory"
            );
            cleanup_metrics::increment_cleanup_total(COMPONENT_SCCACHE, OUTCOME_ERROR, MODE_DELETE);
            SccacheSweepOutcome::DeletionError
        }
    }
}

/// Recursively compute the total byte size of all files under `dir`.
/// Returns 0 if the directory doesn't exist or can't be read.
async fn dir_size_recursive(dir: &Path) -> u64 {
    use std::path::PathBuf;

    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => continue,
            };

            let file_type = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(entry.metadata().await.map(|m| m.len()).unwrap_or(0));
            }
        }
    }

    total
}

/// Walk `dir` recursively and return the latest (most recent) file mtime
/// as a unix-seconds timestamp.  Returns `None` when the directory is empty
/// or metadata is unreadable.
async fn latest_mtime_unix_secs(dir: &Path) -> Option<u64> {
    use std::path::PathBuf;

    let mut latest: Option<u64> = None;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(e) => e,
            Err(_) => continue,
        };

        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => continue,
            };

            let file_type = match entry.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };

            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(metadata) = entry.metadata().await
                && let Ok(modified) = metadata.modified()
                && let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                let mtime = dur.as_secs();
                if latest.is_none_or(|prev| mtime > prev) {
                    latest = Some(mtime);
                }
            }
        }
    }

    latest
}

#[cfg(test)]
mod sccache_guard_tests {
    use super::*;
    use crate::context::{CacheCleanupConfig, CacheCleanupMode};

    /// Missing sccache path is a harmless skip.
    #[tokio::test]
    async fn missing_path_is_noop() {
        let config = CacheCleanupConfig {
            mode: CacheCleanupMode::DryRun,
            sccache_enabled: true,
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nonexistent_sccache");

        let outcome = sweep_sccache_guard_under(&config, &missing).await;
        assert_eq!(outcome, SccacheSweepOutcome::Missing);
    }

    /// Disabled config is a no-op.
    #[tokio::test]
    async fn disabled_config_skips() {
        let config = CacheCleanupConfig {
            sccache_enabled: false,
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache_dir = tmp.path().join("sccache");
        std::fs::create_dir(&sccache_dir).unwrap();

        let outcome = sweep_sccache_guard_under(&config, &sccache_dir).await;
        assert_eq!(outcome, SccacheSweepOutcome::NotEnabled);
        // Directory still exists.
        assert!(sccache_dir.exists());
    }

    /// Dry-run reports candidate but does not delete.
    #[tokio::test]
    async fn dry_run_does_not_delete() {
        let config = CacheCleanupConfig {
            mode: CacheCleanupMode::DryRun,
            sccache_enabled: true,
            sccache_max_age_hours: 0, // any age is stale
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache_dir = tmp.path().join("sccache");
        std::fs::create_dir(&sccache_dir).unwrap();
        // Write a file so size > 0.
        std::fs::write(sccache_dir.join("dummy.bin"), b"hello").unwrap();

        let outcome = sweep_sccache_guard_under(&config, &sccache_dir).await;
        assert_eq!(outcome, SccacheSweepOutcome::DryRunReported);
        // Directory and file still exist.
        assert!(sccache_dir.exists());
        assert!(sccache_dir.join("dummy.bin").exists());
    }

    /// Delete mode removes an old sccache directory.
    #[tokio::test]
    async fn delete_removes_old_directory() {
        let config = CacheCleanupConfig {
            mode: CacheCleanupMode::Delete,
            sccache_enabled: true,
            sccache_max_age_hours: 0, // any age is stale
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache_dir = tmp.path().join("sccache");
        std::fs::create_dir(&sccache_dir).unwrap();
        std::fs::write(sccache_dir.join("old_cache"), b"data").unwrap();

        let outcome = sweep_sccache_guard_under(&config, &sccache_dir).await;
        assert_eq!(outcome, SccacheSweepOutcome::Deleted);
        assert!(!sccache_dir.exists());
    }

    /// Fresh sccache directory (newer than threshold) is retained with a warning.
    #[tokio::test]
    async fn fresh_directory_is_retained() {
        let config = CacheCleanupConfig {
            mode: CacheCleanupMode::Delete,
            sccache_enabled: true,
            sccache_max_age_hours: 24, // 24h threshold
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache_dir = tmp.path().join("sccache");
        std::fs::create_dir(&sccache_dir).unwrap();
        std::fs::write(sccache_dir.join("fresh_cache"), b"data").unwrap();

        // Directory was just created, so it's fresh (< 24h old).
        let outcome = sweep_sccache_guard_under(&config, &sccache_dir).await;
        assert_eq!(outcome, SccacheSweepOutcome::RetainedFresh);
        assert!(sccache_dir.exists());
        assert!(sccache_dir.join("fresh_cache").exists());
    }

    /// Delete mode with a high threshold retains a fresh directory.
    #[tokio::test]
    async fn delete_mode_with_high_threshold_retains_fresh() {
        let config = CacheCleanupConfig {
            mode: CacheCleanupMode::Delete,
            sccache_enabled: true,
            sccache_max_age_hours: 1000, // very high threshold
            ..CacheCleanupConfig::default()
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache_dir = tmp.path().join("sccache");
        std::fs::create_dir(&sccache_dir).unwrap();

        let outcome = sweep_sccache_guard_under(&config, &sccache_dir).await;
        assert_eq!(outcome, SccacheSweepOutcome::RetainedFresh);
        assert!(sccache_dir.exists());
    }

    /// `dir_size_recursive` computes size of nested files.
    #[tokio::test]
    async fn dir_size_recursive_computes_total() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap(); // 5 bytes
        std::fs::write(root.join("sub").join("b.txt"), b"world!").unwrap(); // 6 bytes

        let size = dir_size_recursive(root).await;
        assert_eq!(size, 11);
    }

    /// `dir_size_recursive` returns 0 for a missing directory.
    #[tokio::test]
    async fn dir_size_recursive_missing_returns_zero() {
        let size = dir_size_recursive(Path::new("/nonexistent/path")).await;
        assert_eq!(size, 0);
    }

    /// `latest_mtime_unix_secs` returns a reasonable mtime for a newly
    /// created file.
    #[tokio::test]
    async fn latest_mtime_returns_recent_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"content").unwrap();

        #[allow(clippy::disallowed_methods)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mtime = latest_mtime_unix_secs(tmp.path()).await;
        assert!(mtime.is_some());
        // mtime should be within a few seconds of now.
        assert!(now - mtime.unwrap() <= 2);
    }

    /// `latest_mtime_unix_secs` returns None for empty/missing dir.
    #[tokio::test]
    async fn latest_mtime_none_for_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();

        let mtime = latest_mtime_unix_secs(&empty).await;
        assert!(mtime.is_none());
    }
}

// ─── Cargo target run-dir GC ────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct CargoTargetRunDirSweepStats {
    pub(super) scanned: usize,
    /// UUID orphan dirs deleted (existing behaviour, unchanged).
    pub(super) deleted: usize,
    pub(super) retained: usize,
    pub(super) errors: usize,
    /// Dirs LRU-trimmed by the state-independent hard cap (oldest-first).
    pub(super) cap_trimmed: usize,
    /// Per-entry errors during the hard-cap trim.
    pub(super) cap_errors: usize,
    // ── Debris / age-sweep counters (n5cp) ──────────────────────────────
    /// Non-UUID directories older than the retention window that were
    /// deleted (or counted as dry-run candidates).
    pub(super) malformed_dir_deleted: usize,
    /// Loose files (any non-directory) older than the retention window
    /// that were deleted (or counted as dry-run candidates).
    pub(super) loose_file_deleted: usize,
    /// Fresh malformed entries (dirs or files) whose mtime is within the
    /// retention window — retained with a warning.
    pub(super) retained_fresh_malformed: usize,
    /// Non-UTF8 entry names — always retained (unsafe to delete via
    /// lossy name conversion).
    pub(super) retained_non_utf8: usize,
    /// Total bytes reclaimed by debris deletion (dirs + files).
    pub(super) debris_bytes_deleted: u64,
}

async fn sweep_orphaned_cargo_target_run_dirs(
    db: &djinn_db::Database,
    root: Option<&Path>,
    config: &crate::context::CacheCleanupConfig,
) {
    // Production wiring sets `cargo_target_runs_root` explicitly (the server pod
    // mounts the shared cache PVC at `$DJINN_HOME/cache`, NOT the Job-pod
    // `/cache` path the [`CARGO_TARGET_RUNS_ROOT`] constant names). The
    // fallback only fires in tests/contexts that don't set it.
    let root = match root {
        Some(root) => root,
        None => Path::new(CARGO_TARGET_RUNS_ROOT),
    };
    sweep_orphaned_cargo_target_run_dirs_under(db, root, config).await;
}

pub(super) async fn sweep_orphaned_cargo_target_run_dirs_under(
    db: &djinn_db::Database,
    root: &Path,
    config: &crate::context::CacheCleanupConfig,
) -> CargoTargetRunDirSweepStats {
    let protected = match protected_cargo_target_run_ids(db).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir sweep failed to load protected task_run ids"
            );
            return CargoTargetRunDirSweepStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir root does not exist; skipping sweep"
            );
            return CargoTargetRunDirSweepStats::default();
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir sweep failed to enumerate root"
            );
            return CargoTargetRunDirSweepStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut stats = CargoTargetRunDirSweepStats::default();

    // Debris age-gate: non-UUID entries and loose files older than this
    // threshold become cleanup candidates.  None disables debris cleanup
    // (cargo_debris_enabled=false or zero retention).
    let debris_threshold_secs: Option<u64> =
        if config.cargo_debris_enabled && config.cargo_debris_max_age_days > 0 {
            Some(config.cargo_debris_max_age_days * 86400)
        } else {
            None
        };
    let mode_label = if config.mode.is_delete() {
        djinn_telemetry::cache_cleanup::MODE_DELETE
    } else {
        djinn_telemetry::cache_cleanup::MODE_DRY_RUN
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    root = %root.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to read directory entry; continuing"
                );
                continue;
            }
        };

        let path = entry.path();
        stats.scanned += 1;

        // ── Non-UTF8 entry name: always retained ─────────────────────
        let Some(task_run_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            stats.retained += 1;
            stats.retained_non_utf8 += 1;
            tracing::warn!(
                path = %path.display(),
                cleanup_outcome = "retained_non_utf8",
                "CoordinatorActor: cargo target run-dir sweep retained non-UTF8 entry name \
                 (cannot safely compare for age-gate deletion)"
            );
            djinn_telemetry::cache_cleanup::increment_cleanup_total(
                djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                djinn_telemetry::cache_cleanup::OUTCOME_RETAINED_NON_UTF8,
                mode_label,
            );
            continue;
        };

        // ── Get file type early for age-gate decisions ───────────────
        let file_type = match entry.file_type().await {
            Ok(ft) => ft,
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to inspect entry; continuing"
                );
                continue;
            }
        };

        let is_uuid = uuid::Uuid::parse_str(&task_run_id).is_ok();

        if is_uuid && file_type.is_dir() {
            // ── UUID directory: existing behaviour, unchanged ─────────
            if protected.contains(&task_run_id) {
                stats.retained += 1;
                tracing::debug!(
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    "CoordinatorActor: cargo target run-dir sweep retained live task-run directory"
                );
                continue;
            }

            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    stats.deleted += 1;
                    tracing::info!(
                        task_run_id = %task_run_id,
                        path = %path.display(),
                        cleanup_outcome = "uuid_orphan_deleted",
                        "CoordinatorActor: deleted orphaned cargo target run-dir"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_UUID_ORPHAN_DELETED,
                        mode_label,
                    );
                }
                Err(e) => {
                    stats.errors += 1;
                    tracing::warn!(
                        error = %e,
                        task_run_id = %task_run_id,
                        path = %path.display(),
                        cleanup_outcome = "error",
                        "CoordinatorActor: failed to delete orphaned cargo target run-dir; continuing"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_ERROR,
                        mode_label,
                    );
                }
            }
            continue;
        }

        // ── Non-UUID entry OR UUID loose file: age-gated debris ──────
        // Both malformed dirs and loose files (UUID or non-UUID named)
        // are age-gated when cargo_debris_enabled.
        let Some(threshold_secs) = debris_threshold_secs else {
            // Debris cleanup disabled — retain everything (legacy behaviour).
            stats.retained += 1;
            tracing::debug!(
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep retained entry (debris cleanup disabled)"
            );
            continue;
        };

        // Check mtime against the age threshold.
        let entry_mtime_secs = match entry.metadata().await {
            Ok(meta) => meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to read entry metadata; continuing"
                );
                continue;
            }
        };

        let now_secs: u64 = time::OffsetDateTime::now_utc()
            .unix_timestamp()
            .try_into()
            .unwrap_or(0_u64);

        let is_stale = match entry_mtime_secs {
            Some(mtime) => now_secs.saturating_sub(mtime) >= threshold_secs,
            // Metadata mtime unreadable — treat as stale (conservative toward
            // cleanup, matching the sccache guard behaviour).
            None => true,
        };

        if !is_stale {
            // Fresh entry — retain with warning.
            stats.retained += 1;
            stats.retained_fresh_malformed += 1;
            let entry_kind = if file_type.is_dir() { "dir" } else { "file" };
            tracing::warn!(
                path = %path.display(),
                entry_kind,
                age_secs = ?entry_mtime_secs.map(|m| now_secs.saturating_sub(m)),
                threshold_secs,
                cleanup_outcome = "retained_fresh_malformed",
                "CoordinatorActor: cargo target run-dir sweep retaining fresh malformed \
                 entry — an accidental new writer may have created it"
            );
            djinn_telemetry::cache_cleanup::increment_cleanup_total(
                djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                djinn_telemetry::cache_cleanup::OUTCOME_RETAINED_FRESH_MALFORMED,
                mode_label,
            );
            continue;
        }

        // Entry is stale — compute size and delete/report.
        let entry_bytes: u64 = if file_type.is_dir() {
            dir_size_recursive(&path).await
        } else {
            tokio::fs::metadata(&path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        };

        djinn_telemetry::cache_cleanup::increment_candidates(
            djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
            mode_label,
            1,
        );

        if !config.mode.is_delete() {
            // Dry-run: report but don't delete.
            let entry_kind = if file_type.is_dir() {
                stats.malformed_dir_deleted += 1;
                "malformed_dir"
            } else {
                stats.loose_file_deleted += 1;
                "loose_file"
            };
            tracing::info!(
                path = %path.display(),
                entry_kind,
                size_bytes = entry_bytes,
                projected_bytes = entry_bytes,
                age_secs = ?entry_mtime_secs.map(|m| now_secs.saturating_sub(m)),
                mode = "dry_run",
                cleanup_outcome = if file_type.is_dir() { "malformed_dir_deleted" } else { "loose_file_deleted" },
                "CoordinatorActor: cargo target run-dir sweep dry-run — would delete stale entry"
            );
            djinn_telemetry::cache_cleanup::increment_cleanup_total(
                djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                if file_type.is_dir() {
                    djinn_telemetry::cache_cleanup::OUTCOME_MALFORMED_DIR_DELETED
                } else {
                    djinn_telemetry::cache_cleanup::OUTCOME_LOOSE_FILE_DELETED
                },
                mode_label,
            );
            continue;
        }

        // Destructive mode: actually delete.
        if file_type.is_dir() {
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    stats.malformed_dir_deleted += 1;
                    stats.debris_bytes_deleted += entry_bytes;
                    tracing::info!(
                        path = %path.display(),
                        size_bytes = entry_bytes,
                        cleanup_outcome = "malformed_dir_deleted",
                        "CoordinatorActor: deleted stale malformed cargo target run-dir"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_MALFORMED_DIR_DELETED,
                        mode_label,
                    );
                    djinn_telemetry::cache_cleanup::record_reclaimed_bytes(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        mode_label,
                        entry_bytes,
                    );
                }
                Err(e) => {
                    stats.errors += 1;
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        cleanup_outcome = "error",
                        "CoordinatorActor: failed to delete stale malformed cargo target run-dir; continuing"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_ERROR,
                        mode_label,
                    );
                }
            }
        } else {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    stats.loose_file_deleted += 1;
                    stats.debris_bytes_deleted += entry_bytes;
                    tracing::info!(
                        path = %path.display(),
                        size_bytes = entry_bytes,
                        cleanup_outcome = "loose_file_deleted",
                        "CoordinatorActor: deleted stale loose file from cargo target runs"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_LOOSE_FILE_DELETED,
                        mode_label,
                    );
                    djinn_telemetry::cache_cleanup::record_reclaimed_bytes(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        mode_label,
                        entry_bytes,
                    );
                }
                Err(e) => {
                    stats.errors += 1;
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        cleanup_outcome = "error",
                        "CoordinatorActor: failed to delete stale loose file from cargo target runs; continuing"
                    );
                    djinn_telemetry::cache_cleanup::increment_cleanup_total(
                        djinn_telemetry::cache_cleanup::COMPONENT_CARGO_TARGET_RUNS,
                        djinn_telemetry::cache_cleanup::OUTCOME_ERROR,
                        mode_label,
                    );
                }
            }
        }
    }

    // Hard backstop: LRU-trim the runs root below an absolute count cap,
    // independent of task-run state. Even if both the deterministic teardown
    // and the orphan sweep above regress (e.g. the protected-ids query starts
    // over-protecting, or a new keying bug leaves rows "running"), this still
    // bounds worst-case disk so a reaping regression can't refill the PVC and
    // re-trigger node DiskPressure eviction. Runs in `spawn_blocking` since the
    // trim is synchronous `std::fs`.
    let cap = djinn_core::cargo_target_runs::hard_cap_dirs_from_env();
    let trim_root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        djinn_core::cargo_target_runs::trim_run_dirs_to_cap(&trim_root, cap)
    })
    .await
    {
        Ok(Ok(trim)) => {
            stats.cap_trimmed = trim.trimmed;
            stats.cap_errors = trim.errors;
            if trim.trimmed > 0 || trim.errors > 0 {
                tracing::warn!(
                    root = %root.display(),
                    cap,
                    scanned = trim.scanned,
                    trimmed = trim.trimmed,
                    retained = trim.retained,
                    errors = trim.errors,
                    "CoordinatorActor: cargo target run-dir hard cap trimmed dirs \
                     (orphan sweep is not keeping the runs root bounded)"
                );
            }
        }
        Ok(Err(e)) => {
            stats.cap_errors += 1;
            tracing::warn!(
                error = %e,
                root = %root.display(),
                cap,
                "CoordinatorActor: cargo target run-dir hard cap trim failed"
            );
        }
        Err(e) => {
            stats.cap_errors += 1;
            tracing::warn!(
                error = %e,
                root = %root.display(),
                cap,
                "CoordinatorActor: cargo target run-dir hard cap trim task join failed"
            );
        }
    }

    tracing::info!(
        root = %root.display(),
        scanned = stats.scanned,
        deleted = stats.deleted,
        retained = stats.retained,
        errors = stats.errors,
        cap_trimmed = stats.cap_trimmed,
        cap_errors = stats.cap_errors,
        malformed_dir_deleted = stats.malformed_dir_deleted,
        loose_file_deleted = stats.loose_file_deleted,
        retained_fresh_malformed = stats.retained_fresh_malformed,
        retained_non_utf8 = stats.retained_non_utf8,
        debris_bytes_deleted = stats.debris_bytes_deleted,
        mode = %config.mode.as_metric_label(),
        cargo_debris_enabled = config.cargo_debris_enabled,
        cargo_debris_max_age_days = config.cargo_debris_max_age_days,
        cleanup_outcome = if stats.errors == 0 && stats.cap_errors == 0 {
            "completed"
        } else {
            "completed_with_errors"
        },
        "CoordinatorActor: cargo target run-dir sweep completed"
    );

    stats
}

async fn protected_cargo_target_run_ids(
    db: &djinn_db::Database,
) -> djinn_db::Result<HashSet<String>> {
    let task_run_repo = djinn_db::TaskRunRepository::new(db.clone());
    let session_repo =
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());

    let task_run_ids = task_run_repo.running_ids().await?;
    let session_task_run_ids = session_repo.running_task_run_ids().await?;

    Ok(task_run_ids
        .into_iter()
        .chain(session_task_run_ids)
        .collect())
}

fn session_status_classification(sessions: &[djinn_core::models::SessionRecord]) -> &'static str {
    if sessions
        .iter()
        .any(|session| session.status == "interrupted")
    {
        "session_interrupted"
    } else if sessions.iter().any(|session| session.status == "completed") {
        "session_completed"
    } else if sessions.iter().any(|session| session.status == "failed") {
        "session_failed"
    } else if sessions.is_empty() {
        "task_run_running_without_session"
    } else {
        "task_run_running_without_live_session"
    }
}

// ─── K8s task-run Job backstop ───────────────────────────────────────────────

/// Reconcile runtime task-run Jobs against DB truth and foreground-delete Jobs
/// for task-runs that are absent or already finalized.
///
/// This is intentionally a runtime-bridge policy, not a Kubernetes policy: the
/// coordinator sees only [`djinn_control_plane::bridge::TaskrunJobRef`] values
/// and calls [`djinn_control_plane::bridge::RuntimeOps::teardown_taskrun_job`].
/// Inline teardown, stall reaping, and zombie recovery own currently-running
/// rows; this backstop only cleans Jobs with no live DB owner.
pub(super) async fn reap_orphaned_taskrun_jobs(
    db: &djinn_db::Database,
    app_state: &crate::context::CoordinatorContext,
    reason: &'static str,
) {
    let Some(runtime_ops) = app_state.runtime_ops.as_ref() else {
        return;
    };

    let jobs = match runtime_ops.list_taskrun_jobs().await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::warn!(error = %e, reason, "CoordinatorActor: failed to list task-run Jobs for backstop reap");
            return;
        }
    };

    let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(db.clone());

    let now = SystemClock::new().now();

    for job in jobs {
        let task_run_id = job.task_run_id.trim();
        if task_run_id.is_empty() {
            tracing::warn!(
                job_name = %job.job_name,
                task_run_id = %job.task_run_id,
                db_classification = "malformed_inventory",
                reason,
                "CoordinatorActor: task-run Job backstop inventory entry is malformed; skipping"
            );
            continue;
        }

        let task_run = match task_run_repo.get(task_run_id).await {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(
                    job_name = %job.job_name,
                    task_run_id = %task_run_id,
                    error = %e,
                    reason,
                    "CoordinatorActor: failed to load task_run for task-run Job backstop reap"
                );
                continue;
            }
        };

        let sessions = match list_sessions_for_task_run(db, task_run_id).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    job_name = %job.job_name,
                    task_run_id = %task_run_id,
                    error = %e,
                    reason,
                    "CoordinatorActor: failed to load sessions for task-run Job backstop reap"
                );
                continue;
            }
        };

        let classification = classify_taskrun_job_owner(task_run.as_ref(), &sessions);
        if classification.keep_job {
            tracing::debug!(
                job_name = %job.job_name,
                task_run_id = %task_run_id,
                db_classification = classification.db_classification,
                reason,
                "CoordinatorActor: task-run Job backstop preserved live task-run Job"
            );
            continue;
        }

        // Boot-race grace: the worker inserts the task_runs row (and later the
        // session row) from INSIDE the pod via the create_task_run RPC, i.e.
        // only after pod scheduling + image pull + worker boot — which can take
        // minutes. Every new Job therefore has a legitimate window where its DB
        // owner rows are still "absent" (or the run row exists but no session
        // yet). Reaping in that window kills sessions before they start, so we
        // skip young Jobs in the boot-race classes. Terminal task_run statuses
        // and interrupted/completed sessions are genuinely dead and reaped
        // regardless of age (handled by the eligibility check below).
        if is_boot_race_classification(classification.db_classification)
            && job_within_reap_grace(job.created_at, now, TASKRUN_JOB_REAP_GRACE)
        {
            tracing::debug!(
                job_name = %job.job_name,
                task_run_id = %task_run_id,
                db_classification = "young_job_grace",
                original_classification = classification.db_classification,
                reason,
                "CoordinatorActor: task-run Job backstop skipped young Job within boot-race grace window"
            );
            continue;
        }

        if let Err(e) = runtime_ops.teardown_taskrun_job(task_run_id).await {
            tracing::warn!(
                job_name = %job.job_name,
                task_run_id = %task_run_id,
                db_classification = classification.db_classification,
                error = %e,
                reason,
                "CoordinatorActor: task-run Job backstop teardown failed; continuing sweep"
            );
            continue;
        }

        match reason {
            "startup" => {
                djinn_telemetry::zombie::increment_reap(djinn_telemetry::zombie::KIND_STARTUP)
            }
            "periodic" => {
                djinn_telemetry::zombie::increment_reap(djinn_telemetry::zombie::KIND_PERIODIC)
            }
            _ => djinn_telemetry::zombie::increment_reap(reason),
        }

        tracing::info!(
            job_name = %job.job_name,
            task_run_id = %task_run_id,
            db_classification = classification.db_classification,
            reason,
            "CoordinatorActor: backstop reaped orphaned task-run Job"
        );
    }
}

async fn list_sessions_for_task_run(
    db: &djinn_db::Database,
    task_run_id: &str,
) -> djinn_db::Result<Vec<djinn_core::models::SessionRecord>> {
    let session_repo =
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    session_repo.list_for_task_run(task_run_id).await
}

/// Startup/rollout pass for the task-run Job backstop. Server boot first marks
/// previously-running sessions interrupted via `interrupt_stale_sessions_on_startup`;
/// the coordinator then runs this immediate reconcile before waiting for the
/// normal stale-resource interval. The helper is idempotent and safe if startup
/// ordering changes: if the session row is still running, the Job is preserved;
/// if the row was interrupted/finalized or is absent, the Job is deleted.
pub(super) async fn reap_orphaned_taskrun_jobs_for_startup(
    db: &djinn_db::Database,
    app_state: &crate::context::CoordinatorContext,
) {
    tracing::info!("CoordinatorActor: running startup task-run Job backstop reconcile");
    reap_orphaned_taskrun_jobs(db, app_state, "startup").await;
}

struct TaskrunJobClassification {
    keep_job: bool,
    db_classification: &'static str,
}

fn classify_taskrun_job_owner(
    task_run: Option<&djinn_core::models::TaskRunRecord>,
    sessions: &[djinn_core::models::SessionRecord],
) -> TaskrunJobClassification {
    let has_live_session = sessions
        .iter()
        .any(|session| session.status == "running" && session.ended_at.is_none());

    if let Some(task_run) = task_run {
        if task_run.status == "running" && task_run.ended_at.is_none() && has_live_session {
            return TaskrunJobClassification {
                keep_job: true,
                db_classification: "live_running",
            };
        }

        if task_run.status == "running" {
            return TaskrunJobClassification {
                keep_job: false,
                db_classification: session_status_classification(sessions),
            };
        }

        return TaskrunJobClassification {
            keep_job: false,
            db_classification: taskrun_status_classification(&task_run.status),
        };
    }

    TaskrunJobClassification {
        keep_job: has_live_session,
        db_classification: if has_live_session {
            "live_session_without_task_run"
        } else {
            "absent"
        },
    }
}

/// Grace window before the backstop will reap a task-run Job whose DB owner
/// rows are still absent. Sized generously (pod scheduling + image pull +
/// worker boot can take minutes) because the worker only inserts the
/// `task_runs` row from inside the pod after boot; reaping sooner races the
/// worker and kills the session before it starts.
const TASKRUN_JOB_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The classifications that can legitimately appear during pod boot, before the
/// worker has inserted its DB owner rows: no `task_runs` row at all
/// (`"absent"`), or a running `task_runs` row whose session row hasn't been
/// created yet (`"task_run_running_without_session"`). Only these classes are
/// age-gated; every other non-`keep_job` class is a genuinely dead owner.
fn is_boot_race_classification(db_classification: &str) -> bool {
    matches!(
        db_classification,
        "absent" | "task_run_running_without_session"
    )
}

/// True when a Job is younger than `grace` and therefore still inside the
/// boot-race window. A missing `created_at` is treated as old (not within
/// grace) so the backstop still reaps Jobs it cannot age — preserving the
/// cleanup guarantee. A `created_at` in the future (clock skew) is treated as
/// within grace, since such a Job cannot yet have aged out.
fn job_within_reap_grace(
    created_at: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
    grace: std::time::Duration,
) -> bool {
    match created_at {
        Some(created) => match now.duration_since(created) {
            Ok(age) => age < grace,
            Err(_) => true,
        },
        None => false,
    }
}

fn taskrun_status_classification(status: &str) -> &'static str {
    match status {
        "completed" => "task_run_completed",
        "failed" => "task_run_failed",
        "interrupted" => "task_run_interrupted",
        "running" => "task_run_running_without_live_session",
        _ => "task_run_unknown_status",
    }
}

#[cfg(test)]
mod taskrun_backstop_grace_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Mirror of the loop's skip decision: a Job is spared only when its DB
    /// classification is a boot-race class AND it is still within the grace
    /// window.
    fn would_skip_reap(
        db_classification: &str,
        created_at: Option<SystemTime>,
        now: SystemTime,
        grace: Duration,
    ) -> bool {
        is_boot_race_classification(db_classification)
            && job_within_reap_grace(created_at, now, grace)
    }

    fn base() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn young_absent_job_is_kept() {
        let now = base();
        let created = now - Duration::from_secs(30);
        assert!(
            would_skip_reap("absent", Some(created), now, TASKRUN_JOB_REAP_GRACE),
            "a 30s-old Job with no task_runs row must be spared during boot"
        );
    }

    #[test]
    fn young_running_without_session_job_is_kept() {
        let now = base();
        let created = now - Duration::from_secs(60);
        assert!(
            would_skip_reap(
                "task_run_running_without_session",
                Some(created),
                now,
                TASKRUN_JOB_REAP_GRACE
            ),
            "a running task_run whose session row is not yet inserted must be spared during boot"
        );
    }

    #[test]
    fn old_absent_job_is_reaped() {
        let now = base();
        let created = now - (TASKRUN_JOB_REAP_GRACE + Duration::from_secs(1));
        assert!(
            !would_skip_reap("absent", Some(created), now, TASKRUN_JOB_REAP_GRACE),
            "an aged-out Job with no DB owner is genuinely orphaned and must be reaped"
        );
    }

    #[test]
    fn young_terminal_task_run_job_is_reaped() {
        let now = base();
        let created = now - Duration::from_secs(5);
        // Terminal task_run statuses are dead regardless of age.
        for class in [
            "task_run_completed",
            "task_run_failed",
            "task_run_interrupted",
            "session_interrupted",
            "session_completed",
        ] {
            assert!(
                !would_skip_reap(class, Some(created), now, TASKRUN_JOB_REAP_GRACE),
                "terminal classification {class} must be reaped even for a young Job"
            );
        }
    }

    #[test]
    fn missing_timestamp_is_treated_as_old() {
        let now = base();
        assert!(
            !job_within_reap_grace(None, now, TASKRUN_JOB_REAP_GRACE),
            "a Job with no creation timestamp must be eligible for reaping"
        );
    }

    #[test]
    fn future_timestamp_is_treated_as_young() {
        let now = base();
        let created = now + Duration::from_secs(120);
        assert!(
            job_within_reap_grace(Some(created), now, TASKRUN_JOB_REAP_GRACE),
            "clock skew (future creation time) must not make a Job eligible for reaping"
        );
    }

    #[test]
    fn boot_race_classification_membership() {
        assert!(is_boot_race_classification("absent"));
        assert!(is_boot_race_classification(
            "task_run_running_without_session"
        ));
        assert!(!is_boot_race_classification("task_run_completed"));
        assert!(!is_boot_race_classification(
            "task_run_running_without_live_session"
        ));
        assert!(!is_boot_race_classification("live_running"));
    }
}

// ─── Note association pruning ────────────────────────────────────────────────

impl CoordinatorActor {
    /// Prune stale, low-weight note associations for all projects.
    /// Called once per hour from the background tick.
    pub(super) async fn prune_note_associations(&self) {
        let project_repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        let projects = match project_repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list projects for association pruning");
                return;
            }
        };

        let note_repo = djinn_db::NoteRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        for project in projects {
            match note_repo.prune_associations(&project.id).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            project_id = %project.id,
                            deleted = count,
                            "CoordinatorActor: pruned stale note associations"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %project.id,
                        error = %e,
                        "CoordinatorActor: failed to prune note associations"
                    );
                }
            }

            // Refresh and prune embedding-related associations.
            let refresh_start = SystemClock::new().now_instant();
            match note_repo.refresh_embedding_associations(&project.id).await {
                Ok(stats) => {
                    let pruned = note_repo
                        .prune_embedding_associations(&project.id)
                        .await
                        .unwrap_or(0);
                    let elapsed_ms = refresh_start.elapsed().as_millis();
                    tracing::info!(
                        project_id = %project.id,
                        notes_scanned = stats.notes_scanned,
                        notes_missing_embeddings = stats.notes_missing_embeddings,
                        candidates_evaluated = stats.candidates_evaluated,
                        edges_upserted = stats.edges_upserted,
                        edges_pruned = pruned,
                        elapsed_ms = elapsed_ms,
                        "CoordinatorActor: refreshed embedding-related note associations"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %project.id,
                        error = %e,
                        "CoordinatorActor: failed to refresh embedding-related associations"
                    );
                }
            }
        }
    }
}

// Task #8: the `sweep_removes_worktree_for_closed_task` test used to cover
// the worktree-GC branch of `sweep_stale_resources`.  That branch no longer
// exists — the supervisor-driven dispatch path never creates task worktrees,
// so there is nothing to GC.  The per-project task-branch cleanup kept here
// is exercised end-to-end by the task_merge integration tests.

// ─── Stale task_run sweep ────────────────────────────────────────────────────

/// Flip `task_runs` rows still in `running` (with NULL `ended_at`) older than
/// [`STALE_TASK_RUN_THRESHOLD_SECS`] to `interrupted`.
///
/// Catches the residue from worker pods that died without flushing their
/// terminal `update_task_run_status` RPC (host crash mid-run, K8s SIGKILL
/// past the termination grace, network partition). The per-task teardown
/// reap in `supervisor_runner` covers the common case; this is the safety
/// net for paths it can't see (host restart while pods are mid-flight, etc).
async fn reap_stale_task_runs(db: &djinn_db::Database) {
    reap_stale_task_runs_with_threshold(db, STALE_TASK_RUN_THRESHOLD_SECS, "periodic").await;
}

/// Startup variant: aggressive (~10s threshold) because any `running` row
/// older than that at boot is from a prior process whose workers can no
/// longer reach us.
pub(super) async fn reap_stale_task_runs_for_startup(db: &djinn_db::Database) {
    reap_stale_task_runs_with_threshold(db, STARTUP_TASK_RUN_THRESHOLD_SECS, "startup").await;
}

async fn reap_stale_task_runs_with_threshold(
    db: &djinn_db::Database,
    threshold_secs: i64,
    reason: &'static str,
) {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::seconds(threshold_secs);
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let threshold_iso = match cutoff.format(&format) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "reap_stale_task_runs: failed to format threshold");
            return;
        }
    };

    let repo = djinn_db::repositories::task_run::TaskRunRepository::new(db.clone());
    match repo.reap_stale_running(&threshold_iso).await {
        Ok(ids) if !ids.is_empty() => {
            tracing::warn!(
                count = ids.len(),
                threshold = %threshold_iso,
                reason = reason,
                "CoordinatorActor: reaped stale 'running' task_runs"
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, reason = reason, "CoordinatorActor: reap_stale_task_runs failed");
        }
    }
}

// ─── Orphaned pending task_attempt sweep ─────────────────────────────────────

/// Finalize to `crashed` any `pending` `task_attempts` row older than
/// [`ORPHANED_PENDING_ATTEMPT_THRESHOLD_SECS`] whose task has no live
/// (`starting`/`running`) `task_run` and no `running` session.
///
/// Defense-in-depth backstop for the event-path terminalization in
/// `classify_session_exit_liveness`: a run that fails without ever emitting a
/// terminal session event (host crash mid-dispatch, session left unfinalized
/// by an early stage error, missed event during a coordinator restart) leaves
/// its dispatch-start `pending` attempt orphaned, and the respawn guard then
/// defers every future dispatch of that (task, role) pair — a permanent wedge.
///
/// State-driven off DB truth and idempotent: `advance_to_terminal` is
/// forward-only, so a row concurrently advanced by the normal lifecycle is
/// left untouched. `pending` orphans finalize to `crashed`. The same sweep also
/// finalizes `submitted` orphans **whose task carries no open PR** to
/// `reopened` (the ylme orphan): `submitted`-with-PR rows stay owned by the PR
/// poller's adoption/terminalization flow and are strictly untouched.
async fn reap_orphaned_pending_attempts(db: &djinn_db::Database) {
    reap_orphaned_pending_attempts_with_threshold(
        db,
        ORPHANED_PENDING_ATTEMPT_THRESHOLD_SECS,
        "periodic",
    )
    .await;
}

/// Startup variant of [`reap_orphaned_pending_attempts`]. Runs with the same
/// conservative threshold (a fresh boot proves nothing about a minutes-old
/// dispatch on another coordinator instance behind the same DB), but firing
/// at boot means long-orphaned rows self-heal immediately after a deploy
/// instead of waiting for the first periodic stale sweep.
pub(super) async fn reap_orphaned_pending_attempts_for_startup(db: &djinn_db::Database) {
    reap_orphaned_pending_attempts_with_threshold(
        db,
        ORPHANED_PENDING_ATTEMPT_THRESHOLD_SECS,
        "startup",
    )
    .await;
}

pub(super) async fn reap_orphaned_pending_attempts_with_threshold(
    db: &djinn_db::Database,
    threshold_secs: i64,
    reason: &'static str,
) {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::seconds(threshold_secs);
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let threshold_iso = match cutoff.format(&format) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "reap_orphaned_pending_attempts: failed to format threshold");
            return;
        }
    };

    let repo = djinn_db::TaskAttemptRepository::new(db.clone());
    let orphans = match repo.list_orphaned_pending(&threshold_iso).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                reason = reason,
                "CoordinatorActor: reap_orphaned_pending_attempts lookup failed"
            );
            return;
        }
    };

    // Outcome selection by reap reason. A `startup` reap fires immediately after
    // this coordinator boots: any `pending` attempt with no live task_run and no
    // running session at boot was orphaned because a DEPLOY/ROLLOUT killed its
    // pod out from under it — an ENVIRONMENTAL interruption, not a task failure.
    // Stamp it `Interrupted` so the dispatch reappearance path recognizes it and
    // skips the failure streak / cooldown escalation ("treat like it never ran").
    // The `periodic` reap is conservative — a pending attempt still orphaned after
    // the periodic threshold with no live run is more likely a genuine mid-run
    // loss, so it stays `Crashed` (both are is_infra ⇒ quality/park exempt; they
    // differ only in whether the reappearance streak counts them).
    let (orphan_outcome, orphan_failure_class, orphan_summary) = if reason == "startup" {
        (
            djinn_core::models::task_attempt::TaskAttemptOutcome::Interrupted,
            "environmental_interrupt_startup_reap",
            "orphaned pending attempt reaped at startup (deploy/rollout interrupted the run): \
             environmental non-attempt, no dispatch penalty",
        )
    } else {
        (
            djinn_core::models::task_attempt::TaskAttemptOutcome::Crashed,
            "orphaned_pending_attempt",
            "orphaned pending attempt reaped: no live task_run or running session",
        )
    };

    for orphan in orphans {
        let summary_json = serde_json::json!({
            "recovery_classifier": "orphaned_pending_attempt_reaper",
            "reason": reason,
            "threshold_secs": threshold_secs,
            "failure_class": orphan_failure_class,
        })
        .to_string();
        match repo
            .advance_to_terminal(djinn_db::TerminalTaskAttemptParams {
                id: &orphan.id,
                outcome: orphan_outcome,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some(orphan_summary),
                summary_json: Some(&summary_json),
                log_tail: None,
            })
            .await
        {
            Ok(updated) => {
                tracing::warn!(
                    attempt_id = %orphan.id,
                    task_id = %orphan.task_id,
                    role = %orphan.role,
                    dispatch_key = %orphan.dispatch_key,
                    attempt_created_at = %orphan.created_at,
                    threshold = %threshold_iso,
                    outcome = %updated.outcome,
                    reason = reason,
                    "CoordinatorActor: reaped orphaned pending task_attempt"
                );
            }
            Err(e) => {
                tracing::warn!(
                    attempt_id = %orphan.id,
                    task_id = %orphan.task_id,
                    role = %orphan.role,
                    error = %e,
                    reason = reason,
                    "CoordinatorActor: failed to reap orphaned pending task_attempt"
                );
            }
        }
    }

    // Also finalize orphaned `submitted` attempts the PR poller can never own.
    // The poller drives adoption/terminalization ONLY for the statuses it polls
    // (`pr_draft`/`pr_review`), keyed off `tasks.pr_url`. The old assumption
    // "has pr_url ⟹ poller owns it" is FALSE for `open` tasks: the true rule is
    // "the poller owns a `submitted` attempt only when the task is in a
    // poller-polled status". Two poller-blind classes are reaped here, each of
    // which otherwise hard-blocks the respawn guard's step-2 dedup forever:
    //   1. PR-less tasks — e.g. an internal task-review rejection that reopened
    //      the task without terminalizing the worker's `submitted` row (the
    //      ylme orphan); the poller never sees a PR-less task.
    //   2. `open` tasks (the sole dispatchable status) that RETAINED a stale
    //      `pr_url` across a `PrConflict` reopen — never polled, so nothing
    //      advances the `submitted` attempt and the task is permanently stuck
    //      `open` yet un-dispatchable behind the guard.
    // Finalize each to `reopened` (submitted work existed): that also makes the
    // guard's latest-attempt gate treat the task as rework, so a fresh worker
    // dispatches to redo it. `submitted` rows for poller-polled statuses
    // (`pr_draft`/`pr_review`) with a PR are strictly untouched (excluded by the
    // query). Forward-only + idempotent.
    let submitted_orphans = match repo.list_orphaned_submitted_unowned(&threshold_iso).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                reason = reason,
                "CoordinatorActor: reap_orphaned_submitted_unowned lookup failed"
            );
            return;
        }
    };

    for orphan in submitted_orphans {
        let summary_json = serde_json::json!({
            "recovery_classifier": "orphaned_submitted_unowned_reaper",
            "reason": reason,
            "threshold_secs": threshold_secs,
            "failure_class": "orphaned_submitted_unowned",
        })
        .to_string();
        match repo
            .advance_to_terminal(djinn_db::TerminalTaskAttemptParams {
                id: &orphan.id,
                outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some(
                    "orphaned submitted attempt (poller-unowned: no PR, or retained-PR open task) reaped: reopened for a fresh worker",
                ),
                summary_json: Some(&summary_json),
                log_tail: None,
            })
            .await
        {
            Ok(updated) => {
                tracing::warn!(
                    attempt_id = %orphan.id,
                    task_id = %orphan.task_id,
                    role = %orphan.role,
                    dispatch_key = %orphan.dispatch_key,
                    attempt_created_at = %orphan.created_at,
                    threshold = %threshold_iso,
                    outcome = %updated.outcome,
                    reason = reason,
                    "CoordinatorActor: reaped poller-unowned submitted task_attempt to reopened"
                );
            }
            Err(e) => {
                tracing::warn!(
                    attempt_id = %orphan.id,
                    task_id = %orphan.task_id,
                    role = %orphan.role,
                    error = %e,
                    reason = reason,
                    "CoordinatorActor: failed to reap poller-unowned submitted task_attempt"
                );
            }
        }
    }
}

#[cfg(test)]
mod cache_cleanup_cross_path_tests {
    use super::*;
    use crate::context::{CacheCleanupConfig, CacheCleanupMode};

    fn cache_metric_value(metric: &str, labels: &[(&str, &str)]) -> f64 {
        djinn_telemetry::init().unwrap();
        djinn_telemetry::render()
            .unwrap()
            .lines()
            .find_map(|line| {
                let (sample, value) = line.split_once('}')?;
                (sample.starts_with(&format!("{metric}{{"))
                    && labels
                        .iter()
                        .all(|(key, value)| sample.contains(&format!("{key}=\"{value}\""))))
                .then(|| value.trim().parse::<f64>().ok())?
            })
            .unwrap_or(0.0)
    }

    fn backdate(path: &Path, days: u64) {
        let spec = format!("{days} days ago");
        assert!(
            std::process::Command::new("touch")
                .args(["-d", &spec, path.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn dry_run_and_delete_select_same_cross_path_cache_candidates() {
        use djinn_telemetry::cache_cleanup as metrics;

        // Install the global recorder before either sweep emits counters; metrics
        // sent to the default no-op recorder before initialization are discarded.
        djinn_telemetry::init().unwrap();

        let db = crate::test_helpers::create_test_db();
        let _project = crate::test_helpers::create_test_project(&db).await;
        let tmp = tempfile::TempDir::new().unwrap();
        let sccache = tmp.path().join("sccache");
        let runs = tmp.path().join("cargo-target-runs");
        std::fs::create_dir(&sccache).unwrap();
        std::fs::create_dir(&runs).unwrap();
        let cache_file = sccache.join("stale.cache");
        let stale_dir = runs.join("stale-malformed-dir");
        let stale_file = runs.join("stale-loose-file");
        std::fs::write(&cache_file, b"sccache").unwrap();
        std::fs::create_dir(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("artifact"), b"dir debris").unwrap();
        std::fs::write(&stale_file, b"file debris").unwrap();
        backdate(&cache_file, 2);
        backdate(&stale_dir, 30);
        backdate(&stale_file, 30);

        let dry_run = CacheCleanupConfig {
            mode: CacheCleanupMode::DryRun,
            sccache_enabled: true,
            sccache_max_age_hours: 1,
            cargo_debris_enabled: true,
            cargo_debris_max_age_days: 7,
            ..CacheCleanupConfig::default()
        };
        assert_eq!(
            sweep_sccache_guard_under(&dry_run, &sccache).await,
            SccacheSweepOutcome::DryRunReported
        );
        let dry = sweep_orphaned_cargo_target_run_dirs_under(&db, &runs, &dry_run).await;
        assert_eq!(
            dry.errors, 0,
            "dry stats: scanned={}, retained={}",
            dry.scanned, dry.retained
        );
        assert_eq!((dry.malformed_dir_deleted, dry.loose_file_deleted), (1, 1));
        assert_eq!(dry.debris_bytes_deleted, 0);
        assert!(
            sccache.is_dir() && cache_file.exists() && stale_dir.is_dir() && stale_file.exists()
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_total",
                &[
                    ("component", metrics::COMPONENT_SCCACHE),
                    ("outcome", metrics::OUTCOME_DRY_RUN),
                    ("mode", metrics::MODE_DRY_RUN)
                ],
            ) >= 1.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_total",
                &[
                    ("component", metrics::COMPONENT_CARGO_TARGET_RUNS),
                    ("outcome", metrics::OUTCOME_MALFORMED_DIR_DELETED),
                    ("mode", metrics::MODE_DRY_RUN)
                ],
            ) >= 1.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_candidates_total",
                &[
                    ("component", metrics::COMPONENT_SCCACHE),
                    ("mode", metrics::MODE_DRY_RUN)
                ],
            ) >= 1.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_candidates_total",
                &[
                    ("component", metrics::COMPONENT_CARGO_TARGET_RUNS),
                    ("mode", metrics::MODE_DRY_RUN)
                ],
            ) >= 2.0
        );

        let delete = CacheCleanupConfig {
            mode: CacheCleanupMode::Delete,
            ..dry_run
        };
        assert_eq!(
            sweep_sccache_guard_under(&delete, &sccache).await,
            SccacheSweepOutcome::Deleted
        );
        let deleted = sweep_orphaned_cargo_target_run_dirs_under(&db, &runs, &delete).await;
        assert_eq!(
            (deleted.malformed_dir_deleted, deleted.loose_file_deleted),
            (dry.malformed_dir_deleted, dry.loose_file_deleted)
        );
        assert!(deleted.debris_bytes_deleted > 0);
        assert!(!sccache.exists() && !stale_dir.exists() && !stale_file.exists());
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_total",
                &[
                    ("component", metrics::COMPONENT_SCCACHE),
                    ("outcome", metrics::OUTCOME_DELETED),
                    ("mode", metrics::MODE_DELETE)
                ],
            ) >= 1.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_total",
                &[
                    ("component", metrics::COMPONENT_CARGO_TARGET_RUNS),
                    ("outcome", metrics::OUTCOME_LOOSE_FILE_DELETED),
                    ("mode", metrics::MODE_DELETE)
                ],
            ) >= 1.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_reclaimed_bytes_total",
                &[
                    ("component", metrics::COMPONENT_SCCACHE),
                    ("mode", metrics::MODE_DELETE)
                ],
            ) >= 7.0
        );
        assert!(
            cache_metric_value(
                "djinn_cache_cleanup_reclaimed_bytes_total",
                &[
                    ("component", metrics::COMPONENT_CARGO_TARGET_RUNS),
                    ("mode", metrics::MODE_DELETE)
                ],
            ) >= deleted.debris_bytes_deleted as f64
        );
    }
}

/// Inventory and evict idle warm bases during the leader-owned maintenance
/// sweep.  The idle evictor derives activity DB-first, falls back to directory
/// mtime, and acquires a non-blocking per-base lock before any destructive work,
/// rechecking safety while the lock is held.  Both dry-run and delete modes
/// select the same candidates; dry-run reports projected bytes, delete reports
/// reclaimed bytes.
async fn sweep_cargo_warm_base_guard(
    db: &djinn_db::Database,
    config: &crate::context::CacheCleanupConfig,
    warm_job_guard: Option<Arc<dyn crate::cargo_warm_base_gc::WarmJobGuard>>,
) {
    use crate::cargo_warm_base_gc as gc;
    use djinn_core::clock::SystemClock;
    use djinn_telemetry::cache_cleanup as metrics;
    let guard: Arc<dyn gc::WarmJobGuard> =
        warm_job_guard.unwrap_or_else(|| Arc::new(gc::UnavailableWarmJobGuard));
    let inventory = match gc::inventory_under(Path::new(gc::CARGO_WARM_BASE_ROOT)) {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::warn!(error = %error, root = gc::CARGO_WARM_BASE_ROOT, "warm-base GC inventory failed; retaining bases");
            metrics::increment_cleanup_total(
                metrics::COMPONENT_CARGO_WARM_BASE,
                metrics::OUTCOME_ERROR,
                config.mode.as_metric_label(),
            );
            return;
        }
    };
    let inventory_count = inventory.entries.len() as u64;
    if inventory_count > 0 {
        metrics::increment_candidates(
            metrics::COMPONENT_CARGO_WARM_BASE,
            config.mode.as_metric_label(),
            inventory_count,
        );
    }
    let clock = SystemClock::new();
    let locks = gc::FlockBaseLock;
    let activity = gc::DbActivityGuard::new(db.clone());

    // Take the side-effect-free fingerprint snapshot before either whole-base
    // eviction phase. Delete mode can remove an idle or pressure candidate,
    // whereas dry-run preserves it; collecting first keeps the report-only
    // unit count and projected bytes mode-parity independent. The sweep still
    // uses the same activity, warm-job, and per-base lock guards as eviction.
    let fingerprint_inventory = match gc::inventory_under(Path::new(gc::CARGO_WARM_BASE_ROOT)) {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::warn!(
                component = metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
                mode = config.mode.as_metric_label(),
                error = %error,
                "fingerprint report-only sweep inventory failed; skipping"
            );
            metrics::increment_cleanup_total(
                metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
                metrics::OUTCOME_ERROR,
                config.mode.as_metric_label(),
            );
            return;
        }
    };
    let fingerprint_locks = gc::BaseLockPlanningAdapter::new(gc::FlockBaseLock);
    gc::report_only_fingerprint_sweep(
        fingerprint_inventory,
        &activity,
        guard.as_ref(),
        &fingerprint_locks,
        config.mode,
    )
    .await;

    let result = gc::evict_idle_warm_bases(
        inventory,
        &activity,
        guard.as_ref(),
        &locks,
        config,
        &clock,
        config.mode,
        Path::new(gc::CARGO_WARM_BASE_ROOT),
    )
    .await;
    if result.reclaimed_bytes > 0 {
        metrics::record_reclaimed_bytes(
            metrics::COMPONENT_CARGO_WARM_BASE,
            config.mode.as_metric_label(),
            result.reclaimed_bytes,
        );
    }
    if result.projected_bytes > 0 {
        tracing::info!(
            component = metrics::COMPONENT_CARGO_WARM_BASE,
            mode = config.mode.as_metric_label(),
            projected_bytes = result.projected_bytes,
            "warm-base idle GC projected bytes"
        );
    }
    tracing::info!(
        component = metrics::COMPONENT_CARGO_WARM_BASE,
        mode = config.mode.as_metric_label(),
        deleted = result.deleted.len(),
        dry_run = result.dry_run.len(),
        retained = result.retained.len(),
        reclaimed_bytes = result.reclaimed_bytes,
        projected_bytes = result.projected_bytes,
        "warm-base idle GC completed"
    );

    let pressure_inventory = match gc::inventory_under(Path::new(gc::CARGO_WARM_BASE_ROOT)) {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::warn!(component = metrics::COMPONENT_CARGO_WARM_BASE, mode = config.mode.as_metric_label(), error = %error, "warm-base pressure GC inventory failed; retaining bases");
            metrics::increment_cleanup_total(
                metrics::COMPONENT_CARGO_WARM_BASE,
                metrics::OUTCOME_ERROR,
                config.mode.as_metric_label(),
            );
            return;
        }
    };
    let planning_locks = gc::BaseLockPlanningAdapter::new(gc::FlockBaseLock);
    let dry_run_planning_locks = gc::NoopLockGuard;
    // Flock creates a lock file, so a dry-run must use the non-mutating
    // availability policy and leave base mtimes untouched.
    let planning_locks: &dyn gc::BaseLockGuard = match config.mode {
        crate::context::CacheCleanupMode::DryRun => &dry_run_planning_locks,
        crate::context::CacheCleanupMode::Delete => &planning_locks,
    };
    let capacity = gc::StatvfsFilesystemCapacity;
    let plan = gc::plan_pressure_eviction(
        pressure_inventory,
        &activity,
        guard.as_ref(),
        planning_locks,
        &capacity,
        config,
        &clock,
    )
    .await;
    if !plan.candidates.is_empty() {
        metrics::increment_candidates(
            metrics::COMPONENT_CARGO_WARM_BASE,
            config.mode.as_metric_label(),
            plan.candidates.len() as u64,
        );
    }
    let pressure = gc::execute_pressure_eviction(
        plan,
        &activity,
        guard.as_ref(),
        &locks,
        &capacity,
        config,
        &clock,
        config.mode,
        Path::new(gc::CARGO_WARM_BASE_ROOT),
    )
    .await;
    gc::log_pressure_eviction_completion(&pressure, config.mode);
}
