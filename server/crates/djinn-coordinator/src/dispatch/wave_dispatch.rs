use super::super::*;
use super::attempt_lifecycle::{TerminalAdvancementParams, advance_latest_to_terminal};
use crate::pr_poller::pr_cleanup::CloseKind;
#[cfg(test)]
use djinn_core::models::TaskStatus;
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
fn is_oversized_blob_push_rejection(reason: &str) -> bool {
    reason.contains("pre-receive hook declined")
        || reason.contains("GH001")
        || reason.contains("exceeds GitHub's file size limit")
        || reason.contains("Large files detected")
}

/// Wave-dispatch attempt outcome discriminator. Carries the structured context
/// each terminalization path records in `summary_json`. Used only by
/// [`CoordinatorActor::terminalize_wave_dispatch_attempt`].
#[allow(dead_code)]
enum WaveDispatchAttemptOutcome<'a> {
    /// An already-open PR was adopted/reopened instead of a fresh open.
    AdoptedPr { pr_url: &'a str, head_sha: &'a str },
    /// The current worker attempt stops and another process takes over.
    Handoff {
        reason: &'a str,
        replacement: &'a str,
    },
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
            repo_graph_ops: None,
            runtime_ops: None,
            cargo_target_runs_root: Some(djinn_core::paths::cargo_target_runs_root()),
            mirror: self.mirror.clone(),
            rpc_registry: None,
            default_project_id: None,
            reconciliation_sweep: crate::context::ReconciliationSweepConfig::from_env(),
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
                        // Attempt lifecycle: the current worker attempt stops
                        // here and another worker process takes over (the task
                        // is re-queued to `open` for a fresh worker run that
                        // recreates and pushes the missing branch). Terminalize
                        // as `handoff`. Best-effort.
                        self.terminalize_wave_dispatch_attempt(
                            &task,
                            WaveDispatchAttemptOutcome::Handoff {
                                reason: &reason,
                                replacement: "requeued_missing_branch",
                            },
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
                    // A pre-PR gate (CI reproduction preflight, or the uv3p
                    // pre-approval verification gate) blocked the PR-open. The
                    // gate has already routed the task (held in remediation or
                    // returned strike-free to a worker round), so there is no
                    // push failure to surface — clear any stale PR-blocked
                    // banner and move on.
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
        let params = build_wave_dispatch_terminal_params(task, outcome);
        advance_latest_to_terminal(
            &self.db,
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
fn build_wave_dispatch_terminal_params<'a>(
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
struct WaveDispatchTerminalParams<'a> {
    outcome: TaskAttemptOutcome,
    pr_url: Option<&'a str>,
    github_head_sha: Option<&'a str>,
    summary: String,
    summary_json: String,
    submit_ref: String,
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
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
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

    // ── Oversized-blob push rejection → Planner escalation ────────────────────

    /// The classifier must fire on the verbatim error GitHub's pre-receive hook
    /// emits when a >100 MB blob is in the pushed history, as it reaches the
    /// coordinator (wrapped by `push_task_branch_to_github` into the
    /// `"push task_branch to GitHub failed: {e}"` reason, stderr included). This
    /// is the exact failure that previously looped a `pr_errors` banner forever.
    #[test]
    fn oversized_blob_push_rejection_is_classified() {
        let reason = "push task_branch to GitHub failed: git command failed (exit 1) in \
            /tmp/.tmp86MbxD: git push --force ... stdout: stderr: \
            remote: error: File .local/share/pnpm/store/v11/files/ed/63a1c1... is 112.45 MB; \
            this exceeds GitHub's file size limit of 100.00 MB \
            remote: error: GH001: Large files detected. \
            ! [remote rejected] task/aqmk -> task/aqmk (pre-receive hook declined)";
        assert!(
            is_oversized_blob_push_rejection(reason),
            "the real GH001 oversized-blob rejection must be classified for escalation"
        );
    }

    /// Negative: ordinary transient push/transition failures must NOT be
    /// classified as oversized-blob rejections — those still take the existing
    /// retry-next-tick path, not a (history-rewriting) Planner escalation.
    #[test]
    fn transient_push_failures_are_not_oversized_blob_rejections() {
        for reason in [
            "push task_branch to GitHub failed: git command failed (exit 1): \
             fatal: unable to access 'https://github.com/...': Could not resolve host",
            "push task_branch to GitHub failed: ! [rejected] (non-fast-forward)",
            "pr_open transition failed: InvalidTransition",
        ] {
            assert!(
                !is_oversized_blob_push_rejection(reason),
                "transient failure must not be misclassified as an oversized-blob rejection: {reason}"
            );
        }
    }

    /// Idempotency lock: after escalating, the coordinator `ForceClose`s the
    /// source task so it leaves the `approved` status that `process_approved_tasks`
    /// re-queries every tick. If `ForceClose` ever stopped being legal from
    /// `approved` (→ Closed), the task would stay approved and the escalation
    /// would fire — spawning a duplicate Planner review task — every tick.
    #[test]
    fn force_close_moves_approved_task_out_of_queried_state() {
        let apply = compute_transition(&TransitionAction::ForceClose, &TaskStatus::Approved, None)
            .expect("ForceClose must be legal from approved");
        assert_eq!(
            apply.to_status,
            Some(TaskStatus::Closed),
            "ForceClose must move the push-rejected approved task out of `approved` so the \
             per-tick escalation fires exactly once"
        );
    }

    // ── Attempt lifecycle: wave-dispatch terminalization ─────────────────────
    //
    // These tests exercise the `build_wave_dispatch_terminal_params` helper
    // (the extracted pure-logic core of `terminalize_wave_dispatch_attempt`)
    // directly.  This is the smallest testable unit that owns the outcome →
    // terminal params mapping — including `task_branch` in summary_json, the
    // `pr_url` / `ci_head_sha` passthrough for handoff/ForceClosed, and all
    // three outcome classifications.  A separate idempotency test exercises
    // the full `advance_latest_to_terminal` persistence path with the helper's
    // output to prove duplicate dispatch ticks are safe.

    use super::super::attempt_lifecycle::{
        TerminalAdvancementParams, advance_latest_to_terminal, make_dispatch_key,
        record_dispatch_start,
    };
    use djinn_core::events::EventBus;
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

    fn lifecycle_test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn lifecycle_create_task(db: &Database) -> djinn_core::models::Task {
        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let task_repo = TaskRepository::new(db.clone(), event_bus);
        task_repo
            .create(&epic.id, "Test task", "", "", "task", 0, "", None)
            .await
            .unwrap()
    }

    /// Set up a task with a pending worker attempt (mimicking a dispatched
    /// worker that reached the approved-PR-open wave-dispatch tick).
    async fn setup_pending_attempt(db: &Database) -> (djinn_core::models::Task, String) {
        let task = lifecycle_create_task(db).await;
        let dk = make_dispatch_key(&task.id, "worker");
        let attempt_id = record_dispatch_start(db, &task.id, "worker", None, &dk)
            .await
            .unwrap();
        (task, attempt_id)
    }

    /// Construct a `Task` with the minimum set of fields populated for
    /// `build_wave_dispatch_terminal_params` tests.  `short_id` is used as
    /// both `id` and `short_id`; other fields are zeroed/empty.
    fn test_task(short_id: &str) -> djinn_core::models::Task {
        djinn_core::models::Task {
            id: short_id.into(),
            project_id: "p1".into(),
            short_id: short_id.into(),
            epic_id: Some("e1".into()),
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".into(),
            status: "approved".into(),
            priority: 0,
            owner: String::new(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
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
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            unresolved_blocker_count: 0,
        }
    }

    /// The `build_wave_dispatch_terminal_params` helper maps the adopted-PR
    /// outcome to `TaskAttemptOutcome::AdoptedPr` and records the PR URL,
    /// head SHA, `task_branch`, and `submit_ref` in summary_json.
    #[test]
    fn build_terminal_params_adopted_pr_records_pr_context_and_branch() {
        let task = test_task("t-adopt");
        let pr_url = "https://github.example/owner/repo/pull/42";
        let head_sha = "abc123deadbeef";
        let params = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::AdoptedPr { pr_url, head_sha },
        );

        assert_eq!(params.outcome, TaskAttemptOutcome::AdoptedPr);
        assert_eq!(params.pr_url, Some(pr_url));
        assert_eq!(params.github_head_sha, Some(head_sha));
        assert_eq!(params.submit_ref, "refs/heads/task/t-adopt");

        let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
        assert_eq!(ctx["source"], "wave_dispatch");
        assert_eq!(ctx["path"], "adopted_pr");
        assert_eq!(ctx["pr_url"], pr_url);
        assert_eq!(ctx["github_head_sha"], head_sha);
        assert_eq!(ctx["task_branch"], "task/t-adopt");
        assert_eq!(ctx["submit_ref"], "refs/heads/task/t-adopt");
        assert!(
            params.summary.contains(pr_url),
            "summary must mention the adopted PR URL"
        );
    }

    /// The handoff outcome maps to `TaskAttemptOutcome::Handoff` and passes
    /// through `task.pr_url` and `task.ci_head_sha` into the params and the
    /// structured summary_json.
    #[test]
    fn build_terminal_params_handoff_passes_through_task_pr_and_ci_context() {
        let mut task = test_task("t-handoff");
        task.pr_url = Some("https://github.example/owner/repo/pull/99".into());
        task.ci_head_sha = Some("deadbeef".into());
        let params = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::Handoff {
                reason: "branch missing from mirror",
                replacement: "requeued_missing_branch",
            },
        );

        assert_eq!(params.outcome, TaskAttemptOutcome::Handoff);
        // The helper must pass through the task's existing PR URL and CI head SHA.
        assert_eq!(
            params.pr_url,
            Some("https://github.example/owner/repo/pull/99")
        );
        assert_eq!(params.github_head_sha, Some("deadbeef"));
        assert_eq!(params.submit_ref, "refs/heads/task/t-handoff");

        let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
        assert_eq!(ctx["source"], "wave_dispatch");
        assert_eq!(ctx["path"], "handoff");
        assert_eq!(ctx["reason"], "branch missing from mirror");
        assert_eq!(ctx["replacement"], "requeued_missing_branch");
        assert_eq!(ctx["pr_url"], "https://github.example/owner/repo/pull/99");
        assert_eq!(ctx["task_branch"], "task/t-handoff");
        assert_eq!(ctx["submit_ref"], "refs/heads/task/t-handoff");
        assert!(
            params.summary.contains("requeued_missing_branch"),
            "summary must include the replacement identifier"
        );
    }

    /// When the task has no PR URL (e.g. closed-no-commits handoff), the
    /// helper produces `None` for `pr_url` and still includes `null` in
    /// summary_json.
    #[test]
    fn build_terminal_params_handoff_with_no_pr_url() {
        let task = test_task("t-nopr");
        let params = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::Handoff {
                reason: "no commits ahead of base",
                replacement: "task_closed_no_commits",
            },
        );

        assert_eq!(params.outcome, TaskAttemptOutcome::Handoff);
        assert!(params.pr_url.is_none());
        assert!(params.github_head_sha.is_none());

        let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
        assert!(ctx["pr_url"].is_null());
        assert_eq!(ctx["task_branch"], "task/t-nopr");
    }

    /// The ForceClosed outcome maps to `TaskAttemptOutcome::ForceClosed` and
    /// includes the close_reason and reason in summary_json plus task_branch.
    #[test]
    fn build_terminal_params_force_closed_records_close_reason_and_branch() {
        let task = test_task("t-fc");
        let params = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::ForceClosed {
                reason: "GH001: Large files detected. pre-receive hook declined",
                close_reason: "oversized_blob_in_branch_history",
            },
        );

        assert_eq!(params.outcome, TaskAttemptOutcome::ForceClosed);
        assert_eq!(params.submit_ref, "refs/heads/task/t-fc");

        let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
        assert_eq!(ctx["source"], "wave_dispatch");
        assert_eq!(ctx["path"], "force_closed");
        assert_eq!(ctx["close_reason"], "oversized_blob_in_branch_history");
        assert!(
            ctx["reason"]
                .as_str()
                .unwrap()
                .contains("pre-receive hook declined")
        );
        assert_eq!(ctx["task_branch"], "task/t-fc");
    }

    /// Duplicate wave-dispatch terminalization is idempotent: a second call
    /// (mimicking a re-tick that re-processes the same approved task) must NOT
    /// create a second attempt row, move the terminal outcome backward, or
    /// overwrite the original recorded context.  This test exercises the full
    /// `advance_latest_to_terminal` persistence path with params produced by
    /// the helper.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wave_dispatch_duplicate_terminalization_is_idempotent() {
        let db = lifecycle_test_db();
        let (task, attempt_id) = setup_pending_attempt(&db).await;

        // First call: terminalize as adopted-PR using the helper's output.
        let params_first = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::AdoptedPr {
                pr_url: "https://github.example/owner/repo/pull/7",
                head_sha: "sha-orig",
            },
        );
        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: params_first.outcome,
                pr_url: params_first.pr_url,
                submit_ref: Some(&params_first.submit_ref),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: params_first.github_head_sha,
                summary: Some(&params_first.summary),
                summary_json: Some(&params_first.summary_json),
                log_tail: None,
            },
        )
        .await;

        // Second call: a different wave-dispatch path fires (e.g. a late
        // ForceClose from a duplicate supervisor_pr_open race).  The attempt
        // must remain unchanged.
        let params_second = build_wave_dispatch_terminal_params(
            &task,
            WaveDispatchAttemptOutcome::ForceClosed {
                reason: "late race",
                close_reason: "late",
            },
        );
        advance_latest_to_terminal(
            &db,
            TerminalAdvancementParams {
                task_id: &task.id,
                role: "worker",
                outcome: params_second.outcome,
                pr_url: params_second.pr_url,
                submit_ref: Some(&params_second.submit_ref),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: params_second.github_head_sha,
                summary: Some(&params_second.summary),
                summary_json: Some(&params_second.summary_json),
                log_tail: None,
            },
        )
        .await;

        let repo = TaskAttemptRepository::new(db);
        let all = repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "duplicate wave-dispatch terminalization must not create a second attempt row"
        );
        let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
        // The original terminal outcome must be preserved — no backward movement.
        assert_eq!(attempt.outcome, "adopted_pr");
        assert_eq!(
            attempt.pr_url.as_deref(),
            Some("https://github.example/owner/repo/pull/7")
        );
        assert_eq!(attempt.github_head_sha.as_deref(), Some("sha-orig"));
        // The original structured context must not be overwritten.
        let ctx: serde_json::Value =
            serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
        assert_eq!(ctx["path"], "adopted_pr");
        assert_eq!(ctx["pr_url"], "https://github.example/owner/repo/pull/7");
        assert_eq!(ctx["task_branch"], format!("task/{}", task.short_id));
    }
}
