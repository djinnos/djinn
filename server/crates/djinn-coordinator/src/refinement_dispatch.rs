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

use djinn_control_plane::tools::epic_ops::AcceptanceCriterionItem;
use djinn_control_plane::tools::proposal_readiness::evaluate_proposal_readiness;
use djinn_core::models::TransitionAction;
use djinn_db::{ProposalRepository, TaskRepository, UserSettingsRepository};

use super::refinement::{
    AdversaryPassOutcome, AdversaryPassResult, JudgeVerdictResult, ObjectionRecord,
    RefinementLoopState, RefinementPhase, StopReason, build_revision_event_metadata,
    select_refinement_model,
};

use super::actor::CoordinatorActor;

/// How long to wait for a refinement session to start producing output
/// before treating it as stalled (conservative — sessions can take 5+ min).
const REFINEMENT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How many consecutive times a refinement role session may fail to start
/// (runtime/devcontainer setup failure, zero session rows) before the loop
/// gives up and terminates instead of re-dispatching. Bounds the retry on a
/// transient runtime outage (e.g. a cold post-deploy devcontainer warm) while
/// still escalating a persistently broken environment.
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
    async fn dispatch_next_refinement_phase(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };

        let phase = state.phase;
        let round = state.current_round;
        let revision_seq = state.current_revision_seq;

        // Human-review pause gate: when the tribunal has converged (or hit a
        // cap) and is parked for the human's single accept/reject, dispatch no
        // further phases. The loop resumes only when the human resolves the
        // review (`resolve_human_review`): accept → Complete; reject+feedback →
        // a fresh round; reject → Complete (spec reverted to the snapshot).
        if phase == RefinementPhase::AwaitingHumanReview {
            tracing::debug!(
                proposal_id = %proposal_id,
                "Refinement parked: awaiting human accept/reject of the refined spec"
            );
            return;
        }

        // Administrative dispatch-pause gate: a global or project-scoped pause
        // halts the refinement loop just like ordinary task dispatch, so an
        // operator can stop an in-flight run. The state machine is left intact;
        // dispatch resumes on the next tick once the pause is lifted.
        if self.refinement_dispatch_paused(proposal_id).await {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch deferred by administrative dispatch pause"
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
            .resolve_refinement_attributed_user(proposal_id, state.attributed_user_id.clone())
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
                proposal_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "attribution unresolvable: no user identity for refinement dispatch"
                        .into(),
                },
            )
            .await;
            return;
        };

        // Read diverse_refinement setting at the round boundary.
        let diverse_refinement = self.read_diverse_refinement_setting(proposal_id).await;

        // Consult the deterministic P1 DoR evaluator.
        let readiness = self.evaluate_proposal_readiness(proposal_id).await;

        // Log readiness findings for observability.
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

        // Determine the agent_type and model for this phase.
        let (agent_type, model_id) = self
            .resolve_refinement_dispatch_params(
                phase,
                diverse_refinement,
                attributed_user_id.as_deref(),
            )
            .await;

        // ── Step 2: Per-user/model cap admission (check + reserve atomically) ──
        //
        // Use the shared admission surface to check whether the attributed
        // user has room for one more session on the selected model. If at-cap,
        // defer non-terminally — no task row, no spawn-budget consumption, no
        // pool dispatch. The state machine retries on the next tick.
        let user_caps = self.resolve_model_caps_for_user(user_id).await;
        let cap = user_caps.get(&model_id).copied().unwrap_or(1);

        if !self
            .check_user_model_admission(user_id, &model_id, cap)
            .await
        {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %user_id,
                model_id = %model_id,
                cap,
                "Refinement dispatch deferred: user at per-model concurrency cap \
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
            (Some(user_id.clone()), model_id.clone()),
        );

        // Build a readiness-enriched task description so the agent sees
        // current DoR findings.
        let readiness_context = readiness
            .as_ref()
            .and_then(|r| r.to_error_string())
            .unwrap_or_else(|| "Proposal currently meets all DoR checks.".to_string());

        // ── Step 3: Create the refinement task (first DB side effect) ────────
        //
        // The task row is created only AFTER the cap reservation exists. On
        // failure, the provisional reservation is cleared immediately.
        let task_id = match self
            .create_refinement_task_with_context(
                proposal_id,
                &agent_type,
                round,
                revision_seq,
                &readiness_context,
                attributed_user_id.as_deref(),
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

        // Re-key the provisional reservation to the real task id so that
        // existing reconciliation (pool liveness check) and session-start
        // cleanup can clear it, and so subsequent candidates for the same
        // (user, model) see the reservation under the durable key.
        self.rekey_provisional_to_inflight(&provisional_key, &task_id, user_id, &model_id)
            .await;

        // ── Step 4: Consume spawn budget ────────────────────────────────────
        //
        // On spawn-cap overflow, clear the in-flight reservation (the real
        // task id) before terminating so the slot is immediately available.
        {
            let state = self.active_refinements.get_mut(proposal_id).unwrap();
            if let Err(reason) = state.record_spawn() {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    ?reason,
                    "Refinement spawn cap reached"
                );
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(
                    &task_id,
                    "refinement spawn cap reached — task will not be dispatched",
                )
                .await;
                self.persist_refinement_stop(proposal_id, &reason).await;
                self.refinement_sessions.remove(proposal_id);
                return;
            }
        }

        let project_path = self.resolve_refinement_project_path(proposal_id).await;

        // ── Step 5: Dispatch through the slot pool (last side effect) ───────
        //
        // On pool dispatch failure, clear the in-flight reservation so the
        // slot is immediately available and terminate the refinement.
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

    /// Process the outcome of a completed refinement session.
    async fn process_refinement_outcome(&mut self, proposal_id: &str, session: &RefinementSession) {
        let state = match self.active_refinements.get(proposal_id).cloned() {
            Some(s) => s,
            None => return,
        };

        match session.phase {
            RefinementPhase::AdvocateRevision => {
                self.process_advocate_outcome(proposal_id, &state).await;
            }
            RefinementPhase::AdversaryAttack => {
                self.process_adversary_outcome(proposal_id, &state).await;
            }
            RefinementPhase::JudgeAdjudication => {
                self.process_judge_outcome(proposal_id, &state).await;
            }
            RefinementPhase::AwaitingHumanReview | RefinementPhase::Complete => {}
        }
    }

    /// Process an advocate session outcome by reading the proposal's
    /// latest revision and advancing the state machine.
    ///
    /// When the advocate produced a material revision (latest_revision_seq
    /// advanced), the revision's `event_metadata` is patched with
    /// refinement-loop attribution (role, round, authority mode, model)
    /// via `ProposalRepository::set_latest_revision_event_metadata`.
    ///
    /// The advocate's revision applies directly to the working spec — there is
    /// no per-revision checkpoint or body revert. The human reviews the
    /// converged result once, at the end of the loop, and only then is the
    /// spec reverted (to the pre-refinement snapshot) if they reject.
    async fn process_advocate_outcome(&mut self, proposal_id: &str, state: &RefinementLoopState) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(proposal_id = %proposal_id, "Proposal not found after advocate");
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "advocate".into(),
                        error: "proposal not found after session".into(),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(proposal_id = %proposal_id, error = %e, "DB error reading proposal");
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "advocate".into(),
                        error: format!("DB error: {e}"),
                    },
                )
                .await;
                return;
            }
        };

        let new_revision_seq = proposal.latest_revision_seq;
        let advanced = new_revision_seq > state.current_revision_seq;

        // The advocate's revision applies directly to the working spec — no
        // human-in-the-loop checkpoint, no body revert (the human reviews the
        // converged result once, at the end). We only patch the new revision's
        // event_metadata with refinement attribution, since the agent's
        // `proposal_update` call doesn't carry loop context.
        if advanced {
            let model_id = self
                .refinement_sessions
                .get(proposal_id)
                .map(|s| s.model_id.clone());
            let event_meta =
                build_revision_event_metadata(state.current_round, model_id.as_deref());
            let event_bus2 = crate::events::event_bus_for(&self.events_tx);
            let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
            // Tag EVERY revision this advocate session produced this round (a
            // single session may do a body edit + an AC edit), so the history
            // UI can collapse the whole refinement run into one entry.
            if let Err(e) = proposal_repo2
                .set_spec_revisions_event_metadata_range(
                    proposal_id,
                    state.current_revision_seq,
                    new_revision_seq,
                    &event_meta,
                )
                .await
            {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    seq = new_revision_seq,
                    error = %e,
                    "Failed to patch advocate revision event_metadata"
                );
            }
        }

        // Either way, the round now goes to the Judge to rule.
        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            if advanced {
                state.record_advocate_revision(new_revision_seq);
                tracing::info!(
                    proposal_id = %proposal_id,
                    new_seq = new_revision_seq,
                    round = state.current_round,
                    "Advocate produced revision; handing to judge"
                );
            } else {
                state.phase = RefinementPhase::JudgeAdjudication;
                tracing::info!(
                    proposal_id = %proposal_id,
                    revision_seq = state.current_revision_seq,
                    round = state.current_round,
                    "Advocate session produced no revision; handing to judge on current spec"
                );
            }
        }
    }

    /// Process an adversary session outcome by reading debate-trail
    /// entries and feeding them to the state machine.
    async fn process_adversary_outcome(&mut self, proposal_id: &str, state: &RefinementLoopState) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let entries = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "DB error reading debate trail"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "adversary".into(),
                        error: format!("DB error: {e}"),
                    },
                )
                .await;
                return;
            }
        };

        let round = state.current_round;
        let round_objections: Vec<ObjectionRecord> = entries
            .iter()
            .filter(|e| e.agent_role == "adversary" && e.kind == "objection" && e.round == round)
            .map(|e| ObjectionRecord {
                body: e.body.clone(),
                blocking: e.blocking,
                author_model: e.author_model.clone(),
                entry_id: Some(e.id.clone()),
            })
            .collect();

        let explicit_dry = round_objections.is_empty();

        let adversary_result = AdversaryPassResult {
            objections: round_objections,
            explicit_dry,
        };

        let outcome = {
            let state = self.active_refinements.get_mut(proposal_id).unwrap();
            state.process_adversary_pass(&adversary_result)
        };

        match outcome {
            AdversaryPassOutcome::Continue => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    round,
                    "Adversary found blocking objections — next round"
                );
            }
            AdversaryPassOutcome::Dry => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    round,
                    "Adversary dry — judge will adjudicate"
                );
            }
            AdversaryPassOutcome::Escalated(reason) => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    round,
                    ?reason,
                    "Refinement escalated — parking for human review"
                );
                // Escalation parks the loop for the human (it does not silently
                // stop). Surface it with the escalation reason as the summary.
                if let Some(state) = self.active_refinements.get(proposal_id).cloned() {
                    let summary = format!("Escalated to human review: {reason:?}");
                    self.persist_awaiting_review(proposal_id, &summary, &state)
                        .await;
                }
            }
        }
    }

    /// Persist that the tribunal has parked for the human's single accept/reject
    /// review (judge converged, or escalated). The status tool reads this event
    /// to render the review panel; it carries the judge's summary and the
    /// snapshot/refined revision seqs that bound the diff.
    async fn persist_awaiting_review(
        &self,
        proposal_id: &str,
        judge_summary: &str,
        state: &RefinementLoopState,
    ) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_awaiting_review",
            "judge_summary": judge_summary,
            "snapshot_revision_seq": state.snapshot_revision_seq,
            "refined_revision_seq": state.current_revision_seq,
            "stop_reason": state.stop_reason.as_ref().map(|r| r.tag()),
        });
        if let Err(e) = proposal_repo
            .record_refinement_lifecycle(proposal_id, "refinement_awaiting_review", Some(&meta))
            .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to persist refinement_awaiting_review lifecycle metadata"
            );
        }
    }

    /// Process a judge session outcome by reading the verdict from
    /// the debate trail and terminating the refinement loop.
    async fn process_judge_outcome(&mut self, proposal_id: &str, state: &RefinementLoopState) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let entries = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "DB error reading debate trail"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "judge".into(),
                        error: format!("DB error: {e}"),
                    },
                )
                .await;
                return;
            }
        };

        let round = state.current_round;
        let verdict_entry = entries
            .iter()
            .find(|e| e.agent_role == "judge" && e.kind == "verdict" && e.round == round);

        let verdict = if let Some(entry) = verdict_entry {
            JudgeVerdictResult {
                body: entry.body.clone(),
                blocking: entry.blocking,
            }
        } else {
            // The judge completed but filed no `verdict` debate-trail entry.
            // A missing verdict is a non-decision, not an approval — do NOT
            // treat it as "ready". Mark it blocking so the loop runs another
            // round (bounded by the round cap, which escalates to human
            // review) rather than silently parking an un-adjudicated spec for
            // human acceptance.
            tracing::warn!(
                proposal_id = %proposal_id,
                round,
                "Judge session completed without a verdict debate-trail entry; treating as not-ready"
            );
            JudgeVerdictResult {
                body: "Judge did not record an explicit verdict (treated as not-ready)".into(),
                blocking: true,
            }
        };

        let now_awaiting = if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            state.record_judge_verdict(&verdict);
            state.is_awaiting_human_review()
        } else {
            false
        };

        if now_awaiting {
            if let Some(state) = self.active_refinements.get(proposal_id).cloned() {
                self.persist_awaiting_review(proposal_id, &verdict.body, &state)
                    .await;
            }
            tracing::info!(
                proposal_id = %proposal_id,
                round,
                "Judge ruled READY — tribunal parked for human accept/reject"
            );
        } else {
            tracing::info!(
                proposal_id = %proposal_id,
                round,
                "Judge ruled not-ready — running another round"
            );
        }
    }

    /// Terminate a refinement loop and persist stop metadata.
    async fn terminate_refinement(&mut self, proposal_id: &str, reason: StopReason) {
        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            state.terminate(reason.clone());
        }
        self.persist_refinement_stop(proposal_id, &reason).await;
        self.refinement_sessions.remove(proposal_id);
    }

    /// Resolve the human's single accept/reject review of a converged
    /// refinement. `accept` keeps the refined spec; reject restores the
    /// pre-refinement snapshot. `feedback` is recorded for the audit trail.
    /// Returns `Err` if no refinement is parked for review on this proposal.
    pub(super) async fn resolve_refinement_review(
        &mut self,
        proposal_id: &str,
        accept: bool,
        feedback: Option<String>,
    ) -> Result<(), String> {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return Err(format!("no active refinement for proposal {proposal_id}"));
        };
        if !state.is_awaiting_human_review() {
            return Err("refinement is not awaiting human review".into());
        }

        if !accept {
            // Reject → restore the pre-refinement snapshot spec.
            if let Err(e) = self
                .reset_live_spec_to_revision(proposal_id, state.snapshot_revision_seq)
                .await
            {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to revert spec to snapshot on reject"
                );
            }
        }

        if let Some(s) = self.active_refinements.get_mut(proposal_id) {
            // v1: reject always reverts + stops (we record the feedback but do
            // not auto-re-loop yet); accept keeps the refined spec.
            s.resolve_human_review(accept, false);
        }

        let reason_tag = if accept {
            "human_accepted"
        } else {
            "human_rejected"
        };
        let meta = serde_json::json!({
            "source": "human_review",
            "event": "refinement_stop",
            "reason_tag": reason_tag,
            "feedback": feedback,
        });
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        if let Err(e) = proposal_repo
            .record_refinement_lifecycle(proposal_id, "refinement_stop", Some(&meta))
            .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to persist human-review resolution"
            );
        }

        self.refinement_sessions.remove(proposal_id);
        self.active_refinements.retain(|_, s| !s.is_complete());
        tracing::info!(
            proposal_id = %proposal_id,
            accept,
            "Human resolved refinement review"
        );
        Ok(())
    }

    /// Reset the live proposal spec to the state at `target_revision_seq` —
    /// used when the human REJECTS the refined result, to restore the
    /// pre-refinement snapshot. Reads the spec from the `proposal_revisions`
    /// row at `target_revision_seq` and writes it back to the live proposal.
    ///
    /// Best-effort: if the target revision doesn't exist or the write fails,
    /// the caller logs a warning and continues.
    async fn reset_live_spec_to_revision(
        &self,
        proposal_id: &str,
        target_revision_seq: i32,
    ) -> Result<(), String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let revisions = proposal_repo
            .revisions(proposal_id)
            .await
            .map_err(|e| format!("failed to read revisions: {e}"))?;

        // Find the spec_revision at or before the target seq.
        let target_rev = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "spec_revision" && r.seq <= target_revision_seq);

        let Some(rev) = target_rev else {
            return Err(format!(
                "no spec_revision found at or before seq {target_revision_seq}"
            ));
        };

        // Only revert if there's something to revert to.
        if rev.body.is_empty() && rev.title.is_empty() {
            return Ok(());
        }

        // We need direct DB access for the revert. Re-use the proposal repo's
        // update helper by reading the previous revision and writing it back.
        // This uses the standard update path which will create a new revision
        // row — but that's acceptable: the row records the revert event.
        let event_bus2 = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
        let current = proposal_repo2
            .get(proposal_id)
            .await
            .map_err(|e| format!("failed to read proposal: {e}"))?
            .ok_or_else(|| format!("proposal not found: {proposal_id}"))?;

        let event_bus3 = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo3 = ProposalRepository::new(self.db.clone(), event_bus3);
        proposal_repo3
            .update(
                proposal_id,
                djinn_db::ProposalUpdateInput {
                    title: &rev.title,
                    body: &rev.body,
                    acceptance_criteria: &rev.acceptance_criteria,
                    status: &current.status,
                    superseded_by: current.superseded_by.as_deref(),
                    body_format: Some(&rev.body_format),
                    event_metadata: Some(&serde_json::json!({
                        "source": "refinement_reject_revert",
                        "reverted_from_seq": current.latest_revision_seq,
                        "reverted_to_seq": target_revision_seq,
                    })),
                },
            )
            .await
            .map_err(|e| format!("failed to revert proposal body: {e}"))?;

        Ok(())
    }

    /// Startup reconciliation for refinements interrupted by a restart.
    ///
    /// A refinement's loop state lives only in memory (`active_refinements`),
    /// but its `refinement_start` lifecycle row is durable. After a restart the
    /// in-memory loop is gone yet the DB still reports the proposal as `active`,
    /// so it becomes a "zombie": it makes no progress and the human cannot
    /// resolve it. This runs ONCE, before the actor's message/tick loop begins
    /// (so `active_refinements` is empty and there is no race with a fresh
    /// start), and records a `refinement_stop` for every DB-dangling refinement
    /// so the proposal returns to a clean, restartable state.
    pub(super) async fn recover_interrupted_refinements(&mut self) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let dangling = match proposal_repo.dangling_refinement_proposal_ids().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to query dangling refinements for startup recovery"
                );
                return;
            }
        };
        if dangling.is_empty() {
            return;
        }
        tracing::info!(
            count = dangling.len(),
            "Reconciling refinements interrupted by restart"
        );
        for proposal_id in dangling {
            // Skip anything already being driven in memory (defensive — this
            // runs before the loop starts, so the map is normally empty).
            if self.active_refinements.contains_key(&proposal_id) {
                continue;
            }
            self.persist_refinement_stop(&proposal_id, &StopReason::Interrupted)
                .await;
            tracing::info!(
                proposal_id = %proposal_id,
                "Stopped interrupted refinement (lost across restart); proposal is restartable"
            );
        }
    }

    /// Persist refinement-stop lifecycle metadata.
    async fn persist_refinement_stop(&self, proposal_id: &str, reason: &StopReason) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let stop_meta = serde_json::json!({
            "source": "refinement_loop",
            "event": "refinement_stop",
            "reason_tag": reason.tag(),
            "reason_detail": format!("{reason:?}"),
        });

        if let Err(e) = proposal_repo
            .record_refinement_lifecycle(proposal_id, "refinement_stop", Some(&stop_meta))
            .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to persist refinement_stop lifecycle metadata"
            );
        }
    }

    /// Read the `diverse_refinement` user setting for the proposal's owner.
    async fn read_diverse_refinement_setting(&self, proposal_id: &str) -> bool {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let creator_id = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p.author_user_id,
            _ => return true,
        };

        let Some(uid) = creator_id else {
            return true;
        };

        let us_repo = UserSettingsRepository::new(self.db.clone());
        match us_repo.get(&uid).await {
            Ok(Some(s)) => s.diverse_refinement,
            Ok(None) => true,
            Err(_) => true,
        }
    }

    /// Resolve the dispatch parameters (agent_type, model_id) for a
    /// refinement phase.
    async fn resolve_refinement_dispatch_params(
        &self,
        phase: RefinementPhase,
        diverse_refinement: bool,
        attributed_user_id: Option<&str>,
    ) -> (String, String) {
        let agent_type = match phase {
            RefinementPhase::AdvocateRevision => "advocate",
            RefinementPhase::AdversaryAttack => "adversary",
            RefinementPhase::JudgeAdjudication => "judge",
            RefinementPhase::AwaitingHumanReview | RefinementPhase::Complete => {
                return ("advocate".into(), String::new());
            }
        };

        // The tribunal runs on the attributed user's "Plan" role models
        // (planner/architect lane) — the same per-user resolution the worker
        // dispatch path uses — rather than the legacy global priority list.
        let user_models = self
            .resolve_dispatch_models_for_role("planner", attributed_user_id)
            .await;

        let primary_model = self.resolve_refinement_primary_model(&user_models);
        let candidates = self.resolve_refinement_model_candidates(&user_models);

        let (model_id, _same_fallback) =
            select_refinement_model(diverse_refinement, &primary_model, &candidates);

        (agent_type.to_string(), model_id)
    }

    /// Resolve the primary model for a refinement session: prefer the
    /// attributed user's Plan-role models, then the legacy global priority
    /// list, then a last-resort default.
    fn resolve_refinement_primary_model(&self, user_models: &[String]) -> String {
        if let Some(first) = user_models.first() {
            return first.clone();
        }
        self.model_priorities
            .values()
            .next()
            .and_then(|models| models.first().cloned())
            .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".to_string())
    }

    /// Resolve candidate models for diverse-refinement selection: the
    /// attributed user's Plan-role models if any, else the legacy global list.
    fn resolve_refinement_model_candidates(&self, user_models: &[String]) -> Vec<String> {
        if !user_models.is_empty() {
            return user_models.to_vec();
        }
        self.model_priorities
            .values()
            .flat_map(|models| models.iter().cloned())
            .collect()
    }

    /// Resolve the user a refinement run is attributed to: the explicitly
    /// chosen user if present, else the proposal author. Used for both task
    /// ownership and per-user model resolution.
    async fn resolve_refinement_attributed_user(
        &self,
        proposal_id: &str,
        explicit: Option<String>,
    ) -> Option<String> {
        if explicit.is_some() {
            return explicit;
        }
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        proposal_repo
            .get(proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|p| p.author_user_id)
    }

    /// Force-close a finished refinement task so phase/round tasks don't pile
    /// up `open` on the board after their session ends. Best-effort: a failure
    /// to close is logged, not propagated.
    async fn close_refinement_task(&self, task_id: &str, reason: &str) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus);
        if let Err(e) = task_repo
            .transition(
                task_id,
                TransitionAction::ForceClose,
                "coordinator",
                "system",
                Some(reason),
                None,
            )
            .await
        {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "Failed to close completed refinement task"
            );
        }
    }

    /// Resolve the project path for the slot pool dispatch.
    async fn resolve_refinement_project_path(&self, proposal_id: &str) -> String {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        match proposal_repo.targets(proposal_id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => "default".to_string(),
        }
    }

    /// Whether refinement dispatch is currently halted by an administrative
    /// dispatch pause — global, or scoped to the proposal's primary project.
    /// On a DB error loading the pause state, returns `false` (fail open so a
    /// transient blip doesn't silently wedge refinement).
    async fn refinement_dispatch_paused(&self, proposal_id: &str) -> bool {
        let pause_state = match crate::dispatch_pause::load_dispatch_pause_state(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to load dispatch-pause state for refinement; proceeding"
                );
                return false;
            }
        };
        if crate::dispatch_pause::active_global_dispatch_pause(&pause_state).is_some() {
            return true;
        }
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let Some(project_id) = proposal_repo
            .targets(proposal_id)
            .await
            .ok()
            .and_then(|t| t.first().map(|x| x.project_id.clone()))
        else {
            return false;
        };
        pause_state
            .projects
            .get(&project_id)
            .map(crate::dispatch_pause::dispatch_pause_is_active)
            .unwrap_or(false)
    }

    /// Create a refinement task in the DB for the given tribunal role,
    /// enriched with readiness context from the P1 DoR evaluator.
    async fn create_refinement_task_with_context(
        &self,
        proposal_id: &str,
        agent_type: &str,
        round: i32,
        against_revision_seq: i32,
        readiness_context: &str,
        attributed_user_id: Option<&str>,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus.clone());

        // Prefer the human-readable proposal title over the raw UUID so the
        // task is identifiable on the board (falls back to the id if the
        // proposal can't be loaded).
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let proposal = proposal_repo.get(proposal_id).await.ok().flatten();
        let proposal_label = proposal
            .as_ref()
            .map(|p| format!("\"{}\"", p.title))
            .unwrap_or_else(|| proposal_id.to_string());

        let title = format!("Refinement {agent_type} — {proposal_label} (round {round})");
        let description = format!(
            "Proposal refinement session: {agent_type} role for proposal {proposal_id}, \
             round {round}, against revision {against_revision_seq}.\n\n\
             Current DoR status: {readiness_context}"
        );

        // Find a project_id from the proposal's targets.
        let project_id = match proposal_repo.targets(proposal_id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => return None,
        };

        // Ensure the task table has the right columns. Use `create_in_project`
        // with a dummy design string (refinement tasks have no design).
        match task_repo
            .create_in_project(
                &project_id,
                None, // epic_id — refinement tasks aren't epic-scoped
                &title,
                &description,
                "", // design
                "refinement",
                0,        // priority
                "system", // owner
                None,     // status — defaults to "open"
                None,     // acceptance_criteria
            )
            .await
        {
            Ok(task) => {
                // Set the agent_type on the task so the supervisor's
                // role-overrides layer resolves the correct tribunal role.
                let event_bus2 = crate::events::event_bus_for(&self.events_tx);
                let task_repo2 = TaskRepository::new(self.db.clone(), event_bus2);
                if let Err(e) = task_repo2
                    .update_agent_type(&task.id, Some(agent_type))
                    .await
                {
                    tracing::warn!(
                        task_id = %task.id,
                        agent_type,
                        error = %e,
                        "Failed to set agent_type on refinement task"
                    );
                }
                // Attribute the task to the chosen/author user so it has a real
                // owner (refinement tasks are otherwise created owner-less, which
                // trips the ownership guard) and so per-user model resolution is
                // consistent with how it was dispatched.
                if let Some(uid) = attributed_user_id
                    && let Err(e) = task_repo2.set_created_by_user_id(&task.id, uid).await
                {
                    tracing::warn!(
                        task_id = %task.id,
                        error = %e,
                        "Failed to attribute refinement task to user"
                    );
                }
                Some(task.id)
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    agent_type,
                    error = %e,
                    "Failed to create refinement task"
                );
                None
            }
        }
    }

    /// Evaluate proposal readiness using the deterministic P1 DoR evaluator.
    ///
    /// Reads the current proposal body, acceptance criteria, and target count
    /// and calls `evaluate_proposal_readiness`. Returns `None` when the
    /// proposal cannot be loaded (the caller should treat this as
    /// "no readiness data available" rather than a fatal error).
    async fn evaluate_proposal_readiness(
        &self,
        proposal_id: &str,
    ) -> Option<djinn_control_plane::tools::proposal_readiness::ProposalReadinessResult> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            _ => return None,
        };

        let target_count = match proposal_repo.targets(proposal_id).await {
            Ok(targets) => targets.len(),
            _ => 0,
        };

        let ac_items: Vec<AcceptanceCriterionItem> =
            djinn_core::models::parse_json_array(&proposal.acceptance_criteria)
                .into_iter()
                .map(AcceptanceCriterionItem::Text)
                .collect();

        Some(evaluate_proposal_readiness(
            &proposal.body,
            &ac_items,
            target_count,
        ))
    }
}
