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
    build_provider_from_resolved, build_restamp_target, build_telemetry_meta_with_attribution,
    default_base_url, resolved_needs_base_url,
};
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::memory_intent_planner::{
    MEMORY_INTENT_PLANNER_PROMPT_ID, PlannerInput, parse_planned_queries, prepare_planner_request,
};
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
use crate::context::MemoryIntentPlannerConfig;
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
        "request_planner" => StageOutcome::Escalate {
            reason: payload_str("reason")
                .filter(|v| !v.is_empty())
                .or_else(|| payload_str("message").filter(|v| !v.is_empty()))
                .or_else(|| payload_str("summary").filter(|v| !v.is_empty()))
                .unwrap_or("reviewer escalated to planner")
                .to_string(),
        },
        // Deprecated drain compatibility: stale request_lead from a
        // pre-cutover reviewer session routes to Planner (not Lead) and
        // is treated as deprecated planner escalation/failure.
        "request_lead" => {
            tracing::warn!(
                "deprecated request_lead finalize tool called by reviewer; \
                 routing to planner escalation (drain compatibility)"
            );
            StageOutcome::Escalate {
                reason: format!(
                    "deprecated request_lead: {}",
                    payload_str("reason")
                        .filter(|v| !v.is_empty())
                        .unwrap_or("reviewer escalated via deprecated request_lead"),
                ),
            }
        }
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

/// Map a finished lead/arbiter stage's finalize tool + payload onto a
/// [`StageOutcome`]. Pure (no `task`/tracing deps) so the arbiter decision
/// validation branches are unit-testable.
///
/// Validates that:
/// - `approve` / `approve_conflict` require an `evidence` object with non-empty `summary`.
/// - `reopen` requires non-empty `directive` and `verification_command`.
/// - `park` requires a `park_dossier` object with non-empty `hold_description`
///   and `failure_analysis`.
/// - `supersede` requires a non-empty `created_tasks` array (the replacement
///   subtask ids); an empty list is rejected with guidance to `park` instead.
/// - Legacy decisions (`escalate`, `decompose`, `force_close`) are rejected.
fn lead_stage_outcome(
    finalize_name: &str,
    finalize_payload: Option<&serde_json::Value>,
) -> StageOutcome {
    let payload_str = |key: &str| {
        finalize_payload
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_str())
    };
    let reason = || -> Option<String> {
        for key in ["reason", "message", "summary"] {
            if let Some(v) = payload_str(key).filter(|v| !v.is_empty()) {
                return Some(v.to_string());
            }
        }
        None
    };
    match finalize_name {
        "submit_decision" => {
            let decision = payload_str("decision").unwrap_or("");
            match decision {
                // approve: work is complete + correct.
                // Requires evidence citation.
                "approve" => {
                    let evidence = finalize_payload.and_then(|p| p.get("evidence"));
                    if evidence.is_none()
                        || evidence
                            .and_then(|e| e.get("summary"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.is_empty())
                            .unwrap_or(true)
                    {
                        StageOutcome::Failed {
                            reason: "approve decision requires evidence \
                                     with non-empty summary"
                                .into(),
                            provider_failure: None,
                        }
                    } else {
                        StageOutcome::LeadApproved {
                            evidence: evidence
                                .map(|e| {
                                    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string())
                                })
                                .unwrap_or_else(|| "{}".to_string()),
                        }
                    }
                }
                // approve_conflict: approved but merge conflict.
                // Requires evidence citation.
                "approve_conflict" => {
                    let evidence = finalize_payload.and_then(|p| p.get("evidence"));
                    let has_valid_evidence = evidence.is_some()
                        && evidence
                            .and_then(|e| e.get("summary"))
                            .and_then(|v| v.as_str())
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    if !has_valid_evidence {
                        StageOutcome::Failed {
                            reason: "approve_conflict decision requires \
                                     evidence with non-empty summary"
                                .into(),
                            provider_failure: None,
                        }
                    } else {
                        StageOutcome::LeadApproveConflict {
                            reason: reason()
                                .unwrap_or_else(|| "lead approved with merge conflict".into()),
                            evidence: evidence
                                .map(|e| {
                                    serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string())
                                })
                                .unwrap_or_else(|| "{}".to_string()),
                        }
                    }
                }
                // reopen: rescoped/guided/blocked-on-deps.
                // Requires non-empty directive and verification_command.
                "reopen" => {
                    let directive = payload_str("directive").unwrap_or("");
                    let verification_command = payload_str("verification_command").unwrap_or("");
                    if directive.is_empty() {
                        StageOutcome::Failed {
                            reason: "reopen decision requires a non-empty \
                                     'directive' field"
                                .into(),
                            provider_failure: None,
                        }
                    } else if verification_command.is_empty() {
                        StageOutcome::Failed {
                            reason: "reopen decision requires a non-empty \
                                     'verification_command' field"
                                .into(),
                            provider_failure: None,
                        }
                    } else {
                        let exclude_models: Vec<String> = finalize_payload
                            .and_then(|p| p.get("exclude_models"))
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default();
                        StageOutcome::LeadReopen {
                            reason: reason().unwrap_or_else(|| "lead reopened task".into()),
                            directive: directive.to_string(),
                            verification_command: verification_command.to_string(),
                            exclude_models,
                        }
                    }
                }
                // park: hold for human review with structured dossier.
                "park" => {
                    let dossier = finalize_payload.and_then(|p| p.get("park_dossier"));
                    match dossier {
                        None => StageOutcome::Failed {
                            reason: "park decision requires a 'park_dossier' \
                                     object with 'hold_description' and \
                                     'failure_analysis' fields"
                                .into(),
                            provider_failure: None,
                        },
                        Some(d) => {
                            let hold_desc = d
                                .get("hold_description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let failure_analysis = d
                                .get("failure_analysis")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if hold_desc.is_empty() || failure_analysis.is_empty() {
                                StageOutcome::Failed {
                                    reason: "park_dossier requires non-empty \
                                             'hold_description' and \
                                             'failure_analysis' fields"
                                        .into(),
                                    provider_failure: None,
                                }
                            } else {
                                let dossier_json =
                                    serde_json::to_string(d).unwrap_or_else(|_| "{}".to_string());
                                StageOutcome::LeadParked {
                                    park_dossier_json: dossier_json,
                                }
                            }
                        }
                    }
                }
                // supersede: the arbiter decomposed the task into replacement
                // subtasks that carry the work forward. Force-close the source
                // (and its PR) as superseded — no human-review hold. Valid ONLY
                // when `created_tasks` is non-empty; an empty list means there is
                // no autonomous resolution, so the arbiter must `park` instead.
                "supersede" => {
                    let created: Vec<String> = finalize_payload
                        .and_then(|p| p.get("created_tasks"))
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    if created.is_empty() {
                        StageOutcome::Failed {
                            reason: "supersede decision requires a non-empty \
                                     'created_tasks' array of the replacement \
                                     subtask IDs that carry the work forward; if \
                                     no autonomous resolution exists, use park \
                                     instead"
                                .into(),
                            provider_failure: None,
                        }
                    } else {
                        StageOutcome::LeadSuperseded {
                            reason: reason().unwrap_or_else(|| {
                                "arbiter superseded task with replacement subtasks".into()
                            }),
                            replacement_task_ids: created,
                        }
                    }
                }
                // Legacy decisions removed: escalate, decompose,
                // force_close are no longer valid arbiter outcomes.
                other => StageOutcome::Failed {
                    reason: format!(
                        "lead submitted unknown or removed decision '{other}'; \
                         valid arbiter decisions are: approve, approve_conflict, \
                         reopen, park, supersede"
                    ),
                    provider_failure: None,
                },
            }
        }
        "" => StageOutcome::Failed {
            reason: "lead session ended without calling submit_decision".into(),
            provider_failure: None,
        },
        other => StageOutcome::Failed {
            reason: format!("lead finalized via unexpected tool '{other}'"),
            provider_failure: None,
        },
    }
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
    // Picks up `system_prompt_extensions`, role-level MCP
    // server + skill lists, and swaps `runtime_role`
    // when a Worker stage's `task.agent_type` names a specialist whose
    // `base_role` differs from the injected RoleKind.  Non-Worker stages
    // always use the default-role path.
    let ResolvedRoleOverrides {
        runtime_role,
        system_prompt_extensions,
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
        is_evidence_spike = spec.is_evidence_spike,
        tool_profile = if spec.is_evidence_spike { "evidence_spike" } else { "standard" },
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
        mcp_server_instructions,
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

    // Create the role session before prompt assembly so retrieval traces use real identities.
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

    // The default-off gate precedes prompt rendering and host I/O. This small local config is intentionally environment-free; deployments inject the explicit opt-in at process launch.
    let planner_config = MemoryIntentPlannerConfig::default();
    let planned_queries = if let Some(request) = prepare_planner_request(
        &planner_config,
        PlannerInput {
            title: task.title.clone(),
            description: task.description.clone(),
            acceptance_criteria: serde_json::from_str::<Vec<serde_json::Value>>(
                &task.acceptance_criteria,
            )
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.to_string())
            .collect(),
            resume_compaction_summary: spec
                .resume_lifecycle_metadata
                .as_ref()
                .and_then(|m| m.last_durable_progress_summary.clone()),
        },
    ) {
        let mut planner_conversation = Conversation::new();
        planner_conversation.push(Message::user(request.prompt));
        let attributed = djinn_supervisor::services::wire::AttributedPlannerRequest {
            project_id: task.project_id.clone(),
            task_id: task.id.clone(),
            task_run_id: Some(task_run_id.to_string()),
            session_id: Some(session_id.clone()),
            created_by_user_id: task.created_by_user_id.clone(),
            operation: "memory_intent_planner".into(),
            prompt_id: MEMORY_INTENT_PLANNER_PROMPT_ID.into(),
            conversation: serde_json::to_string(&planner_conversation).unwrap_or_default(),
            tools: "[]".into(),
            tool_choice: None,
            max_tokens: planner_config.max_output as u32,
            timeout_ms: planner_config.timeout.as_millis() as u64,
        };
        match services.plan_memory_intents(attributed).await {
            Ok(result)
                if matches!(
                    result.outcome,
                    djinn_supervisor::services::wire::PlannerOutcome::Success
                ) =>
            {
                result
                    .content
                    .as_deref()
                    .and_then(|raw| parse_planned_queries(raw).ok())
            }
            _ => None,
        }
    } else {
        None
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
    // zkk9: load the arbiter directive for a monitored reopen. Only injected
    // for worker-role prompts when the latest unconsumed arbitration row has a
    // monitored reopen in progress (monitored_reopen_count >= 1). Non-worker
    // roles (planner/reviewer/lead/architect) never receive the directive.
    let arbiter_directive = crate::actors::slot::lifecycle::prompt_context::load_arbiter_directive(
        runtime_role_name,
        &task.id,
        agent_context,
    )
    .await;
    if arbiter_directive.is_some() {
        tracing::info!(
            task_id = %task.short_id,
            task_run_id = %task_run_id,
            role = %runtime_role_name,
            "Supervisor stage: injected arbiter directive for monitored reopen"
        );
    }
    // Prompt-context progress marker after the role session has been created:
    // Prompt assembly happens here,
    // before the reply loop starts.
    let _ = services
        .report_stage_step(djinn_runtime::stage_step::CONTEXT_BUILD)
        .await;
    let PromptContext {
        system_prompt,
        system_prompt_hash,
        ..
    } = assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: runtime_role.as_ref(),
        role_for_epic_check: role.as_ref(),
        project_path: &project_path_str,
        worktree_path,
        conflict_ctx: conflict_ctx.as_ref(),
        merge_validation_ctx: None,
        prompt_setup_commands,
        system_prompt_extensions: &system_prompt_extensions,
        resolved_skills: &resolved_skills,
        app_state: agent_context,
        knowledge_identity: Some(
            crate::actors::slot::lifecycle::prompt_context::KnowledgeContextIdentity {
                session_id: &session_id,
                task_run_id,
                created_by_user_id: task.created_by_user_id.as_deref(),
                resume_progress_summary: spec
                    .resume_lifecycle_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.last_durable_progress_summary.as_deref()),
            },
        ),
        planned_queries: planned_queries.as_deref(),
        read_sources: &read_sources,
        worker_resume_note: worker_resume_note.as_deref(),
        arbiter_directive: arbiter_directive.as_deref(),
        mcp_server_instructions: &mcp_server_instructions,
    })
    .await;

    // 7ry9: Emit session-start structured telemetry with the provider-facing
    // prompt hash. The hash is already computed from the final truncated
    // system prompt; no prompt contents are emitted.
    tracing::info!(
        event = "session_start",
        session_id = %session_id,
        task_id = %task.short_id,
        agent_type = %runtime_role_name,
        prompt_hash = %system_prompt_hash,
        prompt_hash_input = "rendered_system_prompt_v1",
        "Supervisor stage: session started with rendered system prompt hash"
    );
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
        // Build a RestampTarget from catalog metadata so model-dependent
        // defaults (reasoning_effort, max_tokens_default, format_family,
        // tool_schema_compat) reflect the target model.
        let restamp_target = build_restamp_target(
            &resolved.catalog_provider_id,
            &resolved.model_name,
            context_window.max(0) as u32,
            &agent_context.catalog,
        );
        let built = match build_provider_from_resolved(
            resolved,
            context_window.max(0) as u32,
            Some(telemetry_meta),
            Some(session_id.clone()),
            base_url,
            &restamp_target,
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
    // The profile was resolved at dispatch time and propagated via
    // `TaskRunSpec::is_evidence_spike`.  Detection is strict: both labels
    // must be present.  Tasks that carry only one label (or have malformed
    // metadata) fall through to the normal role tool surface —
    // deny-by-default is enforced by the restricted profile itself, not by
    // a separate deny gate.
    let is_evidence_spike = spec.is_evidence_spike;
    let mut tools = if is_evidence_spike {
        crate::extension::tool_schemas_evidence_spike()
    } else {
        crate::roles::tool_schemas_for(agent_type)
    };
    if let Some(ref registry) = mcp_registry {
        tools.extend_from_slice(registry.tool_schemas());
        // Append native MCP resource tools only when at least one connected
        // server advertised the `resources` capability.  These are ordinary
        // native tools (not remote `mcp__...` server tools) and are
        // read-only/non-destructive.
        if registry.has_resource_servers() {
            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "list_mcp_resources",
                    "description": "List MCP resources from connected servers. \
                        Returns resource metadata (URI, name, description, MIME type). \
                        Omit `server` to list from all resource-capable servers.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "server": {
                                "type": "string",
                                "description": "Optional server name to list resources from. \
                                    Omit to list from all resource-capable servers."
                            }
                        },
                        "required": [],
                        "additionalProperties": false
                    }
                },
                "readOnly": true,
                "destructive": false,
                "idempotent": true,
                "openWorld": false,
                "concurrent_safe": true
            }));
            tools.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_mcp_resource",
                    "description": "Read a specific MCP resource by URI from a named server. \
                        Returns the resource content as text; binary resources \
                        produce an omission message.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "server": {
                                "type": "string",
                                "description": "The MCP server name that hosts the resource."
                            },
                            "uri": {
                                "type": "string",
                                "description": "The URI of the resource to read."
                            }
                        },
                        "required": ["server", "uri"],
                        "additionalProperties": false
                    }
                },
                "readOnly": true,
                "destructive": false,
                "idempotent": true,
                "openWorld": false,
                "concurrent_safe": true
            }));
        }
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
    let revision_caller =
        djinn_core::auth_context::TrustedRevisionCallerContext::authenticated_agent(
            runtime_role_name,
        )
        .map(|context| {
            context.with_execution_provenance(
                Some(session_id.clone()),
                Some(task.id.clone()),
                Some(task_run_id.to_owned()),
            )
        });
    let (reply_result, final_output, tokens_in, tokens_out, cache_read, cache_write) =
        djinn_core::auth_context::SESSION_USER_ID
            .scope(
                task.created_by_user_id.clone(),
                djinn_core::auth_context::REVISION_CALLER_CONTEXT
                    .scope(revision_caller, reply_loop_fut),
            )
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
                    RoleKind::Worker => {
                        worker_stage_outcome(finalize_name, final_output.finalize_payload.as_ref())
                    }
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
                    RoleKind::Lead => {
                        lead_stage_outcome(finalize_name, final_output.finalize_payload.as_ref())
                    }
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

/// Map a finished worker stage's finalize tool + payload onto a
/// [`StageOutcome`].  Pure (no `task`/tracing deps) so the worker
/// finalization branches are unit-testable.
///
/// Distinguishes:
/// - `submit_work` → `WorkerDone`
/// - `request_planner` → `Escalate` with the caller's reason
/// - `request_lead` (deprecated drain compat) → `Escalate` with a
///   deprecation-prefixed reason, routed to Planner by the supervisor
/// - empty string → `WorkerDone` (model stopped before calling finalize)
/// - anything else → `Failed`
fn worker_stage_outcome(
    finalize_name: &str,
    finalize_payload: Option<&serde_json::Value>,
) -> StageOutcome {
    match finalize_name {
        "submit_work" => StageOutcome::WorkerDone,
        "request_planner" => StageOutcome::Escalate {
            reason: extract_reason(&finalize_payload.cloned())
                .unwrap_or_else(|| "worker requested planner escalation".into()),
        },
        // Deprecated drain compatibility: stale request_lead from a
        // pre-cutover worker session routes to Planner (not Lead) as
        // deprecated planner escalation.
        "request_lead" => {
            tracing::warn!(
                "deprecated request_lead finalize tool called by worker; \
                 routing to planner escalation (drain compatibility)"
            );
            StageOutcome::Escalate {
                reason: format!(
                    "deprecated request_lead: {}",
                    extract_reason(&finalize_payload.cloned())
                        .unwrap_or_else(|| "worker escalated via deprecated request_lead".into())
                ),
            }
        }
        "" => StageOutcome::WorkerDone,
        other => StageOutcome::Failed {
            reason: format!("worker finalized via unexpected tool '{other}'"),
            provider_failure: None,
        },
    }
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
    fn reviewer_request_planner_escalates() {
        let payload = serde_json::json!({"reason": "needs architectural call"});
        assert!(
            matches!(
                reviewer_stage_outcome("request_planner", Some(&payload)),
                StageOutcome::Escalate { reason } if reason == "needs architectural call"
            ),
            "request_planner must escalate, carrying the stated reason",
        );
    }

    #[test]
    fn reviewer_deprecated_request_lead_escalates_to_planner() {
        // Deprecated request_lead should still produce Escalate (routed to
        // planner by the supervisor), with a deprecation-prefixed reason.
        let payload = serde_json::json!({"reason": "needs architectural call"});
        let outcome = reviewer_stage_outcome("request_lead", Some(&payload));
        match outcome {
            StageOutcome::Escalate { reason } => {
                assert!(
                    reason.contains("deprecated request_lead"),
                    "deprecated request_lead reason must be prefixed with deprecation marker, got: {reason}"
                );
                assert!(
                    reason.contains("needs architectural call"),
                    "deprecated request_lead reason must preserve caller's reason, got: {reason}"
                );
            }
            other => panic!("expected Escalate for deprecated request_lead, got {other:?}"),
        }
    }

    // ── Arbiter submit_decision payload validation ─────────────────────

    #[test]
    fn arbiter_approve_with_evidence_succeeds() {
        let payload = serde_json::json!({
            "decision": "approve",
            "evidence": {
                "source": "ci_run",
                "summary": "All 47 tests green in CI run #1234",
                "reference_id": "ci-1234"
            }
        });
        assert!(
            matches!(
                lead_stage_outcome("submit_decision", Some(&payload)),
                StageOutcome::LeadApproved { ref evidence } if !evidence.is_empty()
            ),
            "approve with valid evidence must succeed",
        );
    }

    #[test]
    fn arbiter_approve_without_evidence_fails() {
        let payload = serde_json::json!({
            "decision": "approve",
            "rationale": "looks good to me"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("evidence"),
                    "failure reason must mention evidence requirement, got: {reason}"
                );
            }
            other => panic!("expected Failed for approve without evidence, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_approve_with_empty_evidence_summary_fails() {
        let payload = serde_json::json!({
            "decision": "approve",
            "evidence": {
                "source": "ci_run",
                "summary": ""
            }
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("evidence"),
                    "failure reason must mention evidence, got: {reason}"
                );
            }
            other => panic!("expected Failed for empty evidence summary, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_approve_conflict_with_evidence_succeeds() {
        let payload = serde_json::json!({
            "decision": "approve_conflict",
            "reason": "correct but conflicts",
            "evidence": {
                "source": "review_round",
                "summary": "Reviewer approved in round 3"
            }
        });
        assert!(
            matches!(
                lead_stage_outcome("submit_decision", Some(&payload)),
                StageOutcome::LeadApproveConflict { ref reason, ref evidence } if reason == "correct but conflicts" && !evidence.is_empty()
            ),
            "approve_conflict with evidence and reason must succeed",
        );
    }

    #[test]
    fn arbiter_reopen_with_directive_and_command_succeeds() {
        let payload = serde_json::json!({
            "decision": "reopen",
            "directive": "Fix the off-by-one error in the pagination logic",
            "verification_command": "cargo test --test pagination",
            "exclude_models": ["gpt-4o"]
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::LeadReopen {
                directive,
                verification_command,
                exclude_models,
                ..
            } => {
                assert_eq!(
                    directive,
                    "Fix the off-by-one error in the pagination logic"
                );
                assert_eq!(verification_command, "cargo test --test pagination");
                assert_eq!(exclude_models, vec!["gpt-4o"]);
            }
            other => panic!("expected LeadReopen, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_reopen_without_directive_fails() {
        let payload = serde_json::json!({
            "decision": "reopen",
            "verification_command": "cargo test"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("directive"),
                    "failure reason must mention directive, got: {reason}"
                );
            }
            other => panic!("expected Failed for reopen without directive, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_reopen_without_verification_command_fails() {
        let payload = serde_json::json!({
            "decision": "reopen",
            "directive": "Fix the bug"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("verification_command"),
                    "failure reason must mention verification_command, got: {reason}"
                );
            }
            other => {
                panic!("expected Failed for reopen without verification_command, got {other:?}")
            }
        }
    }

    #[test]
    fn arbiter_park_with_dossier_succeeds() {
        let payload = serde_json::json!({
            "decision": "park",
            "park_dossier": {
                "hold_description": "Requires senior engineer review of auth flow",
                "failure_analysis": "Three attempts failed; auth logic needs domain expertise",
                "attempted_decisions": ["reopen", "reopen"],
                "recommended_action": "Assign to auth-team lead"
            }
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::LeadParked { park_dossier_json } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&park_dossier_json).expect("valid JSON");
                assert_eq!(
                    parsed["hold_description"],
                    "Requires senior engineer review of auth flow"
                );
                assert_eq!(
                    parsed["failure_analysis"],
                    "Three attempts failed; auth logic needs domain expertise"
                );
            }
            other => panic!("expected LeadParked, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_park_without_dossier_fails() {
        let payload = serde_json::json!({
            "decision": "park",
            "rationale": "stuck"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("park_dossier"),
                    "failure reason must mention park_dossier, got: {reason}"
                );
            }
            other => panic!("expected Failed for park without dossier, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_park_with_empty_hold_description_fails() {
        let payload = serde_json::json!({
            "decision": "park",
            "park_dossier": {
                "hold_description": "",
                "failure_analysis": "some analysis"
            }
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("hold_description"),
                    "failure reason must mention hold_description, got: {reason}"
                );
            }
            other => panic!("expected Failed for empty hold_description, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_legacy_escalate_rejected() {
        let payload = serde_json::json!({
            "decision": "escalate",
            "rationale": "cannot resolve"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("unknown or removed"),
                    "failure reason must indicate removed decision, got: {reason}"
                );
            }
            other => panic!("expected Failed for legacy escalate, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_legacy_decompose_rejected() {
        let payload = serde_json::json!({
            "decision": "decompose",
            "created_tasks": ["task-1", "task-2"]
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("unknown or removed"),
                    "failure reason must indicate removed decision, got: {reason}"
                );
            }
            other => panic!("expected Failed for legacy decompose, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_legacy_force_close_rejected() {
        let payload = serde_json::json!({
            "decision": "force_close",
            "rationale": "redundant"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("unknown or removed"),
                    "failure reason must indicate removed decision, got: {reason}"
                );
            }
            other => panic!("expected Failed for legacy force_close, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_no_finalize_tool_fails() {
        match lead_stage_outcome("", None) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("without calling submit_decision"),
                    "failure reason must mention missing finalize tool, got: {reason}"
                );
            }
            other => panic!("expected Failed for no finalize tool, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_unexpected_finalize_tool_fails() {
        match lead_stage_outcome("submit_work", None) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("unexpected tool"),
                    "failure reason must mention unexpected tool, got: {reason}"
                );
            }
            other => panic!("expected Failed for unexpected tool, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_park_dossier_round_trips_json() {
        // Verify the dossier is serialized as valid JSON with all fields preserved.
        let dossier = serde_json::json!({
            "hold_description": "CI flaky",
            "failure_analysis": "intermittent test",
            "attempted_decisions": ["reopen"],
            "recommended_action": "fix flaky test"
        });
        let payload = serde_json::json!({
            "decision": "park",
            "park_dossier": dossier
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::LeadParked { park_dossier_json } => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&park_dossier_json).expect("valid JSON");
                assert_eq!(parsed["hold_description"], "CI flaky");
                assert_eq!(parsed["failure_analysis"], "intermittent test");
                assert_eq!(parsed["attempted_decisions"][0], "reopen");
                assert_eq!(parsed["recommended_action"], "fix flaky test");
            }
            other => panic!("expected LeadParked, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_supersede_with_created_tasks_maps_to_lead_superseded() {
        let payload = serde_json::json!({
            "decision": "supersede",
            "rationale": "decomposed into 3 replacement subtasks",
            "created_tasks": ["repl-1", "repl-2", "repl-3"]
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::LeadSuperseded {
                replacement_task_ids,
                ..
            } => {
                assert_eq!(
                    replacement_task_ids,
                    vec![
                        "repl-1".to_string(),
                        "repl-2".to_string(),
                        "repl-3".to_string()
                    ],
                    "supersede must carry the created_tasks as replacement ids"
                );
            }
            other => panic!("expected LeadSuperseded, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_supersede_without_created_tasks_fails_with_park_guidance() {
        let payload = serde_json::json!({
            "decision": "supersede",
            "rationale": "meant to decompose but created nothing"
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("created_tasks"),
                    "failure reason must mention created_tasks, got: {reason}"
                );
                assert!(
                    reason.contains("park"),
                    "failure reason must direct the arbiter to park instead, got: {reason}"
                );
            }
            other => panic!("expected Failed for empty created_tasks, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_supersede_empty_created_tasks_array_fails() {
        // An explicit empty array must be treated the same as a missing field.
        let payload = serde_json::json!({
            "decision": "supersede",
            "created_tasks": []
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("created_tasks") && reason.contains("park"),
                    "empty created_tasks must fail with park guidance, got: {reason}"
                );
            }
            other => panic!("expected Failed for empty created_tasks array, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_supersede_blank_ids_are_dropped_and_fail_when_all_blank() {
        // Whitespace-only ids are not real replacements — after trimming they
        // leave an empty set, which must fail with park guidance.
        let payload = serde_json::json!({
            "decision": "supersede",
            "created_tasks": ["   ", ""]
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::Failed { reason, .. } => {
                assert!(
                    reason.contains("created_tasks"),
                    "blank-only created_tasks must fail, got: {reason}"
                );
            }
            other => panic!("expected Failed for blank created_tasks, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_existing_four_decisions_unchanged_by_supersede_addition() {
        // Regression guard: adding supersede must not alter the mapping of the
        // other four decisions.
        let approve = serde_json::json!({
            "decision": "approve",
            "evidence": {"source": "git diff + CI", "summary": "all AC met"}
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&approve)),
            StageOutcome::LeadApproved { .. }
        ));

        let reopen = serde_json::json!({
            "decision": "reopen",
            "directive": "fix the retry loop in dispatch.rs",
            "verification_command": "cargo test -p djinn-coordinator"
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&reopen)),
            StageOutcome::LeadReopen { .. }
        ));

        let park = serde_json::json!({
            "decision": "park",
            "park_dossier": {"hold_description": "stuck", "failure_analysis": "why"}
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&park)),
            StageOutcome::LeadParked { .. }
        ));

        let approve_conflict = serde_json::json!({
            "decision": "approve_conflict",
            "evidence": {"source": "git diff", "summary": "correct but conflict"}
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&approve_conflict)),
            StageOutcome::LeadApproveConflict { .. }
        ));
    }

    #[test]
    fn arbiter_reopen_excludes_models() {
        let payload = serde_json::json!({
            "decision": "reopen",
            "directive": "redo with better error handling",
            "verification_command": "cargo clippy",
            "exclude_models": ["model-a", "model-b"]
        });
        match lead_stage_outcome("submit_decision", Some(&payload)) {
            StageOutcome::LeadReopen { exclude_models, .. } => {
                assert_eq!(exclude_models, vec!["model-a", "model-b"]);
            }
            other => panic!("expected LeadReopen, got {other:?}"),
        }
    }

    #[test]
    fn arbiter_refinement_submit_decision_not_broken() {
        // Refinement tribunal roles use submit_decision as a session terminator
        // and map it to WorkerDone — this must not be affected by the arbiter
        // changes.
        // (This is tested indirectly: the RoleKind::Refinement arm is separate
        // from the RoleKind::Lead arm in execute_stage.)
    }

    // ── Worker stage outcome parsing (request_planner / deprecated
    //    request_lead) ────────────────────────────────────────────────────

    #[test]
    fn worker_submit_work_returns_worker_done() {
        assert!(
            matches!(
                worker_stage_outcome("submit_work", None),
                StageOutcome::WorkerDone
            ),
            "submit_work must produce WorkerDone",
        );
    }

    #[test]
    fn worker_empty_finalize_returns_worker_done() {
        // A worker that stops before calling any finalize tool must produce
        // WorkerDone (not Failed), matching the reviewer no-verdict semantics
        // but for workers.
        assert!(
            matches!(worker_stage_outcome("", None), StageOutcome::WorkerDone),
            "empty finalize name must produce WorkerDone (model stopped before finalize)",
        );
    }

    #[test]
    fn worker_request_planner_escalates() {
        let payload = serde_json::json!({"reason": "blocked on external API"});
        match worker_stage_outcome("request_planner", Some(&payload)) {
            StageOutcome::Escalate { reason } => {
                assert_eq!(
                    reason, "blocked on external API",
                    "request_planner must carry the worker's reason"
                );
            }
            other => panic!("expected Escalate for request_planner, got {other:?}"),
        }
    }

    #[test]
    fn worker_request_planner_escalates_with_message_fallback() {
        let payload = serde_json::json!({"message": "needs replanning"});
        match worker_stage_outcome("request_planner", Some(&payload)) {
            StageOutcome::Escalate { reason } => {
                assert_eq!(reason, "needs replanning");
            }
            other => panic!("expected Escalate for request_planner with message, got {other:?}"),
        }
    }

    #[test]
    fn worker_deprecated_request_lead_escalates_to_planner() {
        // Deprecated request_lead should produce Escalate (routed to Planner
        // by the supervisor), with a deprecation-prefixed reason. This is the
        // drain-compatibility path for stale worker sessions.
        let payload = serde_json::json!({"reason": "task too large"});
        match worker_stage_outcome("request_lead", Some(&payload)) {
            StageOutcome::Escalate { reason } => {
                assert!(
                    reason.contains("deprecated request_lead"),
                    "deprecated request_lead reason must be prefixed with deprecation marker, got: {reason}"
                );
                assert!(
                    reason.contains("task too large"),
                    "deprecated request_lead reason must preserve caller's reason, got: {reason}"
                );
            }
            other => panic!("expected Escalate for deprecated request_lead, got {other:?}"),
        }
    }

    #[test]
    fn worker_deprecated_request_lead_without_reason_uses_default() {
        // When a stale request_lead call has no reason, use a default string
        // rather than leaving the Escalate reason empty.
        match worker_stage_outcome("request_lead", None) {
            StageOutcome::Escalate { reason } => {
                assert!(
                    reason.contains("deprecated request_lead"),
                    "must have deprecation prefix, got: {reason}"
                );
                assert!(
                    reason.contains("worker escalated via deprecated request_lead"),
                    "must have default fallback reason, got: {reason}"
                );
            }
            other => panic!(
                "expected Escalate for deprecated request_lead without payload, got {other:?}"
            ),
        }
    }

    #[test]
    fn worker_deprecated_request_lead_does_not_produce_needs_lead_intervention() {
        // Critical invariant: deprecated request_lead must NOT produce
        // any outcome that transitions the task to needs_lead_intervention.
        // It must produce Escalate (Planner path).
        let payload = serde_json::json!({"reason": "stuck"});
        let outcome = worker_stage_outcome("request_lead", Some(&payload));
        assert!(
            !matches!(outcome, StageOutcome::Failed { .. }),
            "deprecated request_lead must NOT produce Failed (which could be misinterpreted downstream), got: {outcome:?}"
        );
        assert!(
            matches!(outcome, StageOutcome::Escalate { .. }),
            "deprecated request_lead must produce Escalate for planner routing, got: {outcome:?}"
        );
    }

    #[test]
    fn worker_unexpected_finalize_tool_fails() {
        match worker_stage_outcome("unknown_tool", None) {
            StageOutcome::Failed {
                reason,
                provider_failure,
            } => {
                assert!(
                    reason.contains("unknown_tool"),
                    "error must name the unexpected tool, got: {reason}"
                );
                assert_eq!(
                    provider_failure, None,
                    "unexpected tool failure is not a typed provider error"
                );
            }
            other => panic!("expected Failed for unexpected tool, got {other:?}"),
        }
    }

    // ── Grep-style structural guards (10qg) ────────────────────────────────
    // These tests use include_str! to read source files at compile time and
    // assert structural invariants about the codebase, catching regressions
    // that would otherwise require a manual code search.

    /// The old `[LEAD_REQUEST]` comment convention must not appear in the
    /// production `call_request_lead` handler body.  The deprecated handler
    // uses `deprecated_request_lead` typed activity instead.
    #[test]
    fn deprecated_request_lead_handler_does_not_use_lead_request_comment_convention() {
        // Read the task_epic handler source at compile time.
        let src = include_str!("../extension/handlers/task_epic.rs");

        // The `call_request_lead` function must exist (it's the drain compat path).
        assert!(
            src.contains("async fn call_request_lead"),
            "task_epic.rs must contain the call_request_lead handler"
        );

        // The function must NOT contain a [LEAD_REQUEST] comment in its body.
        // Find the function and check its body.
        let fn_start = src
            .find("async fn call_request_lead")
            .expect("call_request_lead must exist");
        // Find the next function definition (or end of file) to bound the search.
        let after_fn = &src[fn_start..];
        let fn_body_end = after_fn[28..]
            .find("\npub")
            .or_else(|| after_fn[28..].find("\nasync fn"))
            .map(|p| p + 28)
            .unwrap_or(after_fn.len());
        let fn_body = &after_fn[..fn_body_end];

        assert!(
            !fn_body.contains("[LEAD_REQUEST]"),
            "call_request_lead must NOT use the [LEAD_REQUEST] comment convention"
        );
        assert!(
            fn_body.contains("deprecated_request_lead"),
            "call_request_lead must log a deprecated_request_lead typed activity"
        );
        assert!(
            fn_body.contains("dispatch_planner_escalation"),
            "call_request_lead must route through dispatch_planner_escalation"
        );
        // Must NOT transition to needs_lead_intervention.
        // Strip `//` comment lines before searching so that explanatory
        // comments (e.g. "no needs_lead_intervention transition") don't
        // produce false positives — only actual code usage triggers failure.
        let code_only: String = fn_body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        assert!(
            !code_only.contains("needs_lead_intervention"),
            "call_request_lead must NOT transition task to needs_lead_intervention"
        );
    }

    /// The worker/reviewer `worker_stage_outcome` and `reviewer_stage_outcome`
    /// must treat `request_lead` as `Escalate` (planner path), never as a
    /// path that produces `needs_lead_intervention`.
    #[test]
    fn stage_outcome_request_lead_routes_to_escalate_not_needs_lead_intervention() {
        // Worker: request_lead → Escalate (not Failed, not any lead status)
        let worker_outcome =
            worker_stage_outcome("request_lead", Some(&serde_json::json!({"reason": "test"})));
        assert!(
            matches!(worker_outcome, StageOutcome::Escalate { .. }),
            "worker request_lead must produce Escalate, got: {worker_outcome:?}"
        );

        // Reviewer: request_lead → Escalate (not Failed, not any lead status)
        let reviewer_outcome =
            reviewer_stage_outcome("request_lead", Some(&serde_json::json!({"reason": "test"})));
        assert!(
            matches!(reviewer_outcome, StageOutcome::Escalate { .. }),
            "reviewer request_lead must produce Escalate, got: {reviewer_outcome:?}"
        );
    }
}
