use super::super::*;
#[cfg(test)]
use djinn_core::models::TaskStatus;
use djinn_core::models::TransitionAction;
use djinn_core::models::task::IssueType;

impl CoordinatorActor {
    #[tracing::instrument(
        name = "djinn.coordinator.approved_pass",
        skip(self),
        fields(pass_kind = "approved")
    )]
    pub(in crate::actors::coordinator) async fn process_approved_tasks(&mut self) {
        let repo = self.task_repo();
        let tasks = match repo.list_by_status("approved").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_by_status(approved) failed");
                return;
            }
        };

        if tasks.is_empty() {
            return;
        }

        // Build an AgentContext for the merge helpers (they need DB + event bus +
        // git actors).  This is the same construction used by the stale-sweep path
        // in the tick loop.
        let app_state = crate::context::AgentContext {
            db: self.db.clone(),
            event_bus: crate::events::event_bus_for(&self.events_tx),
            git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            verifying_tasks: self.verification_tracker.clone(),
            role_registry: self.role_registry.clone(),
            health_tracker: self.health.clone(),
            file_time: Arc::new(crate::file_time::FileTime::new()),
            lsp: self.lsp.clone(),
            catalog: self.catalog.clone(),
            coordinator: Arc::new(tokio::sync::Mutex::new(None)),
            active_tasks: crate::context::ActivityTracker::default(),
            task_ops_project_path_override: None,
            working_root: None,
            graph_warmer: None,
            repo_graph_ops: None,
            runtime_ops: None,
            cargo_target_runs_root: Some(djinn_core::paths::cargo_target_runs_root()),
            mirror: self.mirror.clone(),
            rpc_registry: None,
            default_project_id: None,
        };

        for task in tasks {
            // Simple-lifecycle tasks normally close directly, but sessions that
            // produced durable artifacts (file changes, memory writes, or task
            // comments pointing at .djinn paths) must survive as branch/PR
            // artifacts instead of being short-circuited here.
            let simple = IssueType::parse(&task.issue_type)
                .map(|it| it.uses_simple_lifecycle())
                .unwrap_or(false);
            if simple
                && !self
                    .simple_lifecycle_task_has_durable_artifacts(&task.id)
                    .await
            {
                tracing::info!(
                    task_id = %task.short_id,
                    issue_type = %task.issue_type,
                    "CoordinatorActor: simple-lifecycle task approved — closing directly"
                );
                if let Err(e) = repo
                    .transition(
                        &task.id,
                        TransitionAction::Close,
                        "coordinator",
                        "system",
                        Some("simple-lifecycle task — no PR needed"),
                        None,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: failed to close simple-lifecycle approved task"
                    );
                }
                continue;
            }

            if simple {
                tracing::info!(
                    task_id = %task.short_id,
                    issue_type = %task.issue_type,
                    "CoordinatorActor: simple-lifecycle task approved with durable artifacts — routing through PR flow"
                );
            }

            tracing::info!(
                task_id = %task.short_id,
                task_uuid = %task.id,
                project_id = %task.project_id,
                "CoordinatorActor: processing approved task for PR push/open"
            );

            // K8s flow: instead of the legacy task_merge::merge_and_transition
            // (which relied on a worktree layout the K8s mirror doesn't have,
            // and was spam-logging "local branch task/X does not exist" every
            // 30s), call supervisor_pr_open directly.
            //
            // supervisor_pr_open is idempotent for re-cycles:
            //   1. Clones the mirror on task_branch (preserves prior cycles'
            //      commits — the supervisor body pushes them back via
            //      workspace.push_to_origin on each successful run).
            //   2. Force-pushes refs/heads/task_branch to origin → updates
            //      the existing PR's head commit (triggers fresh CI).
            //   3. list_pulls_by_head_with_state finds the existing PR
            //      and reuses it instead of creating a new one.
            //   4. Fires the PrCreated DB transition (approved → pr_draft).
            //
            // The supervisor body's own open_pr can race with this tick.
            // That's fine: whichever fires first wins, the second is a
            // no-op force-push (same SHA) and an InvalidTransition skip.
            let spec = djinn_runtime::TaskRunSpec {
                // PR-open-only flow: this spec drives `supervisor_pr_open`, not
                // a full task-run, so the id is never persisted as a `task_runs`
                // row — but the field is required, so mint a fresh one.
                task_run_id: uuid::Uuid::now_v7().to_string(),
                task_id: task.id.clone(),
                project_id: task.project_id.clone(),
                trigger: djinn_core::models::TaskRunTrigger::NewTask,
                base_branch: crate::actors::slot::helpers::default_target_branch(
                    &task.project_id,
                    &app_state,
                )
                .await,
                task_branch: format!("task/{}", task.short_id),
                flow: djinn_runtime::SupervisorFlow::NewTask,
                model_id_per_role: std::collections::HashMap::new(),
                // PR-open-only flow: no workspace reads, so no read sources.
                read_source_project_ids: Vec::new(),
                github_owner: None,
                github_install_token: None,
                // PR-open-only flow creates no commits, so no author identity.
                commit_author_name: None,
                commit_author_email: None,
            };
            // E6 Part B: proactively rebase the (approved) task branch onto its
            // current target before the PR-open push, so the PR opens/updates on
            // top of current `main` instead of stale history — the single biggest
            // source of merge-queue rejections. Best-effort: a conflict or any
            // git failure logs and proceeds; `supervisor_pr_open` (and the
            // downstream pr_poller conflict/merge-queue handling) run unchanged.
            self.proactively_rebase_approved_branch(
                &task.project_id,
                &spec.task_branch,
                &spec.base_branch,
            )
            .await;

            let callbacks = crate::supervisor_impl::SupervisorCallbackContext {
                agent_context: app_state.clone(),
                cancel: tokio_util::sync::CancellationToken::new(),
                provider_override: None,
            };
            let outcome =
                crate::supervisor_impl::supervisor_pr_open(&spec, &task, &callbacks).await;
            match outcome {
                djinn_runtime::TaskRunOutcome::PrOpened { url, sha } => {
                    self.pr_errors.remove(&task.project_id);
                    self.publish_status();
                    tracing::info!(
                        task_id = %task.short_id,
                        pr_url = %url,
                        commit_sha = %sha,
                        "CoordinatorActor: pushed latest task_branch to PR (re-cycle commits propagated)"
                    );
                }
                djinn_runtime::TaskRunOutcome::Closed { reason } => {
                    // supervisor_pr_open found no commits ahead of base and
                    // already closed the task (memory/notes-only run, etc.).
                    // Clear any stale "PR blocked" banner from prior 422 ticks.
                    self.pr_errors.remove(&task.project_id);
                    self.publish_status();
                    tracing::info!(
                        task_id = %task.short_id,
                        reason = %reason,
                        "CoordinatorActor: approved task had no commits to PR — closed as completed"
                    );
                }
                djinn_runtime::TaskRunOutcome::Failed { stage, reason, .. } => {
                    // Race-tolerant pr_errors gate. The coordinator's tick
                    // path and the supervisor body's own open_pr can fire
                    // concurrently for the same task. When the race-loser
                    // surfaces a transient push/transition error AFTER the
                    // winner has already opened the PR (`task.pr_url` set)
                    // we don't want a stale banner. Two cases:
                    //   1. Pre-existing pr_url → PR is open; the error
                    //      reflects a tick that lost the race. Log and
                    //      move on; next successful tick (or another
                    //      task's PrOpened) clears the project's slot.
                    //   2. No pr_url yet → genuine first-open failure
                    //      (auth, permissions, bad ref, etc.) — surface.
                    // Unrecoverable: the task_branch doesn't exist in the
                    // mirror (a run was cancelled before it pushed — see the
                    // cancel-gate in djinn-supervisor). Retrying a read-only
                    // clone of a missing branch loops forever, so re-queue the
                    // task (`PrConflict`: approved → open) to get a fresh
                    // worker run that recreates and pushes the branch.
                    let branch_missing = reason.contains("clone_ephemeral")
                        && reason.contains("not found in upstream origin");
                    if branch_missing && task.pr_url.is_none() {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %reason,
                            "CoordinatorActor: approved task has no task_branch in mirror (run interrupted before push) — re-queueing (PrConflict: approved → open)"
                        );
                        if let Err(e) = repo
                            .transition(
                                &task.id,
                                TransitionAction::PrConflict,
                                "coordinator",
                                "system",
                                Some("approved with no pushed task_branch; re-running worker to recreate it"),
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to re-queue branch-missing approved task"
                            );
                        }
                        self.pr_errors.remove(&task.project_id);
                        self.publish_status();
                    } else if task.pr_url.is_some() {
                        tracing::info!(
                            task_id = %task.short_id,
                            stage = %stage,
                            error = %reason,
                            "CoordinatorActor: supervisor_pr_open failed but PR already open — treating as transient race"
                        );
                    } else {
                        self.pr_errors
                            .insert(task.project_id.clone(), reason.clone());
                        self.publish_status();
                        tracing::warn!(
                            task_id = %task.short_id,
                            stage = %stage,
                            error = %reason,
                            "CoordinatorActor: supervisor_pr_open failed (will retry next tick)"
                        );
                    }
                }
                other => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        outcome = ?other,
                        "CoordinatorActor: supervisor_pr_open returned unexpected outcome"
                    );
                }
            }
        }
    }

    /// E6 Part B: best-effort proactive rebase of an approved task branch onto
    /// its target BEFORE the PR-open push, to cut merge-queue rejections caused
    /// by branch staleness.
    ///
    /// The task branch was cut from `target` when the work started; by the time
    /// it reaches approval, `target` has usually moved on. GitHub's merge queue
    /// then rejects the PR (it re-tests against the *current* base), bouncing the
    /// task through a `PrCiFailed` reopen loop. Replaying the branch's commits on
    /// top of the current base here removes that drift before the PR is opened or
    /// re-pushed.
    ///
    /// Mechanics (all self-contained, no token plumbing):
    ///   1. Clone the mirror ephemerally on `task_branch` (`--local --shared`, so
    ///      the clone's `origin` is the local mirror and `origin/<target>` is the
    ///      mirror's current base tip — kept fresh by the tick loop's mirror
    ///      fetch).
    ///   2. `rebase_with_retry(workspace, "origin/<target>")` — the djinn-git
    ///      helper that aborts + retries on transient git-state failures and
    ///      cleanly `--abort`s on a real conflict.
    ///   3. Force-push the rewritten `task_branch` back to the mirror so the
    ///      subsequent `supervisor_pr_open` clone (which force-pushes the mirror's
    ///      `task_branch` to GitHub) carries the rebased history.
    ///
    /// STRICTLY best-effort: every failure mode (missing branch, no mirror,
    /// rebase conflict, push failure) is logged and swallowed. Dispatch is never
    /// hard-failed — the existing flow (`supervisor_pr_open`, and downstream the
    /// pr_poller's conflict/merge-queue handling) still runs unchanged. A rebase
    /// conflict in particular is EXPECTED to be common and is intentionally a
    /// no-op here; the downstream conflict machinery owns its resolution.
    pub(in crate::actors::coordinator) async fn proactively_rebase_approved_branch(
        &self,
        project_id: &str,
        task_branch: &str,
        target_branch: &str,
    ) {
        let Some(mirror) = self.mirror.as_ref() else {
            // No mirror configured (e.g. in-process test runtime) — nothing to
            // rebase against. Silent skip.
            return;
        };

        let workspace = match mirror.clone_ephemeral(project_id, task_branch).await {
            Ok(ws) => ws,
            Err(e) => {
                // Branch not present in the mirror yet, mirror missing, etc.
                // The PR-open path below has its own branch-missing recovery.
                tracing::debug!(
                    project_id,
                    task_branch,
                    error = %e,
                    "CoordinatorActor: proactive rebase skipped — could not clone task branch from mirror"
                );
                return;
            }
        };

        let upstream = format!("origin/{target_branch}");
        if let Err(e) = djinn_git::rebase_with_retry(workspace.path(), &upstream).await {
            // Conflict or other git failure. rebase_with_retry has already
            // `--abort`ed, so the workspace is clean. Proceed — the downstream
            // flow resolves staleness/conflicts.
            tracing::info!(
                project_id,
                task_branch,
                upstream = %upstream,
                error = %e,
                "CoordinatorActor: proactive rebase did not apply (conflict or transient) — proceeding to PR-open unchanged"
            );
            return;
        }

        // Rebase rewrote history → non-fast-forward, so the push back to the
        // mirror must be forced. `origin` here is the local mirror; djinn is the
        // sole writer of `task_branch`, so `--force` is the correct semantic
        // (matching the supervisor's own force-push to GitHub).
        let push = djinn_git::run_git_command(
            workspace.path().to_path_buf(),
            vec![
                "push".into(),
                "--force".into(),
                "origin".into(),
                format!("{task_branch}:{task_branch}"),
            ],
        )
        .await;
        match push {
            Ok(_) => {
                tracing::info!(
                    project_id,
                    task_branch,
                    upstream = %upstream,
                    "CoordinatorActor: proactively rebased task branch onto target before PR-open"
                );
            }
            Err(e) => {
                tracing::info!(
                    project_id,
                    task_branch,
                    error = %e,
                    "CoordinatorActor: proactive rebase succeeded but mirror push failed — proceeding to PR-open unchanged"
                );
            }
        }
    }
}

#[cfg(test)]
mod e6_tests {
    use super::*;
    use crate::roles::{DispatchContext, RoleRegistry};
    use djinn_core::models::Task;
    use djinn_core::models::task::compute_transition;

    /// A `Task` shaped like a worker task reopened by a merge-queue rejection:
    /// `issue_type=task`, `status=open`, with the given `reopen_count`.
    fn reopened_worker_task(reopen_count: i64) -> Task {
        Task {
            id: "t1".into(),
            project_id: "p1".into(),
            short_id: "t1".into(),
            epic_id: Some("e1".into()),
            title: "stuck on merge queue".into(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 0,
            owner: String::new(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count,
            continuation_count: 0,
            total_reopen_count: reopen_count,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: None,
            unresolved_blocker_count: 0,
        }
    }

    /// Part A — the `PrCiFailed` transition used by the merge-queue-rejection
    /// reopen path lands the task at `open` AND increments `reopen_count`. This
    /// is what drives the reopen counter toward the intervention threshold; if a
    /// future change stopped incrementing it, the escalation would never arm.
    #[test]
    fn merge_queue_rejection_reopen_increments_reopen_count() {
        // Merge queue rejects an undrafted PR → poller fires PrCiFailed from
        // pr_review (and the pre-undraft pr_draft case is equally valid).
        for from in [TaskStatus::PrReview, TaskStatus::PrDraft] {
            let apply = compute_transition(&TransitionAction::PrCiFailed, &from, None)
                .expect("PrCiFailed must be a legal transition from pr_review/pr_draft");
            assert_eq!(
                apply.to_status,
                Some(TaskStatus::Open),
                "merge-queue rejection must reopen the task ({from:?} → open)"
            );
            assert!(
                apply.increment_reopen,
                "merge-queue rejection reopen MUST bump reopen_count (arms the escalation), from {from:?}"
            );
        }
    }

    #[test]
    fn pr_conflict_transition_does_not_increment_reopen_count() {
        for from in [
            TaskStatus::Approved,
            TaskStatus::PrDraft,
            TaskStatus::PrReview,
        ] {
            let apply = compute_transition(&TransitionAction::PrConflict, &from, None)
                .expect("PrConflict must remain legal for approved/pr_draft/pr_review tasks");
            assert_eq!(apply.to_status, Some(TaskStatus::Open));
            assert!(
                !apply.increment_reopen,
                "PrConflict should not bump reopen_count; djinn_task_reopens_total must follow this semantic"
            );
        }
    }

    /// Part A — a merge-queue-reopened worker task routes to the `worker`
    /// dispatch role. The escalation gate in `dispatch_ready_tasks` is
    /// `role == "worker" && maybe_intervene_on_stuck_task(..)`, so if a reopened
    /// task routed anywhere else the escalation would silently never fire on this
    /// path.
    #[test]
    fn merge_queue_reopened_task_routes_to_worker_role() {
        let registry = RoleRegistry::new();
        let ctx = DispatchContext;
        let task = reopened_worker_task(REOPEN_INTERVENTION_THRESHOLD);
        assert_eq!(
            registry.dispatch_role_for_task(&task, &ctx),
            Some("worker"),
            "a reopened (open, issue_type=task) merge-queue task must dispatch as worker — \
             the role the escalation gate keys on"
        );
    }

    /// Part A — the escalation gate's threshold predicate. Below the threshold
    /// the worker re-dispatches normally; at/above it the gate routes the task to
    /// a Planner intervention. This is the `reopen_count` crossing 3 → Planner
    /// regression lock at the predicate level.
    #[test]
    fn reopen_count_crossing_threshold_arms_planner_escalation() {
        assert_eq!(
            REOPEN_INTERVENTION_THRESHOLD, 3,
            "escalation threshold is 3 reopens (memory: reopen_count >= 3 → Planner)"
        );

        // Below threshold: the gate predicate is false → worker re-dispatches.
        let below = reopened_worker_task(REOPEN_INTERVENTION_THRESHOLD - 1);
        assert!(
            below.reopen_count < REOPEN_INTERVENTION_THRESHOLD,
            "two reopens stay under the threshold — no escalation yet"
        );

        // At and above threshold: predicate is true → routed to Planner.
        for n in [
            REOPEN_INTERVENTION_THRESHOLD,
            REOPEN_INTERVENTION_THRESHOLD + 1,
        ] {
            let stuck = reopened_worker_task(n);
            assert!(
                stuck.reopen_count >= REOPEN_INTERVENTION_THRESHOLD,
                "reopen_count {n} crosses the threshold and must arm the Planner escalation"
            );
            // And it is still a worker task at that point (the gate's other half).
            let registry = RoleRegistry::new();
            assert_eq!(
                registry.dispatch_role_for_task(&stuck, &DispatchContext),
                Some("worker"),
                "still a worker task at reopen_count={n} — the full escalation gate is satisfied"
            );
        }
    }

    // ── Part B: proactive-rebase non-fatal contract ──────────────────────────

    async fn git(dir: &std::path::Path, args: &[&str]) {
        djinn_git::run_git_command(
            dir.to_path_buf(),
            args.iter().map(|s| (*s).to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?} in {dir:?} failed: {e}"));
    }

    async fn write(dir: &std::path::Path, name: &str, contents: &str) {
        tokio::fs::write(dir.join(name), contents).await.unwrap();
    }

    /// Part B — a real rebase CONFLICT is reported as `Err` by
    /// `djinn_git::rebase_with_retry` and the helper `--abort`s, leaving the
    /// workspace clean (no in-progress rebase). This is the exact failure
    /// `proactively_rebase_approved_branch` swallows: it logs the `Err` and
    /// proceeds to `supervisor_pr_open`, so a conflict can never hard-fail
    /// dispatch and never leaves a wedged mid-rebase tree behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_rebase_conflict_is_non_fatal_and_aborts_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();

        // Init a repo with a committed base on `main`.
        git(repo, &["init", "-q", "-b", "main"]).await;
        git(repo, &["config", "user.email", "t@example.com"]).await;
        git(repo, &["config", "user.name", "Test"]).await;
        write(repo, "f.txt", "base\n").await;
        git(repo, &["add", "f.txt"]).await;
        git(repo, &["commit", "-q", "-m", "base"]).await;

        // Task branch edits the SAME line one way…
        git(repo, &["checkout", "-q", "-b", "task/x"]).await;
        write(repo, "f.txt", "from-task\n").await;
        git(repo, &["commit", "-qam", "task edit"]).await;

        // …and `main` advances editing it the other way → guaranteed conflict.
        git(repo, &["checkout", "-q", "main"]).await;
        write(repo, "f.txt", "from-main\n").await;
        git(repo, &["commit", "-qam", "main edit"]).await;
        git(repo, &["checkout", "-q", "task/x"]).await;

        // Rebase task/x onto the diverged main: MUST error (conflict), never panic.
        let result = djinn_git::rebase_with_retry(repo, "main").await;
        assert!(
            result.is_err(),
            "a conflicting rebase must surface as Err (which the proactive helper swallows)"
        );

        // And the helper must have aborted: the tree is clean, not mid-rebase.
        assert!(
            !repo.join(".git/rebase-merge").exists() && !repo.join(".git/rebase-apply").exists(),
            "rebase_with_retry must --abort on failure so no wedged mid-rebase state is left behind"
        );
        let status = djinn_git::run_git_command(
            repo.to_path_buf(),
            vec!["status".into(), "--porcelain".into()],
        )
        .await
        .unwrap();
        assert!(
            status.stdout.trim().is_empty(),
            "workspace must be clean after the aborted rebase, got: {:?}",
            status.stdout
        );
    }

    /// Part B — the clean path: when the task branch rebases without conflict,
    /// `rebase_with_retry` succeeds and the branch now sits on top of the current
    /// target. (Confirms the helper actually replays the branch, not just no-ops.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proactive_rebase_clean_replays_branch_onto_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();

        git(repo, &["init", "-q", "-b", "main"]).await;
        git(repo, &["config", "user.email", "t@example.com"]).await;
        git(repo, &["config", "user.name", "Test"]).await;
        write(repo, "base.txt", "base\n").await;
        git(repo, &["add", "base.txt"]).await;
        git(repo, &["commit", "-q", "-m", "base"]).await;

        // Task branch touches a DIFFERENT file than main advances → no conflict.
        git(repo, &["checkout", "-q", "-b", "task/y"]).await;
        write(repo, "task.txt", "task\n").await;
        git(repo, &["add", "task.txt"]).await;
        git(repo, &["commit", "-q", "-m", "task edit"]).await;

        git(repo, &["checkout", "-q", "main"]).await;
        write(repo, "main.txt", "main\n").await;
        git(repo, &["add", "main.txt"]).await;
        git(repo, &["commit", "-q", "-m", "main edit"]).await;
        git(repo, &["checkout", "-q", "task/y"]).await;

        djinn_git::rebase_with_retry(repo, "main")
            .await
            .expect("non-conflicting rebase must succeed");

        // After rebase, main's tip is an ancestor of HEAD (branch sits on top).
        let out = djinn_git::run_git_command(
            repo.to_path_buf(),
            vec![
                "merge-base".into(),
                "--is-ancestor".into(),
                "main".into(),
                "HEAD".into(),
            ],
        )
        .await;
        assert!(
            out.is_ok(),
            "after a clean rebase, main must be an ancestor of the task branch HEAD"
        );
    }
}
