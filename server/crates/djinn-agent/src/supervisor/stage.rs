//! Per-stage execution driver for [`crate::supervisor::TaskRunSupervisor`].
//!
//! A *stage* is one role's session inside a supervisor-driven task-run: the
//! supervisor walks the flow's `role_sequence()` and invokes [`execute_stage`]
//! for each role against the shared [`Workspace`].
//!
//! ## Scope
//!
//! This is the Phase 1 minimum — it wires the extracted lifecycle helpers
//! ([`model_resolution`], [`setup`], [`mcp_resolve`]) into the reply loop so a
//! single role stage can run end-to-end against a mirror-born ephemeral
//! workspace, then map the reply-loop outcome onto [`StageOutcome`].  The full
//! 340+ line prompt-context assembly from `run_task_lifecycle` is NOT
//! replicated here; instead we build a **degenerate** [`TaskContext`] with only
//! the fields the base prompt template actually reads for a single-stage
//! worker run, and punt anything flow-specific (conflict context, activity
//! digest, epic context, planner patrol context, knowledge notes…) to future
//! tasks once the full supervisor-driven coordinator rewrite lands.
//!
//! The old coordinator dispatch path (`run_task_lifecycle`) remains fully
//! active and is the only path production traffic travels today.  This module
//! is additive: only callers that explicitly opt into the supervisor see any
//! change.

// TODO(phase-1): prompt-context building
//   `run_task_lifecycle` assembles ~340 lines of role-specific context —
//   conflict metadata, activity log digest, epic context, knowledge notes,
//   planner patrol context, worker resume context.  Until the coordinator
//   rewrite (task #7) moves the full dispatch path onto the supervisor, this
//   stage builds a degenerate `TaskContext` that covers the prompt fields
//   the base template references for a Worker session.  See `build_prompt`
//   below.
//
// TODO(phase-1): paused-session resume
//   `run_task_lifecycle` resumes a paused Worker session (reuses worktree,
//   reloads conversation, compacts before appending reviewer feedback).  The
//   supervisor always starts a fresh session today — acceptable because the
//   supervisor-driven flow owns the whole run and there is no external
//   "pause, release slot, redispatch later" flow to resume across.
//
// TODO(phase-1): session teardown / post-session dispatch
//   `run_task_lifecycle` runs `spawn_post_session_work` and
//   `apply_transition_and_dispatch` so the coordinator knows to reopen /
//   close / escalate the task.  The supervisor owns the whole task-run in
//   one call, so it will synthesize the next stage itself once the
//   cross-stage routing is implemented.  This function only touches the
//   per-stage session-record status; broader task/flow transitions are the
//   caller's job.

use std::sync::Arc;

use djinn_core::models::{SessionStatus, Task};
use djinn_db::SessionRepository;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_workspace::Workspace;

use crate::actors::slot::helpers::ProviderCredential;
use crate::actors::slot::helpers::{
    auth_method_for_provider, build_telemetry_meta, capabilities_for_provider, default_base_url,
    format_family_for_provider,
};
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::model_resolution::{
    ModelResolutionError, resolve_model_and_credential,
};
use crate::actors::slot::lifecycle::setup::{
    SetupAndVerificationContext, SetupError, resolve_setup_and_verification_context,
};
use crate::actors::slot::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::message::{Conversation, Message};
use crate::prompts::TaskContext;
use crate::provider::{LlmProvider, ProviderConfig, create_provider};
use crate::roles::{AgentRole, role_impl_for};
use crate::AgentType;

use super::SupervisorServices;
use super::flow::RoleKind;
use super::spec::TaskRunSpec;

/// Outcome of executing one role stage.  Mapped by [`execute_stage`] from the
/// reply-loop result + finalize payload, then consumed by the supervisor to
/// decide the next stage (or to terminate the task-run).
#[derive(Clone, Debug)]
pub enum StageOutcome {
    /// Worker completed successfully.  The supervisor decides whether the
    /// next stage (reviewer / verifier / PR open) follows based on the flow.
    WorkerDone,
    /// Planner called `submit_grooming` with `decision=execute` — the plan
    /// is ready to hand off to a worker.
    PlannerExecute,
    /// Planner closed the task without execution (e.g. already done, not
    /// actionable).
    PlannerClose { reason: String },
    /// Reviewer approved the worker's submission.
    ReviewerApproved,
    /// Reviewer rejected the worker's submission; feedback travels into the
    /// next worker-resume cycle.
    ReviewerRejected { feedback: String },
    /// Verification suite passed.
    VerifierPassed,
    /// Verification suite failed.
    VerifierFailed { reason: String },
    /// Architect spike completed.
    ArchitectDone,
    /// Any role that surfaced an ambiguity / open question blocking automated
    /// progress (e.g. planner `request_lead`).  Terminal for the task-run.
    Escalate { reason: String },
    /// Stage failed (reply loop error, provider error, session creation
    /// failure, unexpected finalize tool, etc.).  Terminal for the task-run.
    Failed { reason: String },
}

impl StageOutcome {
    /// Whether this outcome should short-circuit the role sequence (the
    /// supervisor stops stepping through further roles).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StageOutcome::PlannerClose { .. }
                | StageOutcome::Escalate { .. }
                | StageOutcome::Failed { .. }
                | StageOutcome::ReviewerRejected { .. }
                | StageOutcome::VerifierFailed { .. }
        )
    }
}

/// Failure that prevented [`execute_stage`] from reaching the reply loop —
/// always fatal for the whole task-run.
#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("model resolution: {0}")]
    ModelResolution(String),

    #[error("setup/verification: {0}")]
    Setup(String),

    #[error("session create: {0}")]
    SessionCreate(String),
}

/// Execute one role stage against the shared workspace.
///
/// Resolves the role → model credential → project setup/verification config →
/// MCP + skills → creates a fresh session record linked to `task_run_id` →
/// builds a degenerate prompt → invokes the reply loop → finalizes the
/// session record → maps the result to [`StageOutcome`].
///
/// See module-level TODOs for what's intentionally not yet wired (prompt
/// enrichment, paused-session resume, post-session transitions).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_stage(
    task: &Task,
    workspace: &Workspace,
    role_kind: RoleKind,
    task_run_id: &str,
    spec: &TaskRunSpec,
    services: &SupervisorServices,
) -> Result<StageOutcome, StageError> {
    let role = role_arc_for(role_kind);
    let role_name = role.config().name;
    let worktree_path = workspace.path();

    tracing::info!(
        task_id = %task.short_id,
        task_run_id = %task_run_id,
        role = %role_name,
        workspace = %worktree_path.display(),
        "Supervisor stage: starting"
    );

    // Resolve the default model for this role from the role→model map.
    // Phase 1 fallback: pick the first model registered for this role's
    // dispatch role; if none is registered, bail with a StageError.  The
    // production coordinator resolves this from the slot pool (ADR roadmap
    // task #7) — until then we use the role's preferred model.
    let model_id = match default_model_for_role(role_name, services) {
        Some(m) => m,
        None => {
            return Err(StageError::ModelResolution(format!(
                "no model registered for role '{role_name}' in the provider catalog"
            )));
        }
    };

    // ── Model + credential ───────────────────────────────────────────────────
    let resolved = match resolve_model_and_credential(
        &model_id,
        &task.id,
        &services.agent_context,
    )
    .await
    {
        Ok(r) => r,
        Err(ModelResolutionError { reason }) => {
            return Err(StageError::ModelResolution(reason));
        }
    };

    // ── MCP + skills ─────────────────────────────────────────────────────────
    // The supervisor always starts a fresh session, so role-level overrides
    // from the agents DB table are skipped here — a default-role session
    // receives project-level MCP + skills only.  (Specialist overrides land
    // when the supervisor learns to consume `task.agent_type`; until then
    // the old dispatch path retains that behaviour.)
    let McpAndSkills {
        settings,
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
    } = resolve_mcp_and_skills(
        worktree_path,
        role.as_ref(),
        &task.short_id,
        None, // role_mcp_servers — specialist path not wired yet
        &[],  // role_skills — specialist path not wired yet
        #[cfg(test)]
        None,
        &services.agent_context,
    )
    .await;

    // ── Setup commands + verification context ────────────────────────────────
    let SetupAndVerificationContext {
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    } = match resolve_setup_and_verification_context(
        settings,
        None, // role_verification_command — specialist path not wired yet
        worktree_path,
        &task.id,
        &task.short_id,
        &services.agent_context,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(SetupError { reason }) => {
            return Err(StageError::Setup(reason));
        }
    };

    // ── Build degenerate prompt context ──────────────────────────────────────
    // TODO(phase-1): enrich with conflict / activity / epic / knowledge /
    // patrol context once the supervisor-driven coordinator rewrite picks
    // them up from the new event stream.  See module-level TODO.
    let task_ctx = TaskContext {
        project_path: worktree_path.display().to_string(),
        workspace_path: worktree_path.display().to_string(),
        diff: None,
        commits: None,
        start_commit: None,
        end_commit: None,
        conflict_files: None,
        merge_base_branch: None,
        merge_target_branch: None,
        merge_failure_context: None,
        setup_commands: prompt_setup_commands,
        verification_commands: prompt_verification_commands,
        verification_rules: prompt_verification_rules,
        activity: None,
        worker_summary: None,
        worker_concerns: None,
        verification_failure: None,
        epic_context: None,
        knowledge_context: None,
        planner_patrol_context: None,
    };

    let base_system_prompt = role.render_prompt(task, &task_ctx);
    // No role extensions / learned prompts in the minimal path — the DB role
    // override wiring is a future task.
    let system_prompt = crate::prompts::apply_skills(&base_system_prompt, &resolved_skills);

    // ── Create the session record linked to the task-run ─────────────────────
    let session_repo =
        SessionRepository::new(services.agent_context.db.clone(), services.agent_context.event_bus.clone());
    let session_record = match session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: &model_id,
            agent_type: role_name,
            worktree_path: worktree_path.to_str(),
            metadata_json: None,
            task_run_id: Some(task_run_id),
        })
        .await
    {
        Ok(r) => r,
        Err(e) => return Err(StageError::SessionCreate(e.to_string())),
    };
    let session_id = session_record.id.clone();

    // ── Build the LLM provider ───────────────────────────────────────────────
    let context_window = services
        .agent_context
        .catalog
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);
    let telemetry_meta = build_telemetry_meta(role_name, &task.id);
    let provider: Box<dyn LlmProvider> = match resolved.provider_credential {
        Some(ProviderCredential::OAuthConfig(mut cfg)) => {
            cfg.model_id = resolved.model_name.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
            cfg.session_affinity_key = Some(session_id.clone());
            create_provider(*cfg)
        }
        Some(ProviderCredential::ApiKey(_key_name, api_key)) => {
            let format_family =
                format_family_for_provider(&resolved.catalog_provider_id, &resolved.model_name);
            let base_url = services
                .agent_context
                .catalog
                .list_providers()
                .iter()
                .find(|p| p.id == resolved.catalog_provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&resolved.catalog_provider_id));
            create_provider(ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&resolved.catalog_provider_id, &api_key),
                format_family,
                model_id: resolved.model_name.clone(),
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                session_affinity_key: Some(session_id.clone()),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&resolved.catalog_provider_id),
            })
        }
        None => {
            let _ = session_repo
                .update(&session_id, SessionStatus::Failed, 0, 0)
                .await;
            return Err(StageError::ModelResolution(
                "no provider credential resolved for model".into(),
            ));
        }
    };

    // ── Build the initial conversation ───────────────────────────────────────
    let mut tools = (role.config().tool_schemas)();
    if let Some(ref registry) = mcp_registry {
        tools.extend_from_slice(registry.tool_schemas());
    }

    let mut conversation = Conversation::new();
    conversation.push(Message::system(system_prompt));
    let initial_user_message = role
        .initial_user_message(&task.id, &services.agent_context)
        .await;
    conversation.push(Message::user(initial_user_message));

    // ── Run the reply loop ───────────────────────────────────────────────────
    let (reply_result, final_output, tokens_in, tokens_out) = run_reply_loop(
        ReplyLoopContext {
            provider: provider.as_ref(),
            tools: &tools,
            task_id: &task.id,
            task_short_id: &task.short_id,
            session_id: &session_id,
            project_path: &worktree_path.display().to_string(),
            worktree_path,
            role_name,
            finalize_tool_names: role.config().finalize_tool_names,
            context_window,
            model_id: &model_id,
            cancel: &services.cancel,
            global_cancel: &services.cancel,
            app_state: &services.agent_context,
            mcp_registry: mcp_registry.as_ref(),
            active_skill_names: &effective_skills,
            active_mcp_server_names: &effective_mcp_servers,
        },
        &mut conversation,
        false, // is_resumed_session — supervisor always starts fresh (see TODO)
    )
    .await;

    // ── Finalize session ─────────────────────────────────────────────────────
    let session_status = if reply_result.is_ok() {
        SessionStatus::Completed
    } else {
        SessionStatus::Failed
    };
    if let Err(e) = session_repo
        .update(&session_id, session_status, tokens_in, tokens_out)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "Supervisor stage: failed to update session record"
        );
    }

    // Drop `spec` reference once we no longer need it — avoids the unused-arg
    // lint when Phase 1 doesn't dispatch based on spec.trigger yet.
    let _ = spec;

    // ── Map the reply-loop outcome to StageOutcome ───────────────────────────
    match reply_result {
        Err(e) => Ok(StageOutcome::Failed {
            reason: format!("reply loop error: {e}"),
        }),
        Ok(()) => {
            let finalize_name = final_output.finalize_tool_name.as_deref().unwrap_or("");
            let outcome = match role_kind {
                RoleKind::Worker => match finalize_name {
                    "submit_work" => StageOutcome::WorkerDone,
                    "request_lead" => StageOutcome::Escalate {
                        reason: extract_reason(&final_output.finalize_payload)
                            .unwrap_or_else(|| "worker requested lead escalation".into()),
                    },
                    "" => StageOutcome::WorkerDone,
                    other => StageOutcome::Failed {
                        reason: format!("worker finalized via unexpected tool '{other}'"),
                    },
                },
                RoleKind::Planner => match finalize_name {
                    "submit_grooming" => {
                        let decision = final_output
                            .finalize_payload
                            .as_ref()
                            .and_then(|p| p.get("decision"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match decision {
                            "execute" => StageOutcome::PlannerExecute,
                            "close" => StageOutcome::PlannerClose {
                                reason: extract_reason(&final_output.finalize_payload)
                                    .unwrap_or_else(|| "planner closed task".into()),
                            },
                            "escalate" => StageOutcome::Escalate {
                                reason: extract_reason(&final_output.finalize_payload)
                                    .unwrap_or_else(|| "planner escalated".into()),
                            },
                            other => StageOutcome::Failed {
                                reason: format!(
                                    "planner submitted unknown decision '{other}'"
                                ),
                            },
                        }
                    }
                    other => StageOutcome::Failed {
                        reason: format!("planner finalized via unexpected tool '{other}'"),
                    },
                },
                RoleKind::Reviewer => match finalize_name {
                    "submit_review" => {
                        let verdict = final_output
                            .finalize_payload
                            .as_ref()
                            .and_then(|p| p.get("verdict"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        match verdict {
                            "approve" => StageOutcome::ReviewerApproved,
                            "reject" => StageOutcome::ReviewerRejected {
                                feedback: final_output
                                    .finalize_payload
                                    .as_ref()
                                    .and_then(|p| p.get("feedback"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            },
                            other => StageOutcome::Failed {
                                reason: format!(
                                    "reviewer submitted unknown verdict '{other}'"
                                ),
                            },
                        }
                    }
                    "request_lead" => StageOutcome::Escalate {
                        reason: extract_reason(&final_output.finalize_payload)
                            .unwrap_or_else(|| "reviewer escalated to lead".into()),
                    },
                    other => StageOutcome::Failed {
                        reason: format!("reviewer finalized via unexpected tool '{other}'"),
                    },
                },
                // TODO(phase-1): the supervisor doesn't yet drive a dedicated
                // verifier stage — verification runs as a post-session job in
                // the old lifecycle.  Treat any "verifier" invocation here as
                // unimplemented.
                RoleKind::Verifier => StageOutcome::Failed {
                    reason: "verifier stage not yet wired in supervisor".into(),
                },
                RoleKind::Architect => match finalize_name {
                    "submit_work" => StageOutcome::ArchitectDone,
                    other => StageOutcome::Failed {
                        reason: format!(
                            "architect finalized via unexpected tool '{other}'"
                        ),
                    },
                },
            };
            Ok(outcome)
        }
    }
}

/// Map a [`RoleKind`] (flow enum) to a concrete `Arc<dyn AgentRole>`.
fn role_arc_for(kind: RoleKind) -> Arc<dyn AgentRole> {
    match kind {
        RoleKind::Planner => role_impl_for(AgentType::Planner),
        RoleKind::Worker => role_impl_for(AgentType::Worker),
        RoleKind::Reviewer => role_impl_for(AgentType::Reviewer),
        // There is no VerifierRole today — verification runs as a
        // post-session job outside any agent session.  Phase 1 maps this to
        // Worker (harmless because execute_stage short-circuits Verifier to
        // StageOutcome::Failed before invoking the role).
        RoleKind::Verifier => role_impl_for(AgentType::Worker),
        RoleKind::Architect => role_impl_for(AgentType::Architect),
    }
}

/// Pull a "reason"-shaped string out of a finalize payload (looks for common
/// field names: `reason`, `message`, `summary`).
fn extract_reason(payload: &Option<serde_json::Value>) -> Option<String> {
    let p = payload.as_ref()?;
    for key in ["reason", "message", "summary"] {
        if let Some(v) = p.get(key).and_then(|v| v.as_str())
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Phase 1 model lookup: pick any model from the catalog.
///
/// TODO(phase-1): consult the slot pool's `role→model` map (see
/// `ModelSlotConfig.roles` in `actors::slot::pool`) or the per-project
/// role_preferred_model setting.  Until the coordinator rewrite (task #7)
/// hands a resolved model into the supervisor directly, we grab whatever
/// model the catalog has available for *any* provider — sufficient to
/// compile + run an integration-level smoke test against a real provider.
fn default_model_for_role(_role_name: &str, services: &SupervisorServices) -> Option<String> {
    let catalog = &services.agent_context.catalog;
    for provider in catalog.list_providers() {
        if let Some(model) = catalog.list_models(&provider.id).first() {
            return Some(format!("{}/{}", provider.id, model.id));
        }
    }
    None
}
