use super::*;

impl CoordinatorActor {
    /// Resolve every unresolved review thread on a PR via GraphQL.
    ///
    /// Idempotent: GitHub no-ops an already-resolved thread and it won't
    /// reappear in the unresolved list, so repeated calls converge. Returns the
    /// number of threads resolved this call, or `None` if the *listing* failed
    /// (caller should retry next tick). `Some(0)` means there were no
    /// unresolved threads to begin with.
    ///
    /// The caller decides WHEN this is appropriate. It is only ever invoked
    /// when the PR is APPROVED: an explicit approval is the override signal that
    /// the reviewer's inline comments are non-blocking. Shared by the direct
    /// REST-merge 405 path and the GitHub-managed auto-merge observe path so
    /// both enforce the same policy.
    pub(in crate::actors::coordinator) async fn resolve_unresolved_conversations(
        &self,
        gh_client: &GitHubApiClient,
        owner: &str,
        repo: &str,
        pull_number: u64,
        task_short_id: &str,
    ) -> Option<usize> {
        let ids = match gh_client
            .list_unresolved_review_thread_ids(owner, repo, pull_number)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to list unresolved review threads"
                );
                return None;
            }
        };

        let mut resolved = 0;
        for tid in &ids {
            match gh_client.resolve_review_thread(tid).await {
                Ok(()) => resolved += 1,
                Err(re) => tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    thread = %tid,
                    error = %re,
                    "PR poller: resolve_review_thread failed"
                ),
            }
        }
        Some(resolved)
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
    pub(in crate::actors::coordinator) async fn observe_auto_merge_state(
        &mut self,
        gh_client: &GitHubApiClient,
        task_id: &str,
        task_short_id: &str,
        pr_url: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
        has_approved: bool,
        current_sha: &str,
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
            // Approved PR whose auto-merge is BLOCKED: the most likely
            // remaining gate (we're already past approval + CI in
            // `poll_pr_review_tasks`) is the "a conversation must be resolved
            // before merge" branch-protection rule. Unlike the direct REST
            // merge path, GitHub-managed auto-merge never surfaces a 405 we can
            // react to — it silently waits forever — so the reactive
            // resolution in `poll_pr_review_tasks` is never reached for
            // auto-merge repos. Resolve the leftover threads here too, mirroring
            // that path's policy: an explicit approval declares the reviewer's
            // inline comments non-blocking. GitHub re-evaluates auto-merge once
            // the block clears, and the next tick observes the merge.
            //
            // `conversations_resolved` (keyed task→SHA) stops us re-querying
            // review threads every 30s when a DIFFERENT rule (e.g. a pending
            // CODEOWNERS review) keeps the PR BLOCKED after the conversations
            // are already resolved.
            let already_resolved_this_sha =
                self.conversations_resolved.get(task_id) == Some(&current_sha.to_string());
            if should_auto_resolve_conversations(
                has_approved,
                state.merge_state_status.as_deref(),
                already_resolved_this_sha,
            ) && let Some(resolved) = self
                .resolve_unresolved_conversations(
                    gh_client,
                    owner,
                    repo,
                    pull_number,
                    task_short_id,
                )
                .await
            {
                if resolved > 0 {
                    tracing::info!(
                        task_id = %task_short_id,
                        pr = pull_number,
                        resolved,
                        "PR poller: approved auto-merge PR blocked on unresolved conversations — resolved threads, GitHub will re-evaluate auto-merge"
                    );
                }
                // Mark this SHA done regardless of count: a successful list
                // (even of zero) means the conversation gate is no longer
                // the blocker, so stop re-querying until a new push lands.
                // A failed list returns None and leaves the entry unset so
                // we retry next tick.
                self.conversations_resolved
                    .insert(task_id.to_string(), current_sha.to_string());
            }
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
    pub(in crate::actors::coordinator) async fn handle_queue_failure(
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
                                // A failure-flavored dequeue means GitHub's
                                // queue itself rejected the group — every
                                // surfaced failure is treated as blocking;
                                // no advisory split here.
                                self.log_ci_failure_comment(
                                    task_id,
                                    &failed,
                                    &[],
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

        let transition_reason =
            format!("merge queue rejected PR (reason: {reason}) — re-run with fresh CI feedback");
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

    /// Clean-merge fast path: before flagging a `mergeable == false` PR for
    /// ConflictRetry, try to resolve it MECHANICALLY (fetch fresh, ephemeral
    /// merge of the target into the task branch, push the result to mirror +
    /// GitHub) so the PR refreshes with zero agent involvement.
    ///
    /// Returns `true` when the merge landed cleanly and was pushed (the caller
    /// must NOT set conflict metadata, NOT reopen — the PR is now mergeable).
    /// Returns `false` for a real conflict OR any indeterminate failure — the
    /// caller proceeds with its normal flag-and-reopen path. On a clean
    /// auto-merge an `auto_merged_conflict` activity event is logged.
    pub(in crate::actors::coordinator) async fn try_auto_merge_before_conflict(
        &self,
        task_id: &str,
        task_short_id: &str,
        project_id: &str,
    ) -> bool {
        let Some(mirror) = self.mirror.as_ref() else {
            return false;
        };

        let project_repo = djinn_db::ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let merge_target = match project_repo.get_config(project_id).await {
            Ok(Some(cfg)) => cfg.target_branch,
            _ => "main".to_string(),
        };
        let task_branch = format!("task/{task_short_id}");

        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let outcome = crate::task_merge::try_auto_merge_target_into_task_branch(
            mirror,
            &self.db,
            &event_bus,
            project_id,
            task_short_id,
            &task_branch,
            &merge_target,
        )
        .await;

        match outcome {
            crate::task_merge::AutoMergeOutcome::AutoMerged => {
                let payload = serde_json::json!({
                    "merge_target": merge_target,
                    "task_branch": task_branch,
                })
                .to_string();
                if let Err(e) = self
                    .task_repo()
                    .log_activity(
                        Some(task_id),
                        "coordinator",
                        "system",
                        "auto_merged_conflict",
                        &payload,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task_short_id,
                        error = %e,
                        "PR poller: failed to log auto_merged_conflict activity"
                    );
                }
                tracing::info!(
                    task_id = %task_short_id,
                    merge_target = %merge_target,
                    "PR poller: auto-merged {merge_target} into task branch; PR refreshed without agent dispatch"
                );
                true
            }
            crate::task_merge::AutoMergeOutcome::Conflicts
            | crate::task_merge::AutoMergeOutcome::Indeterminate => false,
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
    pub(in crate::actors::coordinator) async fn build_pr_conflict_reason(
        &self,
        task_short_id: &str,
        project_id: &str,
    ) -> String {
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
    pub(in crate::actors::coordinator) async fn add_conflict_blocker_for_sibling(
        &self,
        task: &Task,
    ) {
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
    pub(in crate::actors::coordinator) async fn find_auto_approver_session(
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
    pub(in crate::actors::coordinator) async fn task_created_by_user_id(
        &self,
        task_id: &str,
    ) -> Option<String> {
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

pub(in crate::actors::coordinator) fn should_auto_resolve_conversations(
    has_approved: bool,
    merge_state_status: Option<&str>,
    already_resolved_this_sha: bool,
) -> bool {
    has_approved && merge_state_status == Some("BLOCKED") && !already_resolved_this_sha
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
pub(in crate::actors::coordinator) fn dequeue_reason_is_failure(reason: Option<&str>) -> bool {
    match reason {
        None => false,
        // GitHub emits two vocabularies for this reason depending on surface:
        // SCREAMING_CASE GraphQL-enum style (`CHECKS_FAILED`) and lowercase
        // snake_case on `RemovedFromMergeQueueEvent` timeline nodes
        // (`failed_checks`, `merged`) — compare case-insensitively or the
        // safe-list never matches real events and EVERY dequeue (including a
        // successful merge) reopens the task for rework. `MERGED` is the
        // queue's success exit; the next poll tick sees `pr.merged` and closes
        // the task. Unknown reasons stay conservative-failure so a new GitHub
        // vocabulary never silently swallows a real eviction.
        Some(r) => !matches!(
            r.to_ascii_uppercase().as_str(),
            "MERGED" | "BRANCH_INVALIDATED" | "QUEUE_CLEARED" | "DEQUEUED"
        ),
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
pub(in crate::actors::coordinator) async fn enable_auto_merge_best_effort(
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
