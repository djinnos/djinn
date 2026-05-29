//! Per-stage execution driver invoked by [`crate::supervisor::TaskRunSupervisor`].
//!
//! The supervisor orchestration itself lives in `djinn-supervisor`; this file
//! stays in `djinn-agent` because `execute_stage` reaches deeply into
//! `AgentContext`, the role registry, the lifecycle helpers
//! (`model_resolution`, `setup`, `mcp_resolve`, `prompt_context`,
//! `teardown`), the MCP + provider + reply-loop plumbing, and `task_merge`.
//!
//! The supervisor body in `djinn-supervisor` invokes this function through
//! an injected closure stored on `SupervisorServices::execute_stage_fn`;
//! the closure is bound by
//! `actors::slot::supervisor_runner::run_supervisor_dispatch`. This
//! indirection is deliberate — it lets `djinn-supervisor` stay free of the
//! lifecycle/MCP/provider crates this module depends on without moving the
//! whole body across a crate boundary.
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
//! ## Non-goal: worker-pause/resume
//!
//! The supervisor dispatch path deliberately does not support pausing a
//! mid-stage session and resuming it on a later dispatch. Every stage
//! starts a fresh session record with a freshly-built conversation; stages
//! end as `Completed` or `Failed` and tear down at once.
//!
//! This is a design choice, not an outstanding task. Pause/resume would
//! need two pieces that don't exist yet and that span crates:
//!
//! 1. A stable serialized-conversation column in `djinn-db` (the old
//!    `conversation_store.rs` was deleted in commit 110385b07), plus the
//!    migrations and invariants to keep it consistent with the session
//!    record.
//! 2. A `SessionRuntime`/supervisor contract extension: a "pause this run
//!    and let the next dispatch resume it" signal, and a place in the
//!    stage flow that can actually write a `SessionStatus::Paused` row.
//!
//! Until that design lands, the feature is intentionally off the table.
//! If it ever revives, the three helpers that would come back are named
//! here so the archeology is easy:
//!
//! - `slot::helpers::find_paused_session_record` — would scan
//!   `SessionStatus::Paused` rows for `(task_id, role, model_id)` matches.
//! - `slot::helpers::resume_context_for_task` — would build the
//!   resume-prompt preamble (activity log, rejection reasons, conflict
//!   context) the resuming worker sees instead of a fresh
//!   `initial_user_message`.
//! - `compaction::CompactionContext::PreResume` — would compact the
//!   restored conversation before the resumed session enters the reply
//!   loop.
//!
//! All three were deleted as dead code in commit 6bf5d5931; this note
//! records the design, not a promise to revive them.

use std::sync::Arc;

use djinn_core::models::{SessionStatus, Task};
use djinn_runtime::spec::{RoleKind, TaskRunSpec};
use djinn_supervisor::{StageError, StageOutcome, SupervisorServices};
use djinn_workspace::Workspace;
use djinn_db::ProjectRepository;
use djinn_git::run_git_command;

use crate::AgentType;
use crate::actors::slot::helpers::{
    build_provider_from_resolved, build_telemetry_meta, default_base_url, resolved_needs_base_url,
};
use crate::actors::slot::helpers::conflict_context_for_dispatch;
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::model_resolution::{
    ModelResolutionError, resolve_model_and_credential,
};
use crate::actors::slot::lifecycle::prompt_context::{
    PromptContext, PromptContextInputs, ReadSourceInfo, build_prompt_context,
};
use crate::actors::slot::lifecycle::role_overrides::{ResolvedRoleOverrides, resolve_role_overrides};
use crate::actors::slot::lifecycle::setup::{
    SetupAndVerificationContext, SetupError, resolve_setup_and_verification_context,
};
use crate::actors::slot::lifecycle::teardown::{PostSessionParams, spawn_post_session_work};
use crate::actors::slot::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::context::AgentContext;
use djinn_provider::message::{Conversation, Message};
use djinn_provider::provider::LlmProvider;
use crate::roles::{AgentRole, role_impl_for};

use super::SupervisorCallbackContext;

/// Read-only multi-repo: clone each of the epic's read-source projects
/// read-only into a gitignored dir inside the worktree and resolve their
/// slugs/names, so the prompt can advertise them and the agent's
/// worktree-scoped file tools can read them. Best-effort: a missing
/// mirror or clone failure just drops the on-disk path (slug-based
/// `code_graph` / `memory_*` access still works). The clones are excluded
/// from git so the task's auto-commit never captures them.
async fn materialize_read_sources(
    spec: &TaskRunSpec,
    agent_context: &AgentContext,
    worktree_path: &std::path::Path,
) -> Vec<ReadSourceInfo> {
    if spec.read_source_project_ids.is_empty() {
        return Vec::new();
    }
    let project_repo =
        ProjectRepository::new(agent_context.db.clone(), agent_context.event_bus.clone());
    add_git_exclude(worktree_path, ".djinn/read-sources/").await;
    let dest_root = worktree_path.join(".djinn/read-sources");
    let mut out = Vec::new();
    for pid in &spec.read_source_project_ids {
        let project = match project_repo.get(pid).await {
            Ok(Some(p)) => p,
            _ => {
                tracing::warn!(read_source_id = %pid, "read-source project not found; skipping");
                continue;
            }
        };
        let slug = format!("{}/{}", project.github_owner, project.github_repo);
        // Resolve the read-source mirror against DJINN_MIRROR_ROOT (the mirror
        // PVC, mounted at /mirror on the K8s worker — same root the worker uses
        // for the PRIMARY workspace via MirrorManager) when set, falling back to
        // the DJINN_HOME-based default for host/in-process layouts. The bare
        // `mirror_path_for` only consults DJINN_HOME/$HOME (~/.djinn/mirrors),
        // which is absent in the worker Pod (DJINN_HOME unset, mirrors at
        // /mirror) — so it ALWAYS missed and every read source silently fell
        // back to "slug only", leaving the agent without the read-source code on
        // disk. Matches `MirrorManager::mirror_path` (`<root>/<pid>.git`).
        let mirror = std::env::var_os("DJINN_MIRROR_ROOT")
            .map(|root| std::path::PathBuf::from(root).join(format!("{pid}.git")))
            .unwrap_or_else(|| djinn_workspace::mirror_path_for(pid));
        let mut path = None;
        if mirror.exists() {
            let dest = dest_root.join(pid);
            match run_git_command(
                worktree_path.to_path_buf(),
                vec![
                    "clone".into(),
                    "--local".into(),
                    "--shared".into(),
                    mirror.display().to_string(),
                    dest.display().to_string(),
                ],
            )
            .await
            {
                Ok(_) => path = Some(dest.display().to_string()),
                Err(e) => tracing::warn!(
                    read_source = %slug,
                    error = %e,
                    "read-source clone failed; advertising slug only"
                ),
            }
        } else {
            tracing::warn!(
                read_source = %slug,
                "read-source mirror not present on worker; advertising slug only"
            );
        }
        out.push(ReadSourceInfo {
            slug,
            name: project.name.clone(),
            path,
        });
    }
    out
}

/// Best-effort append of a pattern to the worktree's `.git/info/exclude`
/// so the read-source clones never enter the task's commits.
async fn add_git_exclude(worktree_path: &std::path::Path, pattern: &str) {
    let exclude = worktree_path.join(".git/info/exclude");
    let existing = tokio::fs::read_to_string(&exclude).await.unwrap_or_default();
    if existing.lines().any(|l| l.trim() == pattern) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(pattern);
    content.push('\n');
    let _ = tokio::fs::write(&exclude, content).await;
}

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
    services: &dyn SupervisorServices,
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
        None => {
            let fallback = services
                .pick_any_default_model()
                .await
                .map_err(StageError::ModelResolution)?;
            match fallback {
                Some(m) => m,
                None if provider_override.is_some() => "test/supervisor-stub".to_string(),
                None => {
                    return Err(StageError::ModelResolution(format!(
                        "no model registered for role '{role_name}' in the provider catalog"
                    )));
                }
            }
        }
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
    // Pre-verification hooks come from `lifecycle.pre_verification`,
    // rules from `verification.rules`. Missing / malformed configs degrade
    // to empty lists (see `verification::environment`). Phase 6d routes the
    // DB lookup through `SupervisorServices` so the worker (Phase 7) gets
    // it via RPC without opening its own DB pool.
    let env_config = services
        .get_environment_config(task.project_id.clone())
        .await
        .map_err(|e| StageError::Setup(format!("env_config: {e}")))?;
    let SetupAndVerificationContext {
        prompt_setup_commands,
        prompt_verification_commands,
        prompt_verification_rules,
    } = match resolve_setup_and_verification_context(
        env_config.lifecycle.pre_verification,
        env_config.verification.rules,
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
    //
    // {{project_path}} feeds MCP tool calls (`memory_*`, `build_context`, etc.)
    // as the `project=...` argument. ProjectRepository::resolve accepts UUIDs
    // and `owner/repo` slugs but NOT filesystem paths. The worktree path
    // (`/workspace/.tmpXXX` in K8s pods) is not a registered project, so
    // feeding it here caused every memory-tool call from the planner to fail
    // with "project not found" and the planner re-dispatched in a tight loop.
    let project_path_str = task.project_id.clone();
    // Read-only multi-repo: materialize + resolve the epic's read-source
    // projects so the prompt can advertise them (and check out their files
    // read-only for direct inspection during a migration).
    let read_sources = materialize_read_sources(spec, agent_context, worktree_path).await;
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
        read_sources: &read_sources,
    })
    .await;

    // ── Create the session record linked to the task-run ─────────────────────
    // Phase 6c routes session creation through `SupervisorServices` so the
    // in-Pod worker never opens its own DB connection.  Host-side
    // `DirectServices` delegates to `SessionRepository::create` verbatim.
    let session_record = services
        .create_session(djinn_supervisor::services::SerializableCreateSessionParams {
            project_id: task.project_id.clone(),
            task_id: Some(task.id.clone()),
            model: model_id.clone(),
            agent_type: role_name.to_string(),
            metadata_json: None,
            task_run_id: Some(task_run_id.to_string()),
        })
        .await
        .map_err(StageError::SessionCreate)?;
    let session_id = session_record.id.clone();

    // ── Build the LLM provider ───────────────────────────────────────────────
    // Soft fallback: a missing catalog entry surfaces as `Err`, which we map to
    // `0` so the downstream provider builder still gets a sentinel — matches the
    // pre-Phase-6b `unwrap_or(0)` behaviour.
    let context_window = services
        .get_model_context_window(model_id.clone())
        .await
        .unwrap_or(0);

    let provider_arc: Option<Arc<dyn LlmProvider>> = provider_override;
    let provider_owned: Option<Box<dyn LlmProvider>> = if provider_arc.is_some() {
        None
    } else {
        let resolved = resolved
            .expect("resolved model credential must be populated when provider_override is absent");
        let telemetry_meta = build_telemetry_meta(role_name, &task.id);
        // Look up the API base URL only for API-key providers (OAuth configs
        // carry their own). Soft fallback to `default_base_url` on a missing
        // catalog entry / empty URL, matching the pre-Phase-6b behaviour.
        let base_url = if resolved_needs_base_url(&resolved) {
            services
                .get_provider_base_url(resolved.catalog_provider_id.clone())
                .await
                .unwrap_or_else(|_| default_base_url(&resolved.catalog_provider_id))
        } else {
            String::new()
        };
        let built = match build_provider_from_resolved(
            resolved,
            context_window.max(0) as u32,
            Some(telemetry_meta),
            Some(session_id.clone()),
            base_url,
        ) {
            Some(provider) => provider,
            None => {
                let _ = services
                    .update_session_status(session_id.clone(), SessionStatus::Failed, 0, 0)
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
            services,
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
    if let Err(e) = services
        .update_session_status(session_id.clone(), session_status, tokens_in, tokens_out)
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
                            // Empty (LLM omitted the field) and "execute" both
                            // mean "wave was created or board work continues" —
                            // mapping empty → Failed here looped every planner
                            // task whose prompt drifted off the decision field.
                            "" | "execute" => StageOutcome::PlannerExecute,
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
                        // Accept both present-tense ("approve"/"reject") and
                        // past-tense ("approved"/"rejected") forms — gpt-5.x
                        // consistently emits past-tense in the submit_review
                        // payload, which previously fell through to the "Failed"
                        // arm and broke open_pr for every review.
                        match verdict {
                            "approve" | "approved" => StageOutcome::ReviewerApproved,
                            "reject" | "rejected" => StageOutcome::ReviewerRejected {
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
                    // Reviewer session ended naturally without calling
                    // submit_review (LLM stopped emitting before invoking the
                    // finalize tool). Treat as a hard rejection so the task
                    // re-dispatches a fresh reviewer instead of silently
                    // approving unreviewed code.
                    "" => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            task_run_id = %task_run_id,
                            "Reviewer session ended without calling submit_review; treating as rejection so a fresh reviewer runs"
                        );
                        StageOutcome::ReviewerRejected {
                            feedback: "Reviewer session ended without calling submit_review — \
                                       you MUST call submit_review with verdict=\"approve\" \
                                       or verdict=\"reject\" before ending your session."
                                .to_string(),
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
