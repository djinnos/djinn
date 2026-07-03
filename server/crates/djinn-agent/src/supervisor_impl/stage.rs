// djinn:allow-oversize — over size-guard byte threshold after reliability fixes; split when touched substantively.
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
use djinn_db::ProjectRepository;
use djinn_runtime::spec::{RoleKind, TaskRunSpec};
use djinn_supervisor::{ParkReason, StageError, StageOutcome, SupervisorServices};
use djinn_workspace::Workspace;

use crate::AgentType;
use crate::actors::slot::helpers::conflict_context_for_dispatch;
use crate::actors::slot::helpers::{
    build_provider_from_resolved, build_telemetry_meta_with_attribution, default_base_url,
    resolved_needs_base_url,
};
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::model_resolution::{
    ModelResolutionError, attempt_resume_model_rotation, resolve_model_and_credential,
};
use crate::actors::slot::lifecycle::prompt_context::{
    PromptContext, PromptContextInputs, ReadSourceInfo, assemble_prompt_context,
    build_worker_resume_note,
};
use crate::actors::slot::lifecycle::role_overrides::{
    ResolvedRoleOverrides, resolve_role_overrides,
};
use crate::actors::slot::lifecycle::setup::{SetupContext, SetupError, resolve_setup_context};
use crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger;
use crate::actors::slot::lifecycle::teardown::{PostSessionParams, spawn_post_session_work};
use crate::actors::slot::reply_loop::error_handling::BudgetWindDownIgnored;
use crate::actors::slot::reply_loop::loop_guard::{
    LoopGuardError, LoopGuardKind as ReplyLoopGuardKind,
};
use crate::actors::slot::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::context::AgentContext;
use crate::roles::{AgentRole, role_impl_for};
use djinn_provider::message::{Conversation, Message};
use djinn_provider::provider::LlmProvider;
use djinn_provider::provider::error::ProviderError;
use djinn_runtime::{LoopGuardKind as RuntimeLoopGuardKind, LoopGuardTrip, ProviderFailureClass};

use super::SupervisorCallbackContext;

/// Conservative quota-reset window synthesized for an exhausted Codex/OpenAI
/// empty-200 throttle (idea A6/idea 3).
///
/// The ChatGPT consumer Codex backend signals an over-quota *account* by
/// answering a turn with an empty 200 (`response.completed`, zero tokens)
/// instead of an HTTP 429 — so unlike a real 429 there is **no `Retry-After` /
/// rate-limit-reset header** to read (the empty-200 arrives as a normal stream
/// end, never through the client's status-error path that parses headers in
/// `provider/client.rs::retry_after_ms`). Without a provider-stated window the
/// per-task redispatch cooldown would otherwise probe the model again on the
/// fast 60/120/240s ladder, immediately re-hitting the still-exhausted quota.
///
/// We therefore floor the redispatch cooldown on a conservative constant: an
/// account quota resets on a clock (typically hourly/daily), not in seconds, so
/// a ~20-minute hold lets dispatch fail over to the user's next model and avoids
/// hammering the depleted account while still self-healing well within a normal
/// reset window. This is the floor only — a longer escalating cooldown still
/// wins via `max()`.
const CODEX_EMPTY_QUOTA_RETRY_AFTER_MS: u64 = 20 * 60 * 1000;

/// Classify a reply-loop terminal error into the breaker-relevant
/// [`ProviderFailureClass`] the host should act on, or `None` when the host
/// circuit-breaker should stay out of it.
///
/// The typed [`ProviderError`] only exists here, in-pod, where the reply loop
/// drives the provider directly; it is not serde-serializable and cannot ride
/// the report frame, so we fold it into the small serde class that does. The
/// host (`supervisor_runner.rs`) then maps the class back onto
/// `record_failure` / `record_stall` for the task creator's `(scope, model)`
/// bucket.
///
/// Mapping (mirrors the coordinator's throttle→stall / quiet-failure→failure
/// intent):
/// - `Authentication` | `InvalidRequest` | `InvalidOutput` |
///   `ProviderInternal{5xx}` → [`ProviderFailureClass::Failure`]. These are
///   "quiet but broken" — a bad/expired credential, a request the provider
///   keeps rejecting, or a flapping backend. Fed to the gentler
///   consecutive-failure breaker so a single transient blip doesn't demote the
///   user's preferred model; only repeats trip it.
/// - `RateLimit` | `EmptyCompletion` → [`ProviderFailureClass::Throttle`]. A
///   throttle/quota signal; fed to the immediate-failover breaker
///   (`record_stall`) so dispatch moves to the next model at once with a
///   cooldown that outlasts the task's redispatch ladder. `RateLimit` is an
///   explicit 429: its `retry_after_ms` (a `Retry-After` / rate-limit-reset
///   window, if the provider supplied one) rides along so the coordinator can
///   floor the redispatch cooldown on a multi-hour reset instead of probing on
///   the fixed ladder (A6). `EmptyCompletion` is the Codex/OpenAI consumer
///   backend's *implicit* throttle: an over-quota ACCOUNT answers a turn with an
///   empty 200 (zero-token `response.completed`) instead of a 429, so the reply
///   loop classifies an exhausted empty-turn streak on that family as
///   `EmptyCompletion` (see `reply_loop::turn::empty_turn_terminal_error`). It
///   carries no header, so we synthesize a conservative
///   [`CODEX_EMPTY_QUOTA_RETRY_AFTER_MS`] window. Routing it here (instead of the
///   old `None`) stops a *throttle* from being miscounted as a broken provider
///   via `record_failure`, which polluted health stats and escalated the
///   auto-disable cooldown for what is merely an account over quota.
/// - `Transport` → [`ProviderFailureClass::Failure`]. A hard network death
///   (connection refused / instant timeout / broken stream) that kills the
///   session with no work done — quiet-but-broken, just like a 5xx. Fed to the
///   gentle consecutive-failure breaker so a one-off blip is absorbed (a
///   successful session resets the counter via `record_success`) but a model
///   that dies on every dispatch finally auto-disables instead of being
///   re-selected forever (the kimi-for-coding/k2p7 incident).
/// - `ContextOverflow` → `None`. Excluded by design: handled by reactive
///   compaction (the conversation is too big, not a model-health problem).
///   Tripping the breaker on it would needlessly demote a healthy model.
/// - An untyped/legacy error (no `ProviderError` source) → `None`, so non-
///   provider failures (git, tools, finalize-tool misuse) never trip it.
fn classify_provider_failure(err: &anyhow::Error) -> Option<ProviderFailureClass> {
    let provider_err = err.downcast_ref::<ProviderError>()?;
    match provider_err {
        // Auth (401/403) is deterministic — a revoked/invalid credential won't
        // recover on retry. Classify it distinctly so the host trips the breaker
        // immediately (failover at once) and surfaces the revocation, rather than
        // probing the dead model three times like a "quiet but broken" failure.
        ProviderError::Authentication => Some(ProviderFailureClass::AuthInvalid),
        ProviderError::InvalidRequest
        | ProviderError::InvalidOutput
        | ProviderError::ProviderInternal { .. } => Some(ProviderFailureClass::Failure),
        ProviderError::RateLimit { .. } => Some(ProviderFailureClass::Throttle {
            retry_after_ms: provider_err.retry_after_ms(),
        }),
        // The Codex/OpenAI consumer backend's implicit throttle: an over-quota
        // account answers a turn with an empty 200 (zero-token
        // `response.completed`) instead of a 429, surfaced as `EmptyCompletion`
        // by `reply_loop::turn::empty_turn_terminal_error` for that family only.
        // Route it to the SAME immediate-failover `Throttle` path as a 429 so a
        // throttle is no longer miscounted as a broken provider (which would feed
        // `record_failure` and escalate the auto-disable cooldown). There is no
        // `Retry-After` header on an empty 200, so floor the redispatch cooldown
        // on a conservative synthesized quota window.
        ProviderError::EmptyCompletion => Some(ProviderFailureClass::Throttle {
            retry_after_ms: Some(CODEX_EMPTY_QUOTA_RETRY_AFTER_MS),
        }),
        // A hard transport failure (connection refused, instant timeout, broken
        // stream) that kills the session is "quiet but broken" the same way a 5xx
        // is: the model produced no work and exited fast, invisible to the
        // coordinator's stall detector. Feed it to the GENTLE consecutive-failure
        // breaker (`record_failure`), NOT the immediate-failover one — a single
        // network blip on an otherwise-healthy model must not demote it. The
        // breaker only trips after the configured run of consecutive failures, and
        // any successful session calls `record_success` (resets the counter), so a
        // transient blip is absorbed while a model that dies on EVERY dispatch
        // (the kimi-for-coding/k2p7 incident: instant Transport death, 0 tokens,
        // re-dispatched forever, absent from model_health) finally auto-disables.
        ProviderError::Transport => Some(ProviderFailureClass::Failure),
        // ContextOverflow is handled by reactive compaction (the conversation is
        // too big), not a model-health problem — stays `None` so the breaker
        // doesn't demote a healthy model.
        ProviderError::ContextOverflow => None,
    }
}

/// Derive the `(CostBasisHint, BillingSource)` a session is created with from
/// the RESOLVED credential kind plus catalog string rules (t41r PART A).
///
/// `credential_is_oauth` is `true` when the resolved credential is an
/// OAuth-derived config rather than an API key. The subscription decision keys
/// on whether the provider's OAuth flow is a subscription plan
/// ([`oauth_is_subscription_plan`]) — OAuth transport ALONE never flips a
/// metered provider to `SubscriptionPlan`, preserving the guarded invariant
/// that a hypothetical metered OAuth stays metered. The catalog string rules
/// (`is_subscription_provider` / `governable_subscription_for_model`) remain an
/// additional signal so e.g. a `zai-coding-plan/...` API-key session is still
/// `SubscriptionPlan`.
fn derive_billing_signal(
    provider_id: &str,
    model_name: &str,
    credential_is_oauth: bool,
) -> (
    djinn_supervisor::services::CostBasisHint,
    djinn_supervisor::services::BillingSource,
) {
    use djinn_provider::catalog::builtin::{
        governable_subscription_for_model, is_subscription_provider,
    };
    use djinn_supervisor::services::{BillingSource, CostBasisHint};

    let plan_oauth = credential_is_oauth && oauth_is_subscription_plan(provider_id);
    let hint = if plan_oauth
        || is_subscription_provider(provider_id)
        || governable_subscription_for_model(provider_id, model_name).is_some()
    {
        CostBasisHint::SubscriptionPlan
    } else {
        CostBasisHint::MeteredApi
    };
    // `billing_source` records the concrete credential transport. A
    // subscription-plan OAuth credential → `plan_oauth` (the case the model id
    // cannot reveal); everything else — metered API keys AND coding-plan API
    // keys (whose plan nature is already captured by `cost_basis`) → `api_key`.
    let billing_source = if plan_oauth {
        BillingSource::PlanOauth
    } else {
        BillingSource::ApiKey
    };
    (hint, billing_source)
}

/// True when `provider_id`'s OAuth flow is a personal subscription plan, so an
/// OAuth-backed session on it is a plan (no per-token spend) rather than metered
/// API usage.
///
/// The effective OAuth provider handling this catalog id decides (e.g.
/// `openai` → `chatgpt_codex`), and its builtin `credential_class` is
/// authoritative. A provider whose OAuth flow is NOT a subscription returns
/// `false` — this is what keeps OAuth transport alone from implying a plan.
fn oauth_is_subscription_plan(provider_id: &str) -> bool {
    use djinn_provider::catalog::builtin::{is_subscription_provider, resolve_oauth_provider};
    let effective = match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => resolve_oauth_provider(other).unwrap_or(other),
    };
    is_subscription_provider(effective)
}

/// Map a finished reviewer stage's finalize tool + payload onto a
/// [`StageOutcome`]. Pure (no `task`/tracing deps) so the verdict-handling
/// branches are unit-testable.
///
/// Crucially distinguishes an EXPLICIT rejection (the reviewer actually called
/// `submit_review` with a reject/request_changes verdict → `ReviewerRejected`,
/// which the supervisor maps to `task_review_reject` and reopens the worker's
/// PR) from a reviewer that ended with NO verdict at all (`finalize_name == ""`
/// — the model stopped emitting before invoking the finalize tool). A no-verdict
/// completion is NOT evidence the work is bad; mapping it to `ReviewerRejected`
/// FALSE-REJECTS good work over a malfunctioning reviewer model (the
/// kimi-for-coding/k2p7 incident: a model that produces tokens but never calls
/// the verdict tool). It returns a non-terminal [`StageOutcome::Failed`] instead:
/// the supervisor does NOT transition the task on a reviewer `Failed`, so it
/// stays `in_task_review`, and the coordinator's stuck-task recovery scan
/// releases it `in_task_review → needs_task_review` (ReleaseTaskReview, NO
/// reopen bump) to dispatch a FRESH reviewer.
///
/// This retry is bounded by the existing dispatch machinery, not a new counter:
/// a task that keeps reappearing for the same role with no typed provider
/// failure advances `dispatch_failure_streak` and, at
/// `STREAK_INTERVENTION_THRESHOLD`, routes to a Planner intervention (trigger B
/// in `dispatch/retry.rs`), with the terminal close at `MAX_DISPATCH_FAILURES`
/// as the final backstop — so no-verdict reviewers converge instead of looping
/// forever. Bug 3 (a faulty reviewer MODEL now trips the breaker on its
/// Transport/failure deaths) accelerates convergence by failing the run over to
/// a healthy reviewer model.
fn reviewer_stage_outcome(
    finalize_name: &str,
    finalize_payload: Option<&serde_json::Value>,
) -> StageOutcome {
    let payload_str = |key: &str| {
        finalize_payload
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_str())
    };
    match finalize_name {
        "submit_review" => {
            // Did the reviewer mark EVERY acceptance criterion met? `None` when
            // the payload carried no criteria array (can't assert either way).
            // submit_review sets AC met/unmet state atomically, so this array is
            // the reviewer's own structured verdict on the objective
            // definition-of-done — not feedback prose.
            let all_criteria_met = finalize_payload
                .and_then(|p| p.get("acceptance_criteria"))
                .and_then(|v| v.as_array())
                .filter(|arr| !arr.is_empty())
                .map(|arr| {
                    arr.iter()
                        .all(|c| c.get("met").and_then(|m| m.as_bool()).unwrap_or(false))
                });
            // Accept both present-tense ("approve"/"reject") and past-tense
            // ("approved"/"rejected") forms — gpt-5.x consistently emits
            // past-tense in the submit_review payload, which previously fell
            // through to the "Failed" arm and broke open_pr for every review.
            match payload_str("verdict").unwrap_or("") {
                "approve" | "approved" => StageOutcome::ReviewerApproved,
                // Self-contradictory verdict: `rejected` while EVERY acceptance
                // criterion is marked met. The objective definition-of-done is
                // satisfied, yet a reject bounces the task to `open` (+reopen),
                // cycling it into planner escalation (observed 2026-07-01: task
                // 55i8 — reviewer wrote "acceptable ... does not block the P0
                // epic" but emitted `rejected`, looping the task). Resolve in
                // favor of the AC contract and approve. A genuine blocker must be
                // expressed by marking at least one criterion unmet (which keeps
                // the reject). Keys on the structured AC-met array, not prose.
                "reject" | "rejected" if all_criteria_met == Some(true) => {
                    tracing::warn!(
                        feedback = payload_str("feedback").unwrap_or(""),
                        "reviewer submitted `rejected` with all acceptance criteria marked met; \
                         treating as approved (a reject must mark at least one criterion unmet)"
                    );
                    StageOutcome::ReviewerApproved
                }
                "reject" | "rejected" => StageOutcome::ReviewerRejected {
                    feedback: payload_str("feedback").unwrap_or("").to_string(),
                },
                other => StageOutcome::Failed {
                    reason: format!("reviewer submitted unknown verdict '{other}'"),
                    provider_failure: None,
                },
            }
        }
        // No verdict at all: release for a fresh reviewer (see fn doc). NON-
        // terminal — must NOT be `ReviewerRejected` (that reopens the PR).
        "" => StageOutcome::Failed {
            reason: "reviewer session ended without calling submit_review \
                     (no verdict rendered); releasing task for a fresh reviewer"
                .to_string(),
            provider_failure: None,
        },
        "request_lead" => StageOutcome::Escalate {
            reason: payload_str("reason")
                .filter(|v| !v.is_empty())
                .or_else(|| payload_str("message").filter(|v| !v.is_empty()))
                .or_else(|| payload_str("summary").filter(|v| !v.is_empty()))
                .unwrap_or("reviewer escalated to lead")
                .to_string(),
        },
        other => StageOutcome::Failed {
            reason: format!("reviewer finalized via unexpected tool '{other}'"),
            provider_failure: None,
        },
    }
}

fn runtime_trip_for_reply_loop_guard_error(error: &LoopGuardError) -> LoopGuardTrip {
    let kind = match error.condition.kind() {
        ReplyLoopGuardKind::RepeatedToolFailure => RuntimeLoopGuardKind::IdenticalToolFailure,
        ReplyLoopGuardKind::RepeatedPermissionOrSecurityDenial => {
            RuntimeLoopGuardKind::PermissionDenial
        }
        ReplyLoopGuardKind::RepeatedAssistantOutput => RuntimeLoopGuardKind::IdenticalOutput,
        ReplyLoopGuardKind::ConsecutiveToolFailures => RuntimeLoopGuardKind::ConsecutiveFailures,
    };

    LoopGuardTrip {
        kind,
        offending_signature: error.condition.offending_signature_label(),
        threshold: error.condition.threshold,
        observed: error.condition.observed,
        turn_span: error.turn_span,
        session_id: error.session_id.clone(),
    }
}

fn stage_outcome_for_runtime_loop_guard_trip(trip: &LoopGuardTrip) -> StageOutcome {
    StageOutcome::LoopGuardTripped {
        kind: trip.kind,
        offending_signature: trip.offending_signature.clone(),
        threshold: trip.threshold,
        observed: trip.observed,
        turn_span: trip.turn_span,
        session_id: trip.session_id.clone(),
    }
}

fn stage_outcome_for_reply_loop_guard_error(error: &LoopGuardError) -> StageOutcome {
    let trip = runtime_trip_for_reply_loop_guard_error(error);
    stage_outcome_for_runtime_loop_guard_trip(&trip)
}

fn session_settlement_for_stage_outcome(
    stage_outcome: &StageOutcome,
    final_result_ok: bool,
) -> (SessionStatus, Option<String>) {
    match stage_outcome {
        StageOutcome::Parked {
            reason: ParkReason::Budget,
            ..
        } => (SessionStatus::Completed, Some("budget".to_string())),
        _ if final_result_ok => (SessionStatus::Completed, None),
        _ => (SessionStatus::Failed, None),
    }
}

/// Read-only multi-repo: resolve the epic's read-source projects to slugs/names
/// so the prompt can flag them as specifically relevant. We no longer clone
/// them eagerly — the agent reads any registered repo on demand via
/// `read(project=…)` / `code_search`, and `shell(project=…)` lazily checks one
/// out only when it actually needs a working tree.
async fn advertise_read_sources(
    spec: &TaskRunSpec,
    agent_context: &AgentContext,
) -> Vec<ReadSourceInfo> {
    if spec.read_source_project_ids.is_empty() {
        return Vec::new();
    }
    let project_repo =
        ProjectRepository::new(agent_context.db.clone(), agent_context.event_bus.clone());
    let mut out = Vec::new();
    for pid in &spec.read_source_project_ids {
        let project = match project_repo.get(pid).await {
            Ok(Some(p)) => p,
            _ => {
                tracing::warn!(read_source_id = %pid, "read-source project not found; skipping");
                continue;
            }
        };
        out.push(ReadSourceInfo {
            slug: format!("{}/{}", project.github_owner, project.github_repo),
            name: project.name.clone(),
        });
    }
    out
}

/// Execute one role stage against the shared workspace.
///
/// Resolves the role → model credential → project setup config →
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
    // server + skill lists, and swaps `runtime_role`
    // when a Worker stage's `task.agent_type` names a specialist whose
    // `base_role` differs from the injected RoleKind.  Non-Worker stages
    // always use the default-role path.
    let ResolvedRoleOverrides {
        runtime_role,
        system_prompt_extensions,
        learned_prompt,
        mcp_servers: role_mcp_servers,
        skills: role_skills,
        model_preference: _role_model_preference,
        specialist_overrode_runtime_role,
    } = resolve_role_overrides(task, role_kind, agent_context).await;

    // The CONCRETE role actually running this stage after overrides. For a
    // refinement tribunal stage `role_kind` is the generic `Refinement` (whose
    // default arc is Advocate), but `runtime_role` is resolved from
    // `task.agent_type` to advocate/adversary/judge. Everything that defines
    // what the agent IS — the session `agent_type`, telemetry, tool schemas,
    // the system/initial prompt, and the finalize tools — must key off this,
    // not the generic `role`/`role_name`, or the adversary/judge would run the
    // advocate prompt + `submit_work` tool and never produce objections/verdicts.
    let runtime_role_name = runtime_role.config().name;

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

    // ── Model rotation for resume (y8pv / 48ru) ────────────────────────────
    // When resume metadata indicates the prior session used a specific model
    // that terminated for a rotation-worthy cause (no-progress, deadline,
    // flaky, or repeated verify-loop), attempt to select a different model
    // from the connected catalog. Falls back to the current model when
    // rotation is not applicable or no alternate is available.
    let model_id = if provider_override.is_some() {
        model_id
    } else {
        attempt_resume_model_rotation(
            &task.short_id,
            &model_id,
            spec.resume_lifecycle_metadata.as_ref(),
            agent_context,
        )
        .await
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
    //
    // The authoring trigger gates native-skill loading: only proposal-authoring
    // planner sessions (epic_breakdown) receive platform-owned skills like
    // `visual-spec`.  Non-authoring planner sessions (wave planning, dispatch)
    // skip native skills to avoid paying the context cost.
    let authoring_trigger = classify_native_skill_trigger(runtime_role.config().name, task);

    let McpAndSkills {
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
        native_skill_names: _native_skill_names,
    } = resolve_mcp_and_skills(
        worktree_path,
        runtime_role.as_ref(),
        &task.short_id,
        role_mcp_servers.as_deref(),
        &role_skills,
        authoring_trigger,
        #[cfg(test)]
        None,
        agent_context,
    )
    .await;

    // ── Setup commands ───────────────────────────────────────────────────────
    // Pre-verification hooks come from `lifecycle.pre_verification` (via the
    // SupervisorServices RPC). Missing / malformed configs degrade to empty
    // lists (see `environment`).
    let env_config = services
        .get_environment_config(task.project_id.clone())
        .await
        .map_err(|e| StageError::Setup(format!("env_config: {e}")))?;
    let SetupContext {
        prompt_setup_commands,
    } = match resolve_setup_context(
        env_config.lifecycle.pre_verification,
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
    let read_sources = advertise_read_sources(spec, agent_context).await;
    // y8pv / 48ru: build a one-line worker resume note from the coordinator-
    // selected resume lifecycle metadata. Only injected for worker dispatch;
    // non-worker roles receive no resume instructions.
    let worker_resume_note = build_worker_resume_note(
        runtime_role.config().name,
        spec.resume_lifecycle_metadata.as_ref(),
    );
    if worker_resume_note.is_some() {
        tracing::info!(
            task_id = %task.short_id,
            task_run_id = %task_run_id,
            role = %runtime_role_name,
            "Supervisor stage: injected worker resume note"
        );
    }
    // Coarse pre-session progress marker for the host-side liveness deadline:
    // model/credential/MCP/skill resolution and prompt assembly happen here,
    // before any session row exists.
    let _ = services
        .report_stage_step(djinn_runtime::stage_step::CONTEXT_BUILD)
        .await;
    let PromptContext { system_prompt, .. } = assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: runtime_role.as_ref(),
        role_for_epic_check: role.as_ref(),
        project_path: &project_path_str,
        worktree_path,
        conflict_ctx: conflict_ctx.as_ref(),
        merge_validation_ctx: None,
        prompt_setup_commands,
        system_prompt_extensions: &system_prompt_extensions,
        learned_prompt: learned_prompt.as_deref(),
        resolved_skills: &resolved_skills,
        app_state: agent_context,
        read_sources: &read_sources,
        worker_resume_note: worker_resume_note.as_deref(),
    })
    .await;

    // ── Create the session record linked to the task-run ─────────────────────
    // Phase 6c routes session creation through `SupervisorServices` so the
    // in-Pod worker never opens its own DB connection.  Host-side
    // `DirectServices` delegates to `SessionRepository::create` verbatim.
    //
    // Derive the billing classification from the RESOLVED CREDENTIAL (not just
    // model-id substrings) so `DirectServices::create_session` books
    // `sessions.cost_basis` on explicit signal: a session on `openai/gpt-5.5`
    // backed by a ChatGPT/Codex PLAN OAuth credential is a $0-spend plan even
    // though its model id has no `codex` marker. OAuth transport ALONE does not
    // imply a subscription — see `oauth_is_subscription_plan`. The catalog
    // string rules stay as an additional signal (a `zai-coding-plan/...` model
    // remains projected); the legacy fallback covers `hint = None`.
    let billing_signal = resolved.as_ref().map(|r| {
        let credential_is_oauth = matches!(
            r.provider_credential,
            Some(crate::actors::slot::helpers::ProviderCredential::OAuthConfig(_))
        );
        derive_billing_signal(&r.catalog_provider_id, &r.model_name, credential_is_oauth)
    });
    let cost_basis_hint = billing_signal.map(|(hint, _)| hint);
    let billing_source = billing_signal.map(|(_, source)| source);
    let _ = services
        .report_stage_step(djinn_runtime::stage_step::SESSION_CREATE)
        .await;
    let session_record = services
        .create_session(
            djinn_supervisor::services::SerializableCreateSessionParams {
                project_id: task.project_id.clone(),
                task_id: Some(task.id.clone()),
                model: model_id.clone(),
                agent_type: runtime_role_name.to_string(),
                metadata_json: None,
                task_run_id: Some(task_run_id.to_string()),
                cost_basis_hint,
                billing_source,
            },
        )
        .await
        .map_err(StageError::SessionCreate)?;
    let session_id = session_record.id.clone();
    // The session row now exists — the first reply-loop turn is reached. This
    // marker (and the `sessions` row itself) disarms the host-side pre-session
    // liveness deadline; from here liveness is owned by the coordinator's
    // session stall detector / zombie reaper.
    let _ = services
        .report_stage_step(djinn_runtime::stage_step::FIRST_TURN)
        .await;

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
        let telemetry_meta =
            build_telemetry_meta_with_attribution(runtime_role_name, &task.id, None, None);
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
                    .update_session_status(
                        session_id.clone(),
                        SessionStatus::Failed,
                        0,
                        0,
                        0,
                        0,
                        None,
                    )
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
    let agent_type = crate::AgentType::parse(runtime_role_name).unwrap_or(crate::AgentType::Worker);
    // Evidence-spike tasks (created by the Judge demand-evidence path in
    // epic 6tjy) carry the `refinement-evidence` + `read-only` labels and
    // must run under a restricted read-only/fail-closed tool profile.
    // Detection is strict: both labels must be present.  Tasks that carry
    // only one label (or have malformed metadata) fall through to the
    // normal role tool surface — deny-by-default is enforced by the
    // restricted profile itself, not by a separate deny gate.
    let is_evidence_spike = djinn_core::models::task::is_evidence_spike(&task.labels);
    let mut tools = if is_evidence_spike {
        crate::extension::tool_schemas_evidence_spike()
    } else {
        crate::roles::tool_schemas_for(agent_type)
    };
    if let Some(ref registry) = mcp_registry {
        tools.extend_from_slice(registry.tool_schemas());
    }

    let mut conversation = Conversation::new();
    conversation.push(Message::system(system_prompt));
    let initial_user_message = runtime_role
        .initial_user_message(&task.id, agent_context)
        .await;
    conversation.push(Message::user(initial_user_message));

    // ── Run the reply loop ───────────────────────────────────────────────────
    //
    // Scope the whole loop under `SESSION_USER_ID = task creator` so every
    // entity the agent creates via a tool call (epics from the proposal-
    // decomposition planner, tasks from a wave planner, memory notes) inherits
    // the right owner via `auth_context::current_user_id()` — and therefore
    // dispatches under that owner's model/credentials. Without this the worker
    // pod runs outside any user scope: `current_user_id()` is `None`, so
    // `epic_create` stamped NULL and downstream wave-planner tasks fell back to
    // a generic user (losing both attribution and the build owner's model
    // the user picked at kickoff). The host already scopes credential
    // resolution this way (supervisor_runner.rs); this extends the same
    // identity to in-pod tool execution.
    let project_path_str = worktree_path.display().to_string();
    let reply_loop_fut = run_reply_loop(
        ReplyLoopContext {
            provider: provider_ref,
            tools: &tools,
            task_id: &task.id,
            task_short_id: &task.short_id,
            session_id: &session_id,
            project_path: &project_path_str,
            worktree_path,
            role_name: runtime_role_name,
            finalize_tool_names: runtime_role.config().finalize_tool_names,
            context_window,
            model_id: &model_id,
            cancel: &callbacks.cancel,
            global_cancel: &callbacks.cancel,
            app_state: agent_context,
            services,
            mcp_registry: mcp_registry.as_ref(),
            active_skill_names: &effective_skills,
            active_mcp_server_names: &effective_mcp_servers,
            max_turns_override: None,
            is_evidence_spike,
        },
        &mut conversation,
        false,
    );
    let (reply_result, final_output, tokens_in, tokens_out, cache_read, cache_write) =
        djinn_core::auth_context::SESSION_USER_ID
            .scope(task.created_by_user_id.clone(), reply_loop_fut)
            .await;

    // ── Map the reply-loop outcome to StageOutcome ───────────────────────────
    let final_result_ok = reply_result.is_ok();
    let final_error = reply_result.as_ref().err().map(|e| e.to_string());
    let stage_outcome = match reply_result {
        Err(e) => {
            if e.downcast_ref::<BudgetWindDownIgnored>().is_some() {
                StageOutcome::Parked {
                    reason: ParkReason::Budget,
                    summary: None,
                    wind_down_ignored: true,
                    session_id: session_id.clone(),
                    tokens_in,
                    tokens_out,
                }
            } else if let Some(trip) = e.downcast_ref::<LoopGuardTrip>() {
                stage_outcome_for_runtime_loop_guard_trip(trip)
            } else if let Some(guard_error) = e.downcast_ref::<LoopGuardError>() {
                stage_outcome_for_reply_loop_guard_error(guard_error)
            } else {
                StageOutcome::Failed {
                    reason: format!("reply loop error: {e}"),
                    // Classify the typed provider error (if any) so the host breaker
                    // can fail over off a structurally-broken model/credential.
                    provider_failure: classify_provider_failure(&e),
                }
            }
        }
        Ok(()) => {
            if let Some(summary) = final_output.budget_wind_down_summary.clone() {
                StageOutcome::Parked {
                    reason: ParkReason::Budget,
                    summary: Some(summary),
                    wind_down_ignored: false,
                    session_id: session_id.clone(),
                    tokens_in,
                    tokens_out,
                }
            } else {
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
                            provider_failure: None,
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
                                    provider_failure: None,
                                },
                            }
                        }
                        other => StageOutcome::Failed {
                            reason: format!("planner finalized via unexpected tool '{other}'"),
                            provider_failure: None,
                        },
                    },
                    RoleKind::Reviewer => {
                        if finalize_name.is_empty() {
                            // A no-verdict completion is NOT a rejection — log it so
                            // the release-for-fresh-reviewer path is visible in the
                            // task timeline (the helper has no `task` access).
                            tracing::warn!(
                                task_id = %task.short_id,
                                task_run_id = %task_run_id,
                                "Reviewer session ended without calling submit_review; releasing for a fresh reviewer (NOT rejecting the worker's PR)"
                            );
                        }
                        reviewer_stage_outcome(
                            finalize_name,
                            final_output.finalize_payload.as_ref(),
                        )
                    }
                    RoleKind::Verifier => StageOutcome::Failed {
                        reason: "verifier stage not yet wired in supervisor".into(),
                        provider_failure: None,
                    },
                    RoleKind::Architect => match finalize_name {
                        "submit_work" => StageOutcome::ArchitectDone,
                        other => StageOutcome::Failed {
                            reason: format!("architect finalized via unexpected tool '{other}'"),
                            provider_failure: None,
                        },
                    },
                    RoleKind::Lead => match finalize_name {
                        "submit_decision" => {
                            let decision = final_output
                                .finalize_payload
                                .as_ref()
                                .and_then(|p| p.get("decision"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let reason = extract_reason(&final_output.finalize_payload);
                            match decision {
                                // Work is complete + correct; reviewer/worker just
                                // couldn't certify. Reconnects the existing
                                // lead_approve DB transition (the incident fix).
                                "approve" | "approved" => StageOutcome::LeadApproved,
                                "approve_conflict" => StageOutcome::LeadApproveConflict {
                                    reason: reason.unwrap_or_else(|| {
                                        "lead approved with merge conflict".into()
                                    }),
                                },
                                // Rescope / guide / block-on-deps: the Lead made the
                                // edits and sends the task back for a fresh worker.
                                "reopen" => StageOutcome::LeadReopen {
                                    reason: reason.unwrap_or_else(|| "lead reopened task".into()),
                                },
                                // decompose: replacement subtasks already created by
                                // the Lead via MCP; force_close: redundant /
                                // already-landed work. Both terminally close the
                                // original task.
                                "decompose" | "force_close" => StageOutcome::LeadClose {
                                    reason: reason.unwrap_or_else(|| {
                                        format!("lead closed task ({decision})")
                                    }),
                                },
                                "escalate" => StageOutcome::LeadEscalate {
                                    reason: reason.unwrap_or_else(|| {
                                        "lead escalated for board review".into()
                                    }),
                                },
                                other => StageOutcome::Failed {
                                    reason: format!("lead submitted unknown decision '{other}'"),
                                    provider_failure: None,
                                },
                            }
                        }
                        // The Lead ended without calling submit_decision. Releasing
                        // the task back to the lead queue (it stays
                        // in_lead_intervention → redispatch a fresh Lead) is safer
                        // than guessing a board transition.
                        "" => StageOutcome::Failed {
                            reason: "lead session ended without calling submit_decision".into(),
                            provider_failure: None,
                        },
                        other => StageOutcome::Failed {
                            reason: format!("lead finalized via unexpected tool '{other}'"),
                            provider_failure: None,
                        },
                    },
                    // Refinement tribunal roles each finalize via their own
                    // configured tool: the Advocate `submit_work`, the Adversary
                    // `submit_review`, the Judge `submit_decision`. The finalize
                    // tool is only a session terminator — the coordinator reads
                    // the DB (proposal revisions + debate trail) after the
                    // session ends to advance the state machine, so any of the
                    // three counts as a clean completion. (The real output is
                    // written via `proposal_debate_append` / `proposal_update`
                    // during the session, not carried in the finalize call.)
                    RoleKind::Refinement => match finalize_name {
                        "submit_work" | "submit_review" | "submit_decision" => {
                            StageOutcome::WorkerDone
                        }
                        "" => StageOutcome::Failed {
                            reason: "refinement session ended without calling a finalize tool \
                                     (submit_work / submit_review / submit_decision)"
                                .into(),
                            provider_failure: None,
                        },
                        other => StageOutcome::Failed {
                            reason: format!(
                                "refinement session finalized via unexpected tool '{other}'"
                            ),
                            provider_failure: None,
                        },
                    },
                }
            }
        }
    };

    // ── Finalize session ─────────────────────────────────────────────────────
    let (session_status, parked_reason) =
        session_settlement_for_stage_outcome(&stage_outcome, final_result_ok);
    if let Err(e) = services
        .update_session_status(
            session_id.clone(),
            session_status,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            parked_reason,
        )
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "Supervisor stage: failed to update session record"
        );
    }

    if let StageOutcome::Parked {
        reason: ParkReason::Budget,
        summary: Some(summary),
        wind_down_ignored: false,
        ..
    } = &stage_outcome
    {
        crate::actors::slot::finalize_handlers::handle_budget_park(
            summary,
            final_output
                .budget_wind_down_details
                .as_deref()
                .unwrap_or("budget-triggered wind-down summary captured"),
            &task.id,
            agent_context,
        )
        .await;
    }

    // ── Dispatch post-session work ───────────────────────────────────────────
    let project_path =
        crate::task_merge::resolve_project_path_for_id(&task.project_id, agent_context)
            .await
            .unwrap_or_else(|| worktree_path.display().to_string());

    let parked = matches!(stage_outcome, StageOutcome::Parked { .. });
    let post_session_result_ok = final_result_ok || parked;
    let post_session_error = if parked { None } else { final_error };

    spawn_post_session_work(PostSessionParams {
        task_id: task.id.clone(),
        project_path,
        role: role.clone(),
        app_state: agent_context.clone(),
        final_output,
        final_result_ok: post_session_result_ok,
        final_error: post_session_error,
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
        RoleKind::Lead => role_impl_for(AgentType::Lead),
        // Default for refinement — the concrete tribunal role (advocate,
        // adversary, judge) is resolved from task.agent_type by role_overrides.
        RoleKind::Refinement => role_impl_for(AgentType::Advocate),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a `ProviderError` the way the reply loop surfaces it: as the source
    /// of an `anyhow::Error` carrying a readable context line.
    fn typed(e: ProviderError) -> anyhow::Error {
        anyhow::Error::new(e).context("provider stream event failed")
    }

    // ── Credential-derived billing signal (t41r PART A) ──────────────────────

    use crate::direct_services::determine_cost_basis;
    use djinn_supervisor::services::{BillingSource, CostBasisHint};

    /// A non-zero pricing snapshot (a "priced" model), so `determine_cost_basis`
    /// can distinguish `actual` from `unpriced` for the `MeteredApi` hint.
    fn priced() -> djinn_core::models::Pricing {
        djinn_core::models::Pricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: 0.5,
            cache_write_per_million: 0.0,
        }
    }

    /// The core forward-fix: `openai/gpt-5.5` (no `codex` marker) backed by a
    /// PLAN OAuth credential is a `SubscriptionPlan` → `projected`, even though
    /// `openai` is an API-key provider and the model id has no codex marker.
    #[test]
    fn billing_signal_openai_plan_oauth_is_projected() {
        let (hint, source) = derive_billing_signal("openai", "gpt-5.5", /* oauth */ true);
        assert_eq!(hint, CostBasisHint::SubscriptionPlan);
        assert_eq!(source, BillingSource::PlanOauth);
        // …and that hint books `projected` (priced) at session creation.
        assert_eq!(
            determine_cost_basis(Some(hint), Some(&priced()), Some("openai")),
            "projected"
        );
    }

    /// The same `openai/gpt-5.5` model backed by an API key stays `MeteredApi`
    /// → `actual`. This is the metered path that must never be mistaken for a
    /// plan.
    #[test]
    fn billing_signal_openai_api_key_is_actual() {
        let (hint, source) = derive_billing_signal("openai", "gpt-5.5", /* oauth */ false);
        assert_eq!(hint, CostBasisHint::MeteredApi);
        assert_eq!(source, BillingSource::ApiKey);
        assert_eq!(
            determine_cost_basis(Some(hint), Some(&priced()), Some("openai")),
            "actual"
        );
    }

    /// Guarded invariant: OAuth transport ALONE does not flip a provider to a
    /// subscription plan. A hypothetical metered OAuth provider (whose OAuth
    /// flow is NOT a subscription) stays `MeteredApi` → `actual`, and is
    /// recorded as `api_key`, not `plan_oauth`.
    #[test]
    fn billing_signal_oauth_transport_alone_is_not_a_plan() {
        // `anthropic` has no subscription OAuth flow, so even an OAuth-backed
        // session on it must stay metered.
        let (hint, source) =
            derive_billing_signal("anthropic", "claude-opus-4-8", /* oauth */ true);
        assert_eq!(hint, CostBasisHint::MeteredApi);
        assert_eq!(source, BillingSource::ApiKey);
        assert_eq!(
            determine_cost_basis(Some(hint), Some(&priced()), Some("anthropic")),
            "actual"
        );
    }

    /// A coding-plan provider stays a `SubscriptionPlan` via the catalog string
    /// rule even on an API-key transport — its `billing_source` is `api_key`
    /// (the plan nature is carried by `cost_basis`), and it books `projected`.
    #[test]
    fn billing_signal_coding_plan_api_key_is_projected() {
        let (hint, source) =
            derive_billing_signal("zai-coding-plan", "glm-5.2", /* oauth */ false);
        assert_eq!(hint, CostBasisHint::SubscriptionPlan);
        assert_eq!(source, BillingSource::ApiKey);
        assert_eq!(
            determine_cost_basis(Some(hint), Some(&priced()), Some("zai-coding-plan")),
            "projected"
        );
    }

    #[test]
    fn quiet_failures_map_to_failure_class() {
        // Invalid-request / invalid-output / 5xx are "quiet but broken"
        // → gentle consecutive-failure breaker.
        for e in [
            ProviderError::InvalidRequest,
            ProviderError::InvalidOutput,
            ProviderError::ProviderInternal { status: 500 },
            ProviderError::ProviderInternal { status: 503 },
        ] {
            assert_eq!(
                classify_provider_failure(&typed(e.clone())),
                Some(ProviderFailureClass::Failure),
                "{e:?} should map to Failure",
            );
        }
    }

    #[test]
    fn auth_maps_to_auth_invalid_class() {
        // A 401/403 is deterministic — it feeds the immediate-failover breaker
        // (AuthInvalid), NOT the gentle 3-strike Failure path.
        assert_eq!(
            classify_provider_failure(&typed(ProviderError::Authentication)),
            Some(ProviderFailureClass::AuthInvalid),
        );
    }

    #[test]
    fn rate_limit_maps_to_throttle_class() {
        // No provider-stated reset → Throttle with `retry_after_ms: None`; the
        // coordinator falls back to the ordinary escalating ladder.
        assert_eq!(
            classify_provider_failure(&typed(ProviderError::RateLimit {
                retry_after_ms: None
            })),
            Some(ProviderFailureClass::Throttle {
                retry_after_ms: None
            }),
        );
        // A provider-stated Retry-After is carried through verbatim so the
        // coordinator can floor the redispatch cooldown on it (A6).
        assert_eq!(
            classify_provider_failure(&typed(ProviderError::RateLimit {
                retry_after_ms: Some(60_000)
            })),
            Some(ProviderFailureClass::Throttle {
                retry_after_ms: Some(60_000)
            }),
        );
    }

    #[test]
    fn breaker_excluded_variants_map_to_none() {
        // ContextOverflow → reactive compaction (not a model-health problem), so
        // it must NOT feed the model-health breaker. (Transport is NO LONGER
        // excluded — a hard transport death feeds the gentle consecutive-failure
        // breaker; see `transport_error_is_breaker_worthy`. EmptyCompletion is NO
        // LONGER excluded either — it is now the Codex/OpenAI implicit throttle;
        // see `empty_completion_maps_to_throttle_with_synthesized_window`.)
        assert_eq!(
            classify_provider_failure(&typed(ProviderError::ContextOverflow)),
            None,
            "ContextOverflow must not feed the breaker",
        );
    }

    #[test]
    fn empty_completion_maps_to_throttle_with_synthesized_window() {
        // An exhausted Codex/OpenAI empty-200 streak is an account-quota THROTTLE,
        // not a broken provider. It must route to the immediate-failover
        // `Throttle` class (→ `record_stall`), NOT `record_failure`, and — since
        // the empty 200 carries no `Retry-After` header — floor the redispatch
        // cooldown on the conservative synthesized quota window so the next
        // dispatch doesn't re-probe the still-exhausted account on the fast ladder.
        assert_eq!(
            classify_provider_failure(&typed(ProviderError::EmptyCompletion)),
            Some(ProviderFailureClass::Throttle {
                retry_after_ms: Some(CODEX_EMPTY_QUOTA_RETRY_AFTER_MS),
            }),
        );
        // Sanity-check the constant is a conservative multi-minute window, not a
        // few-second probe value.
        const { assert!(CODEX_EMPTY_QUOTA_RETRY_AFTER_MS >= 10 * 60 * 1000) };
    }

    #[test]
    fn untyped_error_maps_to_none() {
        // A non-provider failure (git push, tool error, finalize misuse) carries
        // no `ProviderError` source → must never trip the breaker.
        let untyped = anyhow::anyhow!("worker failed to push task_branch to mirror");
        assert_eq!(classify_provider_failure(&untyped), None);
    }

    #[test]
    fn reply_loop_guard_error_maps_to_typed_stage_outcome_not_provider_failure() {
        let signature = crate::actors::slot::reply_loop::loop_guard::ToolCallSignature::new(
            "shell",
            &serde_json::json!({"command": "false"}),
        );
        let error = LoopGuardError {
            condition: crate::actors::slot::reply_loop::loop_guard::LoopGuardCondition {
                reason: crate::actors::slot::reply_loop::loop_guard::LoopGuardReason::RepeatedToolFailure {
                    signature,
                },
                observed: 4,
                threshold: 3,
            },
            turn_span: (2, 5),
            session_id: "session-123".to_string(),
        };
        let anyhow_error = anyhow::Error::new(error.clone());

        assert_eq!(
            classify_provider_failure(&anyhow_error),
            None,
            "loop-guard terminations must not masquerade as provider failures"
        );

        let trip = runtime_trip_for_reply_loop_guard_error(&error);
        assert_eq!(trip.kind, RuntimeLoopGuardKind::IdenticalToolFailure);
        assert_eq!(trip.observed, 4);
        assert_eq!(trip.threshold, 3);
        assert_eq!(trip.turn_span, (2, 5));
        assert_eq!(trip.session_id, "session-123");
        assert!(trip.offending_signature.contains("shell"));
        assert!(trip.offending_signature.contains(r#"{"command":"false"}"#));

        match stage_outcome_for_reply_loop_guard_error(&error) {
            StageOutcome::LoopGuardTripped {
                kind,
                offending_signature,
                threshold,
                observed,
                turn_span,
                session_id,
            } => {
                assert_eq!(kind, RuntimeLoopGuardKind::IdenticalToolFailure);
                assert_eq!(offending_signature, trip.offending_signature);
                assert_eq!(threshold, 3);
                assert_eq!(observed, 4);
                assert_eq!(turn_span, (2, 5));
                assert_eq!(session_id, "session-123");
            }
            other => panic!("expected typed loop-guard stage outcome, got {other:?}"),
        }
    }

    #[test]
    fn budget_park_settles_completed_with_parked_reason_even_when_wind_down_ignored() {
        let ignored_outcome = StageOutcome::Parked {
            reason: ParkReason::Budget,
            summary: None,
            wind_down_ignored: true,
            session_id: "session-budget-ignored".to_string(),
            tokens_in: 10,
            tokens_out: 5,
        };

        assert_eq!(
            session_settlement_for_stage_outcome(&ignored_outcome, false),
            (SessionStatus::Completed, Some("budget".to_string())),
            "typed ignored budget wind-downs must settle as completed parks, not failures"
        );

        let summary_outcome = StageOutcome::Parked {
            reason: ParkReason::Budget,
            summary: Some("handoff summary".to_string()),
            wind_down_ignored: false,
            session_id: "session-budget-summary".to_string(),
            tokens_in: 10,
            tokens_out: 5,
        };
        assert_eq!(
            session_settlement_for_stage_outcome(&summary_outcome, true),
            (SessionStatus::Completed, Some("budget".to_string()))
        );
    }

    #[test]
    fn non_budget_stage_settlement_keeps_existing_success_and_failure_statuses() {
        assert_eq!(
            session_settlement_for_stage_outcome(&StageOutcome::WorkerDone, true),
            (SessionStatus::Completed, None)
        );
        assert_eq!(
            session_settlement_for_stage_outcome(
                &StageOutcome::Failed {
                    reason: "ordinary failure".to_string(),
                    provider_failure: None,
                },
                false,
            ),
            (SessionStatus::Failed, None)
        );
    }

    #[test]
    fn streaming_wrapped_auth_error_still_classifies() {
        // Regression for the bug where the streaming reply loop rebuilt the error
        // as a formatted string (`anyhow!("...display={e}...", e)`), erasing the
        // typed `ProviderError` source so `classify_provider_failure` returned
        // `None` and the per-(scope,model) health breaker never tripped — dispatch
        // then re-selected a dead model forever instead of failing over.
        //
        // Build the error exactly as it flows in production: the provider client
        // mints a typed `ProviderError` wrapped with a "provider API error 401"
        // context, and the streaming loop wraps THAT again with its diagnostics.
        let from_client = anyhow::Error::new(ProviderError::Authentication)
            .context("provider API error 401: {\"code\":\"token_revoked\"}");
        let from_streaming = from_client.context(
            "provider stream event failed: display=provider API error 401 ...; fs_diag; env_diag",
        );
        assert_eq!(
            classify_provider_failure(&from_streaming),
            Some(ProviderFailureClass::AuthInvalid),
            "a 401 wrapped through the streaming loop must still feed the breaker",
        );

        // And document the anti-pattern: formatting the error into a fresh string
        // (what the bug did) loses the typed source and must NOT be reintroduced.
        let inner =
            anyhow::Error::new(ProviderError::Authentication).context("provider API error 401");
        let stringified = anyhow::anyhow!("provider stream event failed: display={inner}");
        assert_eq!(
            classify_provider_failure(&stringified),
            None,
            "stringifying the error erases the type — this is the regression we fixed",
        );
    }

    #[test]
    fn transport_error_is_breaker_worthy() {
        // Regression for the kimi-for-coding/k2p7 incident: a model that dies as a
        // hard `Transport` failure (connection refused / instant timeout / broken
        // stream, 0 tokens) previously classified to `None`, so the per-(scope,
        // model) breaker never tripped and dispatch re-selected the dead model
        // forever (also never surfacing it in model_health). A Transport death
        // must now feed the GENTLE consecutive-failure breaker (`Failure`).
        assert_eq!(
            classify_provider_failure(&anyhow::Error::new(ProviderError::Transport)),
            Some(ProviderFailureClass::Failure),
            "a hard transport death must feed the consecutive-failure breaker",
        );

        // And it must survive the same context/stream wrapping the auth case does:
        // the provider client wraps the typed error with a "provider API error"
        // context and the streaming loop wraps THAT again with diagnostics. As long
        // as the typed `ProviderError` stays the source, the downcast still finds it.
        let from_client = anyhow::Error::new(ProviderError::Transport)
            .context("provider request failed: connection reset by peer");
        let from_streaming = from_client.context(
            "provider stream event failed: display=connection reset ...; fs_diag; env_diag",
        );
        assert_eq!(
            classify_provider_failure(&from_streaming),
            Some(ProviderFailureClass::Failure),
            "a transport error wrapped through the streaming loop must still feed the breaker",
        );
    }

    #[test]
    fn context_overflow_stays_out_of_the_breaker() {
        // ContextOverflow is handled by reactive compaction (the conversation is
        // too big), so it must stay `None` even after the Transport change.
        // (EmptyCompletion is no longer `None` — it is the Codex/OpenAI implicit
        // throttle and maps to `Throttle`; see
        // `empty_completion_maps_to_throttle_with_synthesized_window`.)
        assert_eq!(
            classify_provider_failure(&anyhow::Error::new(ProviderError::ContextOverflow)),
            None,
            "ContextOverflow is handled by reactive compaction, not the model-health breaker",
        );
    }

    #[test]
    fn reviewer_no_verdict_releases_not_rejects() {
        // Bug 4: a reviewer that runs but never calls submit_review (no verdict)
        // must NOT map to ReviewerRejected — that would fire `task_review_reject`
        // (in_task_review → open, reopen_count++) and FALSE-REJECT good work,
        // reopening the worker's PR over a malfunctioning reviewer model. It must
        // instead be a non-terminal Failed so the supervisor leaves the task in
        // in_task_review and the coordinator releases it → needs_task_review for a
        // FRESH reviewer.
        let outcome = reviewer_stage_outcome("", None);
        match outcome {
            StageOutcome::Failed {
                provider_failure, ..
            } => {
                assert_eq!(
                    provider_failure, None,
                    "a no-verdict completion is a structural reviewer failure, not a typed provider error",
                );
            }
            other => {
                panic!("expected non-terminal Failed for a no-verdict reviewer, got {other:?}")
            }
        }
        assert!(
            !matches!(
                reviewer_stage_outcome("", None),
                StageOutcome::ReviewerRejected { .. }
            ),
            "a no-verdict reviewer must never map to ReviewerRejected (would reopen the worker's PR)",
        );
    }

    #[test]
    fn reviewer_explicit_reject_still_rejects() {
        // The genuine submit_review(reject) path must stay intact (still reopens
        // the PR via task_review_reject), in both present- and past-tense forms,
        // and must carry the reviewer's feedback through.
        let reject = serde_json::json!({"verdict": "reject", "feedback": "missing tests"});
        assert!(
            matches!(
                reviewer_stage_outcome("submit_review", Some(&reject)),
                StageOutcome::ReviewerRejected { feedback } if feedback == "missing tests"
            ),
            "an explicit reject verdict must still map to ReviewerRejected with its feedback",
        );

        let rejected_past = serde_json::json!({"verdict": "rejected", "feedback": "regression"});
        assert!(
            matches!(
                reviewer_stage_outcome("submit_review", Some(&rejected_past)),
                StageOutcome::ReviewerRejected { feedback } if feedback == "regression"
            ),
            "past-tense 'rejected' must also map to ReviewerRejected",
        );
    }

    #[test]
    fn reviewer_reject_with_all_criteria_met_is_coerced_to_approve() {
        // Regression (2026-07-01, task 55i8): a reviewer that marks every
        // acceptance criterion met but emits `rejected` is self-contradictory —
        // it bounced the task to `open` (+reopen) and cycled it into planner
        // escalation. Such a verdict must be treated as an approval, keyed on the
        // structured AC-met array (not feedback prose).
        let contradictory = serde_json::json!({
            "verdict": "rejected",
            "feedback": "this submission is acceptable as it stands and does not block the P0 epic",
            "acceptance_criteria": [
                {"criterion": "a", "met": true},
                {"criterion": "b", "met": true},
            ],
        });
        assert!(
            matches!(
                reviewer_stage_outcome("submit_review", Some(&contradictory)),
                StageOutcome::ReviewerApproved
            ),
            "a `rejected` verdict with all criteria met must be coerced to approve, not reopen the task",
        );
    }

    #[test]
    fn reviewer_reject_with_an_unmet_criterion_still_rejects() {
        // A genuine blocker is expressed by marking at least one criterion unmet;
        // that must keep the reject (and its feedback) intact.
        let genuine = serde_json::json!({
            "verdict": "rejected",
            "feedback": "criterion b not covered",
            "acceptance_criteria": [
                {"criterion": "a", "met": true},
                {"criterion": "b", "met": false},
            ],
        });
        assert!(
            matches!(
                reviewer_stage_outcome("submit_review", Some(&genuine)),
                StageOutcome::ReviewerRejected { feedback } if feedback == "criterion b not covered"
            ),
            "a reject with an unmet criterion must still reject",
        );
    }

    #[test]
    fn reviewer_explicit_approve_is_approved() {
        let approve = serde_json::json!({"verdict": "approve"});
        assert!(matches!(
            reviewer_stage_outcome("submit_review", Some(&approve)),
            StageOutcome::ReviewerApproved
        ));
        let approved_past = serde_json::json!({"verdict": "approved"});
        assert!(matches!(
            reviewer_stage_outcome("submit_review", Some(&approved_past)),
            StageOutcome::ReviewerApproved
        ));
    }

    #[test]
    fn reviewer_request_lead_escalates() {
        let payload = serde_json::json!({"reason": "needs architectural call"});
        assert!(
            matches!(
                reviewer_stage_outcome("request_lead", Some(&payload)),
                StageOutcome::Escalate { reason } if reason == "needs architectural call"
            ),
            "request_lead must escalate to the lead, carrying the stated reason",
        );
    }
}
