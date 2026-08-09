// djinn:allow-oversize — legacy phase driving and the durable-intent authority remain co-located to prevent duplicate dispatch.
// Proposal-refinement tribunal dispatch orchestration.
//
// Drives the Advocate → Adversary → Judge refinement loop by dispatching
// sessions through the slot pool and reading outcomes from the DB.
//
// Architecture:
//   - `drive_active_refinements()` is called from `run_tick()` on every
//     coordinator tick (~30s).
//   - For each active refinement, it checks for in-flight sessions. If a
//     session completed, it reads the outcome from the DB and advances the
//     `RefinementLoopState`.
//   - If no session is in-flight and the loop isn't complete, dispatch the
//     next phase.
//   - Each refinement task is created with `issue_type = "refinement"` and
//     `agent_type = "advocate"/"adversary"/"judge"`, so the supervisor's
//     role-overrides layer resolves the correct `AgentType`.
//
// Persistence:
//   - Adversary objections: persisted through
//     `ProposalRepository::add_debate_trail_entry()` with `kind = "objection"`.
//   - Judge verdicts: persisted through
//     `ProposalRepository::add_debate_trail_entry()` with `kind = "verdict"`.
//   - Stop metadata: persisted via `record_refinement_lifecycle`.

use std::collections::HashSet;
use std::time::{Duration, Instant as StdInstant};

use super::evidence_lifecycle_state::EvidenceLifecycleState;
use super::refinement::{RefinementPhase, StopReason, role_for_phase};

use super::actor::CoordinatorActor;
use super::refinement_inflight::DurableInflightRegistration;
use super::refinement_outcome::RefinementOutcomeApplication;
use super::types::{InflightDispatch, REFINEMENT_INTENT_CLAIM_LEASE_MILLIS};
use crate::poll_stack;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::TaskRefinementCorrelation;
use djinn_core::refinement_liveness::RefinementRole;
use djinn_db::{
    AcknowledgeRefinementTaskMaterializationRequest, ClaimRefinementIntentRequest,
    ProposalRepository, TaskRepository,
};

/// How long a refinement agent session may run (measured from **session
/// start**, not dispatch) before treating it as stalled (conservative —
/// sessions can take 5+ min). Time the task-run pod spends Pending in the k8s
/// scheduler queue before any agent session starts does NOT count against this
/// budget — see [`REFINEMENT_PENDING_START_TIMEOUT`].
const REFINEMENT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How long a dispatched refinement session may sit **without its agent
/// session ever starting** (e.g. the task-run pod stuck Pending in the k8s
/// scheduler queue under CPU pressure) before the dispatch is abandoned and
/// retried. Distinct from [`REFINEMENT_SESSION_TIMEOUT`], which bounds actual
/// agent execution once a session has started. Expiry is retryable (bounded by
/// [`REFINEMENT_DISPATCH_RETRY_CAP`]), never terminal on its own — a slow
/// scheduler must not permanently kill a tribunal that never got to run
/// (incident 2026-07-12).
const REFINEMENT_PENDING_START_TIMEOUT: Duration = Duration::from_secs(1200);

/// How many consecutive times a refinement role session may fail to start
/// before the loop terminates instead of re-dispatching.
const REFINEMENT_DISPATCH_RETRY_CAP: i32 = 3;

/// The in-flight session tracking for one active refinement loop.
#[derive(Debug, Clone)]
pub(super) struct RefinementSession {
    /// Exact durable run and generation observed when this disposable session
    /// projection was created. These fence late task observations.
    pub run_id: String,
    pub generation: i32,
    /// The task id of the refinement task currently dispatched.
    pub task_id: String,
    /// Which phase this session is executing.
    pub phase: RefinementPhase,
    /// When the session was dispatched (task-run pool dispatch time). Used to
    /// bound the Pending-in-queue wait before the agent session starts, NOT the
    /// execution budget — see `session_started_at`.
    pub dispatched_at: StdInstant,
    /// First tick at which an agent session row was observed for `task_id`,
    /// i.e. when the agent actually started running. `None` while the task-run
    /// pod is still Pending / no session row exists yet. The execution timeout
    /// (`REFINEMENT_SESSION_TIMEOUT`) is measured from this instant so pod
    /// scheduler-queue time is never charged against the agent's budget. Cached
    /// so start detection stops re-querying the DB once confirmed. In-memory
    /// only; a coordinator restart drops in-flight sessions entirely (the loop
    /// re-dispatches), so this needs no persistence.
    pub session_started_at: Option<StdInstant>,
    /// The model used for this session.
    #[allow(dead_code)]
    pub model_id: String,
}

// ─── Main dispatch loop ─────────────────────────────────────────────────────

fn durable_role_name(role: RefinementRole) -> &'static str {
    match role {
        RefinementRole::Adversary => "adversary",
        RefinementRole::Advocate => "advocate",
        RefinementRole::Judge => "judge",
    }
}

/// Project a durable intent phase onto the in-process loop phase. Sole
/// conversion point, so a new durable phase variant fails to compile here
/// rather than silently mis-projecting at one of two call sites.
fn refinement_phase_for_intent(
    phase: djinn_core::refinement_liveness::RefinementPhase,
) -> RefinementPhase {
    match phase {
        djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack => {
            RefinementPhase::AdversaryAttack
        }
        djinn_core::refinement_liveness::RefinementPhase::AdvocateRevision => {
            RefinementPhase::AdvocateRevision
        }
        djinn_core::refinement_liveness::RefinementPhase::JudgeAdjudication => {
            RefinementPhase::JudgeAdjudication
        }
    }
}

/// Why the administrative / terminal gates forbid **new** durable refinement
/// dispatch on this tick.
///
/// The durable intent ledger is the only dispatch path for a run with a
/// `run_id`, so it must honour the same operator controls
/// `dispatch_next_refinement_phase` already honours — otherwise
/// `dispatch_pause` and `proposal_stop_build --freeze` stop epic work while the
/// tribunal keeps spawning adversary/advocate/judge sessions. Derivation is
/// shared with the legacy path (`derive_proposal_evidence_lifecycle`), so this
/// is gate parity, not a second policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableDispatchBlock {
    /// Administrative dispatch pause (global or project) or
    /// `proposals.build_frozen`.
    PausedOrFrozen,
    /// The proposal reached a terminal status (`TERMINAL_PROPOSAL_STATUSES`).
    Terminal,
    /// The proposal row could not be read, so no gate state is known. Fail
    /// closed, exactly as the legacy path does.
    ProposalUnreadable,
    /// Typed evidence is demanded, active, failed, or invalid; do not create
    /// another tribunal task until the authoritative finding changes.
    EvidenceBlocked,
}

impl DurableDispatchBlock {
    fn as_str(self) -> &'static str {
        match self {
            Self::PausedOrFrozen => "paused_or_frozen",
            Self::Terminal => "terminal",
            Self::ProposalUnreadable => "proposal_unreadable",
            Self::EvidenceBlocked => "typed_evidence_blocked",
        }
    }
}

/// Shared readiness context plus deliberately separate feedback channels.
///
/// Ordinary reviewer feedback remains available to every tribunal role.
/// Immutable captured human-feedback obligations are source-aware work for the
/// Advocate and Judge only; they must not alter an Adversary's context.
pub(super) struct RefinementRoleContext {
    /// Rendered current-head DoR failures and `SpecLintResultV1` summary, plus
    /// any advocate spec-lint correction owed for this same round.
    pub readiness_context: String,
    /// Latest current-revision reviewer feedback.
    pub reviewer_feedback: Option<String>,
    /// Immutable boundary-captured feedback obligations.
    pub human_feedback_context: Option<String>,
}

impl RefinementRoleContext {
    fn feedback_for_phase(
        reviewer_feedback: Option<String>,
        human_feedback_context: Option<String>,
        phase: RefinementPhase,
    ) -> Option<String> {
        let obligations = matches!(
            phase,
            RefinementPhase::AdvocateRevision | RefinementPhase::JudgeAdjudication
        )
        .then_some(human_feedback_context)
        .flatten();
        match (reviewer_feedback, obligations) {
            (Some(reviewer), Some(obligations)) => Some(format!("{reviewer}\n\n{obligations}")),
            (Some(reviewer), None) => Some(reviewer),
            (None, Some(obligations)) => Some(obligations),
            (None, None) => None,
        }
    }
}

impl CoordinatorActor {
    /// Drive all active refinement loops. Called from `run_tick()`.
    pub(super) async fn drive_active_refinements(&mut self) {
        let run_ids: Vec<String> = self.active_refinements.keys().cloned().collect();
        if run_ids.is_empty() {
            // The durable ledger remains dispatch authority when no disposable
            // projection survived to this tick.
            poll_stack::boxed(|| self.drive_durable_refinement_intents()).await;
            return;
        }

        // Fetch the pool's running-task set ONCE per tick and check membership
        // in memory, instead of issuing one `pool.has_session(...)` ask per
        // active refinement. During the 2026-07-09 freeze ~12 active refinements
        // meant ~12 un-timed pool round-trips every tick, amplifying the single
        // stalled ask into a per-tick pile-up on the coordinator's mailbox. One
        // `get_status()` (already the primitive used by the in-flight-ledger
        // reconciler) is O(1) in pool round-trips regardless of refinement count.
        //
        // On a pool error (including the new `PoolError::Timeout`) liveness is
        // unknown this tick → `None`. `drive_one_refinement` then DEFERS only the
        // in-flight liveness check (it must not misclassify a possibly-running
        // session as finished and prematurely process its outcome / burn a
        // round) while still driving refinements that have no in-flight session,
        // where `dispatch_next_refinement_phase` already handles pool failures.
        // This preserves the pre-existing per-refinement behavior for the
        // no-session path while staying conservative for the stall case.
        let running_tasks: Option<HashSet<String>> = match self.pool.get_status().await {
            Ok(status) => Some(
                status
                    .running_tasks
                    .into_iter()
                    .map(|t| t.task_id)
                    .collect(),
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: pool get_status failed while driving refinements; \
                     in-flight liveness unknown this tick (those refinements deferred; \
                     new dispatches still attempted)"
                );
                None
            }
        };

        for run_id in run_ids {
            poll_stack::boxed(|| self.drive_one_refinement(&run_id, running_tasks.as_ref())).await;
        }

        // The durable intent ledger is the dispatch authority. Run it after
        // monitoring the projections that existed at tick entry: a successful
        // enqueue must leave its newly-created outcome projection intact until
        // the next tick can observe its real session/outcome evidence.
        poll_stack::boxed(|| self.drive_durable_refinement_intents()).await;

        // Clean up completed refinements.
        self.active_refinements
            .retain(|_, state| !state.is_complete());
    }

    /// Lease and materialize exact-run intents. A task is acknowledged only
    /// after its correlation is durably readable. Pool failure intentionally
    /// leaves the materialized task open for a later tick.
    async fn drive_durable_refinement_intents(&mut self) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus.clone());
        let task_repo = TaskRepository::new(self.db.clone(), event_bus);
        let runs = match proposal_repo.load_active_refinement_runs().await {
            Ok(runs) => runs,
            Err(error) => {
                tracing::warn!(%error, "failed to load durable refinement runs");
                return;
            }
        };
        for run in runs {
            let intents = match proposal_repo
                .load_dispatchable_refinement_intents(&run.run_id, run.generation)
                .await
            {
                Ok(intents) => intents,
                Err(error) => {
                    tracing::warn!(run_id = %run.run_id, %error, "failed to load refinement intents");
                    continue;
                }
            };
            if intents.is_empty() {
                continue;
            }
            // Operator-control parity with the legacy phase dispatcher, read
            // once per run rather than once per intent. Consulted only at the
            // points that would create or enqueue role work — never before the
            // recovery arm below, which merely re-registers an already-running
            // role so its outcome is still processed while the gate holds.
            let dispatch_block = self
                .durable_refinement_dispatch_block(&run.proposal_id)
                .await;
            for intent in intents {
                if matches!(
                    intent.state,
                    djinn_core::refinement_liveness::RefinementIntentState::Materialized
                ) {
                    match task_repo
                        .find_by_refinement_intent_id(&intent.intent_id)
                        .await
                    {
                        Ok(Some(task)) => {
                            let proposal = match proposal_repo.get(&run.proposal_id).await {
                                Ok(Some(proposal)) => proposal,
                                Ok(None) | Err(_) => continue,
                            };
                            // A previously successful enqueue may have completed
                            // before this coordinator rebuilt its disposable
                            // projection. Recover that outcome without sending a
                            // closed role task through the pool again.
                            let role_ran =
                                poll_stack::boxed(|| self.refinement_session_has_started(&task.id))
                                    .await;
                            if role_ran {
                                poll_stack::boxed(|| {
                                    self.register_durable_refinement_inflight(
                                        DurableInflightRegistration {
                                            run_id: &run.run_id,
                                            generation: run.generation,
                                            proposal: &proposal,
                                            phase: intent.phase,
                                            round: intent.round,
                                            task_id: &task.id,
                                        },
                                    )
                                })
                                .await;
                                continue;
                            }
                            // The role never ran, so this arm would push the
                            // materialized task through the pool again. Gated:
                            // leave the intent `materialized` with its task
                            // intact — nothing is consumed, terminalized, or
                            // abandoned, and a `materialized` intent is durable
                            // liveness evidence, so the run cannot be reaped
                            // while it waits. The same retry enqueues it once
                            // the gate clears.
                            if let Some(block) = dispatch_block {
                                tracing::info!(
                                    run_id = %run.run_id,
                                    proposal_id = %run.proposal_id,
                                    intent_id = %intent.intent_id,
                                    gate = block.as_str(),
                                    "durable refinement enqueue skipped by an administrative gate; \
                                     the materialized intent keeps its task and re-enqueues when \
                                     the gate clears"
                                );
                                continue;
                            }
                            let Some(owner) = proposal.refinement_owner_user_id.as_deref() else {
                                tracing::warn!(
                                    intent_id = %intent.intent_id,
                                    run_id = %run.run_id,
                                    proposal_id = %run.proposal_id,
                                    "refinement run has no durable owner; its intent cannot dispatch and will \
                                     be retried on every tick until the proposal is attributed"
                                );
                                continue;
                            };
                            let Some((_agent_type, model_id)) = self
                                .resolve_durable_refinement_dispatch_params(
                                    &run.proposal_id,
                                    intent.phase,
                                    owner,
                                )
                                .await
                            else {
                                continue;
                            };
                            let project_path = poll_stack::boxed(|| {
                                self.resolve_refinement_project_path(&run.proposal_id)
                            })
                            .await;
                            match self.pool.dispatch(&task.id, &project_path, &model_id).await {
                                Ok(()) => {
                                    // Do not manufacture the outcome projection
                                    // until this retry enqueue has succeeded.
                                    poll_stack::boxed(|| {
                                        self.register_durable_refinement_inflight(
                                            DurableInflightRegistration {
                                                run_id: &run.run_id,
                                                generation: run.generation,
                                                proposal: &proposal,
                                                phase: intent.phase,
                                                round: intent.round,
                                                task_id: &task.id,
                                            },
                                        )
                                    })
                                    .await;
                                    if let Some(session) =
                                        self.refinement_sessions.get_mut(&run.run_id)
                                        && session.run_id == run.run_id
                                        && session.generation == run.generation
                                        && session.task_id == task.id
                                    {
                                        session.model_id = model_id;
                                        session.session_started_at = None;
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(task_id = %task.id, %error, "materialized refinement enqueue failed; retrying on next durable poll")
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(intent_id = %intent.intent_id, "materialized intent has no correlated task")
                        }
                        Err(error) => {
                            tracing::warn!(intent_id = %intent.intent_id, %error, "failed to load materialized refinement task")
                        }
                    }
                    continue;
                }
                // Gated, and this intent is still `pending`: do not even take
                // the lease. A `pending` intent is permanent durable liveness
                // evidence (`evaluate_refinement_liveness`), so leaving it
                // untouched survives any pause length and any coordinator
                // restart, and the next ungated tick claims it normally.
                // Claiming it here would instead convert it to `claimed`, whose
                // only liveness evidence is an unexpired lease — a coordinator
                // that died mid-pause would then leave the run reapable as
                // `reaped_phantom`.
                if let Some(block) = dispatch_block
                    && matches!(
                        intent.state,
                        djinn_core::refinement_liveness::RefinementIntentState::Pending
                    )
                {
                    tracing::info!(
                        run_id = %run.run_id,
                        proposal_id = %run.proposal_id,
                        intent_id = %intent.intent_id,
                        gate = block.as_str(),
                        "durable refinement dispatch skipped by an administrative gate; the pending \
                         intent is left unleased and dispatches when the gate clears"
                    );
                    continue;
                }
                let owner = format!("coordinator:{}", self.coordinator_incarnation_id);
                let Some(lease) = (match proposal_repo
                    .claim_refinement_intent(ClaimRefinementIntentRequest {
                        run_id: run.run_id.clone(),
                        intent_id: intent.intent_id.clone(),
                        generation: run.generation,
                        owner: owner.clone(),
                        // Derived from the poll cadence, never a bare number:
                        // this same call is what RENEWS the lease on every
                        // durable poll, so the lease has to outlive the worst
                        // case interval between two polls.
                        lease_millis: REFINEMENT_INTENT_CLAIM_LEASE_MILLIS,
                    })
                    .await
                {
                    Ok(lease) => lease,
                    Err(error) => {
                        tracing::warn!(intent_id = %intent.intent_id, %error, "failed to claim refinement intent");
                        continue;
                    }
                }) else {
                    continue;
                };

                // Gated, and this intent was already `claimed` before the gate
                // engaged. The claim above is a *renewal*, which is the only
                // thing that keeps a `claimed` intent's liveness evidence fresh
                // (see `claim_refinement_intent`), so it must keep happening
                // while the gate holds; only task creation and pool enqueue
                // stop. Nothing is consumed — the lease is retained and the
                // next ungated tick materializes this same intent.
                if let Some(block) = dispatch_block {
                    tracing::info!(
                        run_id = %run.run_id,
                        proposal_id = %run.proposal_id,
                        intent_id = %lease.intent_id,
                        gate = block.as_str(),
                        "durable refinement dispatch skipped by an administrative gate; the claimed \
                         intent keeps a renewed lease and dispatches when the gate clears"
                    );
                    continue;
                }

                // Resolve owner/model/admission before task creation. A denied
                // attempt keeps the lease/intention retryable without creating
                // an un-dispatchable board task.
                let proposal = match proposal_repo.get(&run.proposal_id).await {
                    Ok(Some(proposal)) => proposal,
                    Ok(None) | Err(_) => continue,
                };
                let Some(attributed_user) = proposal.refinement_owner_user_id.clone() else {
                    tracing::warn!(
                        intent_id = %lease.intent_id,
                        run_id = %run.run_id,
                        proposal_id = %run.proposal_id,
                        "refinement run has no durable owner; its intent cannot dispatch and will \
                         be retried on every tick until the proposal is attributed"
                    );
                    continue;
                };
                let Some((_agent_type, model_id)) = self
                    .resolve_durable_refinement_dispatch_params(
                        &run.proposal_id,
                        lease.phase,
                        &attributed_user,
                    )
                    .await
                else {
                    continue;
                };

                let task_id = match task_repo
                    .find_by_refinement_intent_id(&lease.intent_id)
                    .await
                {
                    Ok(Some(task)) => task.id,
                    Ok(None) => {
                        let correlation = match TaskRefinementCorrelation::new(
                            lease.run_id.clone(),
                            lease.intent_id.clone(),
                            i64::from(lease.generation),
                            i64::from(lease.round),
                            lease.phase,
                            lease.role,
                        ) {
                            Ok(correlation) => correlation,
                            Err(error) => {
                                tracing::warn!(intent_id = %lease.intent_id, %error, "invalid durable refinement correlation");
                                continue;
                            }
                        };
                        let role = durable_role_name(lease.role);
                        // The durable ledger is the ONLY dispatch path for a
                        // run with a `run_id`, so this context is what every
                        // live tribunal role actually reads. It must carry the
                        // real DoR/lint findings and reviewer feedback — a
                        // placeholder here leaves the Advocate unable to fix
                        // readiness and forces a mandatory Judge reject.
                        let RefinementRoleContext {
                            readiness_context,
                            reviewer_feedback,
                            human_feedback_context,
                        } = self
                            .build_refinement_role_context(
                                &run.proposal_id,
                                &run.run_id,
                                refinement_phase_for_intent(lease.phase),
                            )
                            .await;
                        let reviewer_feedback = RefinementRoleContext::feedback_for_phase(
                            reviewer_feedback,
                            human_feedback_context,
                            refinement_phase_for_intent(lease.phase),
                        );
                        let Some(task_id) = self
                            .create_refinement_task_with_context_and_correlation(
                                &run.proposal_id,
                                role,
                                lease.round,
                                proposal.latest_revision_seq,
                                &readiness_context,
                                reviewer_feedback.as_deref(),
                                Some(&attributed_user),
                                Some(&correlation),
                            )
                            .await
                        else {
                            continue;
                        };
                        task_id
                    }
                    Err(error) => {
                        tracing::warn!(intent_id = %lease.intent_id, %error, "failed to find materialized refinement task");
                        continue;
                    }
                };
                if proposal_repo
                    .acknowledge_refinement_task_materialization(
                        AcknowledgeRefinementTaskMaterializationRequest {
                            run_id: lease.run_id.clone(),
                            intent_id: lease.intent_id.clone(),
                            generation: lease.generation,
                            task_id: task_id.clone(),
                            owner: owner.clone(),
                        },
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                let project_path =
                    poll_stack::boxed(|| self.resolve_refinement_project_path(&run.proposal_id))
                        .await;
                match self.pool.dispatch(&task_id, &project_path, &model_id).await {
                    Ok(()) => {
                        // Register only after the pool accepted this exact task.
                        // This is the run-keyed bridge to outcome completion.
                        poll_stack::boxed(|| {
                            self.register_durable_refinement_inflight(DurableInflightRegistration {
                                run_id: &lease.run_id,
                                generation: lease.generation,
                                proposal: &proposal,
                                phase: lease.phase,
                                round: lease.round,
                                task_id: &task_id,
                            })
                        })
                        .await;
                        if let Some(session) = self.refinement_sessions.get_mut(&lease.run_id)
                            && session.run_id == lease.run_id
                            && session.generation == lease.generation
                            && session.task_id == task_id
                        {
                            session.model_id = model_id;
                            session.session_started_at = None;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(task_id = %task_id, %error, "durable refinement enqueue failed; task remains retryable")
                    }
                }
            }
        }
    }

    /// Whether the operator-facing gates permit **new** durable refinement
    /// dispatch for this proposal on this tick.
    ///
    /// Parity with the three gates `dispatch_next_refinement_phase` honours:
    /// the administrative dispatch pause, `proposals.build_frozen`, and a
    /// terminal proposal status. All three are read through the same
    /// [`EvidenceLifecycleState`] derivation the legacy path uses, so there is
    /// one definition of "paused/frozen/terminal" for both dispatch paths.
    ///
    /// Fail-closed on an unreadable proposal, mirroring the legacy path's
    /// "could not load proposal for evidence lifecycle check (fail-closed)"
    /// arm: without the proposal row the gate state is unknown, and the durable
    /// path must not spawn a tribunal role on a proposal it cannot see. (The
    /// pause-state read *inside* `refinement_dispatch_paused` keeps its own
    /// legacy fail-open behaviour — this helper deliberately does not change it,
    /// so both paths agree.)
    ///
    /// Returns `None` when dispatch may proceed.
    async fn durable_refinement_dispatch_block(
        &self,
        proposal_id: &str,
    ) -> Option<DurableDispatchBlock> {
        let Some(proposal) =
            poll_stack::boxed(|| self.load_proposal_for_lifecycle(proposal_id)).await
        else {
            tracing::warn!(
                proposal_id = %proposal_id,
                "Durable refinement dispatch skipped: could not load proposal for evidence \
                 lifecycle check (fail-closed)"
            );
            return Some(DurableDispatchBlock::ProposalUnreadable);
        };
        match poll_stack::boxed(|| self.derive_proposal_evidence_lifecycle(&proposal)).await {
            EvidenceLifecycleState::PausedOrFrozen => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    lifecycle_state = "paused_or_frozen",
                    build_frozen = proposal.build_frozen,
                    "Durable refinement dispatch skipped: proposal is paused or build-frozen \
                     (persisted state gate)"
                );
                Some(DurableDispatchBlock::PausedOrFrozen)
            }
            EvidenceLifecycleState::Terminal => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    lifecycle_state = "terminal",
                    proposal_status = %proposal.status,
                    "Durable refinement dispatch skipped: proposal in terminal status \
                     (persisted state gate)"
                );
                Some(DurableDispatchBlock::Terminal)
            }
            // Receipt permits only the Advocate fold/resume path. Demanded,
            // active, failed, and invalid projections remain fail-closed.
            EvidenceLifecycleState::Active | EvidenceLifecycleState::EvidenceReady => None,
            EvidenceLifecycleState::AwaitingEvidence | EvidenceLifecycleState::EvidenceFailed => {
                Some(DurableDispatchBlock::EvidenceBlocked)
            }
        }
    }

    /// Resolve the exact phase through the established owner-scoped model
    /// selection and admission path. A denial leaves the durable intent
    /// retryable and never authorizes a second task.
    async fn resolve_durable_refinement_dispatch_params(
        &mut self,
        proposal_id: &str,
        phase: djinn_core::refinement_liveness::RefinementPhase,
        attributed_user: &str,
    ) -> Option<(String, String)> {
        let owner = self
            .resolve_refinement_attributed_user(proposal_id, Some(attributed_user.to_owned()))
            .await?;
        if owner.trim().is_empty()
            || !matches!(
                djinn_db::UserRepository::new(self.db.clone())
                    .get_by_id(&owner)
                    .await,
                Ok(Some(_))
            )
        {
            return None;
        }

        let phase = refinement_phase_for_intent(phase);
        let diverse_refinement =
            poll_stack::boxed(|| self.read_diverse_refinement_setting(proposal_id)).await;
        let (agent_type, model_id) = self
            .resolve_refinement_dispatch_params(phase, diverse_refinement, Some(owner.as_str()))
            .await?;
        let settings = djinn_db::UserSettingsRepository::new(self.db.clone())
            .get(&owner)
            .await
            .ok()
            .flatten();
        let model_cap = settings
            .as_ref()
            .and_then(|settings| settings.max_sessions.as_ref())
            .and_then(|caps| caps.get(&model_id))
            .copied()
            .unwrap_or(1);
        let lane_cap = settings
            .and_then(|settings| settings.lane_max_sessions)
            .map(|limits| limits.for_role(&agent_type));
        poll_stack::boxed(|| {
            self.check_user_model_admission(&owner, &model_id, model_cap, &agent_type, lane_cap)
        })
        .await
        .then_some((agent_type, model_id))
    }

    /// Drive a single refinement loop. `running_tasks` is the pool's running
    /// task-id set, fetched once by the caller so this method issues no
    /// per-refinement pool round-trip for the liveness check. `None` means the
    /// pool status was unavailable this tick (see `drive_active_refinements`):
    /// the in-flight liveness check is deferred, but a refinement with no
    /// in-flight session is still driven to dispatch.
    async fn drive_one_refinement(
        &mut self,
        run_id: &str,
        running_tasks: Option<&HashSet<String>>,
    ) {
        let Some(state) = self.active_refinements.get(run_id).cloned() else {
            return;
        };
        let proposal_id = state.proposal_id.clone();
        if state.is_complete() {
            return;
        }

        if !state.run_id.is_empty()
            && !self
                .refinement_projection_is_current(run_id, state.generation, &proposal_id)
                .await
        {
            self.active_refinements.remove(run_id);
            self.refinement_sessions.remove(run_id);
            return;
        }

        // Check if there's an in-flight session for this refinement.
        if let Some(session) = self.refinement_sessions.get(run_id).cloned() {
            if session.run_id != state.run_id || session.generation != state.generation {
                self.refinement_sessions.remove(run_id);
                return;
            }
            let Some(running_tasks) = running_tasks else {
                // Pool liveness unknown this tick (get_status errored). Do NOT
                // treat a possibly-running session as finished — defer to the
                // next tick rather than risk prematurely processing its outcome.
                return;
            };
            let still_running = running_tasks.contains(&session.task_id);

            if still_running {
                // Resolve whether the agent session has actually started yet.
                // The execution budget (`REFINEMENT_SESSION_TIMEOUT`) must be
                // measured from session start, NOT from dispatch: time the
                // task-run pod spends Pending in the k8s scheduler queue (before
                // any session row exists) must not count against it. Otherwise
                // scheduler back-pressure silently terminates tribunals that
                // never got to run (incident 2026-07-12). Cache the first
                // observed start so we stop re-querying the DB every tick.
                let session_started_at = match session.session_started_at {
                    Some(started) => Some(started),
                    None => {
                        if poll_stack::boxed(|| {
                            self.refinement_session_has_started(&session.task_id)
                        })
                        .await
                        {
                            let observed = SystemClock::new().now_instant();
                            if let Some(s) = self.refinement_sessions.get_mut(run_id) {
                                s.session_started_at = Some(observed);
                            }
                            Some(observed)
                        } else {
                            None
                        }
                    }
                };

                match session_started_at {
                    Some(started_at) => {
                        // The agent session has started — bound actual execution
                        // from session start.
                        if started_at.elapsed() > REFINEMENT_SESSION_TIMEOUT {
                            tracing::warn!(
                                proposal_id = %proposal_id,
                                task_id = %session.task_id,
                                phase = ?session.phase,
                                "Refinement session timed out (execution budget from session start)"
                            );
                            poll_stack::boxed(|| {
                                self.close_refinement_task(
                                    &session.task_id,
                                    "refinement session timed out",
                                )
                            })
                            .await;
                            poll_stack::boxed(|| {
                                self.terminate_refinement(
                                    run_id,
                                    StopReason::AgentFailure {
                                        role: role_for_phase(session.phase),
                                        error_code: "agent_failure".into(),
                                        message: "session timeout".into(),
                                    },
                                )
                            })
                            .await;
                        }
                    }
                    None => {
                        // Dispatched but the agent session has not started — the
                        // task-run pod is (most likely) still Pending in the
                        // scheduler queue. Do NOT burn the execution budget, but
                        // bound the wait so a forever-Pending pod cannot hold the
                        // tribunal open indefinitely. On expiry, abandon THIS
                        // dispatch and retry the same phase (bounded by the retry
                        // cap) rather than terminally kill the tribunal.
                        if session.dispatched_at.elapsed() > REFINEMENT_PENDING_START_TIMEOUT {
                            tracing::warn!(
                                proposal_id = %proposal_id,
                                task_id = %session.task_id,
                                phase = ?session.phase,
                                elapsed_secs = session.dispatched_at.elapsed().as_secs(),
                                "Refinement session never started (task-run pod Pending past \
                                 pending-start timeout); abandoning dispatch and retrying"
                            );
                            let over_cap_error = format!(
                                "role session failed to start {REFINEMENT_DISPATCH_RETRY_CAP} times \
                                 (task-run pod stuck Pending in scheduler queue)"
                            );
                            poll_stack::boxed(|| self.retry_or_terminate_unstarted_refinement(
                                run_id,
                                &session,
                                "refinement role session never started (task-run pod stuck Pending)",
                                over_cap_error,
                                true,
                            ))
                            .await;
                        }
                    }
                }
                return;
            }

            // The slot is no longer running this task. That can mean two very
            // different things:
            //   (a) the agent session actually ran and finished — process its
            //       outcome from the DB (debate trail / revisions); or
            //   (b) the session never started (runtime/devcontainer setup
            //       failure freed the slot before any session row was created).
            // Treating (b) as a completed-but-"dry" round silently burns rounds
            // on a dispatch outage and can hollow-converge the tribunal. Tell
            // them apart by whether any session row exists for the task.
            let session_ran =
                poll_stack::boxed(|| self.refinement_session_has_started(&session.task_id)).await;

            if !session_ran {
                // Dispatch/setup failure: the role never executed. Re-dispatch
                // the same phase on the next tick, bounded by a retry cap so a
                // persistently broken runtime escalates instead of looping. The
                // slot already freed on its own here (the pool no longer reports
                // the task running), so no pool teardown is needed.
                let over_cap_error = format!(
                    "role session failed to start {REFINEMENT_DISPATCH_RETRY_CAP} times \
                     (runtime/devcontainer setup failure)"
                );
                poll_stack::boxed(|| {
                    self.retry_or_terminate_unstarted_refinement(
                        run_id,
                        &session,
                        "refinement role session never started (dispatch/setup failure)",
                        over_cap_error,
                        false,
                    )
                })
                .await;
                return;
            }

            // (c) the session ran, but the PROVIDER died under it. This is a
            // third case, and reading it as (a) is what broke task `nr41` on
            // 2026-07-29: an OpenAI `server_is_overloaded` 500 killed the
            // Adversary ten seconds into round 3, the loop found a session row,
            // read an empty debate trail, and scored the round as an explicit
            // dry pass — burning a round, force-closing the task, and leaving
            // the run with no live work at all.
            //
            // A round is only evidence about the proposal if the role got to
            // form an opinion. When the upstream failed instead, there is no
            // opinion to record, so the round must be retried rather than
            // counted. This routes into the SAME bounded retry the never-started
            // paths use — no new scheduler, and the same
            // `REFINEMENT_DISPATCH_RETRY_CAP` ceiling so a provider that is down
            // for good still terminates the loop with an honest reason instead
            // of looping forever.
            if self.refinement_round_died_on_transient_provider_fault(&session.task_id) {
                let over_cap_error = format!(
                    "role session died on a transient provider fault \
                     {REFINEMENT_DISPATCH_RETRY_CAP} times (upstream 5xx / overload / \
                     mid-flight stream death)"
                );
                poll_stack::boxed(|| {
                    self.retry_or_terminate_unstarted_refinement(
                    run_id,
                    &session,
                    "refinement role session died on a transient provider fault (parked for retry)",
                    over_cap_error,
                    false,
                )
                })
                .await;
                return;
            }

            // Session actually ran — clear the dispatch-failure counter and
            // process the outcome, then close the task so finished phase/round
            // tasks don't linger `open` on the board.
            let application =
                poll_stack::boxed(|| self.process_refinement_outcome(run_id, &session)).await;
            if application == RefinementOutcomeApplication::Committed {
                if let Some(state) = self.active_refinements.get_mut(run_id) {
                    state.dispatch_failures = 0;
                }
                poll_stack::boxed(|| {
                    self.close_refinement_task(&session.task_id, "refinement phase complete")
                })
                .await;
                self.refinement_sessions.remove(run_id);
            } else {
                self.handle_stalled_outcome_application(run_id, &session, application)
                    .await;
            }
            return;
        }

        // Durable runs are dispatched exclusively by the leased intent ledger.
        // This run-keyed map is a recoverable projection used only to monitor a
        // correlated session/outcome; it must never create a second role task.
        if !state.run_id.is_empty() {
            return;
        }

        // Legacy non-durable compatibility path only.
        poll_stack::boxed(|| self.dispatch_next_refinement_phase(run_id)).await;
    }

    pub(super) async fn handle_stalled_outcome_application(
        &mut self,
        run_id: &str,
        session: &RefinementSession,
        application: RefinementOutcomeApplication,
    ) {
        let repo = djinn_db::ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        match repo
            .increment_refinement_outcome_attempt(run_id, session.generation, &session.task_id)
            .await
        {
            Ok(attempts) if attempts >= REFINEMENT_DISPATCH_RETRY_CAP => {
                tracing::warn!(run_id, task_id = %session.task_id, attempts,
                    "refinement outcome retry cap reached; terminalizing stalled handoff");
                let _ = self.terminate_refinement(run_id, StopReason::AgentFailure {
                    role: role_for_phase(session.phase), error_code: "outcome_application_failed".into(),
                    message: format!("refinement outcome could not be applied after {attempts} durable attempts"),
                }).await;
            }
            Ok(attempts) => tracing::warn!(run_id, task_id = %session.task_id, attempts,
                application = ?application, "stalled refinement handoff will be retried"),
            Err(error) => tracing::warn!(run_id, task_id = %session.task_id, %error,
                "could not persist refinement outcome attempt"),
        }
    }

    /// Whether an agent session row exists yet for a dispatched refinement
    /// task. A dispatched task with no session row means the agent has not
    /// started (e.g. the task-run pod is still Pending in the scheduler queue).
    /// On a DB read error, fail safe toward "started" so a transient DB blip
    /// neither burns the pending-start budget nor strands the loop treating a
    /// possibly-running session as never-started.
    async fn refinement_session_has_started(&self, task_id: &str) -> bool {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let session_repo = djinn_db::SessionRepository::new(self.db.clone(), event_bus);
        match session_repo.list_for_task(task_id).await {
            Ok(sessions) => !sessions.is_empty(),
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "Failed to read sessions for refinement task; assuming it started"
                );
                true
            }
        }
    }

    /// Did the just-finished refinement role session die on a **transient
    /// upstream provider fault** (5xx / `server_is_overloaded` / mid-flight
    /// stream death) rather than on its own work?
    ///
    /// Reads the health tracker's per-task provider-failure side-channel — the
    /// same typed class the slot supervisor-runner records for every
    /// `TaskRunOutcome::Failed`, and the same one the ordinary dispatch path
    /// consults for its A3/A6 backoff. Refinement tasks never travel the
    /// dispatch-reappearance path (they are force-closed once their round
    /// resolves), so this reader also owns cleanup: the entry is cleared
    /// unconditionally, since nothing else will ever collect it for this task id.
    ///
    /// Returns `false` when there is no signal at all, which is the fail-safe
    /// answer: an absent signal means "not a typed provider failure", and the
    /// round is processed exactly as it was before.
    fn refinement_round_died_on_transient_provider_fault(&self, task_id: &str) -> bool {
        let signal = self.health.peek_task_provider_failure(task_id);
        self.health.clear_task_provider_failure(task_id);
        let transient = signal.is_some_and(|signal| signal.transient);
        if transient {
            tracing::warn!(
                task_id = %task_id,
                "Refinement role session died on a transient provider fault — parking the round \
                 for retry instead of scoring it (an empty debate trail here is the provider's \
                 silence, not the role's verdict)"
            );
        }
        transient
    }

    /// Abandon a dispatched refinement role session that did not execute and
    /// either re-dispatch the same phase (bounded by
    /// [`REFINEMENT_DISPATCH_RETRY_CAP`]) or, once the cap is hit, terminate the
    /// loop. Shared by the two "role never ran" paths: the slot freed with no
    /// session row (runtime/devcontainer setup failure), and a dispatch that sat
    /// Pending past [`REFINEMENT_PENDING_START_TIMEOUT`] without the agent
    /// session ever starting.
    ///
    /// `tear_down_slot` kills the pool session and clears the in-flight
    /// reservation for the still-Pending case, where the pool still holds the
    /// slot; the slot-already-freed case leaves those to the paths that already
    /// released them (the in-flight ledger is cleared by the session-start
    /// reconciler, which never fired because no session started).
    async fn retry_or_terminate_unstarted_refinement(
        &mut self,
        run_id: &str,
        session: &RefinementSession,
        close_reason: &str,
        over_cap_error: String,
        tear_down_slot: bool,
    ) {
        let over_cap = if let Some(state) = self.active_refinements.get_mut(run_id) {
            state.dispatch_failures += 1;
            state.dispatch_failures >= REFINEMENT_DISPATCH_RETRY_CAP
        } else {
            true
        };

        if over_cap {
            tracing::warn!(
                run_id = %run_id,
                phase = ?session.phase,
                "Refinement role session repeatedly failed to start — terminating"
            );
            // Keep both run-keyed projections until the exact-run terminal CAS
            // succeeds. A miss or repository error must remain retryable.
            if self
                .terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: role_for_phase(session.phase),
                        error_code: "agent_start_failed".into(),
                        message: over_cap_error,
                    },
                )
                .await
            {
                if tear_down_slot {
                    if let Err(e) = self.pool.kill_session(&session.task_id).await {
                        tracing::warn!(
                            run_id = %run_id,
                            task_id = %session.task_id,
                            error = %e,
                            "Failed to tear down terminal Pending refinement task-run"
                        );
                    }
                    poll_stack::boxed(|| self.clear_inflight_dispatch(&session.task_id)).await;
                }
                poll_stack::boxed(|| self.close_refinement_task(&session.task_id, close_reason))
                    .await;
            }
            return;
        }

        if tear_down_slot {
            // The pool still holds the slot for the Pending task-run. Tear down
            // the (still-queued) Job and clear the in-flight reservation so the
            // retry does not leak a slot. Best-effort: a kill error must not
            // block the retry.
            if let Err(e) = self.pool.kill_session(&session.task_id).await {
                tracing::warn!(
                    run_id = %run_id,
                    task_id = %session.task_id,
                    error = %e,
                    "Failed to tear down Pending refinement task-run; proceeding with retry"
                );
            }
            poll_stack::boxed(|| self.clear_inflight_dispatch(&session.task_id)).await;
        }
        poll_stack::boxed(|| self.close_refinement_task(&session.task_id, close_reason)).await;
        self.refinement_sessions.remove(run_id);
        tracing::warn!(
            run_id = %run_id,
            phase = ?session.phase,
            "Refinement role session never started; will re-dispatch (not counted as dry)"
        );
    }

    /// Build the shared per-role dispatch context every tribunal task needs:
    /// the rendered current-head DoR/lint readiness report (plus any advocate
    /// spec-lint correction for this same round) and the current-revision human
    /// reviewer feedback.
    ///
    /// Both dispatch paths MUST go through this. The durable intent ledger
    /// previously injected the fixed literal `"Durable refinement intent
    /// dispatch."` as the readiness context and passed `None` for reviewer
    /// feedback, so every durable-run role was dispatched blind:
    ///
    /// * the Advocate never learned which DoR checks were failing and so could
    ///   not fix them, and
    /// * the Judge — whose prompt treats any injected `Current DoR status`
    ///   other than the exact clean message as a mandatory blocking reject —
    ///   could never approve, no matter how good the spec was.
    ///
    /// Since `drive_one_refinement` returns early for any run with a non-empty
    /// `run_id` ("durable runs are dispatched exclusively by the leased intent
    /// ledger"), that placeholder was what every live refinement actually got.
    async fn build_refinement_role_context(
        &self,
        proposal_id: &str,
        run_id: &str,
        phase: RefinementPhase,
    ) -> RefinementRoleContext {
        let readiness = poll_stack::boxed(|| self.evaluate_proposal_readiness(proposal_id)).await;
        let mut readiness_context = readiness
            .as_ref()
            .map(CoordinatorActor::format_readiness_context)
            .unwrap_or_else(|| {
                "Current proposal head could not be resolved for shared DoR/lint readiness."
                    .to_string()
            });

        if phase == RefinementPhase::AdvocateRevision
            && let Some(correction_context) =
                self.active_refinements.get(run_id).and_then(|state| {
                    super::refinement_outcome::format_advocate_lint_correction_context(
                        &state.pending_advocate_lint_violations,
                    )
                })
        {
            readiness_context.push_str("\n\nSpec-lint correction required for this same round:\n");
            readiness_context.push_str(&correction_context);
        }

        // Retrieve the latest current-revision human reviewer feedback recorded
        // by a demand round. The helper filters to the proposal's current
        // `latest_revision_seq` so stale feedback from a prior revision is
        // never injected into the next tribunal task.
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = djinn_db::ProposalRepository::new(self.db.clone(), event_bus);
        let reviewer_feedback = match proposal_repo
            .latest_current_revision_reviewer_feedback(proposal_id)
            .await
        {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to retrieve reviewer feedback for refinement dispatch; \
                     proceeding without feedback injection"
                );
                None
            }
        };

        let human_feedback_context = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => {
                let mut obligations = Vec::new();
                for entry in entries
                    .iter()
                    .filter(|entry| entry.kind == "human_feedback")
                {
                    match proposal_repo
                        .feedback_refinement_generation_for_debate(&entry.id)
                        .await
                    {
                        Ok(Some(generation)) => {
                            let sources = generation.sources.iter().map(|source| format!(
                                "{{id={}, severity={}, author_kind={}, author_user_id={:?}, author_model={:?}, body={:?}}}",
                                source.source_feedback_id, source.source_severity, source.source_author_kind,
                                source.source_author_user_id, source.source_author_model, source.source_body
                            )).collect::<Vec<_>>().join(", ");
                            obligations.push(format!(
                                "- id={} generation={} state={} pending_disposition={} against_revision={} sources=[{}]",
                                entry.id, generation.injection.generation, generation.injection.state,
                                generation.injection.state == "injected" && entry.resolved_at.is_none(),
                                entry.against_revision_seq, sources
                            ));
                        }
                        Ok(None) => {}
                        Err(error) => obligations.push(format!(
                            "- id={} generation unavailable ({error}); do not approve",
                            entry.id
                        )),
                    }
                }
                (!obligations.is_empty()).then(|| format!(
                    "Immutable human-feedback obligations (not adversary objections). Advocate must propose a disposition; Judge must accept or reject it.\n{}",
                    obligations.join("\n")
                ))
            }
            Err(error) => Some(format!(
                "Human-feedback obligation read failed ({error}); do not approve this round."
            )),
        };

        RefinementRoleContext {
            readiness_context,
            reviewer_feedback,
            human_feedback_context,
        }
    }

    /// Dispatch the next refinement phase for a proposal.
    ///
    /// At each round boundary the deterministic P1 DoR evaluator is consulted
    /// so that readiness findings are available to the dispatched agent and
    /// included in stop metadata.
    ///
    /// Admission gate (proposal j479 / epic xofs): attribution is resolved and
    /// validated, the per-user/model cap is checked, and an in-flight
    /// reservation is recorded — all **before** task creation, spawn-budget
    /// consumption, or `pool.dispatch()`. At-cap phases defer non-terminally
    /// so the state machine retries on the next tick. Failed paths clear the
    /// reservation so no slot leaks.
    pub(crate) async fn dispatch_next_refinement_phase(&mut self, run_id: &str) {
        let Some(state) = self.active_refinements.get(run_id).cloned() else {
            return;
        };
        let proposal_id = state.proposal_id.clone();

        let phase = state.phase;
        let round = state.current_round;
        let revision_seq = state.current_revision_seq;

        // Human-review pause gate.
        if phase == RefinementPhase::AwaitingHumanReview {
            tracing::debug!(
                proposal_id = %proposal_id,
                "Refinement parked: awaiting human accept/reject of the refined spec"
            );
            return;
        }

        // Evidence pause gate. The Judge demanded evidence via
        // `proposal_refinement_demand_evidence` and the demand was accepted.
        // No further tribunal phases are dispatched until the linked evidence
        // spike produces findings (resumed by the sibling epic oeqd).
        if phase == RefinementPhase::AwaitingEvidence {
            tracing::debug!(
                proposal_id = %proposal_id,
                "Refinement parked: awaiting evidence spike findings"
            );
            return;
        }

        // Administrative dispatch-pause gate.
        if poll_stack::boxed(|| self.refinement_dispatch_paused(&proposal_id)).await {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch deferred by administrative dispatch pause"
            );
            return;
        }

        // ── Evidence lifecycle state gate ──────────────────────────────────
        //
        // Consult the derived evidence lifecycle state from persisted data
        // (proposal fields, lifecycle events, spike task status) before
        // creating any Adversary, Advocate, or Judge tasks.  This is the
        // authoritative check that catches EvidenceFailed, Terminal, and
        // build_frozen states which the in-memory phase checks above cannot
        // detect — particularly after a coordinator restart where in-memory
        // state is lost.
        //
        // The in-memory `AwaitingEvidence` and admin-dispause checks above
        // serve as fast-path early returns that avoid DB reads when the
        // in-memory state is already parked.
        let lifecycle_proposal =
            poll_stack::boxed(|| self.load_proposal_for_lifecycle(&proposal_id)).await;
        if let Some(ref proposal) = lifecycle_proposal {
            let lifecycle_state =
                poll_stack::boxed(|| self.derive_proposal_evidence_lifecycle(proposal)).await;
            match lifecycle_state {
                EvidenceLifecycleState::Active | EvidenceLifecycleState::EvidenceReady => {
                    // Proceed to dispatch — either normal refinement or
                    // evidence findings are available and dispatch is not
                    // paused/frozen.
                }
                EvidenceLifecycleState::AwaitingEvidence => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        lifecycle_state = "awaiting_evidence",
                        "Refinement dispatch skipped: evidence spike still running (persisted state gate)"
                    );
                    return;
                }
                EvidenceLifecycleState::EvidenceFailed => {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        lifecycle_state = "evidence_failed",
                        "Refinement dispatch skipped: evidence demand failed; refinement is blocked (persisted state gate)"
                    );
                    return;
                }
                EvidenceLifecycleState::PausedOrFrozen => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        lifecycle_state = "paused_or_frozen",
                        build_frozen = proposal.build_frozen,
                        "Refinement dispatch skipped: proposal is paused or build-frozen (persisted state gate)"
                    );
                    return;
                }
                EvidenceLifecycleState::Terminal => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        lifecycle_state = "terminal",
                        proposal_status = %proposal.status,
                        "Refinement dispatch skipped: proposal in terminal status (persisted state gate)"
                    );
                    return;
                }
            }
        } else {
            // Failed to load the proposal — fail closed: do not dispatch
            // without the lifecycle check.
            tracing::warn!(
                proposal_id = %proposal_id,
                "Refinement dispatch skipped: could not load proposal for evidence lifecycle check (fail-closed)"
            );
            return;
        }

        // ── Step 1: Resolve and validate attribution before any side effect ──
        //
        // The attributed user determines the per-user cap scope and model
        // resolution. Missing, empty, dangling, or otherwise unresolvable
        // attribution fails closed with an operator-visible trace/status
        // reason, and NO refinement task row, spawn-budget consumption, or
        // pool dispatch occurs.
        let attributed_user_id = self
            .resolve_refinement_attributed_user(&proposal_id, state.attributed_user_id.clone())
            .await;

        let Some(ref user_id) = attributed_user_id else {
            tracing::warn!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch FAIL-CLOSED: no attributed user could be resolved \
                 (no explicit attributed_user_id and no proposal author). \
                 Refusing to dispatch without a real user identity."
            );
            poll_stack::boxed(|| {
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: role_for_phase(phase),
                        error_code: "agent_failure".into(),
                        message:
                            "attribution unresolvable: no user identity for refinement dispatch"
                                .into(),
                    },
                )
            })
            .await;
            return;
        };

        // Empty-attribution gate.
        if user_id.trim().is_empty() {
            tracing::warn!(
                proposal_id = %proposal_id,
                phase = ?phase,
                explicit = ?state.attributed_user_id,
                "Refinement dispatch: attributed user is empty — failing closed"
            );
            poll_stack::boxed(|| {
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: role_for_phase(phase),
                        error_code: "agent_failure".into(),
                        message: "attributed user is empty".into(),
                    },
                )
            })
            .await;
            return;
        }

        // Dangling-attribution gate: user id must exist in DB.
        match djinn_db::UserRepository::new(self.db.clone())
            .get_by_id(user_id)
            .await
        {
            Ok(Some(_user)) => {}
            Ok(None) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %user_id,
                    "Refinement dispatch: attributed user does not resolve to a row — failing closed"
                );
                poll_stack::boxed(|| {
                    self.terminate_refinement(
                        run_id,
                        StopReason::AgentFailure {
                            role: role_for_phase(phase),
                            error_code: "agent_failure".into(),
                            message: format!("attributed user {user_id} not found in users table"),
                        },
                    )
                })
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %user_id,
                    error = %e,
                    "Refinement dispatch deferred: failed to resolve attributed user row"
                );
                return;
            }
        }

        // Read diverse_refinement setting at the round boundary.
        let diverse_refinement =
            poll_stack::boxed(|| self.read_diverse_refinement_setting(&proposal_id)).await;

        let readiness = poll_stack::boxed(|| self.evaluate_proposal_readiness(&proposal_id)).await;

        if let Some(ref readiness) = readiness
            && !readiness.ready
        {
            tracing::info!(
                proposal_id = %proposal_id,
                round,
                failure_count = readiness.failures.len(),
                "DoR evaluator found readiness failures at round boundary"
            );
        }

        let Some((agent_type, model_id)) = self
            .resolve_refinement_dispatch_params(phase, diverse_refinement, Some(user_id))
            .await
        else {
            tracing::warn!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %user_id,
                "Refinement dispatch FAIL-CLOSED: durable owner has no eligible credential-backed model; no task or spawn will be created"
            );
            poll_stack::boxed(|| {
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: role_for_phase(phase),
                        error_code: "agent_failure".into(),
                        message: "no eligible credential-backed model for refinement owner".into(),
                    },
                )
            })
            .await;
            return;
        };

        // ── Step 1b: Provider-health gate ──────────────────────────────────
        //
        // Re-dispatching a round straight back into the provider that just
        // killed it is how a single upstream outage becomes a run of them: the
        // tribunal burns its `REFINEMENT_DISPATCH_RETRY_CAP` inside a couple of
        // ticks and terminates for what was a two-minute capacity blip. Gate on
        // the signal that already tracks exactly this — the per-`(scope, model)`
        // health breaker — and defer non-terminally while it is cooling.
        //
        // Deferral here is free and self-healing: no task row, no spawn budget,
        // no pool dispatch, and the cooldown expires on its own, so the next
        // tick re-evaluates. It is deliberately NOT a terminate: a cooling model
        // is a reason to wait, never a reason to kill the tribunal.
        let model_health = self.health.model_health(Some(user_id.as_str()), &model_id);
        if model_health.auto_disabled || model_health.hard_disabled {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %user_id,
                model_id = %model_id,
                consecutive_failures = model_health.consecutive_failures,
                cooldown_seconds_remaining = model_health.cooldown_seconds_remaining,
                hard_disabled = model_health.hard_disabled,
                "Refinement dispatch deferred: the model's health breaker is cooling \
                 (retryable — no task row, no spawn-budget, no pool dispatch)"
            );
            return;
        }

        // ── Step 2: Per-user/model cap admission (check + reserve atomically) ──
        //
        // Use the shared admission surface to check whether the attributed
        // user has room for one more session on the selected model. If at-cap,
        // defer non-terminally — no task row, no spawn-budget consumption, no
        // pool dispatch. The state machine retries on the next tick.
        let settings = djinn_db::UserSettingsRepository::new(self.db.clone())
            .get(user_id)
            .await
            .ok()
            .flatten();
        let model_cap = settings
            .as_ref()
            .and_then(|s| s.max_sessions.as_ref())
            .and_then(|caps| caps.get(&model_id))
            .copied()
            .unwrap_or(1);
        let lane_cap = settings
            .and_then(|s| s.lane_max_sessions)
            .map(|limits| limits.for_role(&agent_type));

        if !self
            .check_user_model_admission(user_id, &model_id, model_cap, &agent_type, lane_cap)
            .await
        {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %user_id,
                model_id = %model_id,
                model_cap,
                lane_cap,
                "Refinement dispatch deferred: user at model or plan-lane concurrency cap \
                 (retryable — no task row, no spawn-budget, no pool dispatch)"
            );
            return;
        }

        // Record a provisional in-flight reservation so the per-user cap
        // reflects this admission immediately — before the task row exists.
        // This closes the check-reserve race window where another candidate
        // could pass admission for the same (user, model).
        let provisional_key = format!("refinement:{proposal_id}");
        self.provisional_admissions.insert(
            provisional_key.clone(),
            InflightDispatch {
                creator: Some(user_id.clone()),
                model: model_id.clone(),
                lane: djinn_core::models::ModelLane::for_role(&agent_type),
            },
        );

        // Build a readiness-enriched task description so the agent sees
        // current DoR findings.
        let RefinementRoleContext {
            readiness_context,
            reviewer_feedback,
            human_feedback_context,
        } = self
            .build_refinement_role_context(&proposal_id, run_id, phase)
            .await;
        let reviewer_feedback = RefinementRoleContext::feedback_for_phase(
            reviewer_feedback,
            human_feedback_context,
            phase,
        );

        // ── Step 3: Create the refinement task (first DB side effect) ────────
        //
        // The task row is created only AFTER the cap reservation exists. On
        // failure, the provisional reservation is cleared immediately.
        let task_id = match self
            .create_refinement_task_with_context(
                &proposal_id,
                &agent_type,
                round,
                revision_seq,
                &readiness_context,
                reviewer_feedback.as_deref(),
                Some(user_id),
            )
            .await
        {
            Some(id) => id,
            None => {
                self.clear_provisional_admission(&provisional_key);
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    "Refinement dispatch deferred: failed to create refinement task"
                );
                return;
            }
        };

        // Re-key the provisional reservation to the real task id so that
        // existing reconciliation (pool liveness check) and session-start
        // cleanup can clear it, and so subsequent candidates for the same
        // (user, model) see the reservation under the durable key.
        poll_stack::boxed(|| {
            self.rekey_provisional_to_inflight(
                &provisional_key,
                &task_id,
                user_id,
                &model_id,
                &agent_type,
            )
        })
        .await;

        // ── Step 4: Consume spawn budget ────────────────────────────────────
        //
        // On spawn-cap overflow, clear the in-flight reservation (the real
        // task id) before terminating so the slot is immediately available.
        {
            #[allow(clippy::unwrap_used)]
            let state = self.active_refinements.get_mut(run_id).unwrap();
            if let Err(reason) = state.record_spawn() {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    ?reason,
                    "Refinement spawn cap reached"
                );
                poll_stack::boxed(|| self.clear_inflight_dispatch(&task_id)).await;
                poll_stack::boxed(|| {
                    self.close_refinement_task(
                        &task_id,
                        "refinement spawn cap reached — task will not be dispatched",
                    )
                })
                .await;
                poll_stack::boxed(|| self.persist_refinement_stop(&proposal_id, &reason)).await;
                self.refinement_sessions.remove(run_id);
                return;
            }
        }

        let project_path =
            poll_stack::boxed(|| self.resolve_refinement_project_path(&proposal_id)).await;

        // ── Step 5: Dispatch through the slot pool (last side effect) ───────
        //
        // On pool dispatch failure, clear the in-flight reservation so the
        // slot is immediately available. Coordinator infrastructure failures
        // are retryable and must not be classified as agent outcomes.
        match self.pool.dispatch(&task_id, &project_path, &model_id).await {
            Ok(()) => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    round,
                    model_id = %model_id,
                    "Dispatched refinement session"
                );
                self.refinement_sessions.insert(
                    run_id.to_string(),
                    RefinementSession {
                        run_id: run_id.to_string(),
                        generation: state.generation,
                        task_id,
                        phase,
                        dispatched_at: SystemClock::new().now_instant(),
                        session_started_at: None,
                        model_id,
                    },
                );
            }
            Err(e) => {
                poll_stack::boxed(|| self.clear_inflight_dispatch(&task_id)).await;
                poll_stack::boxed(|| {
                    self.close_refinement_task(&task_id, "refinement dispatch failed (pool error)")
                })
                .await;
                tracing::warn!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    error = %e,
                    "Refinement dispatch deferred after pool enqueue failure"
                );
            }
        }
    }

    /// Resume a refinement loop after a linked evidence spike completed with
    /// valid findings. This clears the durable evidence block through the
    /// substrate helper, advances the in-memory loop to the Advocate, then
    /// dispatches only if the normal administrative gates permit it.
    pub(super) async fn resume_refinement_after_evidence_received(
        &mut self,
        proposal_id: &str,
        spike_task_id: &str,
    ) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = djinn_db::ProposalRepository::new(self.db.clone(), event_bus);

        match proposal_repo.clear_needs_evidence_spike(proposal_id).await {
            Ok(proposal) => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    "Cleared linked evidence spike after valid findings receipt"
                );

                let Some(run_id) = self.active_refinements.iter().find_map(|(run_id, state)| {
                    (state.proposal_id == proposal_id).then(|| run_id.clone())
                }) else {
                    tracing::debug!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        "Evidence received for proposal without in-memory refinement loop; startup re-drive owns recovery"
                    );
                    return;
                };
                if let Some(state) = self.active_refinements.get_mut(&run_id) {
                    state.resume_after_evidence_received();
                }

                let lifecycle_state =
                    poll_stack::boxed(|| self.derive_proposal_evidence_lifecycle(&proposal)).await;
                if matches!(lifecycle_state, EvidenceLifecycleState::PausedOrFrozen) {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        build_frozen = proposal.build_frozen,
                        "Evidence findings received; refinement is waiting on manual pause/freeze gate before Advocate resume"
                    );
                    return;
                }

                poll_stack::boxed(|| self.dispatch_next_refinement_phase(&run_id)).await;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    error = %e,
                    "Failed to clear linked evidence spike after valid findings receipt"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "refinement_cap_tests.rs"]
pub(crate) mod refinement_cap_tests;

#[cfg(test)]
#[path = "refinement_pool_watchdog_tests.rs"]
mod refinement_pool_watchdog_tests;

#[cfg(test)]
#[path = "refinement_dor_status_tests.rs"]
mod refinement_dor_status_tests;

#[cfg(test)]
#[path = "refinement_evidence_resume_tests.rs"]
mod refinement_evidence_resume_tests;

#[cfg(test)]
#[path = "refinement_recovery_tests.rs"]
mod refinement_recovery_tests;

#[cfg(test)]
#[path = "refinement_wake_tests.rs"]
mod refinement_wake_tests;

#[cfg(test)]
#[path = "refinement_durable_dispatch_tests.rs"]
mod refinement_durable_dispatch_tests;

#[cfg(test)]
#[path = "refinement_durable_handoff_tests.rs"]
mod refinement_durable_handoff_tests;

#[cfg(test)]
#[path = "refinement_durable_gate_tests.rs"]
mod refinement_durable_gate_tests;
