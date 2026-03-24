use std::sync::Arc;

use djinn_core::models::TransitionAction;
use djinn_db::ActivityQuery;
use djinn_provider::github_api::{
    CheckRun, GitHubApiClient, MergeMethod, PrReviewFeedback, PrState,
};
use djinn_provider::repos::CredentialRepository;

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

/// Activity log event type for stored PR review feedback payloads.
///
/// Re-exported so the worker lifecycle layer can query for PR review feedback
/// without a module dependency on the coordinator's internal pr_poller.
pub const PR_REVIEW_FEEDBACK_EVENT: &str = "pr_review_feedback";

/// Activity log event type for per-cycle markers (used to count rounds).
const PR_REVIEW_CYCLE_EVENT: &str = "pr_review_cycle";

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
        let cred_repo = Arc::new(CredentialRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        ));
        let gh_client = GitHubApiClient::new(cred_repo);

        self.poll_pr_draft_tasks(&gh_client).await;
        self.poll_pr_review_tasks(&gh_client).await;
    }

    // ── pr_draft polling (CI monitoring) ─────────────────────────────────────

    /// Poll tasks in `pr_draft` status: wait for CI to pass, then undraft the PR.
    async fn poll_pr_draft_tasks(&mut self, gh_client: &GitHubApiClient) {
        let task_repo = self.task_repo();
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
                // No checks registered yet — skip, wait for next tick.
                continue;
            }

            let all_completed = checks.check_runs.iter().all(|cr| cr.status == "completed");

            if !all_completed {
                // CI checks still running — skip, check next tick.
                continue;
            }

            // All checks completed — check for failures.
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
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    sha = %current_sha,
                    failed_count = failed_checks.len(),
                    "PR poller: CI check failed on draft PR → reopening task for rework"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrCiFailed,
                    Some("CI checks failed on PR"),
                )
                .await;
                self.log_ci_failure_comment(
                    &task.id,
                    &failed_checks,
                    pr_url,
                    &current_sha,
                    gh_client,
                    &owner,
                    &repo,
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.pr_draft_first_seen.remove(&task.id);
                continue;
            }

            // All CI checks passed. Check for merge conflicts before undrafting.
            if pr.mergeable == Some(false) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: draft PR has merge conflicts → reopening task for rework"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrConflict,
                    Some("PR has merge conflicts"),
                )
                .await;
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
    async fn poll_pr_review_tasks(&mut self, gh_client: &GitHubApiClient) {
        let task_repo = self.task_repo();
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

            let changes_requested = reviews
                .iter()
                .any(|r| r.state.as_str() == "CHANGES_REQUESTED");

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

                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrChangesRequested,
                    Some("Reviewer requested changes on PR"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
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
                    let all_completed =
                        checks.check_runs.iter().all(|cr| cr.status == "completed");

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
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            sha = %current_sha,
                            failed_count = failed_checks.len(),
                            "PR poller: CI check failed on review PR → reopening task for rework"
                        );
                        self.apply_pr_transition(
                            &task.id,
                            TransitionAction::PrCiFailed,
                            Some("CI checks failed on PR"),
                        )
                        .await;
                        self.log_ci_failure_comment(
                            &task.id,
                            &failed_checks,
                            pr_url,
                            &current_sha,
                            gh_client,
                            &owner,
                            &repo,
                        )
                        .await;
                        self.pr_status_cache.remove(&task.id);
                        self.merge_fail_count.remove(&task.id);
                        continue;
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
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::PrConflict,
                    Some("PR has merge conflicts"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                self.merge_fail_count.remove(&task.id);
                continue;
            }

            let has_approved = reviews.iter().any(|r| r.state.as_str() == "APPROVED");
            let has_reviews = !reviews.is_empty();

            if has_reviews && !has_approved {
                // Reviews exist but none APPROVED (and no CHANGES_REQUESTED handled above).
                // This means reviews are pending or only COMMENTED. Wait for approval.
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
                }
                Err(e) => {
                    let count = self
                        .merge_fail_count
                        .entry(task.id.clone())
                        .or_insert(0);
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

            // Look up the task's project_id and escalate to Architect.
            if let Ok(Some(task)) = self.task_repo().get(task_id).await {
                let reason = format!(
                    "PR review loop exceeded {PR_REVIEW_ROUND_THRESHOLD} rounds without approval. PR: {}",
                    feedback.pr_url
                );
                self.dispatch_architect_escalation(task_id, &reason, &task.project_id)
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
        if let Err(e) = task_repo
            .transition(task_id, action, "system", "pr_poller", reason, None)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to apply task transition"
            );
        }
    }

    /// Log a comment on the task with details about which CI checks failed,
    /// including the actual error annotations from GitHub so the worker can fix them.
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
        let mut check_lines: Vec<String> = Vec::new();

        for cr in failed_checks {
            let conclusion = cr.conclusion.as_deref().unwrap_or("unknown");
            check_lines.push(format!(
                "- **{}** ({}): {}",
                cr.name, conclusion, cr.html_url
            ));

            // Fetch annotations for this check run to surface actual error messages.
            match gh_client
                .get_check_run_annotations(owner, repo, cr.id)
                .await
            {
                Ok(annotations) if !annotations.is_empty() => {
                    for ann in &annotations {
                        let title_part = ann
                            .title
                            .as_deref()
                            .map(|t| format!(" ({})", t))
                            .unwrap_or_default();
                        check_lines.push(format!(
                            "  - `{}` L{}-L{} [{}]{}: {}",
                            ann.path,
                            ann.start_line,
                            ann.end_line,
                            ann.annotation_level,
                            title_part,
                            ann.message,
                        ));
                    }
                }
                Ok(_) => {
                    // No annotations — fetch the Actions job logs instead.
                    // Parse run_id from the check run URL to find the failed jobs.
                    let log_snippet = self
                        .fetch_failed_job_log_snippet(gh_client, owner, repo, &cr.html_url)
                        .await;
                    if let Some(snippet) = log_snippet {
                        check_lines.push(format!("  - **Job log (last {} chars):**", snippet.len()));
                        check_lines.push(format!("```\n{}\n```", snippet));
                    } else {
                        check_lines.push("  - _(no annotations or job logs available)_".to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id,
                        check_run_id = cr.id,
                        error = %e,
                        "PR poller: failed to fetch check run annotations"
                    );
                    check_lines.push(format!("  - _(failed to fetch annotations: {})_", e));
                }
            }
        }

        let body = format!(
            "**CI checks failed on PR** (commit `{sha}`)\n\n\
             The following CI checks failed. Review the error details below and fix the failures before the PR can merge.\n\n\
             {checks}\n\n\
             PR: {pr_url}",
            sha = &sha[..sha.len().min(12)],
            checks = check_lines.join("\n"),
            pr_url = pr_url,
        );

        let payload = serde_json::json!({ "body": body }).to_string();
        let task_repo = self.task_repo();
        if let Err(e) = task_repo
            .log_activity(Some(task_id), "pr_poller", "verification", "comment", &payload)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to log CI failure comment"
            );
        }
    }

    /// Fetch the tail of the first failed job's log from a GitHub Actions run.
    ///
    /// Parses the run_id from the check run URL, lists jobs in that run,
    /// finds the first failed job, and returns the last `MAX_LOG_SNIPPET_CHARS`
    /// of its log output.
    async fn fetch_failed_job_log_snippet(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        check_run_url: &str,
    ) -> Option<String> {
        /// Maximum characters to include from a job log tail.
        const MAX_LOG_SNIPPET_CHARS: usize = 4000;

        // Parse run_id from URL like:
        //   https://github.com/{owner}/{repo}/actions/runs/{run_id}/job/{job_id}
        let run_id = check_run_url
            .split("/actions/runs/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .and_then(|s| s.parse::<u64>().ok())?;

        let jobs = match gh_client.list_run_jobs(owner, repo, run_id).await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::debug!(
                    run_id,
                    error = %e,
                    "PR poller: failed to list jobs for run"
                );
                return None;
            }
        };

        // Find the first failed job.
        let failed_job = jobs.iter().find(|j| {
            matches!(
                j.conclusion.as_deref(),
                Some("failure") | Some("timed_out") | Some("cancelled")
            )
        })?;

        match gh_client.get_job_logs(owner, repo, failed_job.id).await {
            Ok(log) => {
                // Take the tail — error output is usually at the end.
                let snippet = if log.len() > MAX_LOG_SNIPPET_CHARS {
                    &log[log.len() - MAX_LOG_SNIPPET_CHARS..]
                } else {
                    &log
                };
                Some(snippet.to_string())
            }
            Err(e) => {
                tracing::debug!(
                    job_id = failed_job.id,
                    error = %e,
                    "PR poller: failed to fetch job logs"
                );
                None
            }
        }
    }
}

/// Parse a GitHub PR URL into `(owner, repo, pull_number)`.
///
/// Handles URLs of the form `https://github.com/{owner}/{repo}/pull/{number}`.
fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
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

#[cfg(test)]
mod tests {
    use super::parse_pr_url;

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
}
