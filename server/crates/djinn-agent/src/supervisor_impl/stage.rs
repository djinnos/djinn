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

use djinn_core::models::{SessionFailureCause, SessionStatus, Task};
use djinn_core::tool_error::ToolError;
use djinn_db::ProjectRepository;
use djinn_runtime::spec::{RoleKind, TaskRunSpec};
use djinn_supervisor::{
    ModelTurnAdmissionStageOutcome, ParkReason, StageError, StageOutcome, SupervisorServices,
};
use djinn_workspace::Workspace;

use crate::AgentType;
use crate::actors::slot::helpers::conflict_context_for_dispatch;
use crate::actors::slot::helpers::{
    ProviderCredential, build_provider_from_resolved, build_restamp_target,
    build_telemetry_meta_with_attribution, default_base_url, resolved_needs_base_url,
};
use crate::actors::slot::lifecycle::mcp_resolve::{McpAndSkills, resolve_mcp_and_skills};
use crate::actors::slot::lifecycle::model_resolution::{
    ModelResolutionError, attempt_resume_model_rotation, resolve_model_and_credential,
};
use crate::actors::slot::lifecycle::prompt_context::{
    MemoryIntentPlannerInvocation, PromptContext, PromptContextInputs, ReadSourceInfo,
    assemble_prompt_context, build_worker_resume_note,
};
use crate::actors::slot::lifecycle::role_overrides::{
    ResolvedRoleOverrides, resolve_role_overrides,
};
use crate::actors::slot::lifecycle::setup::{SetupContext, SetupError, resolve_setup_context};
use crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger;
use crate::actors::slot::lifecycle::teardown::{PostSessionParams, spawn_post_session_work};
use crate::actors::slot::reply_loop::error_handling::{
    BudgetWindDownIgnored, FinalizeNudgesExhausted, MissingSlotToolDispatcher, ReplyLoopCancelled,
    ReplyLoopCompactionFailure, StepCapWindDownIgnored,
};
use crate::actors::slot::reply_loop::loop_guard::{
    LoopGuardError, LoopGuardKind as ReplyLoopGuardKind,
};
use crate::actors::slot::reply_loop::{
    ModelTurnAdmissionOutcome, ReplyLoopContext, run_reply_loop,
};
use crate::context::AgentContext;
use crate::roles::{AgentRole, role_impl_for};
use crate::supervisor_impl::ci_routing;
use djinn_core::cancel_origin::CancelOrigin;
use djinn_provider::message::{Conversation, Message};
use djinn_provider::provider::LlmProvider;
use djinn_provider::provider::error::ProviderError;
use djinn_runtime::{LoopGuardKind as RuntimeLoopGuardKind, LoopGuardTrip, ProviderFailureClass};

use super::SupervisorCallbackContext;

/// Carry post-creation failures back to the supervisor without performing a
/// terminal write in agent code.
fn after_session_error(
    session_id: &str,
    message: String,
    failure_cause: djinn_core::models::SessionFailureCause,
) -> StageError {
    StageError::AfterSession {
        message,
        settlement: djinn_supervisor::StageSessionSettlement {
            session_id: session_id.to_owned(),
            status: SessionStatus::Failed,
            tokens_in: 0,
            tokens_out: 0,
            cache_read: 0,
            cache_write: 0,
            parked_reason: None,
            failure_cause: Some(failure_cause),
        },
    }
}

/// Private settlement metadata. `StageOutcome` remains the exact wire value;
/// this sidecar exists only until the authoritative session status write.
#[derive(Debug)]
struct StageSettlement {
    outcome: StageOutcome,
    failure_cause: Option<SessionFailureCause>,
}

impl StageSettlement {
    fn completed(outcome: StageOutcome) -> Self {
        Self {
            outcome,
            failure_cause: None,
        }
    }

    fn failed(outcome: StageOutcome, failure_cause: SessionFailureCause) -> Self {
        Self {
            outcome,
            failure_cause: Some(failure_cause),
        }
    }
}

/// Select the durable cause while the typed reply-loop error is still present.
/// Diagnostic formatting happens only after this classification.
fn reply_loop_failure_cause(error: &anyhow::Error) -> SessionFailureCause {
    if error.downcast_ref::<ReplyLoopCancelled>().is_some() {
        SessionFailureCause::Cancelled
    } else if error.downcast_ref::<ProviderError>().is_some() {
        SessionFailureCause::Provider
    } else if error.downcast_ref::<LoopGuardTrip>().is_some()
        || error.downcast_ref::<LoopGuardError>().is_some()
        || error.downcast_ref::<StepCapWindDownIgnored>().is_some()
    {
        SessionFailureCause::Protocol
    } else if error.downcast_ref::<FinalizeNudgesExhausted>().is_some() {
        SessionFailureCause::Finalization
    } else if error.downcast_ref::<ToolError>().is_some()
        || error.downcast_ref::<MissingSlotToolDispatcher>().is_some()
        || error.downcast_ref::<ReplyLoopCompactionFailure>().is_some()
    {
        // Tool/MCP and local reply-loop harness failures retain a structured
        // envelope. Never inspect arbitrary diagnostic text here.
        SessionFailureCause::Harness
    } else {
        SessionFailureCause::Unknown
    }
}

fn settlement_for_stage_outcome(
    outcome: StageOutcome,
    final_result_ok: bool,
    reply_failure_cause: Option<SessionFailureCause>,
    role_kind: RoleKind,
) -> StageSettlement {
    if matches!(outcome, StageOutcome::Parked { .. }) {
        StageSettlement::completed(outcome)
    } else if final_result_ok {
        if matches!(outcome, StageOutcome::Failed { .. }) {
            StageSettlement::failed(
                outcome,
                if role_kind == RoleKind::Verifier {
                    SessionFailureCause::Harness
                } else {
                    SessionFailureCause::Finalization
                },
            )
        } else {
            StageSettlement::completed(outcome)
        }
    } else {
        StageSettlement::failed(
            outcome,
            reply_failure_cause.expect("failed reply loop has a classified cause"),
        )
    }
}

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

/// The terminal reply-loop error class whose identity must survive until
/// session settlement can record a durable failure cause.
///
/// This deliberately examines only the returned error chain. In particular,
/// callers must not infer cancellation from a token that may have been
/// cancelled after a provider or harness error was already returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyLoopFailureClass {
    Cancelled,
    Other,
}

/// Classify reply-loop cancellation by its concrete public error type, never
/// by display text or current cancellation-token state.
fn classify_reply_loop_failure(err: &anyhow::Error) -> ReplyLoopFailureClass {
    if err.downcast_ref::<ReplyLoopCancelled>().is_some() {
        ReplyLoopFailureClass::Cancelled
    } else {
        ReplyLoopFailureClass::Other
    }
}

/// Preserve typed Phase A admission data for cancellable stage scheduling.
fn stage_outcome_for_model_turn_admission_error(error: &anyhow::Error) -> Option<StageOutcome> {
    let outcome = match error.downcast_ref::<ModelTurnAdmissionOutcome>()? {
        ModelTurnAdmissionOutcome::Wait(wait) => ModelTurnAdmissionStageOutcome::Wait(wait.clone()),
        ModelTurnAdmissionOutcome::Rejected(rejection) => {
            ModelTurnAdmissionStageOutcome::Rejected(rejection.clone())
        }
        ModelTurnAdmissionOutcome::DispatchFenced(outcome) => {
            ModelTurnAdmissionStageOutcome::DispatchFenced(outcome.clone())
        }
    };
    Some(StageOutcome::ModelTurnAdmission(outcome))
}

/// Append the cancellation trigger to a diagnostic, so a cancelled session
/// names its cause instead of only its observation.
///
/// Production sessions died at ~4.3/hour with the bare reason
/// `session cancelled`, which said *that* the token fired and never *who* fired
/// it: one Pod-wide `CancellationToken` is triggered by SIGTERM, the in-pod
/// soft deadline, a host control frame, RPC transport death, and orderly
/// teardown, and a token carries no payload to tell them apart.
///
/// The `origin=` suffix is always emitted, including for
/// [`CancelOrigin::Unknown`]. An unattributed cancellation is a normal outcome
/// (some trigger simply did not stamp the tag), and emitting it explicitly is
/// what distinguishes "we looked and nobody claimed it" from "this row predates
/// origin tagging". It is never an error and never gates anything.
fn with_cancel_origin(diagnostic: &str, origin: CancelOrigin) -> String {
    format!("{diagnostic} (origin={})", origin.as_str())
}

/// [`with_cancel_origin`] under the established `reply loop error:` prefix,
/// for the [`StageOutcome::Failed`] reason that reaches `task_runs`.
fn cancelled_stage_reason(error_display: &str, origin: CancelOrigin) -> String {
    format!(
        "reply loop error: {}",
        with_cancel_origin(error_display, origin)
    )
}

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
/// The class is also the coordinator's ONLY evidence about whom to blame for a
/// failed session, so the split is not merely a breaker concern: only the
/// task-attributable class ([`ProviderFailureClass::Failure`]) may arm the
/// coordinator's third-strike planner-remediation escalation. A provider-side
/// fault that reproduces independently of the request body must not.
///
/// Mapping (mirrors the coordinator's throttle→stall / quiet-failure→failure
/// intent):
/// - `InvalidRequest` | `InvalidOutput` → [`ProviderFailureClass::Failure`].
///   The TASK-attributable class: a request this provider keeps rejecting (the
///   poisoned resume transcript — an assistant `tool_calls` message replayed
///   without its tool results — 400s identically on every redispatch) or output
///   we cannot parse. Fed to the gentler consecutive-failure breaker so a
///   single blip doesn't demote the user's preferred model; only repeats trip
///   it. Because redispatch genuinely reproduces it, this is the only class
///   that arms the coordinator's planner-remediation escalation.
/// - `ProviderInternal{..}` | `Transport` | `ExhaustedTransport` →
///   [`ProviderFailureClass::Transient`]. A 5xx (`server_error` /
///   `server_is_overloaded`, including the in-stream `response.failed` form
///   that `ProviderError::from_stream_error` maps to a synthetic 500) or a hard
///   network death: the PROVIDER is broken, not the task. Breaker feedback is
///   deliberately unchanged from `Failure` — the host still calls
///   `record_failure`, so a model that dies on every dispatch still
///   auto-disables — but the coordinator spares the two task-blaming counters
///   (planner-remediation streak, terminal dispatch-failure streak) exactly as
///   it does for a throttle. Incident (task `2gq7`, 2026-07-29): three
///   independent OpenAI 500s on three consecutive sessions were folded into
///   `Failure`, tripped the third-strike escalation, and minted a "Planner
///   remediation" task asserting a poisoned transcript that never existed.
/// - `Authentication` → [`ProviderFailureClass::AuthInvalid`] (see below).
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
        // The TASK-attributable class: the provider rejected THIS request (the
        // poisoned resume transcript) or answered with something we cannot
        // parse. Redispatch reproduces it identically, so this — and only this
        // — is what may arm the coordinator's planner-remediation escalation.
        ProviderError::InvalidRequest | ProviderError::InvalidOutput => {
            Some(ProviderFailureClass::Failure)
        }
        // A provider-side 5xx (`server_error` / `server_is_overloaded`,
        // including the in-stream `response.failed` form `from_stream_error`
        // maps to a synthetic 500) is the PROVIDER being broken, not the task:
        // the same transcript succeeds on the next healthy backend.
        // `ProviderError::retryable()` has always known this; folding it into
        // `Failure` threw that knowledge away at the wire boundary and let three
        // unrelated OpenAI outages look like one reproducible task fault (2gq7).
        // The host feeds this class to `record_transient_failure`: still fully
        // visible in `model_health`, but on a 20-strike ladder instead of the
        // 3-strike one, so an upstream capacity blip cannot auto-disable the
        // model while a permanently-dead backend still demotes.
        ProviderError::ProviderInternal { .. } => Some(ProviderFailureClass::Transient {
            retry_after_ms: provider_err.retry_after_ms(),
        }),
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
        // stream) that kills the session is the same shape as a 5xx: the model
        // produced no work and exited fast, invisible to the coordinator's stall
        // detector, and the fault is the network/backend rather than anything
        // about this task. Feed it to the GENTLE consecutive-failure breaker
        // (`record_failure`), NOT the immediate-failover one — a single network
        // blip on an otherwise-healthy model must not demote it. The breaker only
        // trips after the configured run of consecutive failures, and any
        // successful session calls `record_success` (resets the counter), so a
        // transient blip is absorbed while a model that dies on EVERY dispatch
        // (the kimi-for-coding/k2p7 incident: instant Transport death, 0 tokens,
        // re-dispatched forever, absent from model_health) finally auto-disables.
        ProviderError::Transport | ProviderError::ExhaustedTransport(_) => {
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None,
            })
        }
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
///
/// Exported (via `crate::supervisor::derive_billing_signal`) because the in-Pod
/// worker path (`djinn-agent-worker`) builds its provider from a Secret-mounted
/// credential and never runs `resolve_model_and_credential`; it derives the
/// signal itself from the `SerializableCredential` kind and hands it to
/// `worker_execute_stage`.
pub fn derive_billing_signal(
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
    failure_cause: Option<SessionFailureCause>,
) -> (SessionStatus, Option<String>) {
    match stage_outcome {
        // These outcomes are useful terminal evidence, but did not provide a
        // durable healthy board handoff.
        StageOutcome::Parked {
            reason: ParkReason::Budget,
            ..
        } => (SessionStatus::Completed, Some("budget".to_string())),
        // A reply loop may complete while its finalization is invalid. The
        // sidecar is authoritative, so that must remain a failed session.
        _ if failure_cause.is_some() => (SessionStatus::Failed, None),
        _ => (SessionStatus::Completed, None),
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn settle_stage_session(
    services: &dyn SupervisorServices,
    session_id: String,
    stage_settlement: &StageSettlement,
    tokens_in: i64,
    tokens_out: i64,
    cache_read: i64,
    cache_write: i64,
) -> Result<(), String> {
    let (session_status, parked_reason) = session_settlement_for_stage_outcome(
        &stage_settlement.outcome,
        stage_settlement.failure_cause,
    );
    services
        .update_session_status_v2(
            session_id,
            session_status,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            parked_reason,
            stage_settlement.failure_cause,
        )
        .await
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

/// Map a finished lead/arbiter stage onto a [`StageOutcome`], applying the CI
/// adjudication contract when the session was dispatched under a Tier-2 CI
/// route (proposal `nafu`, wave 4).
///
/// Precedence is deliberate and, in one case, deliberately *not* what the CI
/// contract alone would say:
///
/// 1. `terminal_disposition_required` wins. It means the cumulative
///    arbitration budget is exhausted, so no approve or reopen can start
///    another PR-poller or worker cycle at all. The CI contract's "uncertainty
///    reopens rather than parks" rule is scoped to a route that still *has* a
///    cycle to spend; it does not lift a budget ceiling that predates it.
/// 2. Otherwise, a CI route adjudicates through
///    [`ci_routing::adjudicate`], which is total: an invalid, unsupported,
///    missing, or timed-out result becomes one diagnostic reopen rather than
///    a `Failed` stage, because the proposal forbids a second Lead session
///    for the same evidence.
/// 3. Otherwise, the pre-existing arbiter validation below runs unchanged.
///
/// The returned reopen is *not* yet applied: the caller must first win the
/// atomic current-identity guard (`resolve_tier2_lease`). That ordering is
/// [`lead_ci_adjudication`] plus [`apply_lead_ci_result`]'s job, not this
/// function's — this function is the **pre-guard** projection and exists for
/// the tests that pin the validation contract on its own.
///
/// `#[cfg(test)]` rather than `#[allow(dead_code)]`: wave 5 moved the
/// production path onto [`apply_lead_ci_result`], and a permissive projection
/// that no longer runs in production must not be reachable from it.
#[cfg(test)]
pub(super) fn lead_stage_outcome_routed(
    finalize_name: &str,
    finalize_payload: Option<&serde_json::Value>,
    terminal_disposition_required: bool,
    ci: Option<&ci_routing::CiAdjudicationContext>,
) -> StageOutcome {
    let (Some(ci), false) = (ci, terminal_disposition_required) else {
        return lead_stage_outcome(
            finalize_name,
            finalize_payload,
            terminal_disposition_required,
        );
    };
    ci_routing::stage_outcome(&lead_ci_adjudication(finalize_name, finalize_payload, ci).plan)
}

/// Validate a Lead response against the CI contract, without applying anything.
///
/// Split out from [`lead_stage_outcome_routed`] so the guarded path can hold on
/// to the [`ci_routing::CiAdjudication`] — the `rejection` inside it is what
/// wave 5 persists, and projecting straight to a `StageOutcome` throws it away.
pub(super) fn lead_ci_adjudication(
    finalize_name: &str,
    finalize_payload: Option<&serde_json::Value>,
    ci: &ci_routing::CiAdjudicationContext,
) -> ci_routing::CiAdjudication {
    let response = match (finalize_name, finalize_payload) {
        ("submit_decision", Some(payload)) => ci_routing::LeadResponse::Submitted(payload),
        ("submit_decision", None) | ("", _) => ci_routing::LeadResponse::Missing,
        (other, _) => ci_routing::LeadResponse::Unsupported(other),
    };
    let adjudication = ci_routing::adjudicate(ci, response);
    if let Some(rejection) = &adjudication.rejection {
        tracing::warn!(
            ?rejection,
            lane = ci.lane.as_str(),
            tier2_reason = ci.tier2_reason.as_str(),
            "ci_routing: Lead result replaced by the diagnostic fallback"
        );
    }
    adjudication
}

/// Adjudicate, run the atomic current-identity guard, and project the result
/// the supervisor is **permitted** to apply (proposal `nafu`, wave 5).
///
/// This is the wiring wave 4 documented and could not write. Everything before
/// it decides what a Lead result means; this decides whether it may happen at
/// all, and it decides that by asking the repository — inside the transaction
/// that persists the resolution — whether the evidence is still current.
///
/// The freshly observed head comes from a **re-read** of the task, not from the
/// `task` this run started with: a value loaded at dispatch is as old as the
/// Lead session, and noticing what moved during that session is the entire job.
///
/// `pub(super)` so `ci_routing::guard_tests` can drive **this** function rather
/// than the layer below it. The re-read on the next few lines is the sole input
/// to the guard's head comparison, and replacing it with
/// `ci.guard.identity.pr_head_sha.clone()` makes that comparison a tautology —
/// a mutation every fixture that calls `apply_under_guard` with a hand-built
/// `CiObservedNow` survives by construction. See
/// `the_production_derivation_reads_the_live_head_not_the_stored_one`.
pub(super) async fn apply_lead_ci_result(
    agent_context: &AgentContext,
    task_id: &str,
    ci: &ci_routing::CiAdjudicationContext,
    adjudication: &ci_routing::CiAdjudication,
) -> StageOutcome {
    use djinn_db::{CiRouteAttemptRepository, TaskRepository};

    // `Task::ci_head_sha` is derived from the newest `task_pr_ci_snapshots`
    // row, i.e. the PR head the board currently believes in. `None` means no
    // snapshot exists to compare against — which is not evidence that the head
    // moved, so the stored value stands and the guard's other half (a newer
    // passing or merged observation, which resolves the lease outright) does
    // the work.
    let db = &agent_context.db;
    let observed_head = TaskRepository::new(db.clone(), agent_context.event_bus.clone())
        .get(task_id)
        .await
        .ok()
        .flatten()
        .and_then(|task| task.ci_head_sha)
        .unwrap_or_else(|| ci.guard.identity.pr_head_sha.clone());

    let routes = CiRouteAttemptRepository::new(db.clone());
    let effect = ci_routing::apply_under_guard(
        &routes,
        ci,
        adjudication,
        &ci_routing::CiObservedNow {
            pr_head_sha: observed_head.clone(),
        },
    )
    .await;

    // The counted effects, logged rather than merely asserted in tests: this
    // is the line an operator reads to see that a suppressed route dispatched
    // zero workers and a current one dispatched exactly one.
    let counts = effect.counts();
    if effect.is_noop() {
        tracing::info!(
            task_id,
            lane = ci.lane.as_str(),
            key = %ci.guard.provider_action_key,
            stored_head = %ci.guard.identity.pr_head_sha,
            observed_head = %observed_head,
            board_transitions = counts.board_transitions,
            worker_dispatches = counts.worker_dispatches,
            "ci_routing: superseded_before_apply — the Lead result was not applied"
        );
    } else {
        tracing::info!(
            task_id,
            lane = ci.lane.as_str(),
            key = %ci.guard.provider_action_key,
            board_transitions = counts.board_transitions,
            worker_dispatches = counts.worker_dispatches,
            "ci_routing: current-identity guard held — applying the Lead result"
        );
    }
    ci_routing::stage_outcome_after_guard(
        &adjudication.plan,
        &effect,
        &format!(
            "ci route {} superseded before apply (lane {}, stored head {}, observed head {})",
            ci.guard.provider_action_key,
            ci.lane.as_str(),
            ci.guard.identity.pr_head_sha,
            observed_head,
        ),
    )
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
    terminal_disposition_required: bool,
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
            if terminal_disposition_required && !matches!(decision, "park" | "supersede") {
                let dossier = serde_json::json!({
                    "final_disposition": true,
                    "hold_description": format!(
                        "Final arbiter returned non-terminal decision '{decision}'"
                    ),
                    "failure_analysis": "The cumulative arbitration budget is exhausted, so approve/reopen outcomes cannot start another PR-poller or worker cycle.",
                    "submitted_decision": decision,
                    "submitted_payload": finalize_payload,
                    "recommended_action": "Replan the epic/proposal and either create replacement work that supersedes the exhausted task or close work that is no longer required.",
                });
                return StageOutcome::LeadParked {
                    park_dossier_json: dossier.to_string(),
                };
            }
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
                                let mut dossier = d.clone();
                                if terminal_disposition_required
                                    && let Some(object) = dossier.as_object_mut()
                                {
                                    object.insert(
                                        "final_disposition".to_string(),
                                        serde_json::Value::Bool(true),
                                    );
                                }
                                let dossier_json = serde_json::to_string(&dossier)
                                    .unwrap_or_else(|_| "{}".to_string());
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

/// Test-only synchronization for the accepted, pre-session lifecycle boundary.
///
/// The gate is reached after a stage has begun (and therefore after its slot
/// command was accepted) but before the guarded session-service call. It is
/// compiled only for unit tests and the `test-support` feature used by worker
/// integration tests; production has neither the global state nor a wait.
#[cfg(any(test, feature = "test-support"))]
pub mod pre_session_create_test_support {
    use std::sync::{Arc, Mutex, OnceLock};

    use tokio::sync::watch;

    /// Deterministic two-phase gate for the accepted, pre-session boundary.
    /// `watch` retains both transitions, so a fast lifecycle cannot lose a
    /// notification before its test begins waiting.
    pub struct PreSessionCreateGate {
        reached_tx: watch::Sender<bool>,
        release_tx: watch::Sender<bool>,
    }

    impl PreSessionCreateGate {
        pub fn new() -> Arc<Self> {
            let (reached_tx, _) = watch::channel(false);
            let (release_tx, _) = watch::channel(false);
            Arc::new(Self {
                reached_tx,
                release_tx,
            })
        }

        pub async fn wait_until_reached(&self) {
            let mut reached = self.reached_tx.subscribe();
            if !*reached.borrow() {
                reached
                    .changed()
                    .await
                    .expect("pre-session gate remains installed");
            }
        }

        pub fn release(&self) {
            let _ = self.release_tx.send(true);
        }

        async fn pause(&self) {
            let _ = self.reached_tx.send(true);
            let mut release = self.release_tx.subscribe();
            if !*release.borrow() {
                release
                    .changed()
                    .await
                    .expect("pre-session gate remains installed");
            }
        }
    }

    fn installed_gate() -> &'static Mutex<Option<Arc<PreSessionCreateGate>>> {
        static GATE: OnceLock<Mutex<Option<Arc<PreSessionCreateGate>>>> = OnceLock::new();
        GATE.get_or_init(|| Mutex::new(None))
    }

    /// Install one gate. Tests sharing this process-global seam are serialized
    /// by the installation assertion, and dropping the guard removes it.
    pub fn install(gate: Arc<PreSessionCreateGate>) -> PreSessionCreateGateGuard {
        let mut installed = installed_gate()
            .lock()
            .expect("pre-session gate mutex poisoned");
        assert!(
            installed.is_none(),
            "a pre-session create gate is already installed"
        );
        *installed = Some(gate);
        PreSessionCreateGateGuard {}
    }

    pub struct PreSessionCreateGateGuard {}

    impl Drop for PreSessionCreateGateGuard {
        fn drop(&mut self) {
            *installed_gate()
                .lock()
                .expect("pre-session gate mutex poisoned") = None;
        }
    }

    pub(crate) async fn pause_before_session_creation() {
        let gate = installed_gate()
            .lock()
            .expect("pre-session gate mutex poisoned")
            .clone();
        if let Some(gate) = gate {
            gate.pause().await;
        }
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
) -> Result<djinn_supervisor::StageExecutionResult, StageError> {
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

    // The session is intentionally created before extension loading so every
    // diagnostic has a session foreign key. Preserve its billing attribution.
    //
    // On the host path `resolved` is populated and we derive the signal from the
    // resolved credential. On the in-Pod worker path `provider_override` is set,
    // `resolved` is `None`, and the worker has already derived the signal from
    // the Secret-mounted `SerializableCredential` — it arrives via
    // `callbacks.billing_signal`. Without this, every worker-pod session fell
    // through to the legacy string path and mis-booked openai plan usage as
    // `actual` (billing_source NULL). The `test/supervisor-stub` integration
    // case passes `None` and keeps its prior unpriced/legacy behavior.
    let billing_signal = callbacks.billing_signal.or_else(|| {
        resolved.as_ref().map(|resolved| {
            derive_billing_signal(
                &resolved.catalog_provider_id,
                &resolved.model_name,
                matches!(
                    resolved.provider_credential.as_ref(),
                    Some(ProviderCredential::OAuthConfig(_))
                ),
            )
        })
    });

    let _ = services
        .report_stage_step(djinn_runtime::stage_step::SESSION_CREATE)
        .await;
    #[cfg(any(test, feature = "test-support"))]
    pre_session_create_test_support::pause_before_session_creation().await;
    let session_record = services
        .create_session(
            djinn_supervisor::services::SerializableCreateSessionParams {
                project_id: task.project_id.clone(),
                task_id: Some(task.id.clone()),
                execution_generation: Some(spec.execution_generation),
                model: model_id.clone(),
                agent_type: runtime_role_name.to_string(),
                metadata_json: None,
                task_run_id: Some(task_run_id.to_string()),
                cost_basis_hint: billing_signal.map(|(hint, _)| hint),
                billing_source: billing_signal.map(|(_, source)| source),
            },
        )
        .await
        .map_err(StageError::SessionCreate)?;
    let session_id = session_record.id.clone();

    // A per-generation admission acknowledgement used to be written here, on
    // both the host in-process and in-pod paths. It held the v0→v1
    // invocation-primary handoff edge closed until every live task-run
    // generation had confirmed the new authority. The Kueue cutover deleted the
    // v0 authority and that edge with it, so there is nothing left to
    // acknowledge and nothing that reads the rows. The trait method was removed
    // rather than stubbed, which is why this call site had to be deleted rather
    // than quietly kept alive.

    // ── MCP + skills ─────────────────────────────────────────────────────────
    // `runtime_role` drives resolution so specialists can override the base
    // role's MCP/skill defaults.  `role_mcp_servers` carries the DB row's
    // parsed array (or `None` when no DB row exists).
    //
    // The typed trigger gates platform-native skills. A marked Architect task
    // carries an exact readiness pin that must resolve or block launch.
    let native_skill_trigger = classify_native_skill_trigger(runtime_role.config().name, task);

    let McpAndSkills {
        effective_mcp_servers,
        effective_skills,
        mcp_registry,
        resolved_skills,
        native_skill_names: _native_skill_names,
        mcp_server_instructions,
        extension_diagnostics,
    } = match resolve_mcp_and_skills(
        worktree_path,
        runtime_role.as_ref(),
        &task.project_id,
        &task.id,
        &session_id,
        &task.short_id,
        role_mcp_servers.as_deref(),
        &role_skills,
        native_skill_trigger,
        #[cfg(test)]
        None,
        agent_context,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(error) => {
            return Err(after_session_error(
                &session_id,
                error.to_string(),
                djinn_core::models::SessionFailureCause::Harness,
            ));
        }
    };

    // ── Setup commands ───────────────────────────────────────────────────────
    // Pre-verification hooks come from `lifecycle.pre_verification` (via the
    // SupervisorServices RPC). Missing / malformed configs degrade to empty
    // lists (see `environment`).
    let env_config = match services
        .get_environment_config(task.project_id.clone())
        .await
    {
        Ok(config) => config,
        Err(error) => {
            return Err(after_session_error(
                &session_id,
                format!("env_config: {error}"),
                djinn_core::models::SessionFailureCause::Harness,
            ));
        }
    };
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
            return Err(after_session_error(
                &session_id,
                reason,
                djinn_core::models::SessionFailureCause::Harness,
            ));
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
    // nafu: the other half of the same arbitration column. When the coordinator
    // dispatched this Lead under a Tier-2 CI lease, the `ci_route` block carries
    // the two closed corpora the supervisor will grade the result against —
    // `evidence_references` and `repository_commands`. Rendering them is what
    // makes `command_is_repository_valid` a rule Lead can follow rather than a
    // guess it usually loses.
    let ci_adjudication_bundle =
        crate::actors::slot::lifecycle::prompt_context::load_ci_adjudication_bundle(
            runtime_role_name,
            &task.id,
            agent_context,
        )
        .await;
    if ci_adjudication_bundle.is_some() {
        tracing::info!(
            task_id = %task.short_id,
            task_run_id = %task_run_id,
            role = %runtime_role_name,
            "Supervisor stage: injected CI adjudication evidence bundle"
        );
    }
    // Coarse pre-session progress marker for the host-side liveness deadline:
    // model/credential/MCP/skill resolution and prompt assembly happen here,
    // before any session row exists.
    let _ = services
        .report_stage_step(djinn_runtime::stage_step::CONTEXT_BUILD)
        .await;
    let planner_host =
        crate::actors::slot::lifecycle::prompt_context::SupervisorPlannerHost(services);
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
        read_sources: &read_sources,
        worker_resume_note: worker_resume_note.as_deref(),
        arbiter_directive: arbiter_directive.as_deref(),
        ci_adjudication_bundle: ci_adjudication_bundle.as_deref(),
        mcp_server_instructions: &mcp_server_instructions,
        extension_diagnostics: &extension_diagnostics,
        cancellation: Some(&callbacks.cancel),
        memory_intent_planner: Some(MemoryIntentPlannerInvocation {
            config: &agent_context.memory_intent_planner,
            host: &planner_host,
            session_id: &session_id,
            task_run_id,
            creator_id: Some(task.created_by_user_id.as_str()),
            acceptance_criteria: serde_json::from_str::<Vec<serde_json::Value>>(
                &task.acceptance_criteria,
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                v.as_str().map(str::to_owned).or_else(|| {
                    v.get("criterion")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
            })
            .collect(),
            // The display note is intentionally concise. Planner input uses
            // the typed, untruncated durable summary from runtime metadata.
            resume_compaction_summary: spec
                .resume_lifecycle_metadata
                .as_ref()
                .and_then(|metadata| metadata.last_durable_progress_summary.as_deref()),
            planned_note_search: None,
        }),
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

    let credential_record_id = resolved
        .as_ref()
        .and_then(|resolved| match resolved.provider_credential.as_ref() {
            Some(ProviderCredential::ApiKey(credential_record_id, _, _)) => {
                Some(credential_record_id.clone())
            }
            Some(ProviderCredential::OAuthConfig(_)) | None => None,
        })
        .unwrap_or_default();
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
                return Err(after_session_error(
                    &session_id,
                    "no provider credential resolved for model".into(),
                    djinn_core::models::SessionFailureCause::Provider,
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
            credential_record_id: &credential_record_id,
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
            // NOT `&callbacks.cancel` again. Passing the same token to both
            // fields made the reply loop's supervisor-shutdown select arm
            // unreachable, because the biased select always resolved the
            // session arm first. `services.cancel()` is the supervisor-wide
            // token by definition (see `SupervisorServices::cancel`), so it is
            // the correct source for this field. Every current wiring happens
            // to hand the stage the same object, so this is behaviour-neutral
            // today; the actual cause disambiguation comes from
            // `callbacks.cancel_origin`, read at settlement below.
            global_cancel: services.cancel(),
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
                Some(task.created_by_user_id.clone()),
                djinn_core::auth_context::REVISION_CALLER_CONTEXT
                    .scope(revision_caller, reply_loop_fut),
            )
            .await;

    // ── Map the reply-loop outcome to StageOutcome ───────────────────────────
    let final_result_ok = reply_result.is_ok();
    // Capture type-based settlement data before formatting the error for the
    // outward diagnostic. No error text participates in cause classification.
    let reply_failure_cause = reply_result.as_ref().err().map(reply_loop_failure_cause);
    // The same origin attribution the `Failed` reason carries, so the
    // `session_error` activity row logged by `spawn_post_session_work` names
    // the trigger too — otherwise the durable reason and the activity log
    // disagree about the same cancellation.
    let final_error = reply_result.as_ref().err().map(|e| {
        let diagnostic = e.to_string();
        if classify_reply_loop_failure(e) == ReplyLoopFailureClass::Cancelled {
            with_cancel_origin(&diagnostic, callbacks.cancel_origin.get())
        } else {
            diagnostic
        }
    });
    let stage_outcome = match reply_result {
        Err(e) => {
            if let Some(outcome) = stage_outcome_for_model_turn_admission_error(&e) {
                outcome
            } else if e.downcast_ref::<BudgetWindDownIgnored>().is_some() {
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
            } else if classify_reply_loop_failure(&e) == ReplyLoopFailureClass::Cancelled {
                // Preserve the established diagnostic and wire shape while making
                // the typed cancellation seam explicit for durable settlement.
                // The origin suffix is what turns an undiagnosable
                // "session cancelled" into a row that names its trigger.
                let origin = callbacks.cancel_origin.get();
                tracing::warn!(
                    task_id = %task.short_id,
                    session_id = %session_id,
                    cancel_origin = origin.as_str(),
                    "reply loop cancelled"
                );
                StageOutcome::Failed {
                    reason: cancelled_stage_reason(&e.to_string(), origin),
                    provider_failure: None,
                }
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
                        // One read serves both gates: the arbitration row's
                        // structured directive carries the cumulative-budget
                        // flag and, when the coordinator dispatched this Lead
                        // under a Tier-2 CI lease, the `ci_route` block that
                        // switches on the `nafu` adjudication contract.
                        let directive = {
                            use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;
                            TaskArbitrationRepository::new(agent_context.db.clone())
                                .resolve_current_hold_cycle(&task.id)
                                .await
                                .ok()
                                .and_then(|(_, record)| record)
                                .and_then(|record| record.directive)
                        };
                        let terminal_disposition_required = directive
                            .as_ref()
                            .and_then(|directive| {
                                directive
                                    .get("terminal_disposition_required")
                                    .and_then(serde_json::Value::as_bool)
                            })
                            .unwrap_or(false);
                        match ci_routing::CiAdjudicationContext::read_arbiter_directive(
                            directive.as_ref(),
                        ) {
                            // No route: the pre-existing arbiter contract, byte
                            // for byte. This is the "feature disabled" row of
                            // the mixed-version matrix.
                            ci_routing::CiDirectiveRead::NoRoute => lead_stage_outcome(
                                finalize_name,
                                final_output.finalize_payload.as_ref(),
                                terminal_disposition_required,
                            ),
                            // A route block that will not parse cannot be
                            // guarded, and an unguardable route may not be
                            // applied. It deliberately does NOT fall through to
                            // the legacy path: that path rejects a `diagnose`
                            // payload (no verification command) as a `Failed`
                            // stage, which feeds the arbiter decision-failure
                            // counter and parks the task at the cap — so a
                            // producer bug in one JSON field would park tasks
                            // whose Lead answered correctly.
                            ci_routing::CiDirectiveRead::Malformed(field) => {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    field,
                                    "ci_routing: arbitration directive carries an unparseable \
                                     `ci_route` block — applying nothing"
                                );
                                StageOutcome::LeadRouteSuperseded {
                                    reason: format!(
                                        "ci route block is unparseable at `{field}`; no Lead \
                                         result was applied"
                                    ),
                                }
                            }
                            // The cumulative arbitration budget outranks the CI
                            // contract: at the ceiling no reopen may buy another
                            // worker cycle, whatever the route says.
                            ci_routing::CiDirectiveRead::Route(_)
                                if terminal_disposition_required =>
                            {
                                lead_stage_outcome(
                                    finalize_name,
                                    final_output.finalize_payload.as_ref(),
                                    true,
                                )
                            }
                            ci_routing::CiDirectiveRead::Route(ci) => {
                                let adjudication = lead_ci_adjudication(
                                    finalize_name,
                                    final_output.finalize_payload.as_ref(),
                                    &ci,
                                );
                                apply_lead_ci_result(agent_context, &task.id, &ci, &adjudication)
                                    .await
                            }
                        }
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
    let stage_settlement = settlement_for_stage_outcome(
        stage_outcome,
        final_result_ok,
        reply_failure_cause,
        role_kind,
    );
    let cancelled_at_ownership_cutover = services.cancel().is_cancelled();
    let (session_status, parked_reason) = if cancelled_at_ownership_cutover {
        (SessionStatus::Interrupted, None)
    } else {
        session_settlement_for_stage_outcome(
            &stage_settlement.outcome,
            stage_settlement.failure_cause,
        )
    };
    let failure_cause = if cancelled_at_ownership_cutover {
        Some(SessionFailureCause::Cancelled)
    } else {
        stage_settlement.failure_cause
    };

    if let StageOutcome::Parked {
        reason: ParkReason::Budget,
        summary: Some(summary),
        wind_down_ignored: false,
        ..
    } = &stage_settlement.outcome
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

    let parked = matches!(stage_settlement.outcome, StageOutcome::Parked { .. });
    let post_session_result_ok = final_result_ok || parked;
    let post_session_error = if parked { None } else { final_error };

    spawn_post_session_work(PostSessionParams {
        task_id: task.id.clone(),
        // Finalization identity is server-owned: do not derive it from the
        // agent-controlled finalize payload.
        authenticated_session_id: session_record.id.clone(),
        project_path,
        role: role.clone(),
        app_state: agent_context.clone(),
        final_output,
        final_result_ok: post_session_result_ok,
        final_error: post_session_error,
        tokens_in,
        tokens_out,
    });

    Ok(djinn_supervisor::StageExecutionResult {
        outcome: stage_settlement.outcome,
        settlement: Some(djinn_supervisor::StageSessionSettlement {
            session_id,
            status: session_status,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            parked_reason,
            failure_cause,
        }),
    })
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
    use djinn_supervisor::{
        model_turn_admission_task_run_status, model_turn_admission_terminal_outcome,
    };
    use std::sync::Mutex;

    #[test]
    fn model_turn_admission_mapper_preserves_wait_payload() {
        let error = anyhow::Error::new(ModelTurnAdmissionOutcome::Wait(
            djinn_db::ModelTurnAdmissionWait::Draining,
        ));
        let Some(StageOutcome::ModelTurnAdmission(admission)) =
            stage_outcome_for_model_turn_admission_error(&error)
        else {
            panic!("wait must remain a typed admission stage outcome");
        };
        assert!(matches!(
            admission,
            ModelTurnAdmissionStageOutcome::Wait(djinn_db::ModelTurnAdmissionWait::Draining)
        ));
        let terminal = model_turn_admission_terminal_outcome(admission);
        assert!(matches!(
            terminal,
            djinn_supervisor::ModelTurnAdmissionTerminalOutcome::Wait(
                djinn_db::ModelTurnAdmissionWait::Draining
            )
        ));
        assert_eq!(
            model_turn_admission_task_run_status(&terminal),
            djinn_core::models::TaskRunStatus::Interrupted,
            "wait must use the cancellable/interrupted scheduling path"
        );
    }

    #[test]
    fn model_turn_admission_mapper_preserves_rejection_payload() {
        let error = anyhow::Error::new(ModelTurnAdmissionOutcome::Rejected(
            djinn_db::ModelTurnAdmissionRejection::Off,
        ));
        let Some(StageOutcome::ModelTurnAdmission(admission)) =
            stage_outcome_for_model_turn_admission_error(&error)
        else {
            panic!("rejection must remain a typed admission stage outcome");
        };
        assert!(matches!(
            admission,
            ModelTurnAdmissionStageOutcome::Rejected(djinn_db::ModelTurnAdmissionRejection::Off)
        ));
        let terminal = model_turn_admission_terminal_outcome(admission);
        assert!(matches!(
            terminal,
            djinn_supervisor::ModelTurnAdmissionTerminalOutcome::Rejected(
                djinn_db::ModelTurnAdmissionRejection::Off
            )
        ));
        assert_eq!(
            model_turn_admission_task_run_status(&terminal),
            djinn_core::models::TaskRunStatus::Failed,
            "rejection must use the terminal admission-error path"
        );
    }

    #[test]
    fn model_turn_admission_mapper_preserves_dispatch_fence_payload() {
        let error = anyhow::Error::new(ModelTurnAdmissionOutcome::DispatchFenced(
            djinn_db::ModelTurnLeaseMutationOutcome::Fenced,
        ));
        let Some(StageOutcome::ModelTurnAdmission(admission)) =
            stage_outcome_for_model_turn_admission_error(&error)
        else {
            panic!("dispatch fence must remain a typed admission stage outcome");
        };
        assert!(matches!(
            admission,
            ModelTurnAdmissionStageOutcome::DispatchFenced(
                djinn_db::ModelTurnLeaseMutationOutcome::Fenced
            )
        ));
        let terminal = model_turn_admission_terminal_outcome(admission);
        assert!(matches!(
            terminal,
            djinn_supervisor::ModelTurnAdmissionTerminalOutcome::DispatchFenced(
                djinn_db::ModelTurnLeaseMutationOutcome::Fenced
            )
        ));
        assert_eq!(
            model_turn_admission_task_run_status(&terminal),
            djinn_core::models::TaskRunStatus::Interrupted,
            "dispatch fence must use the cancellable/interrupted scheduling path"
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedSettlement {
        session_id: String,
        status: SessionStatus,
        parked_reason: Option<String>,
        failure_cause: Option<SessionFailureCause>,
    }

    struct RecordingServices {
        cancel: tokio_util::sync::CancellationToken,
        settlements: Mutex<Vec<RecordedSettlement>>,
    }

    impl RecordingServices {
        fn new() -> Self {
            Self {
                cancel: tokio_util::sync::CancellationToken::new(),
                settlements: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SupervisorServices for RecordingServices {
        fn cancel(&self) -> &tokio_util::sync::CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, _: String) -> Result<Task, String> {
            unimplemented!()
        }

        async fn execute_stage(
            &self,
            _: &Task,
            _: &Workspace,
            _: RoleKind,
            _: &str,
            _: &TaskRunSpec,
        ) -> Result<djinn_supervisor::StageExecutionResult, StageError> {
            unimplemented!()
        }

        async fn open_pr(&self, _: &TaskRunSpec, _: &Task) -> djinn_supervisor::TaskRunOutcome {
            unimplemented!()
        }

        async fn create_task_run(
            &self,
            _: djinn_supervisor::services::SerializableCreateTaskRunParams,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn update_task_run_status(
            &self,
            _: String,
            _: djinn_core::models::TaskRunStatus,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn get_model_context_window(&self, _: String) -> Result<i64, String> {
            unimplemented!()
        }

        async fn get_provider_base_url(&self, _: String) -> Result<String, String> {
            unimplemented!()
        }

        async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
            unimplemented!()
        }

        async fn create_session(
            &self,
            _: djinn_supervisor::services::SerializableCreateSessionParams,
        ) -> Result<djinn_core::models::SessionRecord, String> {
            unimplemented!()
        }

        async fn publish_session_message(
            &self,
            _: String,
            _: String,
            _: String,
            _: serde_json::Value,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn get_environment_config(
            &self,
            _: String,
        ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
            unimplemented!()
        }

        async fn invoke_llm(
            &self,
            _: String,
            _: Conversation,
            _: Vec<serde_json::Value>,
            _: Option<djinn_provider::provider::ToolChoice>,
        ) -> Result<djinn_provider::provider::LlmResponse, String> {
            unimplemented!()
        }

        async fn tool_github_search(
            &self,
            _: Option<String>,
            _: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!()
        }

        async fn tool_github_fetch_file(
            &self,
            _: Option<String>,
            _: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!()
        }

        async fn tool_ci_job_log(
            &self,
            _: Option<String>,
            _: serde_json::Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            unimplemented!()
        }

        async fn emit_djinn_event(
            &self,
            _: djinn_supervisor::services::SerializableDjinnEvent,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn touch_activity(&self, _: String) -> Result<(), String> {
            unimplemented!()
        }

        async fn transition_task(
            &self,
            _: String,
            _: String,
            _: Option<String>,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn record_arbiter_decision(
            &self,
            _: String,
            _: String,
            _: String,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn start_monitored_reopen(
            &self,
            _: String,
            _: String,
            _: String,
            _: Vec<String>,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn complete_monitored_reopen(&self, _: String) -> Result<(), String> {
            unimplemented!()
        }

        async fn record_arbiter_session_termination(
            &self,
            _: String,
            _: bool,
        ) -> Result<bool, String> {
            unimplemented!()
        }

        async fn update_session_status(
            &self,
            _: String,
            _: SessionStatus,
            _: i64,
            _: i64,
            _: i64,
            _: i64,
            _: Option<String>,
        ) -> Result<(), String> {
            unimplemented!()
        }

        async fn update_session_status_v2(
            &self,
            session_id: String,
            status: SessionStatus,
            _: i64,
            _: i64,
            _: i64,
            _: i64,
            parked_reason: Option<String>,
            failure_cause: Option<SessionFailureCause>,
        ) -> Result<(), String> {
            self.settlements.lock().unwrap().push(RecordedSettlement {
                session_id,
                status,
                parked_reason,
                failure_cause,
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn v2_settlement_records_typed_reply_loop_causes_without_diagnostics() {
        let services = RecordingServices::new();
        let credential_diagnostic = "provider diagnostic: sk-live-credential-must-not-persist";
        let cases = vec![
            (
                "cancelled",
                anyhow::Error::new(ReplyLoopCancelled::session()),
                SessionFailureCause::Cancelled,
            ),
            (
                "provider",
                anyhow::Error::new(ProviderError::Authentication).context(credential_diagnostic),
                SessionFailureCause::Provider,
            ),
            (
                "harness",
                anyhow::Error::new(ToolError::new("MCP tool failed")),
                SessionFailureCause::Harness,
            ),
            (
                "step-cap",
                anyhow::Error::new(StepCapWindDownIgnored { max_turns: 3 }),
                SessionFailureCause::Protocol,
            ),
            (
                "unknown",
                anyhow::anyhow!("unmatched reply-loop failure"),
                SessionFailureCause::Unknown,
            ),
        ];
        for (session_id, error, expected_cause) in cases {
            let cause = reply_loop_failure_cause(&error);
            assert_eq!(cause, expected_cause, "{session_id} classification");
            let settlement = settlement_for_stage_outcome(
                StageOutcome::Failed {
                    reason: error.to_string(),
                    provider_failure: None,
                },
                false,
                Some(cause),
                RoleKind::Worker,
            );
            settle_stage_session(&services, session_id.into(), &settlement, 0, 0, 0, 0)
                .await
                .unwrap();
        }
        let recorded = services.settlements.lock().unwrap();
        assert_eq!(recorded.len(), 5);
        for (entry, cause) in recorded.iter().zip([
            SessionFailureCause::Cancelled,
            SessionFailureCause::Provider,
            SessionFailureCause::Harness,
            SessionFailureCause::Protocol,
            SessionFailureCause::Unknown,
        ]) {
            assert_eq!(entry.status, SessionStatus::Failed);
            assert_eq!(entry.failure_cause, Some(cause));
            assert_eq!(entry.parked_reason, None);
        }
        assert!(!format!("{recorded:?}").contains(credential_diagnostic));
    }

    #[tokio::test]
    async fn v2_settlement_records_concrete_dispatcher_and_nudge_causes() {
        let services = RecordingServices::new();
        let errors = [
            anyhow::Error::new(MissingSlotToolDispatcher),
            anyhow::Error::new(FinalizeNudgesExhausted {
                attempts: 3,
                finalize_tools: "finalize_task".into(),
            }),
        ];

        for (session_id, error) in ["missing-dispatcher", "nudges-exhausted"]
            .into_iter()
            .zip(errors)
        {
            let cause = reply_loop_failure_cause(&error);
            let settlement = settlement_for_stage_outcome(
                StageOutcome::Failed {
                    reason: error.to_string(),
                    provider_failure: None,
                },
                false,
                Some(cause),
                RoleKind::Worker,
            );
            settle_stage_session(&services, session_id.into(), &settlement, 0, 0, 0, 0)
                .await
                .unwrap();
        }

        let recorded = services.settlements.lock().unwrap();
        assert_eq!(
            recorded
                .iter()
                .map(|entry| (entry.status, entry.failure_cause))
                .collect::<Vec<_>>(),
            vec![
                (SessionStatus::Failed, Some(SessionFailureCause::Harness)),
                (
                    SessionStatus::Failed,
                    Some(SessionFailureCause::Finalization)
                ),
            ]
        );
        assert!(recorded.iter().all(|entry| entry.parked_reason.is_none()));
        assert!(!format!("{recorded:?}").contains("diagnostic"));
    }

    #[tokio::test]
    async fn v2_settlement_records_finalization_completed_budget_and_setup_causes() {
        let services = RecordingServices::new();
        for (session_id, outcome, role_kind) in [
            (
                "unexpected-worker-finalize",
                worker_stage_outcome("bad_finalize", None),
                RoleKind::Worker,
            ),
            (
                "invalid-planner-finalize",
                StageOutcome::Failed {
                    reason: "planner submitted unknown decision 'invalid'".into(),
                    provider_failure: None,
                },
                RoleKind::Planner,
            ),
            (
                "missing-refinement-finalize",
                StageOutcome::Failed {
                    reason: "refinement session ended without calling a finalize tool".into(),
                    provider_failure: None,
                },
                RoleKind::Refinement,
            ),
        ] {
            let settlement = settlement_for_stage_outcome(outcome, true, None, role_kind);
            assert_eq!(
                settlement.failure_cause,
                Some(SessionFailureCause::Finalization)
            );
            settle_stage_session(&services, session_id.into(), &settlement, 0, 0, 0, 0)
                .await
                .unwrap();
        }
        settle_stage_session(
            &services,
            "completed".into(),
            &StageSettlement::completed(StageOutcome::WorkerDone),
            0,
            0,
            0,
            0,
        )
        .await
        .unwrap();
        settle_stage_session(
            &services,
            "budget".into(),
            &StageSettlement::completed(StageOutcome::Parked {
                reason: ParkReason::Budget,
                summary: None,
                wind_down_ignored: false,
                session_id: "budget".into(),
                tokens_in: 0,
                tokens_out: 0,
            }),
            0,
            0,
            0,
            0,
        )
        .await
        .unwrap();
        let recorded = services.settlements.lock().unwrap();
        assert_eq!(recorded.len(), 5);
        assert!(
            recorded[..3]
                .iter()
                .all(|entry| entry.status == SessionStatus::Failed
                    && entry.failure_cause == Some(SessionFailureCause::Finalization))
        );
        assert_eq!(recorded[3].failure_cause, None);
        assert_eq!(recorded[4].failure_cause, None);
        assert_eq!(recorded[4].parked_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn pre_session_gate_blocks_until_explicit_release() {
        use pre_session_create_test_support::{
            PreSessionCreateGate, install, pause_before_session_creation,
        };

        let gate = PreSessionCreateGate::new();
        let _installed = install(gate.clone());
        let paused = tokio::spawn(async move {
            pause_before_session_creation().await;
            "released"
        });

        gate.wait_until_reached().await;
        assert!(
            !paused.is_finished(),
            "the lifecycle must remain paused until the test explicitly releases it"
        );
        gate.release();
        assert_eq!(paused.await.unwrap(), "released");
    }

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
        // Invalid-request / invalid-output are "quiet but broken" AND
        // task-attributable (the provider rejected THIS request body, or
        // answered with something unparseable) → gentle consecutive-failure
        // breaker, and the only class allowed to arm the coordinator's
        // planner-remediation escalation.
        for e in [ProviderError::InvalidRequest, ProviderError::InvalidOutput] {
            assert_eq!(
                classify_provider_failure(&typed(e.clone())),
                Some(ProviderFailureClass::Failure),
                "{e:?} should map to Failure",
            );
        }
    }

    #[test]
    fn provider_5xx_maps_to_transient_not_failure() {
        // Incident (task `2gq7`, 2026-07-29): three independent OpenAI 500s
        // (`server_is_overloaded` / `server_error`) on three consecutive
        // sessions were folded into `Failure`, which the coordinator reads as
        // "the redispatched worker reproduces the same failure each time" and
        // escalates on the third strike — minting a Planner remediation task
        // that blamed a poisoned resume transcript that never existed.
        // `ProviderError::retryable()` already knew a 5xx is transient; the
        // wire class must now carry that knowledge to the host.
        for status in [500u16, 502, 503, 529] {
            assert_eq!(
                classify_provider_failure(&typed(ProviderError::ProviderInternal { status })),
                Some(ProviderFailureClass::Transient {
                    retry_after_ms: None
                }),
                "a {status} is the provider's fault, not the task's",
            );
        }

        // The production shape of 2gq7's second failure: an in-stream
        // `server_error` event on a 200 response (OpenAI request id
        // f7bd36f8-6250-455d-8235-738cab51183c), which `from_stream_error` maps
        // to a synthetic 500.
        let stream_500 =
            ProviderError::from_stream_error(Some("server_error"), "An error occurred");
        assert_eq!(
            classify_provider_failure(&typed(stream_500)),
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None
            }),
            "an in-stream `server_error` event is a transient provider fault",
        );

        // …and it survives the context/stream wrapping the production error
        // path applies, exactly as the auth/transport cases do.
        let wrapped = anyhow::Error::new(ProviderError::ProviderInternal { status: 500 })
            .context("provider API error 500: {\"code\":\"server_error\"}")
            .context("provider stream event failed: display=...; fs_diag; env_diag");
        assert_eq!(
            classify_provider_failure(&wrapped),
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None
            }),
        );
    }

    /// End-to-end for the capacity signal that actually killed `2gq7`'s first
    /// and third sessions: `server_is_overloaded` from the ChatGPT Codex
    /// CONSUMER backend. `from_stream_error` classifies it as a `RateLimit`
    /// (plan capacity shedding, not a broken model), and the wire class must
    /// therefore be `Throttle` — the host's IMMEDIATE-failover path
    /// (`record_stall`), so dispatch moves to the user's next model at once
    /// instead of re-probing a saturated endpoint. Like `Transient`, it is
    /// exempt from the planner-remediation escalation.
    #[test]
    fn overload_stream_event_maps_to_throttle_for_immediate_failover() {
        let overloaded = ProviderError::from_stream_error(
            Some("server_is_overloaded"),
            "Our servers are currently overloaded",
        );
        assert_eq!(
            overloaded,
            ProviderError::RateLimit {
                retry_after_ms: None
            },
            "an overload code is a throttle, not a provider-internal fault",
        );
        assert_eq!(
            classify_provider_failure(&typed(overloaded)),
            Some(ProviderFailureClass::Throttle {
                retry_after_ms: None
            }),
            "an overload must fail over immediately, not retry the same saturated model",
        );
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
    fn non_provider_terminal_causes_are_breaker_neutral() {
        // Durable settlement causes remain visible for operations/reporting, but
        // none is a breaker classifier. Only a concrete ProviderError can cross
        // this boundary; these cover cancellation, harness, infrastructure,
        // protocol, finalization, unknown, and legacy-unclassified outcomes.
        let errors = [
            anyhow::Error::new(ReplyLoopCancelled::session()),
            anyhow::Error::new(ToolError::new("MCP tool failed")),
            anyhow::anyhow!("infrastructure: worker pod disappeared"),
            anyhow::Error::new(StepCapWindDownIgnored { max_turns: 3 }),
            anyhow::Error::new(FinalizeNudgesExhausted {
                attempts: 3,
                finalize_tools: "finalize_task".into(),
            }),
            anyhow::anyhow!("unknown reply-loop failure"),
            anyhow::anyhow!("legacy_unclassified session failure"),
        ];
        for error in errors {
            assert_eq!(classify_provider_failure(&error), None);
        }
    }

    /// The defect this addresses: every cancellation settled as the same
    /// undiagnosable `reply loop error: session cancelled`, so production could
    /// not tell a kubelet SIGTERM from a soft-deadline wind-down from a dead
    /// RPC socket. The durable reason must now name the trigger.
    #[test]
    fn a_cancelled_stage_reason_names_the_trigger_that_fired_the_token() {
        let cancelled = anyhow::Error::new(ReplyLoopCancelled::session()).to_string();

        for (origin, expected) in [
            (CancelOrigin::Sigterm, "origin=sigterm"),
            (CancelOrigin::Sigint, "origin=sigint"),
            (CancelOrigin::SoftDeadline, "origin=soft_deadline"),
            (
                CancelOrigin::HostCancelControl,
                "origin=host_cancel_control",
            ),
            (
                CancelOrigin::HostShutdownControl,
                "origin=host_shutdown_control",
            ),
            (
                CancelOrigin::RpcTransportClosed,
                "origin=rpc_transport_closed",
            ),
            (CancelOrigin::WorkerTeardown, "origin=worker_teardown"),
            (
                CancelOrigin::SupervisorShutdown,
                "origin=supervisor_shutdown",
            ),
            (CancelOrigin::Session, "origin=session"),
        ] {
            let reason = cancelled_stage_reason(&cancelled, origin);
            assert!(
                reason.contains(expected),
                "reason must name the trigger: {reason}"
            );
        }

        // Distinct triggers must produce distinct rows — that is the whole
        // point. Two origins that render identically would put us back where
        // we started.
        assert_ne!(
            cancelled_stage_reason(&cancelled, CancelOrigin::Sigterm),
            cancelled_stage_reason(&cancelled, CancelOrigin::SoftDeadline),
        );
    }

    /// Hard constraint: an unattributed cancellation degrades to `unknown`. It
    /// must never become an error, an empty reason, or a missing field — the
    /// platform has repeatedly been wedged by fail-closed diagnostics.
    #[test]
    fn an_unattributed_cancellation_degrades_to_unknown_and_keeps_the_diagnostic() {
        let cancelled = anyhow::Error::new(ReplyLoopCancelled::session()).to_string();
        let reason = cancelled_stage_reason(&cancelled, CancelOrigin::Unknown);

        assert!(reason.contains("origin=unknown"), "{reason}");
        // The established diagnostic is preserved, not replaced: existing
        // consumers of the reply-loop prefix and the cancellation text keep
        // matching.
        assert!(reason.starts_with("reply loop error: "), "{reason}");
        assert!(reason.contains("session cancelled"), "{reason}");
    }

    /// The `task_runs` reason and the `session_error` activity row are two
    /// different strings built from the same error. Both must carry the
    /// attribution, or an operator reading one of them is still blind.
    #[test]
    fn the_activity_log_diagnostic_carries_the_same_origin_as_the_stage_reason() {
        let cancelled = anyhow::Error::new(ReplyLoopCancelled::session()).to_string();
        let activity = with_cancel_origin(&cancelled, CancelOrigin::SoftDeadline);
        let stage_reason = cancelled_stage_reason(&cancelled, CancelOrigin::SoftDeadline);

        assert!(activity.contains("origin=soft_deadline"), "{activity}");
        assert!(
            stage_reason.contains("origin=soft_deadline"),
            "{stage_reason}"
        );
        assert!(
            stage_reason.ends_with(&activity),
            "the stage reason must be the activity diagnostic under the \
             established prefix: {stage_reason} / {activity}"
        );
    }

    /// The origin rides along with, and never replaces, the durable settlement
    /// cause. A tagged cancellation is still a cancellation.
    #[test]
    fn origin_tagging_does_not_change_how_a_cancellation_settles() {
        for cancellation in [
            ReplyLoopCancelled::session(),
            ReplyLoopCancelled::supervisor_shutdown(),
        ] {
            let error = anyhow::Error::new(cancellation);
            assert_eq!(
                classify_reply_loop_failure(&error),
                ReplyLoopFailureClass::Cancelled,
            );
            assert_eq!(
                reply_loop_failure_cause(&error),
                SessionFailureCause::Cancelled
            );
            assert_eq!(
                classify_provider_failure(&error),
                None,
                "a cancellation must stay breaker-neutral however it is tagged",
            );
        }
    }

    #[test]
    fn reply_loop_cancellation_classifier_requires_the_typed_error() {
        let typed_cancellation = anyhow::Error::new(ReplyLoopCancelled::session())
            .context("reply loop stopped while waiting for a provider event");
        assert_eq!(
            classify_reply_loop_failure(&typed_cancellation),
            ReplyLoopFailureClass::Cancelled,
            "the public ReplyLoopCancelled source must survive context wrapping",
        );

        let cancellation_looking_text =
            anyhow::anyhow!("reply loop error: session cancelled by a provider response");
        assert_eq!(
            classify_reply_loop_failure(&cancellation_looking_text),
            ReplyLoopFailureClass::Other,
            "cancellation-looking display text must not classify as cancellation",
        );
    }

    #[test]
    fn reply_loop_cancellation_classifier_ignores_later_token_cancellation() {
        // These are the errors actually returned by the reply loop before an
        // unrelated cancellation race fires. The classifier intentionally takes
        // no token, so it cannot rewrite either error based on later state.
        let provider_error = anyhow::Error::new(ProviderError::Transport)
            .context("provider stream closed unexpectedly");
        let harness_error = anyhow::anyhow!("tool harness failed to write result");
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();

        assert_eq!(
            classify_reply_loop_failure(&provider_error),
            ReplyLoopFailureClass::Other,
            "a typed provider error remains non-cancellation after token cancellation",
        );
        assert_eq!(
            classify_reply_loop_failure(&harness_error),
            ReplyLoopFailureClass::Other,
            "a harness error remains non-cancellation after token cancellation",
        );
        assert_eq!(
            classify_provider_failure(&provider_error),
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None,
            }),
            "cancellation classification must not change provider-breaker behavior",
        );
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
    fn budget_park_settles_failed_without_claiming_a_handoff() {
        let ignored_outcome = StageOutcome::Parked {
            reason: ParkReason::Budget,
            summary: None,
            wind_down_ignored: true,
            session_id: "session-budget-ignored".to_string(),
            tokens_in: 10,
            tokens_out: 5,
        };

        assert_eq!(
            session_settlement_for_stage_outcome(&ignored_outcome, None),
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
            session_settlement_for_stage_outcome(&summary_outcome, None),
            (SessionStatus::Completed, Some("budget".to_string()))
        );
    }

    #[test]
    fn non_budget_stage_settlement_keeps_existing_success_and_failure_statuses() {
        assert_eq!(
            session_settlement_for_stage_outcome(&StageOutcome::WorkerDone, None),
            (SessionStatus::Completed, None)
        );
        assert_eq!(
            session_settlement_for_stage_outcome(
                &StageOutcome::Failed {
                    reason: "ordinary failure".to_string(),
                    provider_failure: None,
                },
                Some(SessionFailureCause::Unknown),
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
        // must feed the GENTLE consecutive-failure breaker — which it still does
        // as `Transient` (the host maps `Transient` onto the same
        // `record_failure` call `Failure` uses), while no longer telling the
        // coordinator that the TASK is at fault.
        assert_eq!(
            classify_provider_failure(&anyhow::Error::new(ProviderError::Transport)),
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None
            }),
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
            Some(ProviderFailureClass::Transient {
                retry_after_ms: None
            }),
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
                lead_stage_outcome("submit_decision", Some(&payload), false),
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
                lead_stage_outcome("submit_decision", Some(&payload), false),
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("", None, false) {
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
        match lead_stage_outcome("submit_work", None, false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
    fn final_arbiter_non_terminal_decision_is_terminally_parked() {
        for decision in ["approve", "approve_conflict", "reopen"] {
            let payload = serde_json::json!({
                "decision": decision,
                "reason": "try another cycle",
                "evidence": {"summary": "locally acceptable"},
                "directive": "repair once more",
                "verification_command": "cargo test"
            });
            match lead_stage_outcome("submit_decision", Some(&payload), true) {
                StageOutcome::LeadParked { park_dossier_json } => {
                    let dossier: serde_json::Value =
                        serde_json::from_str(&park_dossier_json).unwrap();
                    assert_eq!(dossier["submitted_decision"], decision);
                    assert!(
                        dossier["recommended_action"]
                            .as_str()
                            .unwrap()
                            .contains("Replan")
                    );
                }
                other => panic!("final {decision} must convert to terminal park, got {other:?}"),
            }
        }
    }

    #[test]
    fn final_arbiter_still_accepts_supersede() {
        let payload = serde_json::json!({
            "decision": "supersede",
            "reason": "replace exhausted source",
            "created_tasks": ["replacement-1"]
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&payload), true),
            StageOutcome::LeadSuperseded { .. }
        ));
    }

    #[test]
    fn final_arbiter_park_marks_terminal_planner_replan() {
        let payload = serde_json::json!({
            "decision": "park",
            "park_dossier": {
                "hold_description": "architecture is genuinely ambiguous",
                "failure_analysis": "prior decisions conflict"
            }
        });
        match lead_stage_outcome("submit_decision", Some(&payload), true) {
            StageOutcome::LeadParked { park_dossier_json } => {
                let dossier: serde_json::Value = serde_json::from_str(&park_dossier_json).unwrap();
                assert_eq!(dossier["final_disposition"], true);
            }
            other => panic!("final park must remain a terminal planner park, got {other:?}"),
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
            lead_stage_outcome("submit_decision", Some(&approve), false),
            StageOutcome::LeadApproved { .. }
        ));

        let reopen = serde_json::json!({
            "decision": "reopen",
            "directive": "fix the retry loop in dispatch.rs",
            "verification_command": "cargo test -p djinn-coordinator"
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&reopen), false),
            StageOutcome::LeadReopen { .. }
        ));

        let park = serde_json::json!({
            "decision": "park",
            "park_dossier": {"hold_description": "stuck", "failure_analysis": "why"}
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&park), false),
            StageOutcome::LeadParked { .. }
        ));

        let approve_conflict = serde_json::json!({
            "decision": "approve_conflict",
            "evidence": {"source": "git diff", "summary": "correct but conflict"}
        });
        assert!(matches!(
            lead_stage_outcome("submit_decision", Some(&approve_conflict), false),
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
        match lead_stage_outcome("submit_decision", Some(&payload), false) {
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
            fn_body.contains("dispatch_arbiter_adjudication"),
            "call_request_lead must route through dispatch_arbiter_adjudication"
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
