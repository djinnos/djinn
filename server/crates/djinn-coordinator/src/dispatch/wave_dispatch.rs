use super::super::*;
use super::attempt_lifecycle::{
    TerminalAdvancementParams, advance_latest_to_terminal, ensure_rework_marker,
};
use crate::pr_poller::pr_cleanup::CloseKind;
use djinn_core::models::TransitionAction;
use djinn_core::models::task::IssueType;
use djinn_core::models::task_attempt::TaskAttemptOutcome;

/// Classify a `supervisor_pr_open` push failure as "an oversized blob is
/// committed in the branch history" (GitHub's 100 MB hard limit, enforced by
/// the remote pre-receive hook). Such a push is rejected identically on every
/// retry — the offending blob never leaves the branch history on its own — so
/// the coordinator must escalate (Planner rewrites the history) rather than
/// loop a transient-retry banner. Matches the verbatim remote error text
/// GitHub emits (`GH001` / `exceeds GitHub's file size limit` /
/// `Large files detected` / `pre-receive hook declined`).
pub(super) fn is_oversized_blob_push_rejection(reason: &str) -> bool {
    reason.contains("pre-receive hook declined")
        || reason.contains("GH001")
        || reason.contains("exceeds GitHub's file size limit")
        || reason.contains("Large files detected")
}

/// Wave-dispatch attempt outcome discriminator. Carries the structured context
/// each terminalization path records in `summary_json`. Used only by
/// [`CoordinatorActor::terminalize_wave_dispatch_attempt`] and the standalone
/// [`terminalize_wave_dispatch_attempt_on_db`].
#[allow(dead_code)]
pub(super) enum WaveDispatchAttemptOutcome<'a> {
    /// An already-open PR was adopted/reopened instead of a fresh open.
    AdoptedPr { pr_url: &'a str, head_sha: &'a str },
    /// The current worker attempt stops and another process takes over.
    Handoff {
        reason: &'a str,
        replacement: &'a str,
    },
    /// A rework reopen owned by wave dispatch: the task is re-queued to `open`
    /// via a `PrConflict` transition (e.g. an approved task whose `task_branch`
    /// never reached the mirror), so a fresh worker must redo the work. Unlike
    /// [`Handoff`], this MUST leave a durable `reopened` latest attempt to honor
    /// the rework-marker invariant the respawn guard's `latest_attempt_is_reopened`
    /// check relies on — terminalizing any live attempt to `reopened` AND
    /// inserting a marker when nothing was in-flight.
    Reopened { reason: &'a str },
    /// Dispatch-owned ForceClose (oversized blob in branch history, etc.).
    ForceClosed {
        reason: &'a str,
        close_reason: &'a str,
    },
}

impl CoordinatorActor {
    #[tracing::instrument(
        name = "djinn.coordinator.approved_pass",
        skip(self),
        fields(pass_kind = "approved")
    )]
    pub(crate) async fn process_approved_tasks(&mut self) {
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
        let app_state = crate::context::CoordinatorContext {
            db: self.db.clone(),
            event_bus: crate::events::event_bus_for(&self.events_tx),
            git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            background_work_tasks: self.background_work_tracker.clone(),
            role_registry: self.role_registry.clone(),
            health_tracker: self.health.clone(),
            file_time: Arc::new(crate::file_time::FileTime::new()),
            lsp: self.lsp.clone(),
            catalog: self.catalog.clone(),
            active_tasks: crate::context::ActivityTracker::default(),
            task_ops_project_path_override: None,
            working_root: None,
            graph_warmer: None,
            warm_job_guard: None,
            repo_graph_ops: None,
            runtime_ops: None,
            cargo_target_runs_root: Some(djinn_core::paths::cargo_target_runs_root()),
            mirror: self.mirror.clone(),
            rpc_registry: None,
            default_project_id: None,
            reconciliation_sweep: crate::context::ReconciliationSweepConfig::from_env(),
            cache_cleanup: crate::context::CacheCleanupConfig::from_env(),
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
                match repo
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
                    Ok(_) => {
                        self.cleanup_pr_and_branch_on_close(&task, CloseKind::NonMerge)
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "CoordinatorActor: failed to close simple-lifecycle approved task"
                        );
                    }
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
                task_attempt_id: None,
                task_id: task.id.clone(),
                project_id: task.project_id.clone(),
                trigger: djinn_core::models::TaskRunTrigger::NewTask,
                base_branch: {
                    let repo = djinn_db::ProjectRepository::new(
                        app_state.db.clone(),
                        app_state.event_bus.clone(),
                    );
                    match repo.get_config(&task.project_id).await {
                        Ok(Some(cfg)) => cfg.target_branch,
                        _ => "main".to_string(),
                    }
                },
                task_branch: format!("task/{}", task.short_id),
                flow: djinn_runtime::SupervisorFlow::NewTask,
                model_id_per_role: std::collections::HashMap::new(),
                // PR-open-only flow: no workspace reads, so no read sources.
                read_source_project_ids: Vec::new(),
                knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
                github_owner: None,
                github_install_token: None,
                // PR-open-only flow creates no commits, so no author identity.
                commit_author_name: None,
                commit_author_email: None,
                resume_lifecycle_metadata: None,
                // PR-open-only flow: not an evidence spike.
                is_evidence_spike: false,
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
            let pr_url_existed_before = task.pr_url.is_some();
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
                    // Attempt lifecycle: if a PR was already open before this
                    // supervisor_pr_open call (adoption/reopen of an existing
                    // PR rather than a fresh open), terminalize the current
                    // worker attempt as `adopted_pr`. A fresh open leaves the
                    // attempt live — it continues through PR review. Best-effort.
                    if pr_url_existed_before {
                        self.terminalize_wave_dispatch_attempt(
                            &task,
                            WaveDispatchAttemptOutcome::AdoptedPr {
                                pr_url: &url,
                                head_sha: &sha,
                            },
                        )
                        .await;
                    }
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
                    // Attempt lifecycle: the current worker attempt intentionally
                    // stops here — the task is closed without a PR. Terminalize
                    // as `handoff` (another process — the close itself — takes
                    // over). Best-effort.
                    self.terminalize_wave_dispatch_attempt(
                        &task,
                        WaveDispatchAttemptOutcome::Handoff {
                            reason: &reason,
                            replacement: "task_closed_no_commits",
                        },
                    )
                    .await;
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
                    // GitHub rejected the push because a blob in the branch's
                    // history exceeds the 100 MB hard limit — almost always a
                    // cache/store directory swept into a commit (e.g. a pnpm or
                    // cargo store when HOME drifted into the worktree). This is
                    // NON-TRANSIENT: the oversized blob stays in history, so
                    // every retry is declined identically by the pre-receive
                    // hook (the symptom is an endless `pr_errors` banner). Don't
                    // treat it as a transient race; escalate to the Planner/Lead
                    // to rewrite the branch history and re-land, then ForceClose
                    // the source so this approved pass stops re-selecting it.
                    // ForceClose is legal from any non-closed state and moves the
                    // task out of `approved`, so the escalation fires exactly once
                    // (no per-tick duplicate review tasks).
                    let push_rejected_oversized_blob = is_oversized_blob_push_rejection(&reason);
                    if push_rejected_oversized_blob && task.pr_url.is_none() {
                        let escalation_reason = format!(
                            "PR push for approved task was rejected by GitHub: an oversized file \
                             (>100 MB) is committed in branch `{branch}`'s history, so every push \
                             is declined by the pre-receive hook and the task cannot land. Rewrite \
                             the branch history to drop the oversized blob — it is almost always a \
                             cache/store artifact (e.g. `.local/share/pnpm`, a cargo target dir, or \
                             `node_modules`) that must be git-ignored, not committed — then re-cut / \
                             force-push `{branch}` and re-open the PR. Underlying push error: {reason}",
                            branch = spec.task_branch,
                        );
                        let comment_payload = serde_json::json!({
                            "body": format!(
                                "**PR push blocked — oversized blob in branch history**\n\n{escalation_reason}"
                            )
                        })
                        .to_string();
                        let _ = repo
                            .log_activity(
                                Some(&task.id),
                                "coordinator",
                                "system",
                                "comment",
                                &comment_payload,
                            )
                            .await;
                        tracing::warn!(
                            task_id = %task.short_id,
                            branch = %spec.task_branch,
                            error = %reason,
                            "CoordinatorActor: PR push rejected (oversized blob in history) — escalating to Planner to rewrite history"
                        );
                        self.dispatch_planner_escalation(
                            &task.id,
                            &escalation_reason,
                            &task.project_id,
                        )
                        .await;
                        if let Err(e) = repo
                            .transition(
                                &task.id,
                                TransitionAction::ForceClose,
                                "coordinator",
                                "system",
                                Some(
                                    "push rejected: oversized blob in branch history; escalated to Planner to rewrite history",
                                ),
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to force-close push-rejected approved task after escalation"
                            );
                        }
                        self.pr_errors.remove(&task.project_id);
                        self.publish_status();
                        // Attempt lifecycle: dispatch-owned ForceClose. The
                        // push was rejected by GitHub for a non-transient reason
                        // (oversized blob in branch history). Terminalize the
                        // current worker attempt as `force_closed`. Best-effort.
                        self.terminalize_wave_dispatch_attempt(
                            &task,
                            WaveDispatchAttemptOutcome::ForceClosed {
                                reason: &reason,
                                close_reason: "oversized_blob_in_branch_history",
                            },
                        )
                        .await;
                    } else if branch_missing && task.pr_url.is_none() {
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
                        // Attempt lifecycle: this is a rework reopen — the
                        // `PrConflict` transition re-queued the task to `open`
                        // for a fresh worker run that recreates and pushes the
                        // missing branch. Terminalize as `reopened` (NOT
                        // `handoff`) and record a durable rework marker so the
                        // respawn guard's `latest_attempt_is_reopened` gate sees
                        // this reopen — honoring #1719's invariant that every
                        // rework reopen leaves a `reopened` latest attempt.
                        // Best-effort.
                        self.terminalize_wave_dispatch_attempt(
                            &task,
                            WaveDispatchAttemptOutcome::Reopened { reason: &reason },
                        )
                        .await;
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
                djinn_runtime::TaskRunOutcome::Escalated { reason } => {
                    // A pre-PR gate (the CI reproduction preflight) blocked the
                    // PR-open. The gate has already routed the task (held in
                    // remediation or returned strike-free to a worker round), so
                    // there is no push failure to surface — clear any stale
                    // PR-blocked banner and move on.
                    self.pr_errors.remove(&task.project_id);
                    self.publish_status();
                    tracing::info!(
                        task_id = %task.short_id,
                        reason = %reason,
                        "CoordinatorActor: approved task PR-open blocked by a pre-PR gate (re-routed, no PR opened)"
                    );
                    // Attempt lifecycle: a pre-PR gate intentionally stopped
                    // the current worker attempt and re-routed the task to
                    // another process (remediation or a fresh worker round).
                    // Terminalize as `handoff`. Best-effort.
                    self.terminalize_wave_dispatch_attempt(
                        &task,
                        WaveDispatchAttemptOutcome::Handoff {
                            reason: &reason,
                            replacement: "pre_pr_gate_rerouted",
                        },
                    )
                    .await;
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

    /// Best-effort terminalize the latest pending/submitted worker attempt for a
    /// wave-dispatch (`process_approved_tasks`) outcome. Maps each
    /// [`WaveDispatchAttemptOutcome`] to the corresponding
    /// [`TaskAttemptOutcome`] and fills available PR URL / head SHA / submit ref
    /// context. Uses the shared `attempt_lifecycle::advance_latest_to_terminal`
    /// helper — never creates a second lookup convention.
    ///
    /// Idempotent: if no live attempt exists (or it is already terminal), the
    /// underlying helper is a silent no-op, so duplicate wave-dispatch ticks do
    /// not create rows or move a terminal attempt backward.
    async fn terminalize_wave_dispatch_attempt(
        &self,
        task: &djinn_core::models::Task,
        outcome: WaveDispatchAttemptOutcome<'_>,
    ) {
        terminalize_wave_dispatch_attempt_on_db(&self.db, task, outcome).await;
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
    pub(crate) async fn proactively_rebase_approved_branch(
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

/// Build the [`TerminalAdvancementParams`] fields for a wave-dispatch
/// terminalization.  Pure parameter construction — no I/O, no `self`.
/// Factored out of [`CoordinatorActor::terminalize_wave_dispatch_attempt`]
/// so that the mapping logic is directly unit-testable without constructing
/// a full `CoordinatorActor`.
pub(super) fn build_wave_dispatch_terminal_params<'a>(
    task: &'a djinn_core::models::Task,
    outcome: WaveDispatchAttemptOutcome<'a>,
) -> WaveDispatchTerminalParams<'a> {
    let submit_ref = format!("refs/heads/task/{}", task.short_id);
    let task_branch = format!("task/{}", task.short_id);
    match outcome {
        WaveDispatchAttemptOutcome::AdoptedPr { pr_url, head_sha } => {
            let summary =
                format!("wave_dispatch: adopted existing open PR {pr_url} for approved task");
            let summary_json = serde_json::json!({
                "source": "wave_dispatch",
                "path": "adopted_pr",
                "pr_url": pr_url,
                "github_head_sha": head_sha,
                "submit_ref": submit_ref,
                "task_branch": task_branch,
            })
            .to_string();
            WaveDispatchTerminalParams {
                outcome: TaskAttemptOutcome::AdoptedPr,
                pr_url: Some(pr_url),
                github_head_sha: Some(head_sha),
                summary,
                summary_json,
                submit_ref,
            }
        }
        WaveDispatchAttemptOutcome::Handoff {
            reason,
            replacement,
        } => {
            let summary = format!(
                "wave_dispatch: current worker attempt handed off ({replacement}): {reason}"
            );
            let summary_json = serde_json::json!({
                "source": "wave_dispatch",
                "path": "handoff",
                "reason": reason,
                "replacement": replacement,
                "submit_ref": submit_ref,
                "pr_url": task.pr_url,
                "task_branch": task_branch,
            })
            .to_string();
            WaveDispatchTerminalParams {
                outcome: TaskAttemptOutcome::Handoff,
                pr_url: task.pr_url.as_deref(),
                github_head_sha: task.ci_head_sha.as_deref(),
                summary,
                summary_json,
                submit_ref,
            }
        }
        WaveDispatchAttemptOutcome::Reopened { reason } => {
            let summary =
                format!("wave_dispatch: current worker attempt reopened for rework: {reason}");
            let summary_json = serde_json::json!({
                "source": "wave_dispatch",
                "path": "reopened",
                "reason": reason,
                "submit_ref": submit_ref,
                "pr_url": task.pr_url,
                "task_branch": task_branch,
            })
            .to_string();
            WaveDispatchTerminalParams {
                outcome: TaskAttemptOutcome::Reopened,
                pr_url: task.pr_url.as_deref(),
                github_head_sha: task.ci_head_sha.as_deref(),
                summary,
                summary_json,
                submit_ref,
            }
        }
        WaveDispatchAttemptOutcome::ForceClosed {
            reason,
            close_reason,
        } => {
            let summary = format!("wave_dispatch: attempt force-closed ({close_reason}): {reason}");
            let summary_json = serde_json::json!({
                "source": "wave_dispatch",
                "path": "force_closed",
                "close_reason": close_reason,
                "reason": reason,
                "submit_ref": submit_ref,
                "task_branch": task_branch,
            })
            .to_string();
            WaveDispatchTerminalParams {
                outcome: TaskAttemptOutcome::ForceClosed,
                pr_url: task.pr_url.as_deref(),
                github_head_sha: task.ci_head_sha.as_deref(),
                summary,
                summary_json,
                submit_ref,
            }
        }
    }
}

/// Owned terminalization parameters returned by
/// [`build_wave_dispatch_terminal_params`].  The caller borrows these fields
/// when building [`TerminalAdvancementParams`].
pub(super) struct WaveDispatchTerminalParams<'a> {
    pub(super) outcome: TaskAttemptOutcome,
    pub(super) pr_url: Option<&'a str>,
    pub(super) github_head_sha: Option<&'a str>,
    pub(super) summary: String,
    pub(super) summary_json: String,
    pub(super) submit_ref: String,
}

/// Standalone best-effort wave-dispatch terminalization that takes `&Database`
/// directly (no `CoordinatorActor` construction required).  Delegates to
/// [`build_wave_dispatch_terminal_params`] for the outcome → params mapping
/// and then [`advance_latest_to_terminal`] for the DB write.
///
/// This is the function integration tests exercise to prove the full wiring:
/// `WaveDispatchAttemptOutcome` → `build_wave_dispatch_terminal_params` →
/// `advance_latest_to_terminal` → persisted `task_attempts` row.
pub(super) async fn terminalize_wave_dispatch_attempt_on_db(
    db: &djinn_db::Database,
    task: &djinn_core::models::Task,
    outcome: WaveDispatchAttemptOutcome<'_>,
) {
    let params = build_wave_dispatch_terminal_params(task, outcome);
    let is_rework_reopen = params.outcome == TaskAttemptOutcome::Reopened;
    advance_latest_to_terminal(
        db,
        TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: params.outcome,
            pr_url: params.pr_url,
            submit_ref: Some(&params.submit_ref),
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: params.github_head_sha,
            summary: Some(&params.summary),
            summary_json: Some(&params.summary_json),
            log_tail: None,
        },
    )
    .await;
    // Rework-marker invariant (#1719): a `reopened` wave-dispatch outcome MUST
    // leave a durable `reopened` latest attempt even when no live attempt
    // existed to terminalize above. `ensure_rework_marker` is a no-op when a
    // live attempt was just advanced or the latest attempt is already
    // `reopened`, so this is idempotent.
    if is_rework_reopen {
        ensure_rework_marker(db, &task.id, "worker", Some(&params.summary)).await;
    }
}
