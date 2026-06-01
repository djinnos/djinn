//! Default slot runner: routes dispatch through a
//! [`djinn_runtime::SessionRuntime`] chosen at startup by
//! [`crate::runtime_bridge::runtime_kind`].
//!
//! This is the Phase 2 K8s PR 4 pt2 cutover.  Previously (Phase 1 /
//! PR 4 pt1) this function constructed `Arc<dyn SupervisorServices>` and
//! called `TaskRunSupervisor::new(...).run(spec)` directly in-process.  That
//! path is now relegated to [`djinn_runtime::TestRuntime`] wrapping a
//! [`crate::runtime_bridge::SupervisorTaskRunner`] — which is the path
//! `DJINN_RUNTIME=test` selects and the path the integration tests exercise.
//! The production default (`DJINN_RUNTIME` unset / `"kubernetes"`) constructs
//! a [`djinn_k8s::KubernetesRuntime`] and drives
//! `prepare → await_report → teardown`.
//!
//! The runner receives the same arguments as the legacy runner
//! (`task_id`, `project_path`, `model_id`, `app_state`, `kill`, `pause`) so
//! it drops into the existing `SlotHandle::spawn` seam unchanged.  It
//! translates those into a [`TaskRunSpec`] and drives the runtime; the
//! returned [`djinn_runtime::TaskRunReport`] is collapsed to
//! `anyhow::Result<()>` for the slot actor's `JoinHandle`.
//!
//! `pause` is accepted for signature parity but the supervisor-driven flow
//! owns the whole run and does not release the slot between stages — there
//! is no external pause/resume handoff, so we just drop the token.  `kill`
//! is threaded into [`crate::supervisor::SupervisorServices::cancel`] (for
//! the Test path) and used to drive [`SessionRuntime::cancel`] (for the K8s
//! path).

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{
    BiStream, ResolvedCredentials, SessionRuntime, StreamEvent, TaskRunOutcome, TaskRunReport,
    TestRuntime,
};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context};

use super::helpers::{
    conflict_context_for_dispatch, default_target_branch, load_provider_credential, parse_model_id,
};

/// Default slot-dispatch runner.
///
/// Resolves `(task, flow, base_branch, task_branch, trigger)` from the task
/// row + ambient dispatch context, builds a [`TaskRunSpec`], then:
///
/// - on [`RuntimeKind::Kubernetes`]: constructs a
///   [`djinn_k8s::KubernetesRuntime`] and drives `prepare → teardown` — the
///   worker Pod connects back to djinn-server's TCP listener (bound at boot)
///   and streams events through `serve_on_tcp`'s dispatch.  The supervisor
///   body runs *inside the Pod*; the final `TaskRunReport` is synthesized
///   from the Job's terminal state during `teardown`.
/// - on [`RuntimeKind::Test`]: constructs a [`TestRuntime`] wrapping a
///   [`SupervisorTaskRunner`] — the supervisor runs in-process and the
///   terminal report rides the in-memory `BiStream`.
///
/// Returns:
/// - `Ok(())` on any terminal runtime outcome.  The slot actor treats that as
///   `SlotEvent::Free`; the supervisor has already written the
///   task_run/session/task rows, so there is nothing else for the slot to do.
/// - `Err(..)` only for infra-level setup failures the runtime cannot
///   express through a `TaskRunReport` (task lookup failed, mirror not
///   configured, runtime construction error).  The slot actor logs the
///   error and still emits `SlotEvent::Free`.
pub(crate) async fn run_supervisor_dispatch(
    task_id: String,
    _project_path: String,
    model_id: String,
    app_state: AgentContext,
    kill: CancellationToken,
    _pause: CancellationToken,
) -> anyhow::Result<()> {
    // ── Load the task ─────────────────────────────────────────────────────
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(&task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            anyhow::bail!("supervisor dispatch: task {task_id} not found");
        }
        Err(e) => {
            anyhow::bail!("supervisor dispatch: failed to load task {task_id}: {e}");
        }
    };

    // ── Resolve dispatch context (conflict / review-response) ─────────────
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let has_conflict = conflict_ctx.is_some();
    let has_review_response =
        matches!(task.status.as_str(), "needs_task_review" | "in_task_review");

    // ── Pick the supervisor flow ──────────────────────────────────────────
    let flow = crate::roles::flow_for_task_dispatch(&task, has_conflict, has_review_response);

    // ── Map flow → trigger ────────────────────────────────────────────────
    let trigger = if has_conflict {
        TaskRunTrigger::ConflictRetry
    } else if matches!(flow, SupervisorFlow::ReviewResponse) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    };

    // ── Resolve branches from project config ──────────────────────────────
    let base_branch = default_target_branch(&task.project_id, &app_state).await;
    let task_branch = task_branch_name(&task.short_id);

    // ── Resolve per-role model ids ────────────────────────────────────────
    let mut model_id_per_role: HashMap<RoleKind, String> = HashMap::new();
    for role in flow.role_sequence() {
        let resolved = resolve_role_model_preference(&task.project_id, role.as_str(), &app_state)
            .await
            .unwrap_or_else(|| model_id.clone());
        model_id_per_role.insert(*role, resolved);
    }

    // ── Build the spec ────────────────────────────────────────────────────
    //
    // Mint the canonical task-run id HERE, once, and thread it through the
    // spec. The runtime (`prepare`) derives its K8s resource name + registry
    // key from it, and the in-pod `TaskRunSupervisor` writes the `task_runs`
    // row + every session under it. One id end-to-end means the terminal
    // report's id matches the persisted sessions, which is what post-session
    // extraction keys off.
    // ── Resolve read-only multi-repo sources from the task's epic ─────────
    //
    // The epic may declare other registered projects whose code the worker
    // is allowed to READ (writes stay pinned to `task.project_id`). Thread
    // the set into the spec so the worker materializes each read-only
    // alongside the primary workspace and the prompt advertises them.
    // Non-fatal: an error just yields no read sources (feature degrades to
    // plain single-repo).
    let read_source_project_ids =
        djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone())
            .read_sources_for_task(task.epic_id.as_deref())
            .await
            .unwrap_or_default();

    // ── Private-dependency credentials for the worker Pod (best-effort) ───
    //
    // The project's GitHub owner + a short-lived installation token so the
    // agent's build/test commands in the Pod can fetch the org's PRIVATE
    // transitive deps (Go modules, cargo/pnpm git deps) — wired into the Job
    // env as `GOPRIVATE=github.com/<owner>/*` + a git `url.insteadOf` rewrite.
    // Derived from the project row + its installation; NO hardcoded org.
    // Non-fatal: a missing owner/installation just disables the rewrite (public
    // deps still resolve), so dispatch never fails on this.
    let pd_project_repo =
        djinn_db::ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let github_owner = pd_project_repo
        .get_github_coords(&task.project_id)
        .await
        .ok()
        .flatten()
        .map(|(owner, _repo)| owner);
    let github_install_token = match pd_project_repo.get_installation_id(&task.project_id).await {
        Ok(Some(installation_id)) => {
            djinn_provider::github_app::installations::get_installation_token(installation_id)
                .await
                .map(|t| t.token)
                .ok()
        }
        _ => None,
    };

    // ── Resolve the task's creator ────────────────────────────────────────
    //
    // Used for two things below: (1) per-user provider credential scoping via
    // the `SESSION_USER_ID` task-local (migration 28) — a task uses ITS
    // CREATOR's credential, falling back to the org-shared one when the column
    // is NULL (background / pre-multiuser tasks); and (2) the commit-author
    // identity. The `Task` model doesn't surface this column, so read it
    // directly. A lookup error or missing column is non-fatal: we just resolve
    // org-shared / fall back to the bot identity, preserving historical
    // behaviour.
    let created_by_user_id: Option<String> = sqlx::query_scalar!(
        "SELECT created_by_user_id FROM tasks WHERE id = $1",
        task.id
    )
    .fetch_optional(app_state.db.pool())
    .await
    .ok()
    .flatten()
    .flatten();

    // ── Resolve the commit-author identity (Vercel-friendly attribution) ──
    //
    // Commits the supervisor creates on the task branch are authored as the
    // task's CREATOR, not an anonymous bot. GitHub's per-user no-reply email
    // `<github_id>+<github_login>@users.noreply.github.com` links the commit
    // to that account, so it shows under the human's name AND Vercel's
    // deployment-author check (which rejects commits whose author email
    // matches no GitHub account) authorizes the build. The PR is still OPENED
    // by the App (`djinn-bot[bot]`), so the creator can review/approve their
    // own commits. A NULL creator (system/patrol tasks) — or a user row we
    // can't read — leaves these None; the supervisor then falls back to the
    // bot identity (those PRs don't clear Vercel's author check anyway).
    let (commit_author_name, commit_author_email) = match created_by_user_id.as_deref() {
        Some(uid) => match djinn_db::UserRepository::new(app_state.db.clone())
            .get_by_id(uid)
            .await
        {
            Ok(Some(user)) => (
                Some(
                    user.github_name
                        .clone()
                        .unwrap_or_else(|| user.github_login.clone()),
                ),
                Some(format!(
                    "{}+{}@users.noreply.github.com",
                    user.github_id, user.github_login
                )),
            ),
            _ => (None, None),
        },
        None => (None, None),
    };

    let task_run_id = uuid::Uuid::now_v7().to_string();
    let spec = TaskRunSpec {
        task_run_id,
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        trigger,
        base_branch,
        task_branch,
        flow,
        model_id_per_role,
        read_source_project_ids,
        github_owner,
        github_install_token,
        commit_author_name,
        commit_author_email,
    };

    // ── Resolve per-role provider credentials (Phase 7a) ──────────────────
    //
    // The host pulls every role's credential from the vault (or OAuth token
    // store) and ships them into the worker Pod via the per-task-run K8s
    // Secret. Fast-fail on resolution errors so the operator sees a clean
    // dispatch-time failure in session logs instead of a Pod that crash-loops
    // because the model client can't authenticate.
    //
    // The whole resolution loop runs under `SESSION_USER_ID = task creator` so
    // every `load_provider_credential` → `get_decrypted` (and the codex/copilot
    // OAuth token loads) resolves that user's private credential first.
    let mut credentials = ResolvedCredentials::default();
    let resolve_creds = async {
        for role in spec.flow.role_sequence() {
            let model_id = spec
                .model_id_per_role
                .get(role)
                .cloned()
                .unwrap_or_else(|| model_id.clone());
            let (provider_id, model_name) = parse_model_id(&model_id).map_err(|e| {
                anyhow::anyhow!(
                    "supervisor dispatch: cannot parse model id `{model_id}` for role {role:?}: {e}"
                )
            })?;
            let cred = load_provider_credential(&provider_id, &app_state)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "supervisor dispatch: load_provider_credential({provider_id}) for role {role:?}: {e}"
                    )
                })?;
            // OAuth configs (codex/copilot) hardcode a provider-default model;
            // stamp the resolved per-role model so the worker — which uses this
            // snapshot directly and never runs the live `cfg.model_id =
            // resolved.model_name` override — requests the user's configured
            // model instead of e.g. `gpt-5.1-codex`.
            credentials.insert(*role, cred.with_model_id(&model_name).to_serializable());
        }
        Ok::<(), anyhow::Error>(())
    };
    // Keep a copy for the post-run breaker scope below: `.scope(..)` consumes
    // `created_by_user_id`, but `record_success` must key on the same owner.
    let creator_scope = created_by_user_id.clone();
    djinn_core::auth_context::SESSION_USER_ID
        .scope(created_by_user_id, resolve_creds)
        .await?;

    // ── Resolve the runtime ───────────────────────────────────────────────
    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            anyhow::bail!(
                "supervisor dispatch: AgentContext has no MirrorManager configured — \
                 cannot run supervisor-driven task-run for task {}",
                task.short_id
            );
        }
    };
    let runtime_kind = runtime_kind();

    let runtime: Arc<dyn SessionRuntime> = match runtime_kind {
        RuntimeKind::Kubernetes => {
            let config = djinn_k8s::KubernetesConfig::from_env();
            let registry = match app_state.rpc_registry.as_ref() {
                Some(reg) => reg.clone(),
                None => {
                    anyhow::bail!(
                        "supervisor dispatch: AgentContext has no ConnectionRegistry \
                         — the djinn-server boot path must plumb `rpc_registry` into \
                         `AppState::agent_context()` before the Kubernetes runtime can \
                         be constructed"
                    );
                }
            };
            match djinn_k8s::KubernetesRuntime::with_db(config, registry, app_state.db.clone())
                .await
            {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    anyhow::bail!(
                        "supervisor dispatch: failed to construct KubernetesRuntime \
                         (is a kubeconfig available?): {e}"
                    );
                }
            }
        }
        RuntimeKind::Test => {
            let services = services_for_agent_context(app_state.clone(), kill.clone());
            let runner = SupervisorTaskRunner::new(mirror.clone(), services);
            Arc::new(TestRuntime::new(runner))
        }
    };

    // ── Drive prepare → (await report) → teardown ─────────────────────────
    let handle = runtime
        .prepare(&spec, &credentials)
        .await
        .map_err(|e| anyhow::anyhow!("runtime.prepare failed: {e}"))?;

    // Kill token fires cancel through the runtime.
    let cancel_runtime = runtime.clone();
    let cancel_handle = handle.clone();
    let cancel_task = tokio::spawn({
        let kill = kill.clone();
        async move {
            kill.cancelled().await;
            let _ = cancel_runtime.cancel(&cancel_handle).await;
        }
    });

    // Consume the worker's terminal report off the BiStream. For BOTH runtimes
    // `attach_stdio` bridges the worker's RPC events — including the final
    // `WorkerEvent::TerminalReport` — onto `events_rx`, so the report we read
    // here carries the REAL run id + the stages the in-pod supervisor actually
    // completed. This is the authoritative result. `teardown` below only
    // synthesizes a stub (`Interrupted`, no stages) for the case where the
    // worker died before it could emit a report; we fall back to that stub
    // only when the stream yielded nothing.
    //
    // (Until this change the Kubernetes path discarded the stream and relied on
    // teardown's stub, which always reported `Interrupted`/`[]` under a
    // host-minted id that matched no persisted row — silently disabling
    // post-session extraction. See `~/.claude/plans/memory-extraction-fix.md`.)
    let bistream_result = runtime.attach_stdio(&handle).await;
    let report_result: anyhow::Result<Option<TaskRunReport>> = match bistream_result {
        Ok(bistream) => await_report_from_stream(bistream, &kill).await,
        Err(e) => Err(anyhow::anyhow!("runtime.attach_stdio failed: {e}")),
    };

    // Stop the cancel watcher regardless of success path.
    cancel_task.abort();
    let _ = cancel_task.await;

    let teardown = runtime.teardown(handle).await;

    // Best-effort: if the in-pod supervisor died before sending its terminal
    // `update_task_run_status` RPC (OOM, eviction, SIGKILL past the grace
    // window), the matching `task_runs` row is still 'running' in the host
    // DB. Stamp it now using the terminal status the teardown report
    // synthesized — defaulting to `Interrupted` when the report is missing.
    // Slot serialization means at most one in-flight run per task, so
    // reaping by `task_id` finds exactly the right row.
    let reap_status = teardown
        .as_ref()
        .ok()
        .map(report_to_terminal_status)
        .unwrap_or(TaskRunStatus::Interrupted);
    reap_orphan_task_run(&app_state, &task.id, reap_status).await;

    match (report_result, teardown) {
        (Ok(streamed), Ok(teardown_report)) => {
            // Prefer the streamed terminal report — it carries the real run id
            // and the stages the in-pod supervisor actually completed. The
            // teardown report is a stub fallback for the no-report case (worker
            // died before emitting), which still feeds the orphan reap above.
            let report = select_terminal_report(streamed, teardown_report);
            tracing::info!(
                task_id = %task.short_id,
                task_run_id = %report.task_run_id,
                outcome = ?report.outcome,
                stages_completed = ?report.stages_completed,
                runtime = ?runtime_kind,
                "supervisor dispatch: task-run complete"
            );
            // Feed the model circuit-breaker on a productive run. A terminal
            // outcome that maps to `Completed` (PR opened / closed / escalated)
            // with at least one completed stage means the model produced tokens
            // and drove the flow to a terminal state — a clear "this model is
            // healthy" signal. `record_success` resets the consecutive-failure
            // counter and clears any expired cooldown, so a model that recovers
            // isn't needlessly held in failover. We key on the dispatch-level
            // `model_id` (the one the coordinator's `is_available` gate selects
            // and would re-select), matching what `record_stall`/`record_failure`
            // trip on the stall path. We deliberately do NOT reset on
            // Interrupted/Failed/empty runs (those aren't evidence of recovery).
            if !report.stages_completed.is_empty()
                && matches!(report_to_terminal_status(&report), TaskRunStatus::Completed)
            {
                app_state
                    .health_tracker
                    .record_success(creator_scope.as_deref(), &model_id);
            }
            // Phase 2.2: post-session knowledge extraction. Fire-and-forget on
            // the long-lived server (it owns the embedding model + Qdrant, so
            // notes created here get embedded; worker pods are ephemeral and
            // lack that config). Gated on real work having run — skip
            // interrupted/empty runs so we don't burn an LLM call on nothing.
            // `report.task_run_id` is the canonical id the sessions were
            // written under, so `run_post_session_extraction` matches them.
            // Extraction is fully isolated: any failure is logged and never
            // affects the task-run outcome.
            if !report.stages_completed.is_empty() {
                let app_state_ext = app_state.clone();
                let task_id_ext = task.id.clone();
                let task_run_id_ext = report.task_run_id.clone();
                tokio::spawn(async move {
                    crate::actors::slot::session_extraction::run_post_session_extraction(
                        task_id_ext,
                        task_run_id_ext,
                        app_state_ext,
                    )
                    .await;
                });
            }
            Ok(())
        }
        (Err(e), teardown_result) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                teardown_ok = teardown_result.is_ok(),
                runtime = ?runtime_kind,
                "supervisor dispatch: pre-teardown failure"
            );
            Err(e)
        }
        (Ok(_streamed), Err(e)) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                runtime = ?runtime_kind,
                "supervisor dispatch: teardown failure"
            );
            Err(anyhow::anyhow!("runtime.teardown failed: {e}"))
        }
    }
}

fn report_to_terminal_status(report: &TaskRunReport) -> TaskRunStatus {
    match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        | TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
        TaskRunOutcome::Failed { .. } => TaskRunStatus::Failed,
        TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
    }
}

/// Choose the authoritative terminal report for a completed dispatch.
///
/// The streamed worker report (when present) wins: it carries the canonical
/// run id the sessions were persisted under and the stages actually completed.
/// The runtime's teardown report is a stub fallback used only when the worker
/// died before emitting a report. Keeping this as a named function makes the
/// "streamed id beats teardown id" invariant — the one that gates post-session
/// extraction — directly testable.
fn select_terminal_report(
    streamed: Option<TaskRunReport>,
    teardown: TaskRunReport,
) -> TaskRunReport {
    streamed.unwrap_or(teardown)
}

async fn reap_orphan_task_run(
    app_state: &AgentContext,
    task_id: &str,
    terminal_status: TaskRunStatus,
) {
    let repo = TaskRunRepository::new(app_state.db.clone());
    match repo.reap_running_for_task(task_id, terminal_status).await {
        Ok(Some(run_id)) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %run_id,
                status = %terminal_status,
                "supervisor dispatch: reaped orphan task_run row \
                 (in-pod supervisor never sent terminal RPC)"
            );
        }
        Ok(None) => {
            // Common path: the in-pod supervisor's terminal RPC already
            // flipped the row. Nothing to do.
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "supervisor dispatch: reap_running_for_task failed"
            );
        }
    }
}

/// Drain a [`BiStream`] until we see the terminal [`StreamEvent::Report`]
/// frame, returning the [`TaskRunReport`] it carries.
///
/// Both runtimes bridge the worker's events onto `events_rx`: `TestRuntime`
/// forwards the [`TaskRunReport`] produced by [`SupervisorTaskRunner`], and
/// `KubernetesRuntime::attach_stdio` forwards the worker's
/// `WorkerEvent::TerminalReport` from its RPC connection. We drop non-terminal
/// frames (already observed via the event-bus / DB-write seams) and return:
///
/// - `Ok(Some(report))` — the worker emitted its terminal report. This is the
///   authoritative result (real run id + completed stages).
/// - `Ok(None)` — the channel closed (worker exited / connection dropped) or
///   the `kill` token fired before any report arrived. The caller falls back
///   to the runtime's teardown stub.
///
/// Bounded by `kill`: a hung worker connection can't pin the slot past the
/// cancel the slot actor already requested.
async fn await_report_from_stream(
    mut stream: BiStream,
    kill: &CancellationToken,
) -> anyhow::Result<Option<TaskRunReport>> {
    loop {
        tokio::select! {
            biased;
            _ = kill.cancelled() => {
                tracing::debug!(
                    "supervisor dispatch: kill fired while awaiting terminal report; \
                     proceeding to teardown"
                );
                return Ok(None);
            }
            frame = stream.events_rx.recv() => {
                match frame {
                    Some(StreamEvent::Report(report)) => return Ok(Some(report)),
                    Some(other) => {
                        tracing::trace!(event = ?other, "supervisor dispatch: dropping non-terminal frame");
                    }
                    // Channel closed without a terminal report — the supervisor
                    // path persists state as a side effect; the caller uses the
                    // teardown stub for the terminal status.
                    None => return Ok(None),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_runtime::RoleKind;

    fn report(id: &str, stages: Vec<RoleKind>, outcome: TaskRunOutcome) -> TaskRunReport {
        TaskRunReport {
            task_run_id: id.to_string(),
            outcome,
            stages_completed: stages,
        }
    }

    /// The regression guard: the host's transport/teardown id (A) and the
    /// in-pod/persisted id (B) differ. Post-session extraction must key off the
    /// streamed report (B), under which the sessions were actually written —
    /// NOT the teardown stub (A), which matches no persisted row. Before this
    /// fix the K8s path used the stub, silently disabling extraction.
    #[test]
    fn streamed_report_wins_over_teardown_stub() {
        let streamed = report(
            "id-B-persisted",
            vec![RoleKind::Worker, RoleKind::Reviewer],
            TaskRunOutcome::PrOpened {
                url: "https://example/pr/1".into(),
                sha: "deadbeef".into(),
            },
        );
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);

        let chosen = select_terminal_report(Some(streamed), teardown_stub);

        assert_eq!(
            chosen.task_run_id, "id-B-persisted",
            "extraction id must come from the streamed report, not the teardown stub"
        );
        assert!(
            !chosen.stages_completed.is_empty(),
            "real stages must survive so the extraction gate opens"
        );
    }

    /// No streamed report (worker died before emitting) → fall back to the
    /// teardown stub so the orphan-reap terminal status is still applied.
    #[test]
    fn teardown_stub_used_when_no_streamed_report() {
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);
        let chosen = select_terminal_report(None, teardown_stub);
        assert_eq!(chosen.task_run_id, "id-A-transport");
        assert!(chosen.stages_completed.is_empty());
    }

    #[tokio::test]
    async fn await_report_returns_streamed_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        let streamed = report(
            "id-B",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        events_tx
            .send(StreamEvent::Report(streamed))
            .await
            .expect("send report");

        let got = await_report_from_stream(bistream, &kill)
            .await
            .expect("await ok");
        assert_eq!(got.expect("some report").task_run_id, "id-B");
    }

    #[tokio::test]
    async fn await_report_drops_non_terminal_frames_then_returns_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::AssistantDelta {
                session_id: "s1".into(),
                text: "thinking".into(),
            })
            .await
            .unwrap();
        events_tx
            .send(StreamEvent::Report(report(
                "id-B",
                vec![RoleKind::Worker],
                TaskRunOutcome::Interrupted,
            )))
            .await
            .unwrap();

        let got = await_report_from_stream(bistream, &kill)
            .await
            .expect("await ok");
        assert_eq!(got.expect("some report").task_run_id, "id-B");
    }

    #[tokio::test]
    async fn await_report_returns_none_when_kill_fires() {
        // Sender kept alive so recv() would otherwise pend forever — kill must
        // bound the wait.
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        kill.cancel();
        let got = await_report_from_stream(bistream, &kill)
            .await
            .expect("await ok");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn await_report_returns_none_when_channel_closes_without_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        drop(events_tx);
        let got = await_report_from_stream(bistream, &kill)
            .await
            .expect("await ok");
        assert!(got.is_none());
    }
}
