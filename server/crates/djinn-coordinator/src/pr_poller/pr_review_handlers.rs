use super::*;
use crate::pr_poller::pr_cleanup::CloseKind;

impl CoordinatorActor {
    pub(crate) async fn attach_pr_review_feedback(
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
    pub(crate) async fn maybe_re_request_review(
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

    /// Close a task whose PR has merged, recording the landed merge-commit SHA.
    ///
    /// The SHA is persisted *before* the `PrMerge` transition so the
    /// `task_updated` event the transition emits already carries it — that is
    /// what lets the board place the card in the Merged column and the
    /// coordinator record a throughput event (both gate on
    /// `merge_commit_sha IS NOT NULL`). An empty/absent SHA degrades to a plain
    /// `PrMerge` (task still closes; it just won't show as merged) rather than
    /// blocking the close.
    pub(crate) async fn apply_pr_merge(&self, task_id: &str, merge_commit_sha: Option<&str>) {
        if let Some(sha) = merge_commit_sha.filter(|s| !s.is_empty()) {
            if let Err(e) = self.task_repo().set_merge_commit_sha(task_id, sha).await {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "PR poller: failed to persist merge_commit_sha (task will still close)"
                );
            }
        } else {
            tracing::warn!(
                task_id,
                "PR poller: PR merged without a known merge_commit_sha — task closes but won't show as merged"
            );
        }
        self.apply_pr_transition(task_id, TransitionAction::PrMerge, None)
            .await;
    }

    pub(crate) async fn apply_pr_transition(
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
        record_pr_transition_reopen_metric(&cleanup_action);
        // Drop any clean-merge fast-path tracker entry for a now-terminal task,
        // so a background merge that finished after the PR closed doesn't leave a
        // dangling `Merged`/`Reopen` entry that is never consumed. Bounded leak
        // either way (entries are consumed on read), but kept tidy here.
        if matches!(
            cleanup_action,
            TransitionAction::PrMerge | TransitionAction::ForceClose
        ) {
            let tracked = {
                let mut guard = self.auto_merge_tracker.lock().unwrap();
                guard.remove(task_id);
                guard.len()
            };
            djinn_telemetry::pr_poller::set_tracked(tracked);
        }
        if matches!(
            cleanup_action,
            TransitionAction::PrMerge | TransitionAction::ForceClose
        ) {
            match task_repo.get(task_id).await {
                Ok(Some(task)) => {
                    let close_kind = match cleanup_action {
                        TransitionAction::PrMerge => CloseKind::Merge,
                        _ => CloseKind::NonMerge,
                    };
                    self.cleanup_pr_and_branch_on_close(&task, close_kind).await;
                }
                Ok(None) => {
                    tracing::warn!(
                        task_id,
                        "PR poller: task disappeared before inline PR cleanup"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id,
                        error = %e,
                        "PR poller: failed to reload task for inline PR cleanup"
                    );
                }
            }
        }
    }

    pub(crate) async fn add_conflict_blocker_for_sibling(&self, task: &Task) {
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
    // The advisory list is informational context, not a rework driver; it
    // rides along with the blocking-failure args rather than a bag struct.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn log_ci_failure_comment(
        &self,
        task_id: &str,
        failed_checks: &[&CheckRun],
        advisory_failed: &[&CheckRun],
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
        let jobs: Option<&[ActionsJob]> = if any_fetched { Some(&all_jobs) } else { None };

        let (mut sections, ci_jobs) = build_ci_failure_sections(jobs, failed_checks);

        if capped {
            sections.push(format!(
                "\n_Note: more than {MAX_AGGREGATED_CI_RUNS} workflow runs failed; \
                 showing the first {MAX_AGGREGATED_CI_RUNS}. Re-run CI after fixing \
                 these to surface any remaining failures._"
            ));
        }

        if let Some(advisory) = advisory_checks_section(advisory_failed) {
            sections.push(advisory);
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
    pub(crate) async fn find_auto_approver_session(
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
    pub(crate) async fn task_created_by_user_id(&self, task_id: &str) -> Option<String> {
        match self.task_repo().created_by_user_id(task_id).await {
            Ok(opt) => opt,
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

pub(crate) fn pr_transition_increments_reopen_count(action: &TransitionAction) -> bool {
    matches!(
        action,
        TransitionAction::PrCiFailed | TransitionAction::PrChangesRequested
    )
}

pub(crate) fn record_pr_transition_reopen_metric(action: &TransitionAction) -> bool {
    let increments_reopen = pr_transition_increments_reopen_count(action);
    if increments_reopen {
        djinn_telemetry::task::increment_reopen();
    }
    increments_reopen
}

/// Parse a GitHub PR URL into `(owner, repo, pull_number)`.
///
/// Handles URLs of the form `https://github.com/{owner}/{repo}/pull/{number}`.
pub fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
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
pub(crate) fn is_racing_unmerged_status(status: &str) -> bool {
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
pub(crate) fn pick_conflict_blocker_sibling(
    this_task_id: &str,
    siblings: &[Task],
) -> Option<String> {
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
pub(crate) fn effective_review_decision(reviews: &[PrReview]) -> (bool, bool) {
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
