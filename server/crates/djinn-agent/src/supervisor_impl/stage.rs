//! Per-stage execution driver invoked by [`crate::supervisor::TaskRunSupervisor`].
//!
//! The supervisor orchestration itself lives in `djinn-supervisor`; this file
//! stays in `djinn-agent` because `execute_stage` reaches deeply into
//! `AgentContext`, the role registry, the lifecycle helpers
//! (`model_resolution`, `setup`, `mcp_resolve`, `prompt_context`,
//! `teardown`), the MCP + provider + reply-loop plumbing, and `task_merge`.
//!
//! Phase 2 PR 2 (extraction): the supervisor body in `djinn-supervisor`
//! invokes this function through an injected closure stored on
//! `SupervisorServices::execute_stage_fn`; the closure is bound by
//! `actors::slot::supervisor_runner::run_supervisor_dispatch`. Moving this
//! body out of `djinn-agent` is deferred to PR 3 (or a follow-up
//! `djinn-lifecycle` extraction) when we convert `SupervisorServices` into a
//! trait.
//!
//! A *stage* is one role's session inside a supervisor-driven task-run: the
//! supervisor walks the flow's `role_sequence()` and invokes this fn for each
//! role against the shared [`Workspace`].
//!
//! ## Scope
//!
//! Wires the extracted lifecycle helpers ([`model_resolution`], [`setup`],
//! [`mcp_resolve`], [`prompt_context`], [`role_overrides`]) into the reply
//! loop so a single role stage can run end-to-end against a mirror-born
//! ephemeral workspace, then maps the reply-loop outcome onto
//! [`StageOutcome`] (re-exported from `djinn-supervisor`).
//!
//! ## Deferred: worker-resume
//!
//! The legacy `run_task_lifecycle` re-attached a paused session's
//! conversation and carried forward the prior reply-loop state when a
//! worker slot resumed a previously-paused task.  The supervisor path does
//! **not** implement that yet — every stage starts a fresh session record
//! with a freshly-built conversation.  Plumbing this through would require
//! reviving three dead-code deletions from commit 6bf5d5931:
//!
//! - `slot::helpers::find_paused_session_record` — scans
//!   `SessionStatus::Paused` rows for `(task_id, role, model_id)` matches.
//! - `slot::helpers::resume_context_for_task` — builds the resume-prompt
//!   preamble (activity log, rejection reasons, conflict context) the
//!   resuming worker sees instead of a fresh `initial_user_message`.
//! - `compaction::CompactionContext::PreResume` — compacts the restored
//!   conversation before the resumed session enters the reply loop.
//!
//! And even past those, the supervisor flow has no place to *write* a
//! paused-session row today: stages end as `Completed` or `Failed` and
//! tear down at once.  A mid-stage pause seam plus its cross-run
//! conversation serialisation (the `conversation_store.rs` file deleted in
//! commit 110385b07) is a cross-crate change — `djinn-db` would need a
//! stable serialized conversation column, and the supervisor/runtime
//! contract would need to surface a "pause this run and let the next
//! dispatch resume it" signal.  Out of scope for the Phase 1 holdover
//! cleanup.

use std::sync::Arc;

use djinn_core::models::{SessionStatus, Task};
use djinn_db::SessionRepository;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_runtime::spec::{RoleKind, TaskRunSpec};
use djinn_supervisor::{StageError, StageOutcome};
use djinn_workspace::Workspace;

use crate::AgentType;
use crate::actors::slot::helpers::ProviderCredential;
use crate::actors::slot::helpers::{
    auth_method_for_provider, build_telemetry_meta, capabilities_for_provider, default_base_url,
    format_family_for_provider,
};
use crate::actors::slot::helpers::conflict_context_for_dispatch;
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::model_resolution::{
    ModelResolutionError, resolve_model_and_credential,
};
use crate::actors::slot::lifecycle::prompt_context::{
    PromptContext, PromptContextInputs, build_prompt_context,
};
use crate::actors::slot::lifecycle::role_overrides::{ResolvedRoleOverrides, resolve_role_overrides};
use crate::actors::slot::lifecycle::setup::{
    SetupAndVerificationContext, SetupError, resolve_setup_and_verification_context,
};
use crate::actors::slot::lifecycle::teardown::{PostSessionParams, spawn_post_session_work};
use crate::actors::slot::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::context::AgentContext;
use crate::message::{Conversation, Message};
use crate::provider::{LlmProvider, ProviderConfig, create_provider};
use crate::roles::{AgentRole, role_impl_for};

use super::SupervisorCallbackContext;

/// Execute one role stage against the shared workspace.
///
/// Resolves the role → model credential → project setup/verification config →
/// MCP + skills → creates a fresh session record linked to `task_run_id` →
/// builds a degenerate prompt → invokes the reply loop → finalizes the
/// session record → maps the result to [`StageOutcome`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_stage(
    task: &Task,
    workspace: &Workspace,
    role_kind: RoleKind,
    task_run_id: &str,
    spec: &TaskRunSpec,
    callbacks: &SupervisorCallbackContext,
) -> Result<StageOutcome, StageError> {
    let role = role_arc_for(role_kind);
    let role_name = role.config().name;
    let worktree_path = workspace.path();
    let agent_context: &AgentContext = &callbacks.agent_context;
    let provider_override = callbacks.provider_override.clone();

    // ── Role-level overrides: specialist (Worker stage) or project default ────
    // Picks up `system_prompt_extensions`, `learned_prompt`, role-level MCP
    // server + skill lists, `verification_command`, and swaps `runtime_role`
    // when a Worker stage's `task.agent_type` names a specialist whose
    // `base_role` differs from the injected RoleKind.  Non-Worker stages
    // always use the default-role path.
    let ResolvedRoleOverrides {
        runtime_role,
        system_prompt_extensions,
        learned_prompt,
        mcp_servers: role_mcp_servers,
        skills: role_skills,
        verification_command: role_verification_command,
        model_preference: _role_model_preference,
        specialist_overrode_runtime_role,
    } = resolve_role_overrides(task, role_kind, agent_context).await;

    // ── Conflict-retry context ────────────────────────────────────────────────
    // Populated when a prior task-run aborted with merge conflicts; drives
    // the `TaskContext::conflict_files` + `merge_*_branch` prompt fields the
    // worker template uses to steer a conflict-resolution session.
    //
    // `merge_validation_ctx` is deliberately left `None`: the legacy
    // `merge_validation_context_for_dispatch` helper + `MergeValidationFailureMetadata`
    // prompt renderer were deleted in commit 6bf5d5931 as dead code during
    // the Phase 1 cut-over.  Resurrecting the merge-validation prompt path
    // is a separate, out-of-scope change — not a supervisor-path gap.
    let conflict_ctx = conflict_context_for_dispatch(&task.id, agent_context).await;

    tracing::info!(
        task_id = %task.short_id,
        task_run_id = %task_run_id,
        role = %role_name,
        runtime_role = %runtime_role.config().name,
        specialist_overrode_runtime_role,
        has_conflict_context = conflict_ctx.is_some(),
        workspace = %worktree_path.display(),
        "Supervisor stage: starting"
    );

    // Resolve the model for this stage.  Preference order:
    //   1. Per-role override threaded in via `TaskRunSpec::model_id_per_role`.
    //   2. Catalog-default fallback.
    //   3. When a `provider_override` is present (integration tests), fall
    //      back to a synthetic identifier so the session record is still
    //      well-formed.
    let model_id = match spec.model_id_per_role.get(&role_kind).cloned() {
        Some(m) => m,
        None => match default_model_for_role(role_name, agent_context) {
            Some(m) => m,
            None if provider_override.is_some() => "test/supervisor-stub".to_string(),
            None => {
                return Err(StageError::ModelResolution(format!(
                    "no model registered for role '{role_name}' in the provider catalog"
                )));
            }
        },
    };

    // ── Model + credential ───────────────────────────────────────────────────
    let resolved = if provider_override.is_some() {
        None
    } else {
        match resolve_model_and_credential(&model_id, &task.id, agent_context).await {
            Ok(r) => Some(r),
            Err(ModelResolutionError { reason }) => {
                return Err(StageError::ModelResolution(reason));
            }
        }
    };

    // ── MCP + skills ─────────────────────────────────────────────────────────
    // `runtime_role` drives resolution so specialists can override the base
    // role's MCP/skill defaults.  `role_mcp_servers` carries the DB row's
    // parsed array (or `None` when no DB row exists).
    let McpAndSkills {
        settings,
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
    } = resolve_mcp_and_skills(
        worktree_path,
        runtime_role.as_ref(),
        &task.short_id,
        role_mcp_servers.as_deref(),
        &role_skills,
        #[cfg(test)]
        None,
        agent_context,
    )
    .await;

    // ── Setup commands + verification context ────────────────────────────────
    let SetupAndVerificationContext {
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    } = match resolve_setup_and_verification_context(
        settings,
        role_verification_command.as_deref(),
        worktree_path,
        &task.id,
        &task.short_id,
        agent_context,
    )
    .await
    {
        Ok(ctx) => ctx,
        Err(SetupError { reason }) => {
            return Err(StageError::Setup(reason));
        }
    };

    // ── Build prompt context ─────────────────────────────────────────────────
    // `runtime_role` renders the template (may be the specialist's base role);
    // `role_for_epic_check` stays the injected base role because the
    // `needs_epic_context` contract is about what the flow-enum role does,
    // not what the specialist's prompt variant says.
    let project_path_str = worktree_path.display().to_string();
    let PromptContext { system_prompt, .. } = build_prompt_context(PromptContextInputs {
        task,
        runtime_role: runtime_role.as_ref(),
        role_for_epic_check: role.as_ref(),
        project_path: &project_path_str,
        worktree_path,
        conflict_ctx: conflict_ctx.as_ref(),
        merge_validation_ctx: None,
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
        system_prompt_extensions: &system_prompt_extensions,
        learned_prompt: learned_prompt.as_deref(),
        resolved_skills: &resolved_skills,
        app_state: agent_context,
    })
    .await;

    // ── Create the session record linked to the task-run ─────────────────────
    let session_repo =
        SessionRepository::new(agent_context.db.clone(), agent_context.event_bus.clone());
    let session_record = match session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: &model_id,
            agent_type: role_name,
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
    let context_window = agent_context
        .catalog
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let provider_arc: Option<Arc<dyn LlmProvider>> = provider_override;
    let provider_owned: Option<Box<dyn LlmProvider>> = if provider_arc.is_some() {
        None
    } else {
        let resolved = resolved
            .expect("resolved model credential must be populated when provider_override is absent");
        let telemetry_meta = build_telemetry_meta(role_name, &task.id);
        let built = match resolved.provider_credential {
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
                let base_url = agent_context
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
        Some(built)
    };
    let provider_ref: &dyn LlmProvider = match (provider_arc.as_deref(), provider_owned.as_deref())
    {
        (Some(p), _) => p,
        (None, Some(p)) => p,
        (None, None) => unreachable!("either provider_override or a built provider is present"),
    };

    // ── Build the initial conversation ───────────────────────────────────────
    let mut tools = (role.config().tool_schemas)();
    if let Some(ref registry) = mcp_registry {
        tools.extend_from_slice(registry.tool_schemas());
    }

    let mut conversation = Conversation::new();
    conversation.push(Message::system(system_prompt));
    let initial_user_message = role.initial_user_message(&task.id, agent_context).await;
    conversation.push(Message::user(initial_user_message));

    // ── Run the reply loop ───────────────────────────────────────────────────
    let (reply_result, final_output, tokens_in, tokens_out) = run_reply_loop(
        ReplyLoopContext {
            provider: provider_ref,
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
            cancel: &callbacks.cancel,
            global_cancel: &callbacks.cancel,
            app_state: agent_context,
            mcp_registry: mcp_registry.as_ref(),
            active_skill_names: &effective_skills,
            active_mcp_server_names: &effective_mcp_servers,
        },
        &mut conversation,
        false,
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

    // ── Map the reply-loop outcome to StageOutcome ───────────────────────────
    let final_result_ok = reply_result.is_ok();
    let final_error = reply_result.as_ref().err().map(|e| e.to_string());
    let stage_outcome = match reply_result {
        Err(e) => StageOutcome::Failed {
            reason: format!("reply loop error: {e}"),
        },
        Ok(()) => {
            let finalize_name = final_output.finalize_tool_name.as_deref().unwrap_or("");
            match role_kind {
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
                                reason: format!("planner submitted unknown decision '{other}'"),
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
                                reason: format!("reviewer submitted unknown verdict '{other}'"),
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
                RoleKind::Verifier => StageOutcome::Failed {
                    reason: "verifier stage not yet wired in supervisor".into(),
                },
                RoleKind::Architect => match finalize_name {
                    "submit_work" => StageOutcome::ArchitectDone,
                    other => StageOutcome::Failed {
                        reason: format!("architect finalized via unexpected tool '{other}'"),
                    },
                },
            }
        }
    };

    // ── Dispatch post-session work ───────────────────────────────────────────
    let project_path =
        crate::task_merge::resolve_project_path_for_id(&task.project_id, agent_context)
            .await
            .unwrap_or_else(|| worktree_path.display().to_string());

    spawn_post_session_work(PostSessionParams {
        task_id: task.id.clone(),
        project_path,
        role: role.clone(),
        app_state: agent_context.clone(),
        final_output,
        final_result_ok,
        final_error,
        tokens_in,
        tokens_out,
    });

    Ok(stage_outcome)
}

/// Map a [`RoleKind`] (flow enum) to a concrete `Arc<dyn AgentRole>`.
fn role_arc_for(kind: RoleKind) -> Arc<dyn AgentRole> {
    match kind {
        RoleKind::Planner => role_impl_for(AgentType::Planner),
        RoleKind::Worker => role_impl_for(AgentType::Worker),
        RoleKind::Reviewer => role_impl_for(AgentType::Reviewer),
        RoleKind::Verifier => role_impl_for(AgentType::Worker),
        RoleKind::Architect => role_impl_for(AgentType::Architect),
    }
}

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
fn default_model_for_role(_role_name: &str, app_state: &AgentContext) -> Option<String> {
    let catalog = &app_state.catalog;
    for provider in catalog.list_providers() {
        if let Some(model) = catalog.list_models(&provider.id).first() {
            return Some(format!("{}/{}", provider.id, model.id));
        }
    }
    None
}
