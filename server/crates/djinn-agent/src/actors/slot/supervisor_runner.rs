// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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
use tracing::Instrument;

use djinn_core::models::{TaskRunStatus, TaskRunTrigger};
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{TaskRepository, task_branch_name};
use djinn_runtime::{
    BiStream, LoopGuardKind, ProviderFailureClass, ResolvedCredentials, SessionRuntime,
    StreamEvent, TaskRunOutcome, TaskRunReport, TestRuntime,
};

use crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference;
use crate::context::AgentContext;
use crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind};
use crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context};

use super::helpers::{
    conflict_context_for_dispatch, default_target_branch, load_provider_credential, parse_model_id,
};

fn supervisor_rpc_span(op: &'static str, session_id: &str, task_id: &str) -> tracing::Span {
    tracing::info_span!(
        "djinn.supervisor.rpc",
        op,
        session_id = %session_id,
        task_id = %task_id,
    )
}

/// Persist and surface a credential revocation after a run hit a 401.
///
/// Marks the stored credential for the run's provider revoked (the F5-safe
/// source of truth the provider catalog reads) and emits a `credential_revoked`
/// event so the UI can prompt a reconnect live. Best-effort: any failure here is
/// logged, never propagated (the run already failed). `owner` is the task
/// creator's user id (`None` for org-shared credentials).
async fn surface_credential_revocation(
    app_state: &AgentContext,
    owner: Option<&str>,
    model_id: &str,
) {
    let Ok((provider_id, _model_name)) = parse_model_id(model_id) else {
        return;
    };
    // The stored credential's provider id for an OAuth provider is the merged
    // child (e.g. "openai" → "chatgpt_codex"); for API-key providers it is the
    // provider id itself.
    let cred_provider = djinn_provider::catalog::builtin::resolve_oauth_provider(&provider_id)
        .map(|s| s.to_string())
        .unwrap_or(provider_id);
    let reason = format!(
        "{cred_provider} rejected the credential (HTTP 401 — token revoked or invalid). \
         Reconnect this provider to resume."
    );
    let repo = djinn_provider::repos::CredentialRepository::new(
        app_state.db.clone(),
        app_state.event_bus.clone(),
    );
    match repo.mark_revoked(&cred_provider, owner, &reason).await {
        Ok(n) if n > 0 => tracing::warn!(
            provider = %cred_provider,
            owner = ?owner,
            "supervisor: marked credential revoked after 401 — owner must reconnect"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            provider = %cred_provider,
            error = %e,
            "supervisor: failed to mark credential revoked after 401"
        ),
    }
}

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
    project_path: String,
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

    // ── Resolve branches from project config ──────────────────────────────
    let base_branch = default_target_branch(&task.project_id, &app_state).await;
    let task_branch = task_branch_name(&task.short_id);

    // ── Pick the supervisor flow ──────────────────────────────────────────
    let base_flow = crate::roles::flow_for_task_dispatch(&task, has_conflict, has_review_response);

    // ── Stage-aware resume: skip the worker redo when its output is durable ──
    //
    // A task that lands back at `needs_task_review` routes to `ReviewResponse`
    // (worker → reviewer). But the ONLY way a task reaches that status is the
    // worker having already submitted (`submit_task_review`), so re-running the
    // worker redoes identical work — the failure mode behind a reviewer-stage
    // pod kill (Job deadline) redispatching as a fresh ~55-min worker run.
    //
    // When the worker's commits are durable on the mirror task_branch (it
    // exists and carries commits beyond its merge-base with base — the sibling
    // eager-push makes this hold after the worker stage), upgrade to the
    // reviewer-only `ReviewResume` flow so the redispatch reviews the existing
    // diff instead of redoing the work. Base having moved on (another task's
    // PR merged mid-cycle) does NOT demote to a worker redo: requiring
    // fast-forwardability here livelocked review-stage tasks on a busy board,
    // because base moved during nearly every cycle (the t9wi/32bk wedge,
    // 2026-06-11). This is driven by what DURABLY happened (branch present),
    // not by an outcome emitted after cancellation — so a cancelled reviewer
    // run never tricks us into skipping real work (cf. the kw7s cancel-gate
    // precedent).
    //
    // The guard is conservative: a missing/empty task_branch (worker never
    // pushed, first cycle) keeps `ReviewResponse` and the full worker redo.
    // Provider-failure and genuine reviewer-rejection paths are unaffected —
    // a rejection moves the task to `open` (→ NewTask worker flow), never to
    // `needs_task_review`, so it never reaches this branch.
    let worker_output_durable = matches!(base_flow, SupervisorFlow::ReviewResponse)
        && match app_state.mirror.as_ref() {
            Some(mirror) => {
                // This runs in the dispatch path. The durability probe is a few
                // git subprocesses against the mirror (read-only, no lock), but a
                // wedged git op (a locked / huge mirror) would otherwise stall the
                // dispatch pass — and with multiple ReviewResponse candidates per
                // pass that latency is sequential. Bound it: a timeout (or any
                // probe failure) conservatively yields `false`, i.e. keep the full
                // worker redo rather than risk skipping it on a stale read.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    mirror.branch_ahead_of_base(&task.project_id, &task_branch, &base_branch),
                )
                .await
                {
                    Ok(durable) => durable,
                    Err(_) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            branch = %task_branch,
                            "supervisor dispatch: branch_ahead_of_base durability probe timed out \
                             (>10s); keeping full worker redo (ReviewResponse)"
                        );
                        false
                    }
                }
            }
            None => false,
        };
    let flow = resume_flow(base_flow, worker_output_durable);
    let loop_guard_intervention_role = flow
        .role_sequence()
        .first()
        .map(|role| role.as_str())
        .unwrap_or("worker");
    if matches!(flow, SupervisorFlow::ReviewResume) {
        tracing::info!(
            task_id = %task.short_id,
            branch = %task_branch,
            "supervisor dispatch: worker output durable on task_branch; \
             resuming at reviewer stage (skipping worker redo)"
        );
    }

    // ── Map flow → trigger ────────────────────────────────────────────────
    let trigger = if has_conflict {
        TaskRunTrigger::ConflictRetry
    } else if matches!(
        flow,
        SupervisorFlow::ReviewResponse | SupervisorFlow::ReviewResume
    ) {
        TaskRunTrigger::ReviewResponse
    } else {
        TaskRunTrigger::NewTask
    };

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
    // own commits. A NULL creator (legacy system tasks) — or a user row we
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
    let cancel_task_id = task.id.clone();
    let cancel_model_id = model_id.clone();
    let cancel_session_id = spec.task_run_id.clone();
    let cancel_task = tokio::spawn({
        let kill = kill.clone();
        async move {
            kill.cancelled().await;
            let span = tracing::info_span!(
                "djinn.slot.kill",
                task_id = %cancel_task_id,
                model_id = %cancel_model_id,
            );
            async move {
                tracing::info!(
                    event = "slot.runtime_cancel",
                    task_id = %cancel_task_id,
                    model_id = %cancel_model_id,
                );
                let rpc_span = supervisor_rpc_span("kill", &cancel_session_id, &cancel_task_id);
                async move {
                    tracing::info!(
                        event = "supervisor.rpc.cancel",
                        op = "kill",
                        session_id = %cancel_session_id,
                        task_id = %cancel_task_id,
                    );
                    let _ = cancel_runtime.cancel(&cancel_handle).await;
                }
                .instrument(rpc_span)
                .await;
            }
            .instrument(span)
            .await;
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
    // D2: distinguish a worker that never completed its startup handshake (Pod
    // failed to start) from a generic attach failure, so we can feed the breaker
    // and fail over below.
    let handshake_timed_out = matches!(
        &bistream_result,
        Err(djinn_runtime::RuntimeError::HandshakeTimeout(_))
    );
    // When the worker is SIGKILLed mid-stage (a memory-cgroup OOM kill of
    // rust-analyzer + rustc + rust-lld, a node eviction), its RPC connection
    // can be left half-open — the kernel reaped the worker's child processes
    // and wedged the supervisor without a clean TCP FIN, so `events_rx` never
    // closes and `await_report_from_stream` would block until `kill` fires
    // (which it won't — the slot isn't being cancelled) or the coordinator's
    // generic 30-minute idle stall reaper finally collected the session,
    // mis-attributing an OOM to a "stall". Race the report stream against the
    // runtime's infra-death watch (`watch_infra_death`, a no-op pend-forever on
    // the Test runtime) so a terminally dead Job/Pod is detected within ~15s,
    // its real death reason captured, and the slot freed promptly.
    let mut infra_death: Option<String> = None;
    let report_result: anyhow::Result<Option<TaskRunReport>> = match bistream_result {
        Ok(bistream) => {
            tokio::select! {
                biased;
                // The report stream is authoritative for a *clean* exit — a
                // worker that exits normally flushes its TerminalReport here.
                // Prefer it (biased) so a Job that flips Failed in the same
                // instant the worker delivered a report doesn't shadow the
                // real outcome.
                res = await_report_from_stream(bistream, &kill, &spec.task_run_id, &spec.task_id) => res,
                reason = runtime.watch_infra_death(&handle) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        %reason,
                        runtime = ?runtime_kind,
                        "supervisor dispatch: worker infra died before terminal report \
                         (OOM / eviction / Job failure); finalizing run as interrupted"
                    );
                    infra_death = Some(reason);
                    Ok(None)
                }
            }
        }
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

    // D2: a worker that never completed its startup handshake (image-pull
    // failure, unschedulable, crash-loop) was torn down above. Treat it as an
    // infra stall: trip the breaker so dispatch fails over off this model, and
    // mark the task's failure as a throttle so the coordinator spares it from the
    // terminal MAX_DISPATCH_FAILURES streak (A3) and redispatches with backoff
    // rather than immediately re-hanging on the same model.
    if handshake_timed_out {
        app_state
            .health_tracker
            .record_stall(creator_scope.as_deref(), &model_id);
        app_state.health_tracker.note_task_provider_failure(
            &task.id,
            djinn_provider::catalog::health::TaskFailureSignal {
                throttle: true,
                retry_after_ms: None,
            },
        );
        let _ = task_repo
            .log_activity(
                Some(&task.id),
                "system",
                "system",
                "comment",
                &serde_json::json!({
                    "body": format!(
                        "Worker Pod failed to complete its startup handshake within the \
                         deadline (image pull, unschedulable, or crash-loop). Tore down the \
                         Job and failed over off model {model_id}."
                    )
                })
                .to_string(),
            )
            .await;
        tracing::warn!(
            task_id = %task.short_id,
            %model_id,
            "supervisor dispatch: worker handshake timed out; recorded stall + failover"
        );
    }

    // The infra-death watch fired before any terminal report: the worker Job/Pod
    // died out-of-band (OOM SIGKILL, node eviction, BackoffLimitExceeded). Surface
    // the REAL reason where operators look and finalize the orphaned `running`
    // session row promptly — without this the session lingered `running` (with a
    // live-looking but half-open RPC connection) until the 30-min idle stall
    // reaper collected it as a generic "stall", losing the OOM attribution.
    //
    // Deliberately does NOT feed the model circuit-breaker: an OOM / eviction /
    // Job failure is an INFRA death, not evidence the model is bad (mirrors the
    // zombie-session backstop's reasoning — tripping the breaker here would
    // auto-disable the often-only model for the whole scope on a memory pinch).
    // The orphan `task_runs` row was already reaped above; releasing the task for
    // redispatch is handled by the coordinator's stuck-task reconciler once the
    // slot frees (this fn returns), exactly as for any other slot-freeing exit.
    if let Some(reason) = infra_death.as_deref() {
        let payload = serde_json::json!({
            "error": format!("Worker infrastructure died before completing the run: {reason}"),
            "agent_type": "system",
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(&task.id),
                "agent-supervisor",
                "system",
                "session_error",
                &payload,
            )
            .await;
        // Finalize any orphaned `running` session row for this task (status →
        // interrupted, ended_at set). Idempotent: only touches `running` rows,
        // so it can't double-finalize a row a late TerminalReport already
        // flipped to `completed`, and it races harmlessly with the coordinator's
        // zombie/stall reapers (same single-statement UPDATE-where-running).
        let session_repo =
            djinn_db::SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        match session_repo.interrupt_running_for_task(&task.id).await {
            Ok(n) if n > 0 => tracing::warn!(
                task_id = %task.short_id,
                %reason,
                sessions = n,
                "supervisor dispatch: finalized orphaned running session(s) after infra death"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor dispatch: failed to finalize session row after infra death"
            ),
        }

        // BUG 1 re-attach: in the in-pod verification design the worker writes a
        // terminal `verification_runs` row (and persists its `WorkerSubmitted`
        // report) BEFORE the pod can die out-of-band. When the pod is TTL-GC'd
        // the report frame is no longer buffered on `events_rx`, so the streamed
        // report is gone and we fall back to the `Interrupted` teardown stub
        // below — whose outcome is NOT `WorkerSubmitted`, so the host
        // verification pipeline never gets armed and the task is left `verifying`
        // with no pipeline. The coordinator's stuck-`verifying` sweep then resets
        // it `verifying → open`, discarding completed-and-verified work.
        //
        // Detect that case at the seam: if a USABLE terminal in-pod
        // `verification_runs` row exists for this task, re-arm the slot-free host
        // pipeline against it directly. `spawn_verification_with_in_pod_run`
        // consumes the existing row (idempotent against the unchanged commit) —
        // exactly what the clean `WorkerSubmitted` path does below. We do this
        // BEFORE the teardown-stub fallback so the re-attach wins the race with
        // the coordinator sweep.
        match djinn_db::VerificationRunRepository::new(app_state.db.clone())
            .latest_terminal_for_task(&task.id)
            .await
        {
            Ok(Some(run_id)) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    verification_run_id = %run_id,
                    "supervisor dispatch: infra death after in-pod verification wrote terminal row; \
                     re-arming host verification pipeline instead of discarding the run"
                );
                crate::actors::slot::verification::spawn_verification_with_in_pod_run(
                    task.id.clone(),
                    project_path.clone(),
                    app_state.clone(),
                    Some(run_id),
                );
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "supervisor dispatch: failed to look up in-pod verification row after infra death; \
                 leaving recovery to the coordinator sweep"
            ),
        }
    }

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
            // Verify BEFORE review. The worker stage completed and the in-pod
            // supervisor fired `submit_verification` (in_progress → verifying);
            // the task-run pod is gone and the model slot is about to free. Spawn
            // the slot-free verification pipeline on the HOST (which owns the
            // MirrorManager): it checks out the durable task_branch, runs the
            // scoped verification commands (a cache hit when the commit is
            // unchanged), and on green transitions verifying → needs_task_review
            // (re-dispatched as a reviewer-only ReviewResume), on red releases the
            // task for worker rework with the failure feedback. `spawn_verification`
            // registers the task in the shared `verifying_tasks` tracker, so the
            // coordinator's stuck-`verifying` recovery sweep ("no active
            // verification pipeline") covers a server restart mid-verification.
            //
            // Guarded on the outcome (not just the status) so a stale/late report
            // can't double-spawn: only a clean `WorkerSubmitted` arms it. The
            // pipeline is idempotent against the cache for an unchanged commit
            // regardless.
            if matches!(report.outcome, TaskRunOutcome::WorkerSubmitted) {
                tracing::info!(
                    task_id = %task.short_id,
                    in_pod_verification_run_id = ?report.verification_run_id,
                    "supervisor dispatch: worker submitted to verification; spawning slot-free verification pipeline"
                );
                // If the worker ran verification IN-POD (reusing its compiled
                // artifacts before its target dir was torn down), it shipped the
                // terminal `verification_runs.id` on the report — the pipeline
                // consumes that row directly instead of dispatching a second
                // verify Job (the double-compile fix). `None` keeps the
                // separate-pod fallback.
                crate::actors::slot::verification::spawn_verification_with_in_pod_run(
                    task.id.clone(),
                    project_path.clone(),
                    app_state.clone(),
                    report.verification_run_id.clone(),
                );
            }

            persist_loop_guard_activity(&task_repo, &task.id, &report).await;
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
            if terminal_report_feeds_model_success(&report) {
                app_state
                    .health_tracker
                    .record_success(creator_scope.as_deref(), &model_id);
            }
            // Symmetric to `record_success`: feed the breaker when a stage
            // failed FAST on a typed provider error that produced no token
            // stall. The coordinator's stall detector only catches sessions that
            // hang; a fast rejection (bad/expired credential, a request the
            // provider keeps rejecting, repeated 5xx, a throttle answered with a
            // 4xx instead of a hang) exits cleanly and was previously invisible
            // to the breaker — so dispatch kept re-selecting a model that is
            // structurally broken for this user. `stage.rs` classified the
            // typed `ProviderError` (which lives only in-pod and can't ride the
            // report frame) into a `ProviderFailureClass`; map it back here onto
            // the SAME knobs the coordinator's stall path uses, keyed on the
            // SAME `creator_scope` as `record_success` so the trip is scoped to
            // this task's creator, not global:
            //   - Throttle  → `record_stall`   (rate-limit: immediate failover,
            //     cooldown outlasts the task's redispatch ladder; mirrors the
            //     coordinator's throttle→stall intent).
            //   - Failure   → `record_failure` (auth/invalid/5xx/invalid-output:
            //     gentler, trips only after repeats — a one-off may be transient).
            // A hard `Transport` death folds into `Failure` in `stage.rs` so a
            // model that dies instantly on every dispatch trips the gentle
            // consecutive-failure breaker (a one-off blip is absorbed by the next
            // successful run's `record_success`). ContextOverflow / EmptyCompletion
            // and untyped errors still classify to `None` and are deliberately NOT
            // fed (reactive compaction / empty-turn backoff handle those;
            // non-provider errors must not over-trip the breaker).
            if let Some(class) = provider_failure_class_for_report(&report) {
                let (is_throttle, retry_after_ms) = match class {
                    djinn_runtime::ProviderFailureClass::Throttle { retry_after_ms } => {
                        app_state
                            .health_tracker
                            .record_stall(creator_scope.as_deref(), &model_id);
                        (true, retry_after_ms)
                    }
                    djinn_runtime::ProviderFailureClass::Failure => {
                        app_state
                            .health_tracker
                            .record_failure(creator_scope.as_deref(), &model_id);
                        (false, None)
                    }
                    djinn_runtime::ProviderFailureClass::AuthInvalid => {
                        // A 401/revoked credential is deterministic — trip the
                        // breaker IMMEDIATELY (like a throttle) so dispatch fails
                        // over to the user's next model at once, and persist +
                        // surface the revocation so the owner is told to
                        // reconnect (F5-safe row + live event).
                        app_state
                            .health_tracker
                            .record_stall(creator_scope.as_deref(), &model_id);
                        surface_credential_revocation(
                            &app_state,
                            creator_scope.as_deref(),
                            &model_id,
                        )
                        .await;
                        // Throttle-like for the per-task redispatch ladder: a dead
                        // credential must never march the task to terminal
                        // force-close. With a healthy fallback the breaker hold
                        // makes dispatch fail over; with none the task parks until
                        // the owner reconnects (which clears the breaker via the
                        // credential-updated event).
                        (true, None)
                    }
                };
                // Side-channel for the coordinator's per-task redispatch logic
                // (A3/A6). The breaker above is keyed per `(scope, model)`; the
                // coordinator's terminal-failure streak + escalating cooldown are
                // keyed per task and observed only via the task reappearing as
                // dispatch-ready (it never sees this report directly). Stash the
                // last failure class — and any provider-stated retry-after — under
                // the task id on the SHARED `HealthTracker` (the same Arc instance
                // the coordinator holds as `self.health`) so the streak/cooldown
                // sites can consult it: a Throttle must not march a healthy task to
                // terminal close (A3), and a provider reset floors the cooldown so
                // a multi-hour quota window isn't probed on the fixed ladder (A6).
                app_state.health_tracker.note_task_provider_failure(
                    &task.id,
                    djinn_provider::catalog::health::TaskFailureSignal {
                        throttle: is_throttle,
                        retry_after_ms,
                    },
                );
            }

            if is_budget_park_report(&report) {
                match app_state.coordinator().await {
                    Some(coordinator) => {
                        if let Err(e) = coordinator
                            .clear_planned_dispatch_completion(
                                &task.id,
                                "budget_park_planned_completion_clear",
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "supervisor dispatch: failed to clear budget-park dispatch state"
                            );
                        }
                        // `ClearPlannedDispatchCompletion` routes through the
                        // no-mover disposition call site after clearing stale
                        // dispatch markers. Avoid sending a second message for
                        // the same settled run.
                    }
                    None => {
                        tracing::debug!(
                            task_id = %task.short_id,
                            "supervisor dispatch: no coordinator handle; budget-park dispatch state clear skipped"
                        );
                    }
                }
            }

            if let TaskRunOutcome::LoopGuardTripped {
                kind,
                offending_signature,
                threshold,
                observed,
                turn_span,
                session_id,
            } = &report.outcome
            {
                let reason = loop_guard_planner_intervention_reason(
                    *kind,
                    offending_signature,
                    *threshold,
                    *observed,
                    *turn_span,
                    session_id,
                );
                tracing::warn!(
                    task_id = %task.short_id,
                    guard_kind = ?kind,
                    offending_signature = %offending_signature,
                    threshold,
                    observed,
                    turn_start = turn_span.0,
                    turn_end = turn_span.1,
                    session_id = %session_id,
                    "supervisor dispatch: loop guard tripped; routing to Planner intervention"
                );
                match app_state.coordinator().await {
                    Some(coordinator) => {
                        if let Err(e) = coordinator
                            .route_loop_guard_planner_intervention(
                                &task.id,
                                loop_guard_intervention_role,
                                &reason,
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "supervisor dispatch: failed to enqueue loop-guard Planner intervention"
                            );
                        }
                    }
                    None => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            "supervisor dispatch: no coordinator handle; loop-guard Planner intervention not enqueued"
                        );
                    }
                }
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

/// Stage-aware-resume decision seam: given the flow the coordinator routed to
/// and whether the worker's output is durably present on the mirror task_branch,
/// pick the flow the run actually executes.
///
/// Only `ReviewResponse` (the worker→reviewer re-entry, reached exclusively from
/// `needs_task_review`/`in_task_review` where the worker already submitted) is a
/// candidate for the reviewer-only `ReviewResume` upgrade, and only when the
/// worker output is durable. Every other flow — and `ReviewResponse` without
/// durable output — passes through unchanged so an absent/empty task_branch
/// falls back to the full worker redo. Kept as a pure free function so the
/// "resume at reviewer vs redo worker" choice is unit-testable without a DB or
/// mirror.
fn resume_flow(base_flow: SupervisorFlow, worker_output_durable: bool) -> SupervisorFlow {
    if matches!(base_flow, SupervisorFlow::ReviewResponse) && worker_output_durable {
        SupervisorFlow::ReviewResume
    } else {
        base_flow
    }
}

fn provider_failure_class_for_report(report: &TaskRunReport) -> Option<ProviderFailureClass> {
    match &report.outcome {
        TaskRunOutcome::Failed {
            provider_failure: Some(class),
            ..
        } => Some(*class),
        _ => None,
    }
}

fn is_budget_park_report(report: &TaskRunReport) -> bool {
    matches!(
        &report.outcome,
        TaskRunOutcome::Parked { reason, .. } if reason == "budget"
    )
}

fn terminal_report_feeds_model_success(report: &TaskRunReport) -> bool {
    !report.stages_completed.is_empty()
        && matches!(report_to_terminal_status(report), TaskRunStatus::Completed)
        && !is_budget_park_report(report)
}

async fn persist_loop_guard_activity(
    task_repo: &TaskRepository,
    task_id: &str,
    report: &TaskRunReport,
) {
    let TaskRunOutcome::LoopGuardTripped {
        kind,
        offending_signature,
        threshold,
        observed,
        turn_span,
        session_id,
    } = &report.outcome
    else {
        return;
    };

    let details = serde_json::json!({
        "kind": loop_guard_kind_label(*kind),
        "offending_signature": offending_signature,
        "threshold": threshold,
        "observed": observed,
        "turn_span": {
            "start": turn_span.0,
            "end": turn_span.1,
        },
        "session_id": session_id,
        "task_run_id": report.task_run_id,
    });
    let payload = serde_json::json!({
        "kind": "loop_guard_tripped",
        "details": details,
        "body": format!(
            "Reply-loop guard `{}` tripped in session `{}` on turns {}..={}: `{}` \
             (observed {}/{})",
            loop_guard_kind_label(*kind),
            session_id,
            turn_span.0,
            turn_span.1,
            offending_signature,
            observed,
            threshold,
        ),
    })
    .to_string();

    if let Err(e) = task_repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "system",
            "loop_guard_tripped",
            &payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "supervisor dispatch: failed to persist loop_guard_tripped activity"
        );
    }
}

fn loop_guard_kind_label(kind: LoopGuardKind) -> &'static str {
    match kind {
        LoopGuardKind::IdenticalToolFailure => "identical_tool_failure",
        LoopGuardKind::PermissionDenial => "permission_denial",
        LoopGuardKind::IdenticalOutput => "identical_output",
        LoopGuardKind::ConsecutiveFailures => "consecutive_failures",
    }
}

fn loop_guard_planner_intervention_reason(
    kind: LoopGuardKind,
    offending_signature: &str,
    threshold: u32,
    observed: u32,
    turn_span: (u32, u32),
    session_id: &str,
) -> String {
    format!(
        "Reply-loop guard `{}` tripped in session `{}`: offending_signature=`{}`, \
         threshold={}, observed={}, turn_span={}..={}. The run completed in a \
         degenerate loop rather than failing to dispatch; do not re-dispatch the \
         identical worker attempt. Decide how to unstick this: DECOMPOSE into \
         focused subtasks, RESCOPE/clarify the acceptance criteria and re-dispatch, \
         or CLOSE if the work is moot/duplicate/already-done.",
        loop_guard_kind_label(kind),
        session_id,
        offending_signature,
        threshold,
        observed,
        turn_span.0,
        turn_span.1,
    )
}

fn report_to_terminal_status(report: &TaskRunReport) -> TaskRunStatus {
    match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        // The worker stage succeeded and handed off to verification — the
        // task-run completed cleanly (the verification pipeline runs as a
        // separate slot-free job on the host).
        | TaskRunOutcome::WorkerSubmitted
        | TaskRunOutcome::Parked { .. }
        | TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
        TaskRunOutcome::Failed { .. } | TaskRunOutcome::LoopGuardTripped { .. } => {
            TaskRunStatus::Failed
        }
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
    session_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskRunReport>> {
    loop {
        tokio::select! {
            biased;
            _ = kill.cancelled() => {
                let rpc_span = supervisor_rpc_span("kill", session_id, task_id);
                async move {
                    tracing::debug!(
                        op = "kill",
                        session_id = %session_id,
                        task_id = %task_id,
                        "supervisor dispatch: kill fired while awaiting terminal report; \
                         proceeding to teardown"
                    );
                }
                .instrument(rpc_span)
                .await;
                return Ok(None);
            }
            frame = stream.events_rx.recv() => {
                match frame {
                    Some(StreamEvent::Report(report)) => {
                        let rpc_span = supervisor_rpc_span("terminal_report", session_id, task_id);
                        let report_task_run_id = report.task_run_id.clone();
                        async move {
                            tracing::info!(
                                event = "supervisor.rpc.terminal_report",
                                op = "terminal_report",
                                session_id = %session_id,
                                task_id = %task_id,
                                task_run_id = %report_task_run_id,
                            );
                        }
                        .instrument(rpc_span)
                        .await;
                        return Ok(Some(report));
                    }
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
    use std::collections::HashMap;
    use std::sync::{Arc as StdArc, Mutex as StdMutex, OnceLock};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{Layer, registry::LookupSpan};

    #[derive(Clone, Debug, Default)]
    struct RecordedSpan {
        name: String,
        fields: HashMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct RecordingLayer {
        spans: StdArc<StdMutex<Vec<RecordedSpan>>>,
    }

    impl RecordingLayer {
        fn spans(&self) -> Vec<RecordedSpan> {
            self.spans.lock().expect("recorded spans mutex").clone()
        }
    }

    #[derive(Default)]
    struct FieldRecorder {
        fields: HashMap<String, String>,
    }

    impl Visit for FieldRecorder {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields.insert(
                field.name().to_owned(),
                format!("{value:?}").trim_matches('\"').to_owned(),
            );
        }
    }

    impl<S> Layer<S> for RecordingLayer
    where
        S: tracing::Subscriber,
        S: for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            ctx: Context<'_, S>,
        ) {
            let mut recorder = FieldRecorder::default();
            attrs.record(&mut recorder);
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(RecordedSpan {
                    name: attrs.metadata().name().to_owned(),
                    fields: recorder.fields,
                });
            }
        }

        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            ctx: Context<'_, S>,
        ) {
            if let Some(span) = ctx.span(id) {
                let mut recorder = FieldRecorder::default();
                values.record(&mut recorder);
                if let Some(recorded) = span.extensions_mut().get_mut::<RecordedSpan>() {
                    recorded.fields.extend(recorder.fields);
                }
            }
        }

        fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
            if let Some(span) = ctx.span(&id)
                && let Some(recorded) = span.extensions().get::<RecordedSpan>()
            {
                self.spans
                    .lock()
                    .expect("recorded spans mutex")
                    .push(recorded.clone());
            }
        }
    }

    async fn tracing_lock() -> tokio::sync::OwnedMutexGuard<()> {
        static LOCK: OnceLock<StdArc<tokio::sync::Mutex<()>>> = OnceLock::new();
        LOCK.get_or_init(|| StdArc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await
    }

    fn report(id: &str, stages: Vec<RoleKind>, outcome: TaskRunOutcome) -> TaskRunReport {
        TaskRunReport {
            task_run_id: id.to_string(),
            outcome,
            stages_completed: stages,
            verification_run_id: None,
        }
    }

    #[test]
    fn planned_terminal_outcomes_have_no_provider_breaker_signal() {
        let guard_report = report(
            "guard-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::LoopGuardTripped {
                kind: LoopGuardKind::IdenticalToolFailure,
                offending_signature: "shell:cargo-test".into(),
                threshold: 3,
                observed: 4,
                turn_span: (7, 12),
                session_id: "session-123".into(),
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&guard_report),
            None,
            "loop-guard trips must not feed the provider breaker"
        );

        let ordinary_completed = report(
            "closed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        assert!(
            terminal_report_feeds_model_success(&ordinary_completed),
            "ordinary completed runs still record model-health success"
        );

        for (outcome, label) in [
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: false,
                    session_id: "session-budget-summary".into(),
                    tokens_in: 4_096,
                    tokens_out: 512,
                },
                "successful-summary budget parks",
            ),
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: true,
                    session_id: "session-budget-ignored".into(),
                    tokens_in: 4_096,
                    tokens_out: 12,
                },
                "ignored-wind-down budget parks",
            ),
        ] {
            let budget_report = report("budget-run", vec![RoleKind::Worker], outcome);
            assert_eq!(
                report_to_terminal_status(&budget_report),
                TaskRunStatus::Completed,
                "{label} are planned completed lifecycle endings"
            );
            assert_eq!(
                provider_failure_class_for_report(&budget_report),
                None,
                "{label} must not feed provider breaker failure accounting"
            );
            assert!(
                !terminal_report_feeds_model_success(&budget_report),
                "{label} must not feed model-health success accounting either"
            );
        }

        let failed_report = report(
            "failed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Failed {
                stage: "worker".into(),
                reason: "provider rejected request".into(),
                provider_failure: Some(ProviderFailureClass::Failure),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&failed_report),
            Some(ProviderFailureClass::Failure),
            "typed provider failures still feed the breaker"
        );
    }

    #[test]
    fn loop_guard_reason_names_full_trip_payload() {
        let reason = loop_guard_planner_intervention_reason(
            LoopGuardKind::IdenticalToolFailure,
            "shell:cargo-test",
            3,
            4,
            (7, 12),
            "session-123",
        );

        for expected in [
            "identical_tool_failure",
            "shell:cargo-test",
            "threshold=3",
            "observed=4",
            "turn_span=7..=12",
            "session-123",
            "do not re-dispatch the identical worker attempt",
        ] {
            assert!(
                reason.contains(expected),
                "reason must include `{expected}`; got {reason}"
            );
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

    // ── Stage-aware resume decision ───────────────────────────────────────────

    #[test]
    fn resume_flow_upgrades_review_response_to_reviewer_only_when_durable() {
        // The regression target: a reviewer-stage pod kill leaves the task at
        // needs_task_review (worker already submitted), which routes to
        // ReviewResponse (worker→reviewer). With the worker's commits durable on
        // the mirror task_branch we must resume at the reviewer, NOT redo the
        // worker.
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, true),
            SupervisorFlow::ReviewResume
        );
        assert_eq!(
            SupervisorFlow::ReviewResume.role_sequence(),
            &[djinn_runtime::RoleKind::Reviewer],
            "ReviewResume must skip the worker stage"
        );
    }

    #[test]
    fn resume_flow_keeps_review_response_when_output_not_durable() {
        // No durable worker output (task_branch missing / not ahead of base, or
        // no mirror) → fall back to the full worker redo. Safety guard.
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, false),
            SupervisorFlow::ReviewResponse
        );
    }

    #[test]
    fn resume_flow_leaves_non_review_response_flows_untouched() {
        // Only ReviewResponse is a resume candidate. Durability is irrelevant for
        // every other flow — they must pass through unchanged regardless.
        for flow in [
            SupervisorFlow::NewTask,
            SupervisorFlow::ConflictRetry,
            SupervisorFlow::Spike,
            SupervisorFlow::Planning,
            SupervisorFlow::Lead,
        ] {
            assert_eq!(resume_flow(flow, true), flow);
            assert_eq!(resume_flow(flow, false), flow);
        }
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

        let got = await_report_from_stream(bistream, &kill, "session-id-b", "task-id-b")
            .await
            .expect("await ok");
        assert_eq!(got.expect("some report").task_run_id, "id-B");
    }

    #[tokio::test]
    async fn supervisor_rpc_terminal_report_span_records_fields() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::Report(report(
                "run-terminal",
                vec![RoleKind::Worker],
                TaskRunOutcome::Interrupted,
            )))
            .await
            .unwrap();

        let got = await_report_from_stream(bistream, &kill, "session-terminal", "task-terminal")
            .await
            .expect("await ok");

        assert!(got.is_some());
        let span = layer
            .spans()
            .into_iter()
            .find(|span| span.name == "djinn.supervisor.rpc")
            .expect("djinn.supervisor.rpc span recorded");
        assert_eq!(
            span.fields.get("op").map(String::as_str),
            Some("terminal_report")
        );
        assert_eq!(
            span.fields.get("session_id").map(String::as_str),
            Some("session-terminal")
        );
        assert_eq!(
            span.fields.get("task_id").map(String::as_str),
            Some("task-terminal")
        );
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

        let got = await_report_from_stream(bistream, &kill, "session-id-b", "task-id-b")
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
        let got = await_report_from_stream(bistream, &kill, "session-kill", "task-kill")
            .await
            .expect("await ok");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn supervisor_rpc_kill_span_records_fields() {
        let _tracing_guard = tracing_lock().await;
        let layer = RecordingLayer::default();
        let subscriber = tracing_subscriber::registry().with(layer.clone());
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        kill.cancel();

        let got = await_report_from_stream(bistream, &kill, "session-kill", "task-kill")
            .await
            .expect("await ok");

        assert!(got.is_none());
        let span = layer
            .spans()
            .into_iter()
            .find(|span| span.name == "djinn.supervisor.rpc")
            .expect("djinn.supervisor.rpc kill span recorded");
        assert_eq!(span.fields.get("op").map(String::as_str), Some("kill"));
        assert_eq!(
            span.fields.get("session_id").map(String::as_str),
            Some("session-kill")
        );
        assert_eq!(
            span.fields.get("task_id").map(String::as_str),
            Some("task-kill")
        );
    }

    #[tokio::test]
    async fn await_report_returns_none_when_channel_closes_without_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        drop(events_tx);
        let got = await_report_from_stream(bistream, &kill, "session-closed", "task-closed")
            .await
            .expect("await ok");
        assert!(got.is_none());
    }
}
