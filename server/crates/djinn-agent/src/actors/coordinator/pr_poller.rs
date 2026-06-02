use djinn_core::models::{Task, TransitionAction};
use djinn_db::{ActivityQuery, SessionAuthRepository, UserSettingsRepository};
use djinn_provider::github_api::{
    ActionsJob, CheckRun, DbBackedRefresher, GitHubApiClient, MergeMethod, PrReview,
    PrReviewFeedback, PrState, PullRequest, UserTokenExpired,
};
use djinn_provider::oauth::github_app_user;

use super::*;

/// Maximum number of review-fix rounds before escalating to Lead/Architect.
///
/// After this many PR review cycles without approval, the task is escalated
/// rather than re-dispatching a worker.
const PR_REVIEW_ROUND_THRESHOLD: u32 = 3;

/// Minimum seconds a task must have been in `pr_draft` before the poller will
/// check CI and potentially undraft it.  This prevents a race where the poller
/// runs before GitHub has registered the required check-runs for a newly-pushed
/// commit, sees an empty/stale check-run list, and incorrectly concludes CI
/// has passed.
const PR_DRAFT_MIN_AGE_SECS: i64 = 10;

/// Maximum consecutive merge failures before the poller invalidates its CI
/// cache and forces a full re-check.  This catches cases where CI failed
/// after we cached a "green" SHA, or where branch-protection rules block
/// the merge for reasons we didn't anticipate.
const MERGE_RETRY_RECHECK_THRESHOLD: u32 = 3;

/// Maximum number of distinct failing workflow runs whose jobs/steps are
/// aggregated into a single CI-failure rework comment. Failures frequently
/// span multiple workflow runs (e.g. separate `CI` and `Release` workflows on
/// the same SHA); we union all of them so the worker fixes every failure in one
/// pass instead of whack-a-mole. Bounded to keep the Actions API fan-out and
/// the comment size sane; if more runs failed we cap here and signal it.
const MAX_AGGREGATED_CI_RUNS: usize = 5;

/// Activity log event type for stored PR review feedback payloads.
///
/// Re-exported so the worker lifecycle layer can query for PR review feedback
/// without a module dependency on the coordinator's internal pr_poller.
pub const PR_REVIEW_FEEDBACK_EVENT: &str = "pr_review_feedback";

/// Activity log event type for per-cycle markers (used to count rounds).
const PR_REVIEW_CYCLE_EVENT: &str = "pr_review_cycle";

/// Activity log event type for per-cycle markers on the CI-failure rework path
/// (the analogue of `PR_REVIEW_CYCLE_EVENT` for the CI loop). Used to count how
/// many times a task has been kicked back for CI failures so the loop can
/// escalate instead of redispatching forever.
const PR_CI_CYCLE_EVENT: &str = "pr_ci_cycle";

/// Maximum CI-failure rework cycles before escalating to the Planner and
/// force-closing the task. Beyond this the worker is demonstrably unable to
/// turn the required checks green (commonly because the *real* failures are
/// non-required preview/deploy infra the diff can't touch), so looping is
/// pointless.
const PR_CI_FAILURE_THRESHOLD: u32 = 3;

impl CoordinatorActor {
    /// Poll GitHub for PR status on all tasks in `pr_draft` and `pr_review` states.
    ///
    /// Runs on every 30-second tick. Returns immediately when no tasks in either
    /// status exist so no GitHub API calls are made during idle periods.
    ///
    /// **`pr_draft` lifecycle** (CI monitoring):
    /// - PR merged → `PrMerge` → closed.
    /// - PR closed without merge → `ForceClose`.
    /// - CI checks still running → skip, check next tick.
    /// - CI checks failed → `PrCiFailed` → open (with CI details logged to activity).
    /// - CI checks passed + merge conflicts → `PrConflict` → open.
    /// - CI checks passed + no conflicts → undraft PR via GitHub API, then `PrUndraft` → pr_review.
    ///
    /// **`pr_review` lifecycle** (review monitoring):
    /// - PR merged → `PrMerge` → closed.
    /// - PR closed without merge → `ForceClose`.
    /// - Changes requested → `PrChangesRequested` → open (review feedback logged to activity).
    /// - Review round >= threshold → escalate to Architect.
    /// - Approved + mergeable → squash merge, `PrMerge` → closed.
    /// - Pending reviews → wait.
    pub(super) async fn poll_pr_statuses(&mut self) {
        // PR polling runs outside any MCP request scope, so there is no
        // `SESSION_USER_TOKEN` task-local to read. Each task's GitHub client
        // is built from its project's GitHub App installation token
        // (resolved per-task inside the loops below).
        self.poll_pr_draft_tasks().await;
        self.poll_pr_review_tasks().await;
    }

    // ── pr_draft polling (CI monitoring) ─────────────────────────────────────

    /// Poll tasks in `pr_draft` status: wait for CI to pass, then undraft the PR.
    async fn poll_pr_draft_tasks(&mut self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let pr_draft_tasks = match task_repo.list_by_status("pr_draft").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query pr_draft tasks");
                return;
            }
        };

        let tasks_with_pr: Vec<_> = pr_draft_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} pr_draft task(s)",
            tasks_with_pr.len()
        );

        for task in tasks_with_pr {
            // ── Minimum-age guard ───────────────────────────────────────────
            // Skip tasks that just entered pr_draft — GitHub needs a few
            // seconds to register workflow check-runs for the new commit.
            let first_seen = *self
                .pr_draft_first_seen
                .entry(task.id.clone())
                .or_insert_with(StdInstant::now);
            let age = first_seen.elapsed();
            if age < Duration::from_secs(PR_DRAFT_MIN_AGE_SECS as u64) {
                tracing::debug!(
                    task_id = %task.short_id,
                    age_secs = age.as_secs(),
                    "PR poller: pr_draft task too young, waiting for check-runs to register"
                );
                continue;
            }

            let pr_url = task.pr_url.as_deref().unwrap();
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR poller: unrecognised PR URL format, skipping"
                );
                continue;
            };

            let gh_client = match resolve_installation_client(&project_repo, &task.project_id).await
            {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        project_id = %task.project_id,
                        "PR poller: no installation_id on project row; skipping (legacy project?)"
                    );
                    continue;
                }
            };
            let gh_client = &gh_client;

            // Fetch current PR state + CI check runs.
            let (pr, checks) = match gh_client.get_pull_request(&owner, &repo, pull_number).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR status"
                    );
                    continue;
                }
            };

            // ── Merged? ───────────────────────────────────────────────────────
            if pr.merged == Some(true) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR merged → closing task"
                );
                self.apply_pr_transition(&task.id, TransitionAction::PrMerge, None)
                    .await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                continue;
            }

            // ── PR closed without merge ───────────────────────────────────────
            if pr.state == PrState::Closed {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR closed without merge → force-closing task"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::ForceClose,
                    Some("PR was closed without merging"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                continue;
            }

            // ── CI checks ─────────────────────────────────────────────────────
            if checks.check_runs.is_empty() {
                // No checks exist — repo has no CI configured.  Since the
                // minimum-age guard above already elapsed, treat as green.
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: no CI check-runs found after min-age guard — treating as passed"
                );
            } else {
                let all_completed = checks.check_runs.iter().all(|cr| cr.status == "completed");
                if !all_completed {
                    // CI checks still running — skip, check next tick.
                    continue;
                }
                // All completed — check for failures.
                let failed_checks: Vec<&CheckRun> = checks
                    .check_runs
                    .iter()
                    .filter(|cr| {
                        matches!(
                            cr.conclusion.as_deref(),
                            Some("failure") | Some("timed_out") | Some("cancelled")
                        )
                    })
                    .collect();
                if !failed_checks.is_empty() {
                    // Route through the shared CI-failure handler: it filters
                    // to *blocking* (required) checks, short-circuits the
                    // diff-empty re-emit, and caps the rework loop with
                    // escalation. Returns `true` when it consumed the event
                    // (reworked or force-closed); `false` when every failure
                    // was non-blocking and we should fall through as if CI
                    // passed.
                    let handled = self
                        .handle_ci_failure(
                            gh_client,
                            &task,
                            &pr,
                            &failed_checks,
                            pr_url,
                            &owner,
                            &repo,
                            pull_number,
                        )
                        .await;
                    if handled {
                        self.pr_status_cache.remove(&task.id);
                        self.pr_draft_first_seen.remove(&task.id);
                        continue;
                    }
                    // else: only advisory checks failed — fall through to the
                    // undraft path below as if CI is green.
                }
            }

            // CI passed (or no CI configured). Check for merge conflicts before undrafting.
            if pr.mergeable == Some(false) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: draft PR has merge conflicts → reopening task for rework"
                );
                let reason = self
                    .build_pr_conflict_reason(&task.short_id, &task.project_id)
                    .await;
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrConflict,
                    Some(&reason),
                )
                .await;
                // Reactive auto-blocker: if exactly one racing same-epic sibling
                // is landing on main, make this task WAIT for it (beside the
                // reopen above) instead of looping on the moving main.
                self.add_conflict_blocker_for_sibling(&task).await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                continue;
            }

            // All CI passed and no merge conflicts — undraft the PR, then transition.
            tracing::info!(
                task_id = %task.short_id,
                pr = pull_number,
                "PR poller: CI passed on draft PR → undrafting and marking ready for review"
            );
            match gh_client.mark_pr_ready_for_review(&pr.node_id).await {
                Ok(_) => {
                    // Best-effort: enable auto-merge so GitHub handles
                    // approval-and-checks gating + merge-queue enqueue.
                    // If it fails (repo doesn't allow auto-merge, branch
                    // protection refuses, etc.) the pr_review loop falls
                    // back to the legacy REST merge path.
                    enable_auto_merge_best_effort(
                        gh_client,
                        &task.short_id,
                        pull_number,
                        &pr.node_id,
                        &pr.title,
                    )
                    .await;

                    self.apply_pr_transition(&task.id, TransitionAction::PrUndraft, None)
                        .await;
                    self.pr_status_cache.remove(&task.id);
                    self.pr_draft_first_seen.remove(&task.id);
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        error = %e,
                        "PR poller: failed to undraft PR (will retry next tick)"
                    );
                    // Don't transition — will retry next tick.
                }
            }
        }
    }

    // ── pr_review polling (review monitoring) ────────────────────────────────

    /// Poll tasks in `pr_review` status: wait for reviewer approval or changes, then merge.
    async fn poll_pr_review_tasks(&mut self) {
        let task_repo = self.task_repo();
        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let pr_review_tasks = match task_repo.list_by_status("pr_review").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query pr_review tasks");
                return;
            }
        };

        let tasks_with_pr: Vec<_> = pr_review_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} pr_review task(s)",
            tasks_with_pr.len()
        );

        for task in tasks_with_pr {
            let pr_url = task.pr_url.as_deref().unwrap();
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR poller: unrecognised PR URL format, skipping"
                );
                continue;
            };

            let gh_client = match resolve_installation_client(&project_repo, &task.project_id).await
            {
                Some(c) => c,
                None => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        project_id = %task.project_id,
                        "PR poller: no installation_id on project row; skipping (legacy project?)"
                    );
                    continue;
                }
            };
            let gh_client = &gh_client;

            // Fetch current PR state + CI check runs.
            let (pr, checks) = match gh_client.get_pull_request(&owner, &repo, pull_number).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR status"
                    );
                    continue;
                }
            };

            let current_sha = pr.head.sha.clone();

            // ── Merged? ───────────────────────────────────────────────────────
            if pr.merged == Some(true) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR merged → closing task"
                );
                self.apply_pr_transition(&task.id, TransitionAction::PrMerge, None)
                    .await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                continue;
            }

            // ── PR closed without merge ───────────────────────────────────────
            if pr.state == PrState::Closed {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR closed without merge → force-closing task"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::ForceClose,
                    Some("PR was closed without merging"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                continue;
            }

            // ── Review state ──────────────────────────────────────────────────
            let reviews = match gh_client
                .list_pr_review_states(&owner, &repo, pull_number)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR reviews, will retry next tick"
                    );
                    continue;
                }
            };

            // Collapse the full review history into the EFFECTIVE gating
            // decision (latest standing review per reviewer), mirroring
            // GitHub's own `reviewDecision`. GitHub keeps every review ever
            // submitted in this list and never flips a `CHANGES_REQUESTED`
            // review's `state` when later commits or approvals land — so a
            // stale CHANGES_REQUESTED that the same reviewer subsequently
            // dismissed or turned into APPROVED would otherwise match forever
            // and bounce the task back to the worker every cycle (task 2sq6:
            // PR approved + mergeable on GitHub, yet looped for hours). We
            // compute `has_approved` here too so both signals come from the
            // same deduped view.
            let (changes_requested, has_approved) = effective_review_decision(&reviews);

            if changes_requested {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: reviewer requested changes → dispatching review feedback loop"
                );

                // Fetch aggregated feedback (reviews + inline comments).
                let feedback = match gh_client
                    .fetch_pr_review_feedback(&owner, &repo, pull_number, pr_url)
                    .await
                {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "PR poller: failed to fetch PR review feedback, using empty"
                        );
                        PrReviewFeedback {
                            pull_number,
                            pr_url: pr_url.to_owned(),
                            change_request_reviews: Vec::new(),
                            inline_comments: Vec::new(),
                        }
                    }
                };

                // Attach feedback + handle escalation threshold.
                self.attach_pr_review_feedback(&task.id, &task.short_id, feedback)
                    .await;

                // Evaluate CI in the SAME tick so a PR that has BOTH reviewer
                // "changes requested" AND failing required checks surfaces both
                // problems to the worker in one rework cycle (instead of
                // reviewer-this-tick / CI-next-tick, which trickled the two
                // signals across many dispatches). We log the CI-failure comment
                // here too, but deliberately do NOT emit a second reopen — the
                // single `PrChangesRequested` transition below carries both
                // feedback artifacts. We use `log_ci_failure_comment` directly
                // (not `handle_ci_failure`) to avoid double-transitioning and to
                // skip the CI-specific cycle-cap/diff-empty escalation: the
                // reviewer's request is the primary driver of this reopen.
                let blocking_failed: Vec<&CheckRun> = checks
                    .check_runs
                    .iter()
                    .filter(|cr| {
                        matches!(
                            cr.conclusion.as_deref(),
                            Some("failure") | Some("timed_out") | Some("cancelled")
                        )
                    })
                    .collect();
                if !blocking_failed.is_empty() {
                    let required_contexts: Option<Vec<String>> = gh_client
                        .list_required_status_checks(&owner, &repo, &pr.base.ref_name)
                        .await
                        .ok()
                        .flatten();
                    let blocking =
                        blocking_failed_checks(&blocking_failed, required_contexts.as_deref());
                    if !blocking.is_empty() {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            blocking_count = blocking.len(),
                            "PR poller: changes requested AND required CI failing in same tick → logging both feedbacks before single reopen"
                        );
                        self.log_ci_failure_comment(
                            &task.id,
                            &blocking,
                            pr_url,
                            &current_sha,
                            gh_client,
                            &owner,
                            &repo,
                        )
                        .await;
                    }
                }

                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrChangesRequested,
                    Some("Reviewer requested changes on PR"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                continue;
            }

            // ── CI checks on review PR (cached per head SHA) ──────────────────
            // Only skip CI re-check if the SHA hasn't changed AND we previously
            // confirmed all checks completed successfully.  If checks were still
            // in-progress last time we looked, we must re-check.
            let sha_changed = self
                .pr_status_cache
                .get(&task.id)
                .map(|cached| cached != &current_sha)
                .unwrap_or(true);

            if (sha_changed || !self.pr_status_cache.contains_key(&task.id))
                && !checks.check_runs.is_empty()
            {
                let all_completed = checks.check_runs.iter().all(|cr| cr.status == "completed");

                let failed_checks: Vec<&CheckRun> = checks
                    .check_runs
                    .iter()
                    .filter(|cr| {
                        matches!(
                            cr.conclusion.as_deref(),
                            Some("failure") | Some("timed_out") | Some("cancelled")
                        )
                    })
                    .collect();

                if !failed_checks.is_empty() {
                    // Shared CI-failure handler (blocking-only filter +
                    // diff-empty short-circuit + capped, escalating rework).
                    // Returns `true` when it consumed the event; `false` when
                    // only advisory/preview checks failed, in which case we
                    // fall through as if CI is green.
                    let handled = self
                        .handle_ci_failure(
                            gh_client,
                            &task,
                            &pr,
                            &failed_checks,
                            pr_url,
                            &owner,
                            &repo,
                            pull_number,
                        )
                        .await;
                    if handled {
                        self.pr_status_cache.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        self.delegated_to_github.remove(&task.id);
                        continue;
                    }
                    // else: only advisory checks failed — treat as green and
                    // fall through to the merge-eligibility path below.
                }

                // Only cache SHA once all checks have completed successfully.
                // If checks are still running, don't cache so we re-check
                // next tick.
                if all_completed {
                    self.pr_status_cache
                        .insert(task.id.clone(), current_sha.clone());
                }
            }

            // ── Merge eligibility check ───────────────────────────────────────
            // No changes requested, CI is green. Check if mergeable and approved.

            if pr.mergeable == Some(false) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR has merge conflicts → reopening task for rework"
                );
                let reason = self
                    .build_pr_conflict_reason(&task.short_id, &task.project_id)
                    .await;
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrConflict,
                    Some(&reason),
                )
                .await;
                // Reactive auto-blocker: if exactly one racing same-epic sibling
                // is landing on main, make this task WAIT for it (beside the
                // reopen above) instead of looping on the moving main.
                self.add_conflict_blocker_for_sibling(&task).await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                self.delegated_to_github.remove(&task.id);
                continue;
            }

            // `has_approved` was already computed (effective, latest-per-reviewer)
            // alongside `changes_requested` above. A merge-gating review exists
            // when some reviewer's latest standing review is APPROVED or
            // CHANGES_REQUESTED; since CHANGES_REQUESTED is handled and `continue`d
            // above, by this point that reduces to `has_approved`. COMMENTED /
            // DISMISSED / PENDING carry no standing and never block auto-merge.
            let has_reviews = changes_requested || has_approved;

            if has_reviews && !has_approved {
                // Merge-gating reviews exist but none APPROVED (and no CHANGES_REQUESTED
                // handled above). Wait for approval.
                self.maybe_re_request_review(
                    &task.id,
                    &task.short_id,
                    gh_client,
                    &owner,
                    &repo,
                    pull_number,
                )
                .await;
                continue;
            }

            // ── Branch up-to-date check ───────────────────────────────────────
            // GitHub reports `mergeable_state == "behind"` when there are no
            // conflicts (`mergeable == true`) but branch protection requires
            // the head to include the latest base. Calling update-branch
            // merges base → head (the equivalent of clicking GitHub's
            // "Update branch" button), which bumps the head SHA and triggers
            // a fresh CI run. We bail out of this tick and let the poller
            // re-evaluate next time around once the new SHA settles.
            if pr.mergeable_state.as_deref() == Some("behind") {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR is behind base — calling update-branch and retrying next tick"
                );
                match gh_client
                    .update_pull_request_branch(&owner, &repo, pull_number, &current_sha)
                    .await
                {
                    Ok(_) => {
                        // Head SHA will change; invalidate the CI cache so
                        // next tick re-checks against the new commit, and
                        // reset merge_fail_count since we're not actually
                        // attempting a merge this tick. Clear the
                        // delegated-to-GitHub marker too — the queue
                        // entry was pinned to the prior SHA and is now
                        // invalidated by update-branch.
                        self.pr_status_cache.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        self.delegated_to_github.remove(&task.id);
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            error = %e,
                            "PR poller: update-branch failed (will retry next tick)"
                        );
                    }
                }
                continue;
            }

            // ── Auto-approve (per-user opt-in, with fallback approver) ────────
            // When some user has `auto_approve_prs=true` and we have their
            // live GitHub session token, POST an APPROVE review using their
            // identity. Branch protection's "1 approval required" gate is
            // what's left between us and the merge; the next poller tick
            // sees the new approval state and falls through to the merge
            // call naturally. Pinning `commit_id = current_sha` means a
            // fresh push automatically invalidates the approval.
            //
            // Approver selection (see `find_auto_approver_session`):
            //   1. The task's `created_by_user_id` if that user has the
            //      toggle on and a live session.
            //   2. Otherwise, any user with the toggle on and a live
            //      session — needed for tasks spawned by background
            //      agents (Planner/Architect/auto-breakdown) whose
            //      `created_by_user_id IS NULL`.
            //
            // Skipped silently when:
            //  - no user has the toggle on, or none has a live session
            //  - an approve attempt was already made on this exact SHA
            //  - we already have an approval (re-approving is a no-op anyway)
            if !has_approved
                && self
                    .auto_approve_attempted
                    .get(&task.id)
                    .is_none_or(|sha| sha != &current_sha)
                && let Some((user_id, session)) =
                    self.find_auto_approver_session(&task.id, &task.short_id).await
            {
                // Build a refreshable client when App OAuth creds are
                // visible to this process — that lets the transport
                // rotate the access/refresh pair transparently on 401
                // instead of asking the user to re-sign in every 8 hours.
                // If creds aren't available we degrade to the legacy
                // non-refreshable client; a 401 will then hard-evict the
                // session row and the next UI hit bounces to login.
                let user_client = match github_app_user::client_credentials_from_env() {
                    Some((cid, secret)) => GitHubApiClient::for_user_session(
                        session.github_access_token.clone(),
                        DbBackedRefresher::new(
                            self.db.clone(),
                            session.token.clone(),
                            cid,
                            secret,
                        )
                        .into_arc(),
                    ),
                    None => {
                        tracing::debug!(
                            "PR poller: GITHUB_APP_CLIENT_ID/SECRET unset; \
                             constructing non-refreshable user client"
                        );
                        GitHubApiClient::for_user_token(session.github_access_token.clone())
                    }
                };
                match user_client
                    .approve_pull_request(&owner, &repo, pull_number, &current_sha)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            task_id = %task.short_id,
                            user_id = %user_id,
                            pr = pull_number,
                            sha = %current_sha,
                            "PR poller: auto-approved PR on user's behalf"
                        );
                        self.auto_approve_attempted
                            .insert(task.id.clone(), current_sha.clone());
                        self.pr_status_cache.remove(&task.id);
                        continue;
                    }
                    Err(e) => {
                        // Suppress regardless of failure mode — same SHA
                        // shouldn't be retried until a fresh push lands.
                        self.auto_approve_attempted
                            .insert(task.id.clone(), current_sha.clone());
                        if e.downcast_ref::<UserTokenExpired>().is_some() {
                            tracing::info!(
                                task_id = %task.short_id,
                                user_id = %user_id,
                                pr = pull_number,
                                "PR poller: auto-approve skipped — user token expired (sign in to re-arm)"
                            );
                        } else {
                            tracing::warn!(
                                task_id = %task.short_id,
                                user_id = %user_id,
                                pr = pull_number,
                                error = %e,
                                "PR poller: auto-approve failed; falling through to wait for human approval"
                            );
                        }
                        // Fall through to the merge attempt below —
                        // GitHub will reject if branch protection requires
                        // an approval, which surfaces as a normal merge
                        // failure in the existing path.
                    }
                }
            }

            // ── Merge path: delegated-to-GitHub vs legacy direct merge ────────
            // Three modes, picked in order:
            //
            //   (a) GitHub already owns the PR's merge timing — either
            //       `enablePullRequestAutoMerge` succeeded earlier (`pr.auto_merge`
            //       is set) OR we explicitly enqueued the PR into the merge
            //       queue on a previous tick (tracked in `delegated_to_github`
            //       keyed by SHA so a new push re-enters this branch). Just
            //       observe; on `UNMERGEABLE` or a failure-flavored dequeue
            //       event the observer surfaces `PrCiFailed`.
            //
            //   (b) REST `PUT /pulls/{n}/merge` succeeds — repos without
            //       merge-queue branch protection. Close the task.
            //
            //   (c) REST returns the merge-queue 405 — repo enforces a
            //       merge queue. We call `enqueuePullRequest` directly
            //       (works regardless of the repo's "Allow auto-merge"
            //       setting; `enable_auto_merge_best_effort` from undraft
            //       time often hits `UNPROCESSABLE` and is unreliable).
            //       Then mark delegated and observe next tick.
            let delegated_for_current_sha = self
                .delegated_to_github
                .get(&task.id)
                .is_some_and(|sha| sha == &current_sha);
            if pr.auto_merge.is_some() || delegated_for_current_sha {
                self.observe_auto_merge_state(
                    gh_client,
                    &task.id,
                    &task.short_id,
                    pr_url,
                    &owner,
                    &repo,
                    pull_number,
                )
                .await;
                continue;
            }
            // SHA moved since we last delegated — drop the stale entry so a
            // fresh enqueue attempt fires below.
            if self.delegated_to_github.contains_key(&task.id) {
                self.delegated_to_github.remove(&task.id);
            }

            // Either approved or no reviews — attempt squash merge.
            tracing::info!(
                task_id = %task.short_id,
                pr = pull_number,
                approved = has_approved,
                "PR poller: attempting squash merge"
            );

            match gh_client
                .merge_pull_request(&owner, &repo, pull_number, MergeMethod::Squash, &pr.title)
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        "PR poller: squash merge succeeded → closing task"
                    );
                    self.apply_pr_transition(&task.id, TransitionAction::PrMerge, None)
                        .await;
                    self.pr_status_cache.remove(&task.id);
                    self.merge_fail_count.remove(&task.id);
                    self.delegated_to_github.remove(&task.id);
                }
                Err(e) => {
                    // Merge-queue 405: the repo's branch protection routes
                    // everything through a merge queue. The REST merge
                    // endpoint is not allowed here — directly enqueue the
                    // PR via GraphQL `enqueuePullRequest`, which works
                    // regardless of the repo's "Allow auto-merge" setting.
                    if is_merge_queue_405(&e) {
                        match gh_client
                            .enqueue_pull_request(&pr.node_id, &current_sha)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    sha = %current_sha,
                                    "PR poller: enqueued PR into merge queue — switching to observe mode"
                                );
                                self.delegated_to_github
                                    .insert(task.id.clone(), current_sha.clone());
                                self.merge_fail_count.remove(&task.id);
                            }
                            Err(enqueue_err) => {
                                // Enqueue failed (PR not ready: missing
                                // approval, failing checks, etc.). Don't
                                // mark delegated — next tick re-checks
                                // upstream gates and tries again. Bump
                                // merge_fail_count so the cache-invalidate
                                // threshold still kicks in on persistent
                                // failure.
                                let count =
                                    self.merge_fail_count.entry(task.id.clone()).or_insert(0);
                                *count += 1;
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    pr = pull_number,
                                    attempt = *count,
                                    error = %enqueue_err,
                                    "PR poller: enqueue_pull_request failed (will retry next tick)"
                                );
                                if *count >= MERGE_RETRY_RECHECK_THRESHOLD {
                                    tracing::info!(
                                        task_id = %task.short_id,
                                        pr = pull_number,
                                        "PR poller: {} consecutive enqueue failures, invalidating CI cache for re-check",
                                        *count
                                    );
                                    self.pr_status_cache.remove(&task.id);
                                    *count = 0;
                                }
                            }
                        }
                        continue;
                    }

                    let count = self.merge_fail_count.entry(task.id.clone()).or_insert(0);
                    *count += 1;
                    tracing::warn!(
                        task_id = %task.short_id,
                        pr = pull_number,
                        attempt = *count,
                        error = %e,
                        "PR poller: merge failed (will retry next tick)"
                    );
                    // After repeated failures, invalidate the CI cache so the
                    // next tick re-checks whether checks actually passed.
                    // This catches the case where CI failed after we cached
                    // a "green" SHA.
                    if *count >= MERGE_RETRY_RECHECK_THRESHOLD {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            "PR poller: {} consecutive merge failures, invalidating CI cache for re-check",
                            *count
                        );
                        self.pr_status_cache.remove(&task.id);
                        *count = 0;
                    }
                }
            }
        }
    }

    /// Observe the merge-queue / auto-merge state for a PR we've delegated
    /// to GitHub's auto-merge.
    ///
    /// Three outcomes:
    /// - PR is queued or auto-merge is still waiting on conditions → log
    ///   once at DEBUG, return. No API spend beyond the state fetch.
    /// - Queue evicted the PR (`UNMERGEABLE` or a failure-flavored
    ///   `DequeuedEvent`) → fetch dequeue diagnostics, attach as PR review
    ///   feedback, transition the task with `PrCiFailed`.
    /// - State fetch failed → log warn, retry next tick.
    #[allow(clippy::too_many_arguments)]
    async fn observe_auto_merge_state(
        &mut self,
        gh_client: &GitHubApiClient,
        task_id: &str,
        task_short_id: &str,
        pr_url: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) {
        let state = match gh_client
            .get_pr_merge_queue_state(owner, repo, pull_number)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to fetch merge-queue state, will retry next tick"
                );
                return;
            }
        };

        // ── In-queue states: just wait ────────────────────────────────────
        if let Some(entry) = &state.merge_queue_entry {
            use djinn_provider::github_api::MergeQueueEntryState as S;
            match entry.state {
                S::Queued | S::AwaitingChecks | S::Locked | S::Mergeable => {
                    tracing::debug!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        state = ?entry.state,
                        position = ?entry.position,
                        "PR poller: PR in merge queue, awaiting GitHub"
                    );
                    return;
                }
                S::Unmergeable => {
                    self.handle_queue_failure(
                        gh_client,
                        owner,
                        repo,
                        task_id,
                        task_short_id,
                        pull_number,
                        pr_url,
                        state.last_dequeue.as_ref(),
                        "merge_queue_unmergeable",
                    )
                    .await;
                    return;
                }
            }
        }

        // ── Not currently queued; check why ───────────────────────────────
        // If GitHub still has an auto-merge request, it's just waiting for
        // approvals or required checks — keep waiting.
        if state.auto_merge_request.is_some() {
            tracing::debug!(
                task_id = %task_short_id,
                pr = pull_number,
                merge_state = ?state.merge_state_status,
                "PR poller: auto-merge armed, awaiting approval/checks"
            );
            return;
        }

        // Auto-merge is no longer enabled and the PR isn't queued. Most
        // commonly this means GitHub disabled auto-merge after a failed
        // merge attempt. If we have a failure-flavored dequeue event,
        // surface it. Otherwise fall through silently — next tick will
        // re-enable auto-merge via the undraft path on a fresh push, or
        // a human will intervene.
        if let Some(dequeue) = &state.last_dequeue
            && dequeue_reason_is_failure(dequeue.reason.as_deref())
        {
            self.handle_queue_failure(
                gh_client,
                owner,
                repo,
                task_id,
                task_short_id,
                pull_number,
                pr_url,
                Some(dequeue),
                "merge_queue_dequeued",
            )
            .await;
            return;
        }

        tracing::debug!(
            task_id = %task_short_id,
            pr = pull_number,
            merge_state = ?state.merge_state_status,
            "PR poller: auto-merge disabled, not queued, no failure signal"
        );
    }

    /// Common failure path for both `UNMERGEABLE` and post-dequeue failure
    /// signals. Attaches structured feedback to the task activity log,
    /// then transitions with `PrCiFailed` so a fresh worker iteration picks
    /// it up.
    // Failure handling threads through several distinct CI/PR signals; each arg
    // is its own piece of context, so a bag struct adds no clarity.
    #[allow(clippy::too_many_arguments)]
    async fn handle_queue_failure(
        &mut self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        task_id: &str,
        task_short_id: &str,
        pull_number: u64,
        pr_url: &str,
        dequeue: Option<&djinn_provider::github_api::DequeueEvent>,
        source: &str,
    ) {
        let reason = dequeue
            .and_then(|d| d.reason.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let created_at = dequeue.and_then(|d| d.created_at.clone());

        tracing::warn!(
            task_id = %task_short_id,
            pr = pull_number,
            source = %source,
            reason = %reason,
            "PR poller: merge queue rejected PR → reopening task for rework"
        );

        // Log a structured feedback entry so the worker prompt picks it up
        // alongside ordinary PR review feedback.
        let feedback_payload = serde_json::json!({
            "pull_number": pull_number,
            "pr_url": pr_url,
            "source": source,
            "dequeue_reason": reason,
            "dequeue_at": created_at,
        })
        .to_string();
        if let Err(e) = self
            .task_repo()
            .log_activity(
                Some(task_id),
                "system",
                "system",
                "merge_queue_rejection",
                &feedback_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "PR poller: failed to log merge_queue_rejection activity"
            );
        }

        // Enrich the feedback with the merge-group run's ACTUAL failing
        // checks. The PR head's own checks pass (the `ci` workflow runs its
        // unit/integration stages only on `merge_group`), so the real failure
        // lives in the merge-group run — without this the worker only sees
        // GitHub's generic dequeue reason ("failed_checks") and re-runs
        // blindly, looping on the same bug. The dequeue event carries no
        // merge-group ref and the merge-queue branch is ephemeral, but the
        // `merge_group` Actions run PERSISTS with
        // `head_branch = gh-readonly-queue/.../pr-<number>-<sha>` and a
        // `head_sha` whose check runs also persist — so we look it up there.
        // Reuses `log_ci_failure_comment`, which logs a `comment`/`verification`
        // entry surfaced to the worker via `recent_feedback`, naming the failed
        // workflow/job/step plus `ci_job_log` hints to read the real log.
        let pr_marker = format!("pr-{pull_number}-");
        match gh_client
            .list_workflow_runs_for_event(owner, repo, "merge_group", 50)
            .await
        {
            Ok(runs) => {
                // Runs are newest-first; take the most recent FAILED merge-group
                // run whose branch belongs to this PR.
                let failing_run = runs.into_iter().find(|r| {
                    r.conclusion.as_deref() == Some("failure")
                        && r.head_branch
                            .as_deref()
                            .is_some_and(|b| b.contains(&pr_marker))
                });
                if let Some(run) = failing_run {
                    match gh_client
                        .list_check_runs_for_ref(owner, repo, &run.head_sha)
                        .await
                    {
                        Ok(checks) => {
                            let failed: Vec<&CheckRun> = checks
                                .check_runs
                                .iter()
                                .filter(|cr| {
                                    matches!(
                                        cr.conclusion.as_deref(),
                                        Some("failure") | Some("timed_out") | Some("cancelled")
                                    )
                                })
                                .collect();
                            if failed.is_empty() {
                                tracing::info!(
                                    task_id = %task_short_id,
                                    run_id = run.id,
                                    "PR poller: merge-group run had no failing check runs to surface; feedback stays generic"
                                );
                            } else {
                                self.log_ci_failure_comment(
                                    task_id,
                                    &failed,
                                    pr_url,
                                    &run.head_sha,
                                    gh_client,
                                    owner,
                                    repo,
                                )
                                .await;
                            }
                        }
                        Err(e) => tracing::warn!(
                            task_id = %task_short_id,
                            run_id = run.id,
                            error = %e,
                            "PR poller: failed to fetch merge-group check runs for feedback enrichment"
                        ),
                    }
                } else {
                    tracing::info!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        "PR poller: no failing merge_group run found for PR; feedback stays generic"
                    );
                }
            }
            Err(e) => tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "PR poller: failed to list merge_group runs for feedback enrichment"
            ),
        }

        let transition_reason = format!(
            "merge queue rejected PR (reason: {reason}) — re-run with fresh CI feedback"
        );
        self.apply_pr_transition(
            task_id,
            TransitionAction::PrCiFailed,
            Some(&transition_reason),
        )
        .await;
        self.pr_status_cache.remove(task_id);
        self.merge_fail_count.remove(task_id);
        self.delegated_to_github.remove(task_id);
    }

    /// Shared handler for a "CI checks failed on PR" event, used by both the
    /// `pr_draft` and `pr_review` polling paths.
    ///
    /// Fixes the infinite CI-failure rework loop by gating the rework on three
    /// things, in order:
    ///
    /// 1. **Blocking-only filter.** The failed check-runs are intersected with
    ///    the branch's *required* status-check contexts (read from branch
    ///    protection — the source of truth). Advisory/preview checks (Vercel
    ///    deploys, preview-env provisioning, etc.) are dropped: a code diff
    ///    cannot fix that infra, so reworking on it loops forever. When branch
    ///    protection is unreadable we fall back to a conservative name-pattern
    ///    heuristic. If nothing blocking remains, we do **not** reopen — the
    ///    required checks are green and the PR is fine to proceed.
    ///
    /// 2. **Diff-empty short-circuit.** If the PR head has no commits ahead of
    ///    base on GitHub (`ahead_by == 0`), the previous worker iteration
    ///    produced no new diff — re-dispatching cannot change anything. We
    ///    escalate to the Planner and force-close instead of looping.
    ///
    /// 3. **Cycle cap.** Each CI-failure rework records a `pr_ci_cycle` marker.
    ///    Past `PR_CI_FAILURE_THRESHOLD` we escalate to the Planner and
    ///    force-close rather than redispatch. Escalation is terminal — the
    ///    counter is never reset on the reopen that re-arms the loop.
    ///
    /// Returns `true` when the event was *consumed* (the task was transitioned
    /// — either reworked or force-closed) and the caller should run its
    /// post-transition cache cleanup and `continue`. Returns `false` when the
    /// failures were all non-blocking and the caller should fall through to its
    /// normal (CI-passed) handling.
    #[allow(clippy::too_many_arguments)]
    async fn handle_ci_failure(
        &mut self,
        gh_client: &GitHubApiClient,
        task: &djinn_core::models::Task,
        pr: &PullRequest,
        failed_checks: &[&CheckRun],
        pr_url: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> bool {
        let task_id = &task.id;
        let task_short_id = &task.short_id;
        let current_sha = &pr.head.sha;
        let base_ref = &pr.base.ref_name;

        // ── 1. Filter to blocking (required) failures ─────────────────────────
        // Read the branch's required status checks from branch protection (the
        // source of truth). On any read failure we pass `None` and fall back to
        // the name-pattern heuristic.
        let required_contexts: Option<Vec<String>> = match gh_client
            .list_required_status_checks(owner, repo, base_ref)
            .await
        {
            Ok(contexts) => contexts,
            Err(e) => {
                tracing::info!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    base = %base_ref,
                    error = %e,
                    "PR poller: could not read required status checks (no admin perm or no protection); \
                     falling back to advisory-name heuristic for blocking-check filter"
                );
                None
            }
        };

        let blocking = blocking_failed_checks(failed_checks, required_contexts.as_deref());

        if blocking.is_empty() {
            tracing::info!(
                task_id = %task_short_id,
                pr = pull_number,
                sha = %current_sha,
                failed_count = failed_checks.len(),
                "PR poller: all failed checks are non-blocking (advisory/preview); \
                 required checks are green — not reopening for rework"
            );
            return false;
        }

        // ── 2. Diff-empty short-circuit ───────────────────────────────────────
        // If the head has no commits ahead of base, the last worker iteration
        // produced no new diff. Re-dispatching cannot change the outcome, so
        // escalate + force-close instead of looping on the same SHA.
        match gh_client
            .compare_commits_ahead_by(owner, repo, base_ref, current_sha)
            .await
        {
            Ok(0) => {
                let blocking_names: Vec<&str> = blocking.iter().map(|cr| cr.name.as_str()).collect();
                let reason = format!(
                    "PR #{pull_number} stuck: required checks keep failing ({}) but the task branch \
                     has no commits ahead of `{base_ref}` (head `{sha}`) — the worker produced no new \
                     diff, so re-running cannot fix it. Escalating for human attention.",
                    blocking_names.join(", "),
                    sha = &current_sha[..current_sha.len().min(12)],
                );
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    sha = %current_sha,
                    "PR poller: CI failed but branch is diff-empty vs base — escalating + force-closing"
                );
                self.escalate_ci_failure_and_close(task, pr_url, &reason).await;
                return true;
            }
            Ok(_) => {}
            Err(e) => {
                // Don't block the rework on a compare-API failure — fall
                // through to the cycle-cap path, which still bounds the loop.
                tracing::info!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: compare-commits precheck failed; proceeding with cycle-cap path"
                );
            }
        }

        // ── 3. Cycle cap ──────────────────────────────────────────────────────
        let task_repo = self.task_repo();
        let prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_CI_CYCLE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries.len() as u32,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: failed to count CI-failure cycles; assuming 0"
                );
                0
            }
        };
        let round = prior_cycles + 1;

        if round > PR_CI_FAILURE_THRESHOLD {
            let blocking_names: Vec<&str> = blocking.iter().map(|cr| cr.name.as_str()).collect();
            let reason = format!(
                "PR #{pull_number} stuck: required checks ({}) have failed across {prior} CI-failure \
                 rework rounds (threshold {PR_CI_FAILURE_THRESHOLD}) without going green. The worker \
                 cannot resolve this on its own — escalating for human attention.",
                blocking_names.join(", "),
                prior = prior_cycles,
            );
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                round,
                threshold = PR_CI_FAILURE_THRESHOLD,
                "PR poller: CI-failure rework threshold exceeded — escalating + force-closing"
            );
            self.escalate_ci_failure_and_close(task, pr_url, &reason).await;
            return true;
        }

        // Below threshold: record the cycle marker, surface CI feedback, and
        // reopen for a fresh worker iteration.
        let cycle_payload = serde_json::json!({ "round": round }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                PR_CI_CYCLE_EVENT,
                &cycle_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "PR poller: failed to store pr_ci_cycle marker"
            );
        }

        tracing::info!(
            task_id = %task_short_id,
            pr = pull_number,
            sha = %current_sha,
            round,
            threshold = PR_CI_FAILURE_THRESHOLD,
            blocking_count = blocking.len(),
            "PR poller: required CI check failed on PR → reopening task for rework (round {}/{})",
            round,
            PR_CI_FAILURE_THRESHOLD,
        );
        self.apply_pr_transition(
            task_id,
            TransitionAction::PrCiFailed,
            Some("CI checks failed on PR"),
        )
        .await;
        self.log_ci_failure_comment(
            task_id,
            &blocking,
            pr_url,
            current_sha,
            gh_client,
            owner,
            repo,
        )
        .await;
        true
    }

    /// Terminal escalation for a CI-failure loop the worker can't resolve
    /// (diff-empty re-emit, or cycle cap exceeded). Logs a visibility comment,
    /// dispatches a Planner escalation, then `ForceClose`s the task so it
    /// leaves the rework loop. Never resets the CI-cycle counter.
    async fn escalate_ci_failure_and_close(
        &mut self,
        task: &djinn_core::models::Task,
        pr_url: &str,
        reason: &str,
    ) {
        let task_repo = self.task_repo();

        let comment_body = format!("**PR CI Escalation**: {reason}\n\nPR: {pr_url}");
        let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "PR poller: failed to log CI escalation comment"
            );
        }

        // Escalate to the Planner (ADR-051 §8 escalation ceiling) before
        // force-closing, so a human / Planner sees why the task gave up.
        self.dispatch_planner_escalation(&task.id, reason, &task.project_id)
            .await;

        self.apply_pr_transition(&task.id, TransitionAction::ForceClose, Some(reason))
            .await;
        self.pr_status_cache.remove(&task.id);
        self.pr_draft_first_seen.remove(&task.id);
        self.merge_fail_count.remove(&task.id);
        self.delegated_to_github.remove(&task.id);
    }

    /// Attach PR review feedback to the task activity log, increment the
    /// review-round counter, log a visibility comment, and optionally escalate
    /// when `PR_REVIEW_ROUND_THRESHOLD` is exceeded.
    ///
    /// Called when the PR poller detects `CHANGES_REQUESTED` on a task.
    async fn attach_pr_review_feedback(
        &mut self,
        task_id: &str,
        task_short_id: &str,
        feedback: PrReviewFeedback,
    ) {
        let task_repo = self.task_repo();

        // ── Count prior review cycles ─────────────────────────────────────────
        let prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_CYCLE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries.len() as u32,
            Err(e) => {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "PR poller: failed to count review cycles"
                );
                0
            }
        };

        let round = prior_cycles + 1;

        // ── Store feedback as activity log entry ──────────────────────────────
        let feedback_payload = serde_json::json!({
            "pull_number": feedback.pull_number,
            "pr_url": feedback.pr_url,
            "round": round,
            "change_request_count": feedback.change_request_reviews.len(),
            "inline_comment_count": feedback.inline_comments.len(),
            "change_request_reviews": feedback.change_request_reviews.iter().map(|r| {
                serde_json::json!({
                    "reviewer": r.user.as_ref().map(|u| u.login.as_str()).unwrap_or("unknown"),
                    "state": r.state,
                    "html_url": r.html_url,
                    "submitted_at": r.submitted_at,
                })
            }).collect::<Vec<_>>(),
            "inline_comments": feedback.inline_comments.iter().map(|c| {
                serde_json::json!({
                    "reviewer": c.user.as_ref().map(|u| u.login.as_str()).unwrap_or("unknown"),
                    "body": c.body,
                    "path": c.path,
                    "line": c.line,
                    "html_url": c.html_url,
                })
            }).collect::<Vec<_>>(),
        })
        .to_string();

        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "system",
                "system",
                PR_REVIEW_FEEDBACK_EVENT,
                &feedback_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to store pr_review_feedback activity"
            );
        }

        // ── Record the review-cycle marker ────────────────────────────────────
        let cycle_payload = serde_json::json!({ "round": round }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                PR_REVIEW_CYCLE_EVENT,
                &cycle_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to store pr_review_cycle marker"
            );
        }

        // ── Log visibility comment for the review-fix cycle ───────────────────
        let reviewer_list = {
            let mut names: Vec<&str> = feedback
                .change_request_reviews
                .iter()
                .filter_map(|r| r.user.as_ref().map(|u| u.login.as_str()))
                .collect();
            names.dedup();
            if names.is_empty() {
                "reviewer(s)".to_string()
            } else {
                names.join(", ")
            }
        };
        let comment_body = format!(
            "**PR Review Round {round}**: Changes requested by {reviewer_list} on PR #{pull_number}. \
            Dispatching worker session with review feedback as context.",
            pull_number = feedback.pull_number
        );
        let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to log review cycle comment"
            );
        }

        tracing::info!(
            task_id = %task_short_id,
            round,
            threshold = PR_REVIEW_ROUND_THRESHOLD,
            inline_comments = feedback.inline_comments.len(),
            "PR poller: review feedback attached (round {}/{})",
            round,
            PR_REVIEW_ROUND_THRESHOLD
        );

        // ── Escalate if threshold exceeded ────────────────────────────────────
        if round >= PR_REVIEW_ROUND_THRESHOLD {
            tracing::warn!(
                task_id = %task_short_id,
                round,
                threshold = PR_REVIEW_ROUND_THRESHOLD,
                "PR poller: review loop threshold reached — escalating to Lead/Architect"
            );

            let escalation_body = format!(
                "**PR Review Escalation**: Task has gone through {round} review rounds without approval \
                (threshold: {threshold}). Escalating to Lead/Architect for strategic review.\n\n\
                PR: {pr_url}",
                threshold = PR_REVIEW_ROUND_THRESHOLD,
                pr_url = feedback.pr_url
            );
            let escalation_payload = serde_json::json!({ "body": escalation_body }).to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(task_id),
                    "coordinator",
                    "system",
                    "comment",
                    &escalation_payload,
                )
                .await
            {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "PR poller: failed to log escalation comment"
                );
            }

            // Look up the task's project_id and escalate to Planner.
            // Per ADR-051 §8 the Planner is the escalation ceiling above Lead;
            // it can dispatch an Architect spike if the PR loop is structurally
            // wrong, or reshape the task itself.
            if let Ok(Some(task)) = self.task_repo().get(task_id).await {
                let reason = format!(
                    "PR review loop exceeded {PR_REVIEW_ROUND_THRESHOLD} rounds without approval. PR: {}",
                    feedback.pr_url
                );
                self.dispatch_planner_escalation(task_id, &reason, &task.project_id)
                    .await;
            }
        }
    }

    /// Re-request review from reviewers who previously submitted CHANGES_REQUESTED
    /// if the task has prior review feedback and no current outstanding changes request.
    ///
    /// This is called when the PR is still open, no CHANGES_REQUESTED is currently
    /// active (meaning the worker already pushed fixup commits), and the task has at
    /// least one prior `pr_review_feedback` activity entry.
    ///
    /// Non-fatal: logs warnings on any GitHub API failure.
    async fn maybe_re_request_review(
        &mut self,
        task_id: &str,
        task_short_id: &str,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) {
        let task_repo = self.task_repo();

        // Check if there is at least one prior review-feedback entry AND at least
        // one review-cycle entry. If no cycle entries exist, the worker has not
        // yet addressed any review, so there's nothing to re-request.
        let has_prior_cycles = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_CYCLE_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 1,
                offset: 0,
            })
            .await
        {
            Ok(entries) => !entries.is_empty(),
            Err(_) => false,
        };

        if !has_prior_cycles {
            return;
        }

        // Check whether we have already re-requested review for the current
        // SHA (tracked via a `pr_re_review_requested` marker per SHA).
        // This avoids re-requesting on every 30-second tick.
        let current_sha_cache_key = format!("re_review:{task_id}");
        if self.pr_status_cache.contains_key(&current_sha_cache_key) {
            return; // Already re-requested for this SHA.
        }

        // Collect reviewer logins from the most recent pr_review_feedback entry.
        let reviewer_logins: Vec<String> = match task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task_id.to_owned()),
                event_type: Some(PR_REVIEW_FEEDBACK_EVENT.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 1,
                offset: 0,
            })
            .await
        {
            Ok(entries) => entries
                .into_iter()
                .flat_map(|entry| {
                    let payload: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
                    let reviews = payload
                        .get("change_request_reviews")?
                        .as_array()?
                        .iter()
                        .filter_map(|r| {
                            r.get("reviewer")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_owned())
                        })
                        .collect::<Vec<_>>();
                    Some(reviews)
                })
                .flatten()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect(),
            Err(_) => return,
        };

        if reviewer_logins.is_empty() {
            return;
        }

        tracing::info!(
            task_id = %task_short_id,
            pr = pull_number,
            reviewers = ?reviewer_logins,
            "PR poller: re-requesting review after fixup commits"
        );

        if let Err(e) = gh_client
            .re_request_review(owner, repo, pull_number, &reviewer_logins)
            .await
        {
            tracing::warn!(
                task_id = %task_short_id,
                pr = pull_number,
                error = %e,
                "PR poller: re-request review failed (non-fatal)"
            );
        } else {
            // Mark as done for this SHA so we don't re-request repeatedly.
            self.pr_status_cache
                .insert(current_sha_cache_key, "done".to_string());

            // Log a comment for visibility.
            let comment_body = format!(
                "**Re-requested review** from {} on PR #{pull_number} after fixup commits.",
                reviewer_logins.join(", ")
            );
            let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
            let _ = task_repo
                .log_activity(
                    Some(task_id),
                    "coordinator",
                    "system",
                    "comment",
                    &comment_payload,
                )
                .await;
        }
    }

    async fn apply_pr_transition(
        &self,
        task_id: &str,
        action: TransitionAction,
        reason: Option<&str>,
    ) {
        let task_repo = self.task_repo();
        let cleanup_action = action.clone();
        if let Err(e) = task_repo
            .transition(task_id, action, "system", "pr_poller", reason, None)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to apply task transition"
            );
            return;
        }
        // Branch hygiene: once the task is closed (merged or force-closed via
        // any pr_poller path), delete the task branch on both the local mirror
        // and the GitHub remote.  Without this, stale `task/<short_id>` refs
        // pile up on every mirror clone and on GitHub.  Best-effort.
        if matches!(
            cleanup_action,
            TransitionAction::PrMerge | TransitionAction::ForceClose
        ) {
            let event_bus = crate::events::event_bus_for(&self.events_tx);
            crate::task_merge::cleanup_task_branches_post_close(
                task_id,
                &self.db,
                &event_bus,
                self.mirror.as_deref(),
            )
            .await;
        }
    }

    /// Build the `PrConflict` transition reason for a task whose PR was just
    /// flagged `mergeable == false` by GitHub.
    ///
    /// When the local mirror can reproduce the conflict, returns
    /// `merge_conflict:{JSON}` so the task's `merge_conflict_metadata` column
    /// (and from there `conflict_context_for_dispatch`) picks up the file
    /// list and `SupervisorFlow::ConflictRetry` is selected on re-dispatch.
    /// Otherwise (no mirror configured, mirror stale, trial merge errored)
    /// falls back to a plain string — the task still transitions to `open`,
    /// just without the structured payload.
    async fn build_pr_conflict_reason(&self, task_short_id: &str, project_id: &str) -> String {
        const FALLBACK: &str = "PR has merge conflicts";

        let Some(mirror) = self.mirror.as_ref() else {
            return FALLBACK.to_string();
        };

        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let target_branch = match project_repo.get_config(project_id).await {
            Ok(Some(cfg)) => cfg.target_branch,
            _ => "main".to_string(),
        };
        let task_branch = format!("task/{task_short_id}");

        let files = match crate::task_merge::detect_pr_conflict_files(
            mirror,
            project_id,
            &task_branch,
            &target_branch,
        )
        .await
        {
            Ok(files) if !files.is_empty() => files,
            Ok(_) => {
                tracing::info!(
                    task_short_id,
                    "PR poller: local trial merge found no conflicts (mirror likely stale on target branch); using plain reason"
                );
                return FALLBACK.to_string();
            }
            Err(e) => {
                tracing::warn!(
                    task_short_id,
                    error = %e,
                    "PR poller: trial merge against mirror failed; using plain reason"
                );
                return FALLBACK.to_string();
            }
        };

        let meta = serde_json::json!({
            "conflicting_files": files,
            "base_branch": task_branch,
            "merge_target": target_branch,
        });
        match serde_json::to_string(&meta) {
            Ok(json) => format!("merge_conflict:{json}"),
            Err(_) => FALLBACK.to_string(),
        }
    }

    /// Reactive conflict auto-blocker: when `task`'s PR is flagged
    /// `mergeable == false`, try to make it WAIT for the racing same-epic
    /// sibling that is landing on main, instead of re-dispatching it straight
    /// back into the moving-main loop.
    ///
    /// This runs ALONGSIDE the `PrConflict` reopen (it does not replace it).
    /// The reopen already moves the task to `open`; this just adds a blocker
    /// edge so the readiness gate holds it until the sibling merges
    /// (`PrMerge` → `closed`), at which point re-dispatch branches from a main
    /// that already contains the sibling's work.
    ///
    /// Conservative + graceful-degrading. An edge is added ONLY when attribution
    /// is unambiguous (exactly one racing same-epic sibling). It falls back to
    /// today's behaviour — no edge — when there are zero racing siblings (the
    /// conflict is against already-merged main, nothing to wait on), more than
    /// one candidate (ambiguous), the task has no epic, or adding the edge would
    /// create a cycle (rejected by `update_blockers_atomic`'s cycle detection,
    /// caught here and skipped). It never blocks on a closed/merged sibling and
    /// never deadlocks.
    async fn add_conflict_blocker_for_sibling(&self, task: &Task) {
        let Some(epic_id) = task.epic_id.as_deref() else {
            // No epic → no siblings to attribute the conflict to.
            return;
        };

        let task_repo = self.task_repo();
        let siblings = match task_repo.list_by_epic(epic_id).await {
            Ok(siblings) => siblings,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    epic_id,
                    error = %e,
                    "PR poller: conflict auto-blocker: failed to list epic siblings; skipping (plain reopen only)"
                );
                return;
            }
        };

        let Some(sibling_id) = pick_conflict_blocker_sibling(&task.id, &siblings) else {
            // Zero or >1 racing siblings → ambiguous / nothing to wait on.
            // Fall back to plain reopen (no edge).
            let racing = siblings
                .iter()
                .filter(|s| s.id != task.id && is_racing_unmerged_status(&s.status))
                .count();
            tracing::info!(
                task_id = %task.short_id,
                epic_id,
                racing_sibling_count = racing,
                "PR poller: conflict auto-blocker: not exactly one racing same-epic sibling → plain reopen, no blocker added"
            );
            return;
        };

        match task_repo
            .update_blockers_atomic(&task.id, std::slice::from_ref(&sibling_id), &[])
            .await
        {
            Ok(()) => {
                let blocked_on_short = siblings
                    .iter()
                    .find(|s| s.id == sibling_id)
                    .map(|s| s.short_id.as_str())
                    .unwrap_or("?");
                tracing::info!(
                    task_id = %task.short_id,
                    epic_id,
                    blocked_on = blocked_on_short,
                    "PR poller: conflict auto-blocker: added blocker on racing same-epic sibling; task will wait for it to merge before re-dispatch"
                );
            }
            Err(e) => {
                // Cycle (or any other DB error) → graceful degradation: the
                // task already reopened via PrConflict; we simply skip the edge.
                tracing::info!(
                    task_id = %task.short_id,
                    epic_id,
                    error = %e,
                    "PR poller: conflict auto-blocker: could not add edge (likely a cycle) → plain reopen, no blocker added"
                );
            }
        }
    }

    /// Log a comment on the task with details about which CI checks failed,
    /// including the actual job logs from GitHub so the worker can fix them.
    ///
    /// This comment becomes part of the activity log that the re-dispatched worker
    /// reads in its system prompt, giving it context about what needs to be fixed.
    #[allow(clippy::too_many_arguments)]
    async fn log_ci_failure_comment(
        &self,
        task_id: &str,
        failed_checks: &[&CheckRun],
        pr_url: &str,
        sha: &str,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
    ) {
        // A PR's failures often span *multiple* workflow runs (e.g. a separate
        // `CI` run and a `Release` run on the same SHA). Aggregating only the
        // first run's jobs/steps drops the rest, so the worker fixes one and the
        // others keep failing on the next push. Collect every distinct failing
        // workflow run (bounded by `MAX_AGGREGATED_CI_RUNS`) and union their
        // jobs into a single feedback comment.
        let mut run_ids: Vec<u64> = Vec::new();
        for cr in failed_checks {
            if let Some(rid) = parse_actions_run_id(&cr.html_url)
                && !run_ids.contains(&rid)
            {
                run_ids.push(rid);
            }
        }

        let capped = run_ids.len() > MAX_AGGREGATED_CI_RUNS;
        if capped {
            tracing::warn!(
                task_id,
                total_failing_runs = run_ids.len(),
                cap = MAX_AGGREGATED_CI_RUNS,
                "PR poller: more failing workflow runs than the aggregation cap; \
                 truncating CI-failure feedback to the first {MAX_AGGREGATED_CI_RUNS} runs"
            );
            run_ids.truncate(MAX_AGGREGATED_CI_RUNS);
        }

        // Fetch jobs for each run and union them, de-duping by job id so the same
        // job reported under multiple check runs of one run isn't double-counted.
        let mut all_jobs: Vec<ActionsJob> = Vec::new();
        let mut seen_job_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut any_fetched = false;
        for rid in &run_ids {
            if let Ok(jobs) = gh_client.list_run_jobs(owner, repo, *rid).await {
                any_fetched = true;
                for job in jobs {
                    if seen_job_ids.insert(job.id) {
                        all_jobs.push(job);
                    }
                }
            }
        }

        // `None` means "no run jobs available at all" → fall back to listing
        // raw check-run names. An empty-but-fetched list is still `Some`.
        let jobs: Option<&[ActionsJob]> = if any_fetched {
            Some(&all_jobs)
        } else {
            None
        };

        let (mut sections, ci_jobs) = build_ci_failure_sections(jobs, failed_checks);

        if capped {
            sections.push(format!(
                "\n_Note: more than {MAX_AGGREGATED_CI_RUNS} workflow runs failed; \
                 showing the first {MAX_AGGREGATED_CI_RUNS}. Re-run CI after fixing \
                 these to surface any remaining failures._"
            ));
        }

        sections.push(format!("\nPR: {pr_url}"));

        let body = format!(
            "**CI checks failed on PR** (commit `{sha}`)\n\n{sections}",
            sha = &sha[..sha.len().min(12)],
            sections = sections.join("\n"),
        );

        let payload = serde_json::json!({
            "body": body,
            "ci_jobs": ci_jobs,
            "owner": owner,
            "repo": repo,
        })
        .to_string();
        let task_repo = self.task_repo();
        if let Err(e) = task_repo
            .log_activity(
                Some(task_id),
                "pr_poller",
                "verification",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to log CI failure comment"
            );
        }
    }

    /// Resolve a user identity + live GitHub session that can act as an
    /// auto-approver for the given task's PR.
    ///
    /// Resolution rule:
    ///   * If the task HAS a `created_by_user_id` (a human owns it), the
    ///     approval is governed **solely** by that owner's setting. We use
    ///     them only if they have `auto_approve_prs = true` and a live
    ///     session; otherwise we return `None` and the PR waits for a
    ///     manual approval. We do NOT fall back to another user — approving
    ///     someone else's task with your own toggle is exactly the
    ///     multi-user leak this guards against (an admin with auto-approve
    ///     on must not silently approve other devs' PRs).
    ///   * Only when `created_by_user_id` is NULL (background-agent-spawned
    ///     tasks — Planner / Architect / auto-breakdown output that no human
    ///     owns) do we fall back to any user with `auto_approve_prs = true`
    ///     and a non-expired session, most-recently-updated first, so those
    ///     PRs don't sit in `pr_review` forever.
    ///
    /// Returns `None` when the resolved owner hasn't opted in / has no live
    /// session, or (for unattributed tasks) when nobody opted in. Logs the
    /// outcome at debug for visibility.
    async fn find_auto_approver_session(
        &self,
        task_id: &str,
        task_short_id: &str,
    ) -> Option<(String, djinn_db::UserAuthSessionRecord)> {
        let us_repo = UserSettingsRepository::new(self.db.clone());
        let sa_repo = SessionAuthRepository::new(self.db.clone());

        // If the task has a human owner, the decision is governed *solely*
        // by that owner's setting — never fall back to another user.
        if let Some(user_id) = self.task_created_by_user_id(task_id).await {
            let toggle = match us_repo.get_or_default(&user_id).await {
                Ok(s) => s.auto_approve_prs,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_short_id,
                        user_id = %user_id,
                        error = %e,
                        "PR poller: user_settings read failed for task owner; leaving PR for manual approval"
                    );
                    return None;
                }
            };
            if !toggle {
                tracing::debug!(
                    task_id = %task_short_id,
                    user_id = %user_id,
                    "PR poller: task owner has not opted into auto-approval; leaving PR for manual approval"
                );
                return None;
            }
            match sa_repo.latest_token_for_user(&user_id).await {
                Ok(Some(session)) => return Some((user_id, session)),
                Ok(None) => {
                    tracing::debug!(
                        task_id = %task_short_id,
                        user_id = %user_id,
                        "PR poller: task owner opted in but has no live session; leaving PR for manual approval"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_short_id,
                        user_id = %user_id,
                        error = %e,
                        "PR poller: session lookup failed for task owner; leaving PR for manual approval"
                    );
                }
            }
            return None;
        }

        // Unattributed task (created_by_user_id IS NULL — background-agent
        // output). Fall back to any opted-in user with a live session.
        let candidates = match us_repo.list_users_with_auto_approve().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    error = %e,
                    "PR poller: list_users_with_auto_approve failed; skipping fallback approver"
                );
                return None;
            }
        };
        for uid in candidates {
            match sa_repo.latest_token_for_user(&uid).await {
                Ok(Some(session)) => {
                    tracing::debug!(
                        task_id = %task_short_id,
                        user_id = %uid,
                        "PR poller: selected fallback auto-approver"
                    );
                    return Some((uid, session));
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_short_id,
                        user_id = %uid,
                        error = %e,
                        "PR poller: session lookup failed for fallback approver candidate"
                    );
                }
            }
        }

        tracing::debug!(
            task_id = %task_short_id,
            "PR poller: no eligible auto-approver (nobody opted in with a live session)"
        );
        None
    }

    /// Side-query for the task's `created_by_user_id` column. The `Task`
    /// model deliberately does not expose this column (added by migration 3
    /// for attribution; only repositories and the auto-approve path need it),
    /// so the auto-approve branch reads it directly here. Returns `None` for
    /// background-agent-created tasks (column is NULL) or on DB error.
    async fn task_created_by_user_id(&self, task_id: &str) -> Option<String> {
        match sqlx::query_scalar!(
            "SELECT created_by_user_id FROM tasks WHERE id = $1",
            task_id,
        )
        .fetch_optional(self.db.pool())
        .await
        {
            Ok(Some(opt)) => opt,
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "PR poller: failed to read created_by_user_id; treating as unattributed"
                );
                None
            }
        }
    }
}

/// Resolve a project's `installation_id` and build a GitHub API client
/// authenticating as that GitHub App installation. Returns `None` when the
/// project row has no installation (legacy pre-Migration-2 rows) or the
/// lookup fails.
async fn resolve_installation_client(
    project_repo: &djinn_db::ProjectRepository,
    project_id: &str,
) -> Option<GitHubApiClient> {
    match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => Some(GitHubApiClient::for_installation(id)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                project_id,
                error = %e,
                "PR poller: failed to read installation_id for project"
            );
            None
        }
    }
}

/// Recognise the merge-queue 405 from a REST `PUT /pulls/{n}/merge` error.
///
/// GitHub returns `405 Method Not Allowed` with body
/// `"Pull Request is in the merge queue."` when a queue-enabled repo
/// receives a direct merge call for a PR the queue is already handling.
/// We treat this as "GitHub is handling it" rather than a failure.
fn is_merge_queue_405(err: &anyhow::Error) -> bool {
    let msg = format!("{err}");
    msg.contains("405") && msg.contains("merge queue")
}

/// Determine if a `DequeuedEvent.reason` indicates a real failure that
/// should kick the task back into the worker loop.
///
/// Reasons that are NOT failures: `"BRANCH_INVALIDATED"` (head moved —
/// queue will pick up the new SHA), `"QUEUE_CLEARED"` (admin reset),
/// `"DEQUEUED"` (manual intervention by a human).
///
/// All other reasons (CHECKS_FAILED, MERGE_CONFLICT, NO_RESPONSE,
/// NOT_QUEUEABLE, ROLL_BACK, UNKNOWN_REMOVAL_REASON, anything new GitHub
/// adds) are treated as failures by default — safer to surface a spurious
/// re-run than to silently drop a real failure.
fn dequeue_reason_is_failure(reason: Option<&str>) -> bool {
    match reason {
        None => false,
        Some(r) => !matches!(r, "BRANCH_INVALIDATED" | "QUEUE_CLEARED" | "DEQUEUED"),
    }
}

/// Enable GitHub's native auto-merge on a PR, soft-failing on errors.
///
/// Called immediately after `mark_pr_ready_for_review` so GitHub takes over
/// gating: it watches required checks + approvals + branch-protection rules
/// and either auto-merges or enqueues the PR into the repo merge queue.
///
/// Soft-fails because not every repo supports it:
/// - "Pull request Auto merge is not allowed on this repository" — repo
///   settings have auto-merge disabled.
/// - Branch protection misconfigured.
///
/// In those cases the legacy REST merge path in `poll_pr_review_tasks`
/// takes over.
async fn enable_auto_merge_best_effort(
    gh_client: &GitHubApiClient,
    task_short_id: &str,
    pull_number: u64,
    pr_node_id: &str,
    pr_title: &str,
) {
    match gh_client
        .enable_auto_merge(
            "", // owner unused by GraphQL mutation
            "", // repo unused by GraphQL mutation
            pull_number,
            MergeMethod::Squash,
            pr_node_id,
            pr_title,
        )
        .await
    {
        Ok(_) => {
            tracing::info!(
                task_id = %task_short_id,
                pr = pull_number,
                "PR poller: auto-merge enabled — GitHub will merge once approval+checks land"
            );
        }
        Err(e) => {
            // Already enabled (re-undraft) is harmless; other errors mean
            // the repo can't auto-merge and we'll fall back to manual.
            tracing::info!(
                task_id = %task_short_id,
                pr = pull_number,
                error = %e,
                "PR poller: auto-merge not enabled (will use legacy merge path)"
            );
        }
    }
}

/// Heuristic: is this check-run name an *advisory* (non-merge-gating) check?
///
/// Used only as a fallback when we couldn't read the branch's required-status-
/// check contexts from branch protection (no protection configured, or the
/// installation lacks the permission). Matches the common preview/deploy
/// integrations whose failures cannot be fixed by a code diff (Vercel/Netlify
/// deploy previews, preview-environment provisioning, etc.).
///
/// Matching is case-insensitive and substring-based so it catches the various
/// per-app context names GitHub emits (e.g. `Vercel – acme-portal`,
/// `PR Preview Environment Setup / setup-preview`).
fn is_advisory_check_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    const ADVISORY_MARKERS: &[&str] = &[
        "vercel",
        "netlify",
        "preview",
        "deploy-preview",
        "deployment",
        "deploy /",
        "cloudflare pages",
        "render.com",
        "surge",
        "amplify",
    ];
    ADVISORY_MARKERS.iter().any(|m| lower.contains(m))
}

/// From the set of failed check-runs, return only those that are *blocking*
/// (merge-gating).
///
/// - When `required_contexts` is `Some` (we read the branch's required status
///   checks from branch protection — the source of truth), keep only failed
///   checks whose name matches a required context. Anything not required is
///   advisory and must not trigger a rework.
/// - When `required_contexts` is `None` (branch protection unreadable), fall
///   back to the name-pattern heuristic: keep failed checks that are *not*
///   recognised as advisory. This is intentionally conservative — an unknown
///   check is treated as blocking so we never silently swallow a real failure.
fn blocking_failed_checks<'a>(
    failed: &[&'a CheckRun],
    required_contexts: Option<&[String]>,
) -> Vec<&'a CheckRun> {
    match required_contexts {
        Some(required) => failed
            .iter()
            .filter(|cr| required.iter().any(|ctx| ctx == &cr.name))
            .copied()
            .collect(),
        None => failed
            .iter()
            .filter(|cr| !is_advisory_check_name(&cr.name))
            .copied()
            .collect(),
    }
}

/// Parse a GitHub PR URL into `(owner, repo, pull_number)`.
///
/// Handles URLs of the form `https://github.com/{owner}/{repo}/pull/{number}`.
pub(crate) fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.splitn(5, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let segment = parts.next()?;
    if segment != "pull" {
        return None;
    }
    let number_str = parts.next()?;
    // Strip any trailing fragment/query.
    let number_str = number_str.split(&['?', '#'][..]).next()?;
    let number: u64 = number_str.parse().ok()?;
    Some((owner.to_string(), repo.to_string(), number))
}

/// True when `status` is a post-implementation, UNMERGED, racing PR state — the
/// shape of an epic sibling whose work is landing on main and can cause a
/// conflict. A merged/closed sibling is `closed`, which is deliberately
/// excluded (nothing to wait on; blocking on it would never release).
fn is_racing_unmerged_status(status: &str) -> bool {
    matches!(status, "approved" | "pr_draft" | "pr_review")
}

/// Pick the single racing same-epic sibling to block `this_task_id` on, if
/// attribution is unambiguous.
///
/// Returns `Some(sibling_id)` ONLY when exactly one sibling (other than
/// `this_task_id`) is in a racing-unmerged state (see
/// [`is_racing_unmerged_status`]). Returns `None` for zero candidates (conflict
/// is against already-merged main — nothing to wait on) or more than one
/// (ambiguous — don't guess). Conservative by design: correctness over coverage.
fn pick_conflict_blocker_sibling(this_task_id: &str, siblings: &[Task]) -> Option<String> {
    let mut candidates = siblings
        .iter()
        .filter(|s| s.id != this_task_id && is_racing_unmerged_status(&s.status));
    let first = candidates.next()?;
    // More than one racing sibling → ambiguous attribution → fall back.
    if candidates.next().is_some() {
        return None;
    }
    Some(first.id.clone())
}

/// Collapse a PR's full review history into the *effective* merge-gating
/// decision, mirroring GitHub's own `reviewDecision`.
///
/// `GET /pulls/{n}/reviews` returns every review a reviewer ever submitted, and
/// a `CHANGES_REQUESTED` review's `state` stays `CHANGES_REQUESTED` forever —
/// pushing new commits, dismissing it, or even approving afterwards does NOT
/// rewrite that historical entry. The merge-gating decision therefore depends
/// only on each reviewer's *latest* standing review, never on whether any
/// historical review was CHANGES_REQUESTED.
///
/// Per author we take the most-recent review (by `submitted_at`, which is
/// RFC-3339 UTC so lexical ordering is chronological) whose state is one of
/// `APPROVED` / `CHANGES_REQUESTED` / `DISMISSED`. `COMMENTED` and `PENDING`
/// carry no standing and are ignored; `DISMISSED` clears a reviewer's prior
/// standing without adding a new one. Then:
///   - `changes_requested` is true iff some author's latest standing review is
///     `CHANGES_REQUESTED`.
///   - `has_approved` is true iff some author's latest standing review is
///     `APPROVED`.
///
/// Reviews with no author or no `submitted_at` are skipped (can't attribute or
/// order them). Returns `(changes_requested, has_approved)`.
fn effective_review_decision(reviews: &[PrReview]) -> (bool, bool) {
    use std::collections::HashMap;
    // author login -> (submitted_at, state) of their latest standing review.
    let mut latest: HashMap<&str, (&str, &str)> = HashMap::new();
    for r in reviews {
        let state = r.state.as_str();
        if !matches!(state, "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED") {
            continue; // COMMENTED / PENDING carry no merge-gating standing.
        }
        let Some(login) = r.user.as_ref().map(|u| u.login.as_str()) else {
            continue;
        };
        let Some(submitted) = r.submitted_at.as_deref() else {
            continue;
        };
        latest
            .entry(login)
            .and_modify(|cur| {
                if submitted > cur.0 {
                    *cur = (submitted, state);
                }
            })
            .or_insert((submitted, state));
    }
    let changes_requested = latest.values().any(|(_, s)| *s == "CHANGES_REQUESTED");
    let has_approved = latest.values().any(|(_, s)| *s == "APPROVED");
    (changes_requested, has_approved)
}

/// Parse a GitHub Actions workflow-run id out of a check-run's `html_url`.
///
/// URLs look like `https://github.com/{owner}/{repo}/actions/runs/{run_id}/...`.
/// Returns `None` for URLs that don't carry a run id (e.g. non-Actions checks).
fn parse_actions_run_id(html_url: &str) -> Option<u64> {
    html_url
        .split("/actions/runs/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|s| s.parse::<u64>().ok())
}

/// True when a job/step conclusion represents a CI failure worth surfacing.
fn is_failing_conclusion(conclusion: Option<&str>) -> bool {
    matches!(
        conclusion,
        Some("failure") | Some("timed_out") | Some("cancelled")
    )
}

/// Build the human-readable `sections` and the structured `ci_jobs` payload for
/// a CI-failure rework comment from the *union* of failing jobs across one or
/// more workflow runs.
///
/// `jobs` is `Some` when the Actions API returned job data for at least one run
/// (possibly empty), and `None` when no run jobs could be fetched at all — in
/// which case we fall back to listing the raw failed check-run names. The output
/// format matches what the worker already expects (Workflow / Failed job /
/// Failed step lines plus `ci_job_log(...)` hint lines and a `ci_jobs` array);
/// it is just unioned across every failing run rather than the first one only.
///
/// `jobs` is expected to already be de-duplicated by job id by the caller.
fn build_ci_failure_sections(
    jobs: Option<&[ActionsJob]>,
    failed_checks: &[&CheckRun],
) -> (Vec<String>, Vec<serde_json::Value>) {
    let mut sections: Vec<String> = Vec::new();
    let mut ci_jobs: Vec<serde_json::Value> = Vec::new();

    match jobs {
        Some(jobs) => {
            // One `**Workflow:** <name>` header per distinct workflow, in first-seen
            // order, so multi-run aggregation names every workflow that failed.
            let mut seen_workflows: Vec<&str> = Vec::new();
            for name in jobs.iter().filter_map(|j| j.workflow_name.as_deref()) {
                if !seen_workflows.contains(&name) {
                    seen_workflows.push(name);
                    sections.push(format!("**Workflow:** {name}"));
                }
            }

            for job in jobs
                .iter()
                .filter(|j| is_failing_conclusion(j.conclusion.as_deref()))
            {
                let conclusion = job.conclusion.as_deref().unwrap_or("unknown");
                sections.push(format!("**Failed job:** {} ({})", job.name, conclusion));

                let failed_steps: Vec<_> = job
                    .steps
                    .iter()
                    .filter(|s| is_failing_conclusion(s.conclusion.as_deref()))
                    .collect();

                for step in &failed_steps {
                    let step_conclusion = step.conclusion.as_deref().unwrap_or("unknown");
                    sections.push(format!(
                        "**Failed step:** {} (step #{}, {})",
                        step.name, step.number, step_conclusion
                    ));
                }

                sections.push(format!("Job URL: {}", job.html_url));

                // Structured metadata for the `ci_job_log` tool. The worker can
                // call `ci_job_log(job_id=...)` to fetch the full log on demand.
                let failed_step_names: Vec<serde_json::Value> = failed_steps
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "number": s.number,
                        })
                    })
                    .collect();

                ci_jobs.push(serde_json::json!({
                    "job_id": job.id,
                    "name": job.name,
                    "failed_steps": failed_step_names,
                }));
            }
        }
        None => {
            // Fallback: just list the check run names.
            for cr in failed_checks {
                let conclusion = cr.conclusion.as_deref().unwrap_or("unknown");
                sections.push(format!(
                    "- **{}** ({}): {}",
                    cr.name, conclusion, cr.html_url
                ));
            }
        }
    }

    // Build `ci_job_log` hint lines so the worker knows exactly which tool call
    // to make for each failed job (across all aggregated runs).
    if !ci_jobs.is_empty() {
        let hints: Vec<String> = ci_jobs
            .iter()
            .map(|j| {
                let job_id = j["job_id"].as_u64().unwrap_or(0);
                let name = j["name"].as_str().unwrap_or("unknown");
                let steps: Vec<String> = j["failed_steps"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| s["name"].as_str().map(|n| n.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(step) = steps.first() {
                    format!(
                        "Use `ci_job_log(job_id={job_id}, step=\"{step}\")` to view the **{name}** failed step log."
                    )
                } else {
                    format!("Use `ci_job_log(job_id={job_id})` to view the **{name}** job log.")
                }
            })
            .collect();
        sections.push(format!("\n{}", hints.join("\n")));
    }

    (sections, ci_jobs)
}

#[cfg(test)]
mod tests {
    use super::{
        Task, blocking_failed_checks, build_ci_failure_sections, dequeue_reason_is_failure,
        effective_review_decision, is_advisory_check_name, is_merge_queue_405,
        is_racing_unmerged_status, parse_actions_run_id, parse_pr_url, pick_conflict_blocker_sibling,
    };
    use djinn_provider::github_api::{ActionsJob, ActionsJobStep, CheckRun, GitHubUser, PrReview};

    /// Minimal `PrReview` builder for the effective-decision tests.
    fn review(author: &str, state: &str, submitted_at: &str) -> PrReview {
        PrReview {
            id: 1,
            user: Some(GitHubUser {
                login: author.to_string(),
                id: 1,
            }),
            state: state.to_string(),
            submitted_at: Some(submitted_at.to_string()),
            html_url: String::new(),
            body: String::new(),
        }
    }

    #[test]
    fn effective_decision_latest_approval_supersedes_stale_changes_requested() {
        // Exact task-2sq6 shape: the same reviewer left COMMENTED twice, then
        // CHANGES_REQUESTED, then dismissed it three times, then APPROVED on the
        // current head. GitHub's reviewDecision is APPROVED — ours must agree,
        // NOT keep matching the stale 17:35:01 CHANGES_REQUESTED entry.
        let reviews = vec![
            review("claude", "COMMENTED", "2026-06-01T17:34:53Z"),
            review("claude", "COMMENTED", "2026-06-01T17:34:57Z"),
            review("claude", "CHANGES_REQUESTED", "2026-06-01T17:35:01Z"),
            review("claude", "DISMISSED", "2026-06-01T19:03:50Z"),
            review("claude", "DISMISSED", "2026-06-01T19:17:56Z"),
            review("claude", "DISMISSED", "2026-06-01T19:34:10Z"),
            review("claude", "APPROVED", "2026-06-01T20:11:37Z"),
        ];
        let (changes_requested, has_approved) = effective_review_decision(&reviews);
        assert!(
            !changes_requested,
            "a superseded CHANGES_REQUESTED must not force rework once the same reviewer approves"
        );
        assert!(has_approved, "latest standing review is APPROVED");
    }

    #[test]
    fn effective_decision_standing_changes_requested_still_blocks() {
        // Reviewer requested changes and hasn't approved since → still blocks.
        let reviews = vec![
            review("claude", "COMMENTED", "2026-06-01T17:34:53Z"),
            review("claude", "APPROVED", "2026-06-01T18:00:00Z"),
            review("claude", "CHANGES_REQUESTED", "2026-06-01T19:00:00Z"),
        ];
        let (changes_requested, has_approved) = effective_review_decision(&reviews);
        assert!(
            changes_requested,
            "latest standing review is CHANGES_REQUESTED"
        );
        assert!(!has_approved);
    }

    #[test]
    fn effective_decision_is_per_reviewer() {
        // One reviewer approved on the current head; another still has an
        // outstanding change request → the PR is blocked (any blocker blocks).
        let reviews = vec![
            review("alice", "APPROVED", "2026-06-01T20:00:00Z"),
            review("bob", "CHANGES_REQUESTED", "2026-06-01T19:00:00Z"),
        ];
        let (changes_requested, has_approved) = effective_review_decision(&reviews);
        assert!(changes_requested, "bob's outstanding change request blocks");
        assert!(has_approved, "alice approved");
    }

    #[test]
    fn effective_decision_dismissed_clears_standing() {
        // A lone CHANGES_REQUESTED later DISMISSED leaves no standing → neither
        // blocks nor approves (falls through to the merge-eligibility path).
        let reviews = vec![
            review("claude", "CHANGES_REQUESTED", "2026-06-01T17:00:00Z"),
            review("claude", "DISMISSED", "2026-06-01T18:00:00Z"),
        ];
        let (changes_requested, has_approved) = effective_review_decision(&reviews);
        assert!(!changes_requested);
        assert!(!has_approved);
    }

    #[test]
    fn effective_decision_ignores_commented_only() {
        // COMMENTED reviews are informational — never gating.
        let reviews = vec![
            review("claude", "COMMENTED", "2026-06-01T17:00:00Z"),
            review("claude", "COMMENTED", "2026-06-01T18:00:00Z"),
        ];
        assert_eq!(effective_review_decision(&reviews), (false, false));
    }

    /// Minimal `Task` builder for the sibling-attribution heuristic tests.
    /// Only `id` and `status` are load-bearing; everything else is filler.
    fn task(id: &str, status: &str) -> Task {
        Task {
            id: id.to_string(),
            project_id: "p".to_string(),
            short_id: id.to_string(),
            epic_id: Some("ep".to_string()),
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_string(),
            status: status.to_string(),
            priority: 0,
            owner: String::new(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_string(),
            agent_type: None,
            created_by_user_id: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn racing_status_classification() {
        // Post-implementation, unmerged → racing.
        assert!(is_racing_unmerged_status("approved"));
        assert!(is_racing_unmerged_status("pr_draft"));
        assert!(is_racing_unmerged_status("pr_review"));
        // Pre-implementation / terminal → NOT racing (nothing to wait on, or
        // would never release the block).
        assert!(!is_racing_unmerged_status("open"));
        assert!(!is_racing_unmerged_status("in_progress"));
        assert!(!is_racing_unmerged_status("closed"));
    }

    #[test]
    fn picks_single_racing_sibling() {
        // Exactly one racing same-epic sibling → unambiguous → block on it.
        let siblings = vec![
            task("self", "open"),
            task("peer", "pr_review"),
            task("done", "closed"),
        ];
        assert_eq!(
            pick_conflict_blocker_sibling("self", &siblings),
            Some("peer".to_string())
        );
    }

    #[test]
    fn no_block_when_zero_racing_siblings() {
        // Conflict is against already-merged main (sibling closed) → nothing to
        // wait on → fall back to plain reopen.
        let siblings = vec![task("self", "open"), task("done", "closed")];
        assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
    }

    #[test]
    fn no_block_when_multiple_racing_siblings() {
        // Two racing siblings → ambiguous attribution → don't guess.
        let siblings = vec![
            task("self", "open"),
            task("peer1", "pr_review"),
            task("peer2", "approved"),
        ];
        assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
    }

    #[test]
    fn never_blocks_on_self() {
        // The task itself, even in a racing state, is never a candidate.
        let siblings = vec![task("self", "pr_review")];
        assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
    }

    fn check(name: &str) -> CheckRun {
        CheckRun {
            id: 1,
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: "https://github.com/o/r/runs/1".to_string(),
        }
    }

    #[test]
    fn advisory_check_names_classified() {
        // The real 1ck3 offenders.
        assert!(is_advisory_check_name("PR Preview Environment Setup / setup-preview"));
        assert!(is_advisory_check_name("Vercel – acme-portal"));
        assert!(is_advisory_check_name("Vercel – admin-portal"));
        assert!(is_advisory_check_name("Netlify deploy"));
        assert!(is_advisory_check_name("Cloudflare Pages"));
        assert!(is_advisory_check_name("Deployment to staging"));

        // Required checks must NOT be classified as advisory.
        assert!(!is_advisory_check_name("Sentinel"));
        assert!(!is_advisory_check_name("unit tests"));
        assert!(!is_advisory_check_name("ci / build"));
        assert!(!is_advisory_check_name("lint"));
    }

    #[test]
    fn blocking_filter_uses_required_contexts_when_present() {
        let preview = check("Vercel – acme-portal");
        let unit = check("unit tests");
        let failed = vec![&preview, &unit];
        // Branch protection lists only "unit tests" + "Sentinel" as required.
        let required = vec!["unit tests".to_string(), "Sentinel".to_string()];

        let blocking = blocking_failed_checks(&failed, Some(&required));
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].name, "unit tests");
    }

    #[test]
    fn blocking_filter_empty_when_only_non_required_fail() {
        // The exact 1ck3 shape: required checks (unit tests / Sentinel) are
        // GREEN — only preview/Vercel checks failed, so none of the *failed*
        // checks are in the required set.
        let preview = check("PR Preview Environment Setup / setup-preview");
        let vercel = check("Vercel – acme-portal");
        let failed = vec![&preview, &vercel];
        let required = vec!["unit tests".to_string(), "Sentinel".to_string()];

        let blocking = blocking_failed_checks(&failed, Some(&required));
        assert!(
            blocking.is_empty(),
            "no required check failed → nothing should trigger rework"
        );
    }

    #[test]
    fn blocking_filter_falls_back_to_heuristic_without_contexts() {
        let preview = check("Vercel – acme-portal");
        let unit = check("unit tests");
        let failed = vec![&preview, &unit];

        // No branch-protection contexts available → name-pattern heuristic.
        let blocking = blocking_failed_checks(&failed, None);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].name, "unit tests");
    }

    #[test]
    fn blocking_filter_heuristic_keeps_unknown_checks_as_blocking() {
        // Conservative fallback: an unrecognised check is treated as blocking
        // so we never silently swallow a real failure.
        let mystery = check("some-custom-gate");
        let failed = vec![&mystery];
        let blocking = blocking_failed_checks(&failed, None);
        assert_eq!(blocking.len(), 1, "unknown checks must be treated as blocking");
    }

    #[test]
    fn blocking_filter_heuristic_drops_only_advisory() {
        let preview = check("Deploy Preview");
        let vercel = check("Vercel – portal");
        let failed = vec![&preview, &vercel];
        let blocking = blocking_failed_checks(&failed, None);
        assert!(blocking.is_empty());
    }

    #[test]
    fn is_merge_queue_405_matches_real_payload() {
        let err = anyhow::anyhow!(
            r#"merge_pull_request failed (405 Method Not Allowed): {{"message":"Pull Request is in the merge queue.","status":"405"}}"#
        );
        assert!(is_merge_queue_405(&err));
    }

    #[test]
    fn is_merge_queue_405_ignores_unrelated_405s() {
        let err = anyhow::anyhow!("merge_pull_request failed (405): {{\"message\":\"locked\"}}");
        assert!(!is_merge_queue_405(&err));
    }

    #[test]
    fn is_merge_queue_405_ignores_other_status_codes() {
        let err = anyhow::anyhow!(
            "merge_pull_request failed (422): Pull Request is in the merge queue."
        );
        // Not a 405 — must not match.
        assert!(!is_merge_queue_405(&err));
    }

    #[test]
    fn dequeue_reasons_classified_correctly() {
        // Failures: anything not on the safe-list.
        assert!(dequeue_reason_is_failure(Some("CHECKS_FAILED")));
        assert!(dequeue_reason_is_failure(Some("MERGE_CONFLICT")));
        assert!(dequeue_reason_is_failure(Some("NO_RESPONSE")));
        assert!(dequeue_reason_is_failure(Some("NOT_QUEUEABLE")));
        assert!(dequeue_reason_is_failure(Some("ROLL_BACK")));
        assert!(dequeue_reason_is_failure(Some("UNKNOWN_REMOVAL_REASON")));
        assert!(dequeue_reason_is_failure(Some("SOMETHING_NEW")));

        // Non-failures: head moved, queue admin reset, manual intervention.
        assert!(!dequeue_reason_is_failure(Some("BRANCH_INVALIDATED")));
        assert!(!dequeue_reason_is_failure(Some("QUEUE_CLEARED")));
        assert!(!dequeue_reason_is_failure(Some("DEQUEUED")));
        assert!(!dequeue_reason_is_failure(None));
    }

    #[test]
    fn parses_standard_pr_url() {
        let result = parse_pr_url("https://github.com/djinnos/server/pull/42");
        assert_eq!(
            result,
            Some(("djinnos".to_string(), "server".to_string(), 42))
        );
    }

    #[test]
    fn parses_pr_url_with_trailing_fragment() {
        let result = parse_pr_url("https://github.com/owner/repo/pull/7#discussion");
        assert_eq!(result, Some(("owner".to_string(), "repo".to_string(), 7)));
    }

    #[test]
    fn rejects_non_pr_url() {
        assert_eq!(parse_pr_url("https://github.com/owner/repo/issues/1"), None);
    }

    #[test]
    fn rejects_non_github_url() {
        assert_eq!(parse_pr_url("https://gitlab.com/owner/repo/pull/1"), None);
    }

    // ---- CI-failure aggregation (E3) -------------------------------------

    fn check_run(name: &str, run_id: u64) -> CheckRun {
        CheckRun {
            id: run_id * 100 + name.len() as u64,
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: format!("https://github.com/o/r/actions/runs/{run_id}/job/{run_id}9"),
        }
    }

    fn failed_step(name: &str, number: u64) -> ActionsJobStep {
        ActionsJobStep {
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            number,
        }
    }

    fn failed_job(
        id: u64,
        name: &str,
        workflow: &str,
        steps: Vec<ActionsJobStep>,
    ) -> ActionsJob {
        ActionsJob {
            id,
            name: name.to_string(),
            status: "completed".to_string(),
            conclusion: Some("failure".to_string()),
            html_url: format!("https://github.com/o/r/actions/runs/x/job/{id}"),
            workflow_name: Some(workflow.to_string()),
            steps,
        }
    }

    #[test]
    fn parse_actions_run_id_extracts_id() {
        assert_eq!(
            parse_actions_run_id("https://github.com/o/r/actions/runs/123456/job/99"),
            Some(123456)
        );
        // Non-Actions check-run URL carries no run id.
        assert_eq!(
            parse_actions_run_id("https://github.com/o/r/checks/abc"),
            None
        );
    }

    #[test]
    fn ci_failure_sections_single_run() {
        // Baseline: one run with one failing job/step still renders correctly.
        let jobs = vec![failed_job(
            10,
            "build",
            "CI",
            vec![failed_step("cargo build", 3)],
        )];
        let checks = [check_run("CI / build", 100)];
        let refs: Vec<&CheckRun> = checks.iter().collect();
        let (sections, ci_jobs) = build_ci_failure_sections(Some(&jobs), &refs);

        let body = sections.join("\n");
        assert!(body.contains("**Workflow:** CI"), "{body}");
        assert!(body.contains("**Failed job:** build"), "{body}");
        assert!(body.contains("**Failed step:** cargo build"), "{body}");
        assert!(body.contains("ci_job_log(job_id=10"), "{body}");
        assert_eq!(ci_jobs.len(), 1);
        assert_eq!(ci_jobs[0]["job_id"].as_u64(), Some(10));
    }

    #[test]
    fn ci_failure_sections_unions_multiple_runs() {
        // Failures spread across two workflow runs (CI + Release): BOTH must be
        // represented, not just the first — this is the core E3 fix.
        let jobs = vec![
            failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
            failed_job(
                20,
                "publish",
                "Release",
                vec![failed_step("cargo publish", 5)],
            ),
        ];
        let checks = [check_run("CI / build", 100), check_run("Release / publish", 200)];
        let refs: Vec<&CheckRun> = checks.iter().collect();
        let (sections, ci_jobs) = build_ci_failure_sections(Some(&jobs), &refs);

        let body = sections.join("\n");
        // Both workflows headered.
        assert!(body.contains("**Workflow:** CI"), "{body}");
        assert!(body.contains("**Workflow:** Release"), "{body}");
        // Both jobs + steps present.
        assert!(body.contains("**Failed job:** build"), "{body}");
        assert!(body.contains("**Failed job:** publish"), "{body}");
        assert!(body.contains("cargo build"), "{body}");
        assert!(body.contains("cargo publish"), "{body}");
        // Both ci_jobs entries (the structured payload the worker consumes).
        assert_eq!(ci_jobs.len(), 2);
        let ids: Vec<u64> = ci_jobs
            .iter()
            .map(|j| j["job_id"].as_u64().unwrap())
            .collect();
        assert!(ids.contains(&10) && ids.contains(&20), "{ids:?}");
        // Hint lines reference both jobs.
        assert!(body.contains("ci_job_log(job_id=10"), "{body}");
        assert!(body.contains("ci_job_log(job_id=20"), "{body}");
    }

    #[test]
    fn ci_failure_sections_dedups_identical_jobs() {
        // The caller de-dups by job id; verify a duplicate job id collapses to
        // a single entry (defends the grouping invariant).
        let jobs = vec![
            failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
            failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
        ];
        // Simulate the caller's de-dup so the helper sees deduped input.
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<ActionsJob> =
            jobs.into_iter().filter(|j| seen.insert(j.id)).collect();
        let refs: Vec<&CheckRun> = Vec::new();
        let (_sections, ci_jobs) = build_ci_failure_sections(Some(&deduped), &refs);
        assert_eq!(ci_jobs.len(), 1);
    }

    #[test]
    fn ci_failure_run_cap_bounds_aggregation() {
        // The run cap is MAX_AGGREGATED_CI_RUNS. Emulate the caller's run-id
        // collection + truncation and assert it bounds the number of runs and
        // flags the cap, while keeping every run under the cap represented.
        let cap = super::MAX_AGGREGATED_CI_RUNS;
        let checks: Vec<CheckRun> = (0..(cap as u64 + 3))
            .map(|i| check_run(&format!("wf{i} / job"), 1000 + i))
            .collect();
        let refs: Vec<&CheckRun> = checks.iter().collect();

        let mut run_ids: Vec<u64> = Vec::new();
        for cr in &refs {
            if let Some(rid) = parse_actions_run_id(&cr.html_url)
                && !run_ids.contains(&rid)
            {
                run_ids.push(rid);
            }
        }
        let capped = run_ids.len() > cap;
        assert!(capped, "should exceed the cap with {} runs", run_ids.len());
        run_ids.truncate(cap);
        assert_eq!(run_ids.len(), cap, "run ids bounded to the cap");
    }

    #[test]
    fn ci_failure_sections_fallback_when_no_jobs() {
        // No Actions job data → fall back to listing raw check-run names.
        let checks = [check_run("lint", 100)];
        let refs: Vec<&CheckRun> = checks.iter().collect();
        let (sections, ci_jobs) = build_ci_failure_sections(None, &refs);
        let body = sections.join("\n");
        assert!(body.contains("**lint**"), "{body}");
        assert!(ci_jobs.is_empty());
    }
}
