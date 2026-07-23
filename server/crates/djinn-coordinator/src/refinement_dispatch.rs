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
use super::refinement::{RefinementPhase, StopReason};

use super::actor::CoordinatorActor;
use super::types::InflightDispatch;
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

impl CoordinatorActor {
    /// Drive all active refinement loops. Called from `run_tick()`.
    pub(super) async fn drive_active_refinements(&mut self) {
        // The durable intent ledger is the dispatch authority. This pass happens
        // even when the recoverable projection has been dropped.
        self.drive_durable_refinement_intents().await;

        let run_ids: Vec<String> = self.active_refinements.keys().cloned().collect();
        if run_ids.is_empty() {
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
            self.drive_one_refinement(&run_id, running_tasks.as_ref())
                .await;
        }

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
                            let Some(owner) = proposal.refinement_owner_user_id.as_deref() else {
                                tracing::debug!(intent_id = %intent.intent_id, "awaiting durable refinement attribution");
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
                            let project_path =
                                self.resolve_refinement_project_path(&run.proposal_id).await;
                            if let Err(error) =
                                self.pool.dispatch(&task.id, &project_path, &model_id).await
                            {
                                tracing::warn!(task_id = %task.id, %error, "materialized refinement enqueue failed; retrying on next durable poll");
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
                let owner = format!("coordinator:{}", self.coordinator_incarnation_id);
                let Some(lease) = (match proposal_repo
                    .claim_refinement_intent(ClaimRefinementIntentRequest {
                        run_id: run.run_id.clone(),
                        intent_id: intent.intent_id.clone(),
                        generation: run.generation,
                        owner: owner.clone(),
                        lease_millis: 60_000,
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

                // Resolve owner/model/admission before task creation. A denied
                // attempt keeps the lease/intention retryable without creating
                // an un-dispatchable board task.
                let proposal = match proposal_repo.get(&run.proposal_id).await {
                    Ok(Some(proposal)) => proposal,
                    Ok(None) | Err(_) => continue,
                };
                let Some(attributed_user) = proposal.refinement_owner_user_id.clone() else {
                    tracing::debug!(intent_id = %lease.intent_id, "awaiting durable refinement attribution");
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
                        let Some(task_id) = self
                            .create_refinement_task_with_context_and_correlation(
                                &run.proposal_id,
                                role,
                                lease.round,
                                proposal.latest_revision_seq,
                                "Durable refinement intent dispatch.",
                                None,
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
                let project_path = self.resolve_refinement_project_path(&run.proposal_id).await;
                if let Err(error) = self.pool.dispatch(&task_id, &project_path, &model_id).await {
                    tracing::warn!(task_id = %task_id, %error, "durable refinement enqueue failed; task remains retryable");
                }
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

        let phase = match phase {
            djinn_core::refinement_liveness::RefinementPhase::AdversaryAttack => {
                RefinementPhase::AdversaryAttack
            }
            djinn_core::refinement_liveness::RefinementPhase::AdvocateRevision => {
                RefinementPhase::AdvocateRevision
            }
            djinn_core::refinement_liveness::RefinementPhase::JudgeAdjudication => {
                RefinementPhase::JudgeAdjudication
            }
        };
        let diverse_refinement = self.read_diverse_refinement_setting(proposal_id).await;
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
        self.check_user_model_admission(&owner, &model_id, model_cap, &agent_type, lane_cap)
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
                        if self.refinement_session_has_started(&session.task_id).await {
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
                            self.close_refinement_task(
                                &session.task_id,
                                "refinement session timed out",
                            )
                            .await;
                            self.terminate_refinement(
                                run_id,
                                StopReason::AgentFailure {
                                    role: format!("{:?}", session.phase),
                                    error: "session timeout".into(),
                                },
                            )
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
                            self.retry_or_terminate_unstarted_refinement(
                                run_id,
                                &session,
                                "refinement role session never started (task-run pod stuck Pending)",
                                over_cap_error,
                                true,
                            )
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
            let session_ran = self.refinement_session_has_started(&session.task_id).await;

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
                self.retry_or_terminate_unstarted_refinement(
                    run_id,
                    &session,
                    "refinement role session never started (dispatch/setup failure)",
                    over_cap_error,
                    false,
                )
                .await;
                return;
            }

            // Session actually ran — clear the dispatch-failure counter and
            // process the outcome, then close the task so finished phase/round
            // tasks don't linger `open` on the board.
            if let Some(state) = self.active_refinements.get_mut(run_id) {
                state.dispatch_failures = 0;
            }
            self.process_refinement_outcome(run_id, &session).await;
            self.close_refinement_task(&session.task_id, "refinement phase complete")
                .await;
            self.refinement_sessions.remove(run_id);
            return;
        }

        // Durable runs are dispatched exclusively by the leased intent ledger.
        // This run-keyed map is a recoverable projection used only to monitor a
        // correlated session/outcome; it must never create a second role task.
        if !state.run_id.is_empty() {
            return;
        }

        // Legacy non-durable compatibility path only.
        self.dispatch_next_refinement_phase(run_id).await;
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
            self.clear_inflight_dispatch(&session.task_id).await;
        }
        self.close_refinement_task(&session.task_id, close_reason)
            .await;
        self.refinement_sessions.remove(run_id);
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
            self.terminate_refinement(
                run_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", session.phase),
                    error: over_cap_error,
                },
            )
            .await;
        } else {
            tracing::warn!(
                run_id = %run_id,
                phase = ?session.phase,
                "Refinement role session never started; will re-dispatch (not counted as dry)"
            );
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
        if self.refinement_dispatch_paused(&proposal_id).await {
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
        let lifecycle_proposal = self.load_proposal_for_lifecycle(&proposal_id).await;
        if let Some(ref proposal) = lifecycle_proposal {
            let lifecycle_state = self.derive_proposal_evidence_lifecycle(proposal).await;
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
            self.terminate_refinement(
                run_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "attribution unresolvable: no user identity for refinement dispatch"
                        .into(),
                },
            )
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
            self.terminate_refinement(
                run_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "attributed user is empty".into(),
                },
            )
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
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("attributed user {user_id} not found in users table"),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %user_id,
                    error = %e,
                    "Refinement dispatch: failed to resolve attributed user row — failing closed"
                );
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("failed to resolve attributed user: {e}"),
                    },
                )
                .await;
                return;
            }
        }

        // Read diverse_refinement setting at the round boundary.
        let diverse_refinement = self.read_diverse_refinement_setting(&proposal_id).await;

        let readiness = self.evaluate_proposal_readiness(&proposal_id).await;

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
            self.terminate_refinement(
                run_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "no eligible credential-backed model for refinement owner".into(),
                },
            )
            .await;
            return;
        };

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
        let lint_correction_context = if phase == RefinementPhase::AdvocateRevision {
            self.active_refinements.get(run_id).and_then(|state| {
                super::refinement_outcome::format_advocate_lint_correction_context(
                    &state.pending_advocate_lint_violations,
                )
            })
        } else {
            None
        };

        let mut readiness_context = readiness
            .as_ref()
            .map(CoordinatorActor::format_readiness_context)
            .unwrap_or_else(|| {
                "Current proposal head could not be resolved for shared DoR/lint readiness."
                    .to_string()
            });
        if let Some(correction_context) = lint_correction_context {
            readiness_context.push_str("\n\nSpec-lint correction required for this same round:\n");
            readiness_context.push_str(&correction_context);
        }

        // Retrieve the latest current-revision human reviewer feedback recorded
        // by a demand round. The helper filters to the proposal's current
        // `latest_revision_seq` so stale feedback from a prior revision is
        // never injected into the next tribunal task.
        let reviewer_feedback = {
            let event_bus = crate::events::event_bus_for(&self.events_tx);
            let proposal_repo = djinn_db::ProposalRepository::new(self.db.clone(), event_bus);
            match proposal_repo
                .latest_current_revision_reviewer_feedback(&proposal_id)
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
            }
        };

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
                    "Failed to create refinement task"
                );
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: "task creation failed".into(),
                    },
                )
                .await;
                return;
            }
        };

        // Re-key the provisional reservation to the real task id so that
        // existing reconciliation (pool liveness check) and session-start
        // cleanup can clear it, and so subsequent candidates for the same
        // (user, model) see the reservation under the durable key.
        self.rekey_provisional_to_inflight(
            &provisional_key,
            &task_id,
            user_id,
            &model_id,
            &agent_type,
        )
        .await;

        // The task identity is now fixed; journal CreateStarted before the pool side effect.
        // A cap denial is neutral: leave this open refinement task queued.
        let build_admission = match self
            .begin_task_run_build_admission(
                &agent_type,
                &task_id,
                i64::from(round),
                format!("task-run-{}-{}", task_id, round),
            )
            .await
        {
            Ok(permit) => permit,
            Err(()) => {
                self.clear_inflight_dispatch(&task_id).await;
                return;
            }
        };

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
                // No pool side effect will occur after this deterministic denial.
                self.finish_task_run_build_admission(build_admission, false)
                    .await;
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(
                    &task_id,
                    "refinement spawn cap reached — task will not be dispatched",
                )
                .await;
                self.persist_refinement_stop(&proposal_id, &reason).await;
                self.refinement_sessions.remove(run_id);
                return;
            }
        }

        let project_path = self.resolve_refinement_project_path(&proposal_id).await;

        // ── Step 5: Dispatch through the slot pool (last side effect) ───────
        //
        // On pool dispatch failure, clear the in-flight reservation so the
        // slot is immediately available and terminate the refinement.
        match self.pool.dispatch(&task_id, &project_path, &model_id).await {
            Ok(()) => {
                self.finish_task_run_build_admission(build_admission, true)
                    .await;
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
                self.finish_task_run_build_admission(build_admission, false)
                    .await;
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(&task_id, "refinement dispatch failed (pool error)")
                    .await;
                tracing::warn!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    error = %e,
                    "Failed to dispatch refinement session"
                );
                self.terminate_refinement(
                    run_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("dispatch failed: {e}"),
                    },
                )
                .await;
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

                let lifecycle_state = self.derive_proposal_evidence_lifecycle(&proposal).await;
                if matches!(lifecycle_state, EvidenceLifecycleState::PausedOrFrozen) {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        build_frozen = proposal.build_frozen,
                        "Evidence findings received; refinement is waiting on manual pause/freeze gate before Advocate resume"
                    );
                    return;
                }

                self.dispatch_next_refinement_phase(&run_id).await;
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
