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

use std::time::{Duration, Instant as StdInstant};

use super::refinement::{RefinementPhase, StopReason};

use super::actor::CoordinatorActor;

/// How long to wait for a refinement session to start producing output
/// before treating it as stalled (conservative — sessions can take 5+ min).
const REFINEMENT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How many consecutive times a refinement role session may fail to start
/// before the loop terminates instead of re-dispatching.
const REFINEMENT_DISPATCH_RETRY_CAP: i32 = 3;

/// The in-flight session tracking for one active refinement loop.
#[derive(Debug, Clone)]
pub(super) struct RefinementSession {
    /// The task id of the refinement task currently dispatched.
    pub task_id: String,
    /// Which phase this session is executing.
    pub phase: RefinementPhase,
    /// When the session was dispatched.
    pub dispatched_at: StdInstant,
    /// The model used for this session.
    #[allow(dead_code)]
    pub model_id: String,
}

// ─── Main dispatch loop ─────────────────────────────────────────────────────

impl CoordinatorActor {
    /// Drive all active refinement loops. Called from `run_tick()`.
    pub(super) async fn drive_active_refinements(&mut self) {
        let proposal_ids: Vec<String> = self.active_refinements.keys().cloned().collect();

        for proposal_id in proposal_ids {
            self.drive_one_refinement(&proposal_id).await;
        }

        // Clean up completed refinements.
        self.active_refinements
            .retain(|_, state| !state.is_complete());
    }

    /// Drive a single refinement loop.
    async fn drive_one_refinement(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };
        if state.is_complete() {
            return;
        }

        // Check if there's an in-flight session for this refinement.
        if let Some(session) = self.refinement_sessions.get(proposal_id).cloned() {
            let still_running = self
                .pool
                .has_session(&session.task_id)
                .await
                .unwrap_or(false);

            if still_running {
                if session.dispatched_at.elapsed() > REFINEMENT_SESSION_TIMEOUT {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        task_id = %session.task_id,
                        phase = ?session.phase,
                        "Refinement session timed out"
                    );
                    self.close_refinement_task(&session.task_id, "refinement session timed out")
                        .await;
                    self.terminate_refinement(
                        proposal_id,
                        StopReason::AgentFailure {
                            role: format!("{:?}", session.phase),
                            error: "session timeout".into(),
                        },
                    )
                    .await;
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
            let session_ran = {
                let event_bus = crate::events::event_bus_for(&self.events_tx);
                let session_repo = djinn_db::SessionRepository::new(self.db.clone(), event_bus);
                match session_repo.list_for_task(&session.task_id).await {
                    Ok(sessions) => !sessions.is_empty(),
                    // On a DB read error, fail safe toward "it ran" so we don't
                    // spin forever re-dispatching.
                    Err(e) => {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            task_id = %session.task_id,
                            error = %e,
                            "Failed to read sessions for refinement task; assuming it ran"
                        );
                        true
                    }
                }
            };

            if !session_ran {
                // Dispatch/setup failure: the role never executed. Re-dispatch
                // the same phase on the next tick, bounded by a retry cap so a
                // persistently broken runtime escalates instead of looping.
                self.close_refinement_task(
                    &session.task_id,
                    "refinement role session never started (dispatch/setup failure)",
                )
                .await;
                self.refinement_sessions.remove(proposal_id);
                let over_cap = if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                    state.dispatch_failures += 1;
                    state.dispatch_failures >= REFINEMENT_DISPATCH_RETRY_CAP
                } else {
                    true
                };
                if over_cap {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        phase = ?session.phase,
                        "Refinement role session repeatedly failed to start — terminating"
                    );
                    self.terminate_refinement(
                        proposal_id,
                        StopReason::AgentFailure {
                            role: format!("{:?}", session.phase),
                            error: format!(
                                "role session failed to start {REFINEMENT_DISPATCH_RETRY_CAP} times \
                                 (runtime/devcontainer setup failure)"
                            ),
                        },
                    )
                    .await;
                } else {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        phase = ?session.phase,
                        "Refinement role session never started; will re-dispatch (not counted as dry)"
                    );
                }
                return;
            }

            // Session actually ran — clear the dispatch-failure counter and
            // process the outcome, then close the task so finished phase/round
            // tasks don't linger `open` on the board.
            if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                state.dispatch_failures = 0;
            }
            self.process_refinement_outcome(proposal_id, &session).await;
            self.close_refinement_task(&session.task_id, "refinement phase complete")
                .await;
            self.refinement_sessions.remove(proposal_id);
            return;
        }

        // No in-flight session — dispatch the next phase.
        self.dispatch_next_refinement_phase(proposal_id).await;
    }

    /// Dispatch the next refinement phase for a proposal.
    ///
    /// Cap admission is the SAME shared surface that normal task dispatch uses
    /// (`check_user_model_admission` / `record_inflight_dispatch` /
    /// `clear_inflight_dispatch`). Before ANY side effect we:
    ///   1. Resolve a concrete valid attributed user (fail closed if missing).
    ///   2. Resolve the phase model and per-model cap.
    ///   3. Check `(user, model)` admission; defer retryably if at cap.
    ///   4. Reserve the shared `inflight_dispatches` slot.
    ///
    /// Then side effects run. On failure after reservation, the in-flight
    /// ledger slot is cleared so no slot leaks.
    async fn dispatch_next_refinement_phase(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };

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

        // Administrative dispatch-pause gate.
        if self.refinement_dispatch_paused(proposal_id).await {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch deferred by administrative dispatch pause"
            );
            return;
        }

        // Resolve attributed user before any side effect. Fail closed if
        // there is no concrete valid user to key the per-user cap on.
        let resolved_attributed_user_id = self
            .resolve_refinement_attributed_user(proposal_id, state.attributed_user_id.clone())
            .await;

        let diverse_refinement = self.read_diverse_refinement_setting(proposal_id).await;

        let readiness = self.evaluate_proposal_readiness(proposal_id).await;

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

        let (agent_type, model_id) = self
            .resolve_refinement_dispatch_params(
                phase,
                diverse_refinement,
                resolved_attributed_user_id.as_deref(),
            )
            .await;

        // Fail-closed attribution gate: no concrete valid user → terminate.
        let attributed_user_id = match resolved_attributed_user_id {
            Some(uid) if !uid.trim().is_empty() => uid,
            _ => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    explicit = ?state.attributed_user_id,
                    "Refinement dispatch: attributed user is missing or unresolvable — failing closed"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: "attributed user missing or unresolvable".into(),
                    },
                )
                .await;
                return;
            }
        };

        // Dangling-attribution gate: user id must exist in DB.
        match djinn_db::UserRepository::new(self.db.clone())
            .get_by_id(&attributed_user_id)
            .await
        {
            Ok(Some(_user)) => {}
            Ok(None) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %attributed_user_id,
                    "Refinement dispatch: attributed user does not resolve to a row — failing closed"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!(
                            "attributed user {attributed_user_id} not found in users table"
                        ),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %attributed_user_id,
                    error = %e,
                    "Refinement dispatch: failed to resolve attributed user row — failing closed"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("failed to resolve attributed user: {e}"),
                    },
                )
                .await;
                return;
            }
        }

        // At-cap deferral gate: defer retryably when at the per-user cap.
        let caps = self.resolve_model_caps_for_user(&attributed_user_id).await;
        let cap = caps.get(&model_id).copied().unwrap_or(1);
        if !self
            .check_user_model_admission(&attributed_user_id, &model_id, cap)
            .await
        {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %attributed_user_id,
                model_id = %model_id,
                cap,
                "Refinement dispatch deferred: per-user cap reached for model"
            );
            return;
        }

        // Build readiness-enriched task description.
        let readiness_context = readiness
            .as_ref()
            .and_then(|r| r.to_error_string())
            .unwrap_or_else(|| "Proposal currently meets all DoR checks.".to_string());

        // Create a refinement task in the DB.
        let task_id = match self
            .create_refinement_task_with_context(
                proposal_id,
                &agent_type,
                round,
                revision_seq,
                &readiness_context,
                Some(&attributed_user_id),
            )
            .await
        {
            Some(id) => id,
            None => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    "Failed to create refinement task"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: "task creation failed".into(),
                    },
                )
                .await;
                return;
            }
        };

        // Record the in-flight ledger reservation under the REAL task id.
        // Any candidate admitted later in the same tick that consults
        // `effective_running_by_user_model` will see this reservation via
        // the ledger overlay and defer accordingly.
        self.record_inflight_dispatch(&task_id, None, Some(&attributed_user_id), &model_id)
            .await;

        // Record spawn in the state machine. If the spawn cap is hit, clear
        // the reservation so the slot doesn't leak — and close the orphan task.
        {
            let state = self.active_refinements.get_mut(proposal_id).unwrap();
            if let Err(reason) = state.record_spawn() {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    ?reason,
                    "Refinement spawn cap reached"
                );
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(&task_id, "refinement spawn cap reached")
                    .await;
                self.persist_refinement_stop(proposal_id, &reason).await;
                self.refinement_sessions.remove(proposal_id);
                return;
            }
        }

        let project_path = self.resolve_refinement_project_path(proposal_id).await;

        // Dispatch through the slot pool.
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
                    proposal_id.to_string(),
                    RefinementSession {
                        task_id,
                        phase,
                        dispatched_at: StdInstant::now(),
                        model_id,
                    },
                );
            }
            Err(e) => {
                // Pool dispatch failed after reservation. Clear the
                // in-flight ledger slot so it doesn't leak; close the orphan
                // task so it doesn't linger `open` on the board.
                tracing::warn!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    error = %e,
                    "Failed to dispatch refinement session"
                );
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(&task_id, "refinement dispatch failed")
                    .await;
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("dispatch failed: {e}"),
                    },
                )
                .await;
            }
        }
    }
}
