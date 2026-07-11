use super::*;

impl CoordinatorActor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn observe_auto_merge_state(
        &mut self,
        gh_client: &GitHubApiClient,
        task_id: &str,
        task_short_id: &str,
        pr_url: &str,
        owner: &str,
        repo: &str,
        pull_number: u64,
        pr_node_id: &str,
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

        // ── Sticky failure-dequeue check ──────────────────────────────────
        // A failure-flavored dequeue with no PR head commit after it means
        // the queue rejected exactly this code and no rework has landed
        // since. This must be handled REGARDLESS of the
        // PR's current queue/auto-merge state: GitHub's merge-when-ready can
        // re-add the PR to the queue seconds after the eviction (and a
        // coordinator restart loses `delegated_to_github`, making the merge
        // path blindly re-enqueue) — so a check that only fires when a tick
        // happens to observe the PR *out* of the queue misses the eviction
        // entirely and the same broken head churns the queue forever
        // (observed on PRs #491/#492, 2026-06-12). `handled_dequeues`
        // consumes each event once so the reopen doesn't re-fire while
        // rework for it is still pending on the same SHA.
        if dequeue_requires_rework(
            state.last_dequeue.as_ref(),
            state.head_committed_at.as_deref(),
            self.handled_dequeues.get(task_id).map(|s| s.as_str()),
        ) {
            // Pull the PR back out of the queue / disarm merge-when-ready
            // first so GitHub stops churning merge groups on the rejected
            // head while the reopened task is being reworked. Best-effort:
            // a fresh push from the rework invalidates the entry anyway.
            if state.merge_queue_entry.is_some()
                && let Err(e) = gh_client.dequeue_pull_request(pr_node_id).await
            {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to dequeue rejected PR before reopen (continuing)"
                );
            }
            if state.auto_merge_request.is_some()
                && let Err(e) = gh_client.disable_auto_merge(pr_node_id).await
            {
                tracing::warn!(
                    task_id = %task_short_id,
                    pr = pull_number,
                    error = %e,
                    "PR poller: failed to disarm auto-merge on rejected PR before reopen (continuing)"
                );
            }
            if let Some(created_at) = state
                .last_dequeue
                .as_ref()
                .and_then(|d| d.created_at.clone())
            {
                self.handled_dequeues
                    .insert(task_id.to_string(), created_at);
            }
            self.handle_queue_failure(
                gh_client,
                owner,
                repo,
                task_id,
                task_short_id,
                pull_number,
                pr_url,
                state.last_dequeue.as_ref(),
                "merge_queue_dequeued",
            )
            .await;
            return;
        }

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

        // Auto-merge is no longer enabled and the PR isn't queued, and the
        // sticky dequeue check above found no unconsumed failure eviction
        // for the current head (a failure for a PREVIOUS head means rework
        // already landed and just needs re-delegation). Fall through
        // silently — next tick re-enables auto-merge via the undraft path
        // on a fresh push, or a human intervenes.
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
    pub(crate) async fn handle_queue_failure(
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
        let reason = match dequeue.and_then(|d| d.reason.clone()) {
            Some(reason) => reason,
            None => "unknown".to_string(),
        };
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
        // Reuses `log_ci_failure_comment`, which logs a `comment` entry with
        // `actor_role="verification"` (CI failure logging) surfaced to the
        // worker via `recent_feedback`, naming the failed
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
        self.review_stuck_sha_first_seen.remove(task_id);
        self.merge_fail_count.remove(task_id);
        self.delegated_to_github.remove(task_id);
    }

    pub(crate) fn poll_auto_merge_fast_path(
        &mut self,
        task_id: &str,
        task_short_id: &str,
        project_id: &str,
    ) -> AutoMergeFastPathState {
        // No mirror configured (tests / off-server) → behave as the old
        // synchronous `false`: skip the fast path, reopen immediately.
        if self.mirror.is_none() {
            return AutoMergeFastPathState::Reopen;
        }

        // Consult+mutate the shared tracker atomically under one lock via the
        // pure `decide_auto_merge_tick` seam: a completed entry is consumed and
        // returned, an in-flight entry short-circuits (skip), and an absent entry
        // is marked in-flight so a re-entrant tick (or the sibling draft/review
        // callsite) can't double-spawn.
        let (decision, tracked) = match self.auto_merge_tracker.lock() {
            Ok(mut guard) => {
                let decision = decide_auto_merge_tick(&mut guard, task_id);
                (decision, guard.len())
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "PR poller: auto-merge tracker lock poisoned; skipping fast path"
                );
                return AutoMergeFastPathState::Reopen;
            }
        };
        record_auto_merge_decision_metrics(&decision, tracked);
        match decision {
            AutoMergeTickDecision::Return(state) => return state,
            AutoMergeTickDecision::Spawn => {}
        }

        // Spawn the heavy merge off the actor tick. It records its terminal state
        // back into the tracker and clears the in-flight flag; the next tick
        // consumes it via the match above.
        let Some(mirror) = self.mirror.clone() else {
            tracing::warn!(
                task_id,
                "PR poller: mirror disappeared before spawning auto-merge fast path"
            );
            return AutoMergeFastPathState::Reopen;
        };
        let db = self.db.clone();
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let tracker = self.auto_merge_tracker.clone();
        let task_id = task_id.to_string();
        let task_short_id = task_short_id.to_string();
        let project_id = project_id.to_string();
        tokio::spawn(async move {
            let final_state = Self::run_auto_merge_fast_path(
                &mirror,
                &db,
                &event_bus,
                &task_id,
                &task_short_id,
                &project_id,
            )
            .await;
            match tracker.lock() {
                Ok(mut guard) => {
                    guard.insert(task_id, final_state);
                    djinn_telemetry::pr_poller::set_tracked(guard.len());
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "PR poller: auto-merge tracker lock poisoned; dropping fast-path result"
                    );
                }
            }
        });

        AutoMergeFastPathState::InFlight
    }

    /// The heavy, slot-free body of the clean-merge fast path — runs in a spawned
    /// background task (NOT on the coordinator tick). Returns the terminal
    /// [`AutoMergeFastPathState`] the poller consumes next tick. Takes only
    /// `'static`-cloneable handles (mirror `Arc`, db, event bus) so it can detach.
    pub(crate) async fn run_auto_merge_fast_path(
        mirror: &Arc<djinn_workspace::MirrorManager>,
        db: &djinn_db::Database,
        event_bus: &djinn_core::events::EventBus,
        task_id: &str,
        task_short_id: &str,
        project_id: &str,
    ) -> AutoMergeFastPathState {
        let project_repo = djinn_db::ProjectRepository::new(db.clone(), event_bus.clone());
        let merge_target = match project_repo.get_config(project_id).await {
            Ok(Some(cfg)) => cfg.target_branch,
            _ => "main".to_string(),
        };
        let task_branch = format!("task/{task_short_id}");

        let outcome = crate::task_merge::try_auto_merge_target_into_task_branch(
            mirror,
            db,
            event_bus,
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
                let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
                if let Err(e) = task_repo
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
                    "PR poller: auto-merged {merge_target} into task branch (background); PR refreshed without agent dispatch"
                );
                AutoMergeFastPathState::Merged
            }
            crate::task_merge::AutoMergeOutcome::Conflicts
            | crate::task_merge::AutoMergeOutcome::Indeterminate => AutoMergeFastPathState::Reopen,
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
    pub(crate) async fn build_pr_conflict_reason(
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
}
/// What the PR poller should do with a `mergeable == false` task this tick,
/// decided purely from the clean-merge fast-path tracker state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AutoMergeTickDecision {
    /// Return this terminal/in-flight state to the caller without spawning.
    Return(AutoMergeFastPathState),
    /// No attempt recorded — the caller should spawn the background merge (the
    /// entry has already been marked `InFlight` by this function).
    Spawn,
}

/// Pure state-machine seam for the offloaded clean-merge fast path. Given the
/// shared tracker and a task id, decide-and-mutate atomically (the caller holds
/// the lock):
///
/// - `InFlight` present → `Return(InFlight)` (skip this tick; a merge is running —
///   this is the guard that stops repeated ticks over a slow merge from
///   double-firing the heavy git work or double-dispatching the task).
/// - `Merged`/`Reopen` present → consume (remove) it and `Return` it (the next
///   conflict on the same task re-arms a fresh attempt).
/// - absent → mark `InFlight` and return `Spawn` (first attempt this cycle).
///
/// Kept free + pure so the guard contract is unit-testable without a DB, mirror,
/// or actor.
pub(crate) fn decide_auto_merge_tick(
    tracker: &mut std::collections::HashMap<String, AutoMergeFastPathState>,
    task_id: &str,
) -> AutoMergeTickDecision {
    match tracker.get(task_id).cloned() {
        Some(AutoMergeFastPathState::InFlight) => {
            AutoMergeTickDecision::Return(AutoMergeFastPathState::InFlight)
        }
        Some(state @ (AutoMergeFastPathState::Merged | AutoMergeFastPathState::Reopen)) => {
            tracker.remove(task_id);
            AutoMergeTickDecision::Return(state)
        }
        None => {
            tracker.insert(task_id.to_string(), AutoMergeFastPathState::InFlight);
            AutoMergeTickDecision::Spawn
        }
    }
}

/// Emit PR-poller metrics after the tracker lock has been released.
///
/// The poller holds the tracker mutex only long enough to make the state-machine
/// decision and capture its O(1) size. Counter/gauge recording is synchronous,
/// but keeping it outside the critical section prevents future telemetry changes
/// from accidentally expanding the lock scope or introducing a lock-across-await
/// footgun at the merge-failure call site.
pub(crate) fn record_auto_merge_decision_metrics(decision: &AutoMergeTickDecision, tracked: usize) {
    djinn_telemetry::pr_poller::set_tracked(tracked);
    if matches!(
        decision,
        AutoMergeTickDecision::Return(AutoMergeFastPathState::Reopen)
    ) {
        djinn_telemetry::pr_poller::increment_merge_failure();
    }
}

/// Determine if a `DequeuedEvent.reason` indicates a real failure that
/// should kick the task back into the worker loop.
///
/// Reasons that are NOT failures: `"BRANCH_INVALIDATED"` (head moved —
/// queue will pick up the new SHA), `"QUEUE_CLEARED"` (admin reset),
/// `"DEQUEUED"`/`"MANUAL"` (manual intervention by a human — `manual` is the
/// `RemovedFromMergeQueueEvent` timeline-surface word, `DEQUEUED` the
/// GraphQL-enum surface for the same operator queue-removal).
///
/// All other reasons (CHECKS_FAILED, MERGE_CONFLICT, NO_RESPONSE,
/// NOT_QUEUEABLE, ROLL_BACK, UNKNOWN_REMOVAL_REASON, anything new GitHub
/// adds) are treated as failures by default — safer to surface a spurious
/// re-run than to silently drop a real failure.
pub(crate) fn dequeue_reason_is_failure(reason: Option<&str>) -> bool {
    match reason {
        None => false,
        // GitHub emits two vocabularies for this reason depending on surface:
        // SCREAMING_CASE GraphQL-enum style (`CHECKS_FAILED`, `DEQUEUED`) and
        // lowercase snake_case on `RemovedFromMergeQueueEvent` timeline nodes
        // (`failed_checks`, `merged`, `manual`) — compare case-insensitively
        // or the safe-list never matches real events and EVERY dequeue
        // (including a successful merge) reopens the task for rework. The two
        // surfaces don't share spellings for an operator queue-removal:
        // `manual` is the timeline word, `DEQUEUED` the GraphQL-enum word for
        // the same human dequeue — both are benign and must be safe-listed.
        // `MERGED` is the queue's success exit; the next poll tick sees
        // `pr.merged` and closes the task. Unknown reasons stay
        // conservative-failure so a new GitHub vocabulary never silently
        // swallows a real eviction.
        Some(r) => !matches!(
            r.to_ascii_uppercase().as_str(),
            "MERGED" | "BRANCH_INVALIDATED" | "QUEUE_CLEARED" | "DEQUEUED" | "MANUAL"
        ),
    }
}

/// Decide whether a merge-queue dequeue event demands reopening the task.
///
/// True only when all three hold:
///   * the reason is failure-flavored (see [`dequeue_reason_is_failure`]);
///   * no rework has landed since the rejection: the PR's head commit
///     timestamp (`head_committed_at`) is not later than the dequeue's
///     `created_at`. Both come from GitHub as RFC3339 UTC strings, so a
///     lexicographic compare is a chronological compare. (The event's
///     `beforeCommit` cannot be used for this — it is the merge-group head,
///     not the PR head.) A missing timestamp on either side degrades to
///     the conservative pre-existing behavior (reopen) rather than
///     silently never reopening;
///   * the event hasn't been consumed already (`handled_created_at` is the
///     `created_at` of the last dequeue this task was reopened for).
pub(crate) fn dequeue_requires_rework(
    dequeue: Option<&djinn_provider::github_api::DequeueEvent>,
    head_committed_at: Option<&str>,
    handled_created_at: Option<&str>,
) -> bool {
    let Some(dequeue) = dequeue else {
        return false;
    };
    if !dequeue_reason_is_failure(dequeue.reason.as_deref()) {
        return false;
    }
    if let (Some(head_at), Some(dequeued_at)) = (head_committed_at, dequeue.created_at.as_deref())
        && head_at > dequeued_at
    {
        // A commit landed after the eviction — the rejection is stale.
        return false;
    }
    match (dequeue.created_at.as_deref(), handled_created_at) {
        (Some(event), Some(handled)) => event != handled,
        // No created_at on the event: cannot dedup, err on reopening.
        _ => true,
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
pub(crate) async fn enable_auto_merge_best_effort(
    gh_client: &GitHubApiClient,
    task_short_id: &str,
    pull_number: u64,
    pr_node_id: &str,
    pr_title: &str,
    method: MergeMethod,
) {
    match gh_client
        .enable_auto_merge(
            "", // owner unused by GraphQL mutation
            "", // repo unused by GraphQL mutation
            pull_number,
            method,
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
            let github_error = render_github_write_error("GitHub auto-merge enable failed", &e);
            // Already enabled (re-undraft) is harmless; other errors mean
            // the repo can't auto-merge and we'll fall back to manual.
            tracing::info!(
                task_id = %task_short_id,
                pr = pull_number,
                error = %github_error,
                "PR poller: auto-merge not enabled (will use legacy merge path)"
            );
        }
    }
}
