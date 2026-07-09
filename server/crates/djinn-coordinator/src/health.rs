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
/// Conservative: dispatch-start → session/run creation is seconds, so a
/// 15-minute-old pending attempt with nothing executing behind it can never
/// be advanced by the normal lifecycle paths. Left un-reaped it hard-blocks
/// the respawn guard for its (task, role) pair forever (the guard defers any
/// dispatch while a `pending`/`submitted` attempt exists).
const ORPHANED_PENDING_ATTEMPT_THRESHOLD_SECS: i64 = 15 * 60;

const CARGO_TARGET_RUNS_ROOT: &str = djinn_supervisor::CARGO_TARGET_RUNS_ROOT;

/// Default durable output-stash retention window for coordinator maintenance.
///
/// Terminal-session stash pointers are eligible for GC after 30 days by
/// default. Operators may override this with
/// `DJINN_OUTPUT_STASH_GC_RETENTION_DAYS` (whole days, minimum 1).
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
    sweep_orphaned_cargo_target_run_dirs(db, app_state.cargo_target_runs_root.as_deref()).await;
    sweep_durable_output_stash(db).await;
    sweep_cargo_health().await;

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
                Ok(Some(task)) => task,
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
                    djinn_core::models::Task {
                        id: String::new(),
                        project_id: project.id.clone(),
                        short_id: short_id.to_string(),
                        epic_id: None,
                        title: String::new(),
                        description: String::new(),
                        design: String::new(),
                        issue_type: String::new(),
                        status: "closed".to_string(),
                        priority: 0,
                        owner: String::new(),
                        labels: "[]".to_string(),
                        acceptance_criteria: "[]".to_string(),
                        reopen_count: 0,
                        continuation_count: 0,
                        total_reopen_count: 0,
                        intervention_count: 0,
                        last_intervention_at: None,
                        created_at: "1970-01-01T00:00:00Z".to_string(),
                        updated_at: "1970-01-01T00:00:00Z".to_string(),
                        closed_at: Some("1970-01-01T00:00:00Z".to_string()),
                        close_reason: None,
                        merge_commit_sha: None,
                        pr_url: Some(pr.html_url.clone()),
                        merge_conflict_metadata: None,
                        memory_refs: "[]".to_string(),
                        agent_type: None,
                        created_by_user_id: None,
                        ci_status: djinn_core::models::CiStatus::Unknown.to_string(),
                        ci_head_sha: None,
                        ci_pr_number: None,
                        ci_blocking_required_check_names: "[]".to_string(),
                        ci_failure_fingerprint: None,
                        ci_first_seen_at: None,
                        ci_last_seen_at: None,
                        ci_same_signature_count: 0,
                        ci_last_remediation_base_sha: None,
                        ci_mirror_head_sha: None,
                        ci_github_head_sha: None,
                        ci_heads_diverged: None,
                        ci_head_observation_error: None,
                        unresolved_blocker_count: 0,
                    }
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
            if task.status != "closed" {
                tracing::debug!(
                    project_id = %project.id,
                    pr = pr.number,
                    head = %head_branch,
                    task_status = %task.status,
                    "CoordinatorActor: stale PR sweep skipping PR whose task is still open"
                );
                stats.prs_skipped += 1;
                djinn_telemetry::stale_sweep::increment_pr_skipped(
                    djinn_telemetry::stale_sweep::REASON_TASK_OPEN,
                );
                continue;
            }

            // Run guardrail checks via PrCleanupPolicy.
            match cleanup_policy.should_cleanup_pr(&task, pr).await {
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
                .delete_branch_if_allowed(&task, head_branch)
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

            // ── Task activity log entry ─────────────────────────────────
            let activity_payload = serde_json::json!({
                "event": "stale_pr_swept",
                "pr_number": pr.number,
                "pr_url": pr.html_url,
                "branch": head_branch,
                "project_id": project.id,
            });
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task.id),
                    "system",
                    "system",
                    "stale_pr_swept",
                    &activity_payload.to_string(),
                )
                .await
            {
                tracing::warn!(
                    error = %e,
                    project_id = %project.id,
                    task_id = %task.id,
                    pr = pr.number,
                    "CoordinatorActor: stale PR sweep failed to write activity log; continuing"
                );
                // Non-fatal — the PR is already closed.
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

// ─── Cargo target run-dir GC ────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct CargoTargetRunDirSweepStats {
    pub(super) scanned: usize,
    pub(super) deleted: usize,
    pub(super) retained: usize,
    pub(super) errors: usize,
    /// Dirs LRU-trimmed by the state-independent hard cap (oldest-first).
    pub(super) cap_trimmed: usize,
    /// Per-entry errors during the hard-cap trim.
    pub(super) cap_errors: usize,
}

async fn sweep_orphaned_cargo_target_run_dirs(db: &djinn_db::Database, root: Option<&Path>) {
    // Production wiring sets `cargo_target_runs_root` explicitly (the server pod
    // mounts the shared cache PVC at `$DJINN_HOME/cache`, NOT the Job-pod
    // `/cache` path the [`CARGO_TARGET_RUNS_ROOT`] constant names). The
    // fallback only fires in tests/contexts that don't set it.
    let root = match root {
        Some(root) => root,
        None => Path::new(CARGO_TARGET_RUNS_ROOT),
    };
    sweep_orphaned_cargo_target_run_dirs_under(db, root).await;
}

pub(super) async fn sweep_orphaned_cargo_target_run_dirs_under(
    db: &djinn_db::Database,
    root: &Path,
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

        let Some(task_run_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            stats.retained += 1;
            tracing::debug!(
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored non-UTF8 entry name"
            );
            continue;
        };

        if uuid::Uuid::parse_str(&task_run_id).is_err() {
            stats.retained += 1;
            tracing::debug!(
                task_run_id = %task_run_id,
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored malformed task_run_id entry"
            );
            continue;
        }

        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to inspect entry; continuing"
                );
                continue;
            }
        };

        if !file_type.is_dir() {
            stats.retained += 1;
            tracing::debug!(
                task_run_id = %task_run_id,
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored non-directory entry"
            );
            continue;
        }

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
                    cleanup_outcome = "removed",
                    deleted_count = 1_u64,
                    error_count = 0_u64,
                    "CoordinatorActor: deleted orphaned cargo target run-dir"
                );
            }
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    cleanup_outcome = "failed",
                    deleted_count = 0_u64,
                    error_count = 1_u64,
                    "CoordinatorActor: failed to delete orphaned cargo target run-dir; continuing"
                );
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

fn taskrun_status_classification(status: &str) -> &'static str {
    match status {
        "completed" => "task_run_completed",
        "failed" => "task_run_failed",
        "interrupted" => "task_run_interrupted",
        "running" => "task_run_running_without_live_session",
        _ => "task_run_unknown_status",
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
