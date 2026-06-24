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
use djinn_db::{ProposalRepository, TaskRepository, UserSettingsRepository};

use super::refinement::{
    AdversaryPassOutcome, AdversaryPassResult, JudgeVerdictResult, ObjectionRecord,
    RefinementLoopState, RefinementPhase, StopReason, UpdateAuthority,
    build_revision_event_metadata, parse_needs_evidence_claims, select_refinement_model,
};

use super::actor::CoordinatorActor;

/// How long to wait for a refinement session to start producing output
/// before treating it as stalled (conservative — sessions can take 5+ min).
const REFINEMENT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

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

        // When parked for needs-evidence, skip dispatch until the spike closes.
        if state.phase == RefinementPhase::NeedsEvidenceParked {
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

            // Session completed — process the outcome.
            self.process_refinement_outcome(proposal_id, &session).await;
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
    async fn dispatch_next_refinement_phase(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };

        let phase = state.phase;
        let round = state.current_round;
        let revision_seq = state.current_revision_seq;

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
        let (agent_type, model_id) =
            self.resolve_refinement_dispatch_params(phase, diverse_refinement);

        // Build a readiness-enriched task description so the agent sees
        // current DoR findings.
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

        // Record spawn in the state machine.
        {
            let state = self.active_refinements.get_mut(proposal_id).unwrap();
            if let Err(reason) = state.record_spawn() {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    ?reason,
                    "Refinement spawn cap reached"
                );
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
            RefinementPhase::Complete | RefinementPhase::NeedsEvidenceParked => {}
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
    /// In checkpoint mode, the revision is additionally marked as
    /// `checkpoint_pending` and the live proposal body is reverted to
    /// the previous revision so the pending revision is inspectable
    /// without mutating the live proposal.
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
        let is_checkpoint = state.update_authority() == UpdateAuthority::Checkpoint;

        // Persist revision attribution when the advocate advanced the spec.
        // The agent's `proposal_update` tool call doesn't carry refinement
        // context, so we patch the event_metadata post-hoc.
        if advanced {
            let model_id = self
                .refinement_sessions
                .get(proposal_id)
                .map(|s| s.model_id.clone());
            let mut event_meta = build_revision_event_metadata(
                state.current_round,
                state.update_authority(),
                model_id.as_deref(),
            );

            // In checkpoint mode, mark the revision as pending and revert
            // the live body so the proposal is unchanged until approval.
            if is_checkpoint {
                event_meta["checkpoint_status"] = serde_json::json!("pending");
                event_meta["previous_revision_seq"] = serde_json::json!(state.current_revision_seq);
            }

            let event_bus2 = crate::events::event_bus_for(&self.events_tx);
            let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
            if let Err(e) = proposal_repo2
                .set_latest_revision_event_metadata(proposal_id, new_revision_seq, &event_meta)
                .await
            {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    seq = new_revision_seq,
                    error = %e,
                    "Failed to patch advocate revision event_metadata"
                );
            }

            // In checkpoint mode, revert the live proposal body to the
            // pre-revision state. The revision row retains the advocate's
            // full output for later approval.
            if is_checkpoint {
                if let Err(e) = self
                    .revert_checkpoint_body(proposal_id, state.current_revision_seq)
                    .await
                {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        error = %e,
                        "Failed to revert proposal body for checkpoint pending"
                    );
                } else {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        pending_seq = new_revision_seq,
                        reverted_to_seq = state.current_revision_seq,
                        "Checkpoint: advocate revision marked pending, live body reverted"
                    );
                }
            }
        }

        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            if advanced {
                state.record_advocate_revision(new_revision_seq);
                tracing::info!(
                    proposal_id = %proposal_id,
                    new_seq = new_revision_seq,
                    round = state.current_round,
                    is_checkpoint,
                    "Advocate produced revision"
                );
            } else {
                state.phase = RefinementPhase::AdversaryAttack;
                tracing::info!(
                    proposal_id = %proposal_id,
                    revision_seq = state.current_revision_seq,
                    round = state.current_round,
                    "Advocate session completed without advancing revision"
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
                    "Refinement escalated"
                );
                self.persist_refinement_stop(proposal_id, &reason).await;
            }
        }
    }

    /// Process a judge session outcome by reading the verdict from
    /// the debate trail. When the verdict contains `NEEDS-EVIDENCE:` claims,
    /// creates spike tasks and parks the proposal instead of terminating.
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

        let (body, blocking) = if let Some(entry) = verdict_entry {
            (entry.body.clone(), entry.blocking)
        } else {
            (
                "Judge session completed without explicit verdict".into(),
                false,
            )
        };

        // Parse needs-evidence claims from the verdict body.
        let needs_evidence = parse_needs_evidence_claims(&body);

        let verdict = JudgeVerdictResult {
            body: body.clone(),
            blocking,
            needs_evidence: needs_evidence.clone(),
        };

        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            state.record_judge_verdict(&verdict);
        }

        if needs_evidence.is_empty() {
            // Normal verdict — terminate refinement.
            self.persist_refinement_stop(proposal_id, &StopReason::AdversaryDry)
                .await;

            tracing::info!(
                proposal_id = %proposal_id,
                blocking = verdict.blocking,
                "Judge verdict recorded — refinement complete"
            );
        } else {
            // Needs-evidence verdict — create spike and park proposal.
            let first_claim = &needs_evidence[0].claim;
            match self
                .create_needs_evidence_spike(proposal_id, first_claim, round)
                .await
            {
                Some(spike_task_id) => {
                    // Park the proposal.
                    let event_bus2 = crate::events::event_bus_for(&self.events_tx);
                    let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
                    if let Err(e) = proposal_repo2
                        .set_needs_evidence_spike(proposal_id, &spike_task_id, first_claim)
                        .await
                    {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            error = %e,
                            "Failed to park proposal for needs-evidence spike"
                        );
                        self.terminate_refinement(
                            proposal_id,
                            StopReason::AgentFailure {
                                role: "judge".into(),
                                error: format!("failed to park proposal: {e}"),
                            },
                        )
                        .await;
                        return;
                    }

                    // Record the parked state in the refinement loop.
                    if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                        state.record_needs_evidence_parked(spike_task_id.clone());
                    }

                    // Record a lifecycle event for the parking.
                    let event_bus3 = crate::events::event_bus_for(&self.events_tx);
                    let proposal_repo3 = ProposalRepository::new(self.db.clone(), event_bus3);
                    let park_meta = serde_json::json!({
                        "source": "refinement_loop",
                        "event": "needs_evidence_parked",
                        "spike_task_id": spike_task_id,
                        "claim": first_claim,
                        "round": round,
                    });
                    if let Err(e) = proposal_repo3
                        .record_refinement_lifecycle(
                            proposal_id,
                            "needs_evidence_parked",
                            Some(&park_meta),
                        )
                        .await
                    {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            error = %e,
                            "Failed to record needs_evidence_parked lifecycle"
                        );
                    }

                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        claim = %first_claim,
                        "Judge needs-evidence verdict — proposal parked, spike created"
                    );
                }
                None => {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        "Failed to create needs-evidence spike — terminating refinement"
                    );
                    self.terminate_refinement(
                        proposal_id,
                        StopReason::AgentFailure {
                            role: "judge".into(),
                            error: "spike task creation failed".into(),
                        },
                    )
                    .await;
                }
            }
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

    /// Revert the live proposal body to the state at `target_revision_seq`
    /// after a checkpoint-mode advocate revision. Reads the spec_snapshot
    /// from the proposal_revisions row at `target_revision_seq` and writes
    /// it back to the live proposals table.
    ///
    /// This is a best-effort operation: if the target revision doesn't exist
    /// or the revert fails, the caller logs a warning and continues.
    async fn revert_checkpoint_body(
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
                        "source": "checkpoint_revert",
                        "reverted_from_seq": current.latest_revision_seq,
                        "reverted_to_seq": target_revision_seq,
                    })),
                },
            )
            .await
            .map_err(|e| format!("failed to revert proposal body: {e}"))?;

        Ok(())
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
    fn resolve_refinement_dispatch_params(
        &self,
        phase: RefinementPhase,
        diverse_refinement: bool,
    ) -> (String, String) {
        let agent_type = match phase {
            RefinementPhase::AdvocateRevision => "advocate",
            RefinementPhase::AdversaryAttack => "adversary",
            RefinementPhase::JudgeAdjudication => "judge",
            RefinementPhase::Complete | RefinementPhase::NeedsEvidenceParked => {
                return ("advocate".into(), String::new());
            }
        };

        let primary_model = self.resolve_refinement_primary_model();
        let candidates = self.resolve_refinement_model_candidates();

        let (model_id, _same_fallback) =
            select_refinement_model(diverse_refinement, &primary_model, &candidates);

        (agent_type.to_string(), model_id)
    }

    /// Resolve the primary model for a refinement session.
    fn resolve_refinement_primary_model(&self) -> String {
        self.model_priorities
            .values()
            .next()
            .and_then(|models| models.first().cloned())
            .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".to_string())
    }

    /// Resolve candidate models for diverse-refinement selection.
    fn resolve_refinement_model_candidates(&self) -> Vec<String> {
        self.model_priorities
            .values()
            .flat_map(|models| models.iter().cloned())
            .collect()
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

    /// Create a refinement task in the DB for the given tribunal role,
    /// enriched with readiness context from the P1 DoR evaluator.
    async fn create_refinement_task_with_context(
        &self,
        proposal_id: &str,
        agent_type: &str,
        round: i32,
        against_revision_seq: i32,
        readiness_context: &str,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus.clone());

        let title = format!("Refinement {agent_type} — proposal {proposal_id} (round {round})");
        let description = format!(
            "Proposal refinement session: {agent_type} role for proposal {proposal_id}, \
             round {round}, against revision {against_revision_seq}.\n\n\
             Current DoR status: {readiness_context}"
        );

        // Find a project_id from the proposal's targets.
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
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

    // ── Needs-evidence spike helpers ────────────────────────────────────

    /// Create a spike task for a needs-evidence claim. The spike is an
    /// ordinary Djinn task with `issue_type = "spike"` and concrete
    /// acceptance criteria derived from the Judge's named feasibility claim.
    async fn create_needs_evidence_spike(
        &self,
        proposal_id: &str,
        claim: &str,
        round: i32,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus.clone());

        let title = format!("Spike: investigate feasibility claim for proposal {proposal_id}");
        let description = format!(
            "Research spike created by the Judge during proposal refinement (round {round}).\n\n\
             Feasibility claim: {claim}\n\n\
             Deliverable: Provide evidence (benchmarks, prototypes, documentation, or analysis) \
             that resolves this feasibility question so proposal refinement can resume."
        );
        let acceptance_criteria = serde_json::json!([
            format!("Feasibility claim addressed: {claim}"),
            "Evidence documented and linked to this task"
        ])
        .to_string();

        // Find a project_id from the proposal's targets.
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let project_id = match proposal_repo.targets(proposal_id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => return None,
        };

        match task_repo
            .create_in_project(
                &project_id,
                None, // epic_id — spike tasks aren't epic-scoped
                &title,
                &description,
                "", // design
                "spike",
                0,        // priority
                "system", // owner
                None,     // status — defaults to "open"
                Some(&acceptance_criteria),
            )
            .await
        {
            Ok(task) => Some(task.id),
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    claim = %claim,
                    error = %e,
                    "Failed to create needs-evidence spike task"
                );
                None
            }
        }
    }

    /// Resume refinement after a needs-evidence spike has closed. Reads the
    /// spike's finding, feeds it back into the debate trail as a rebuttal,
    /// clears the parking state, and advances the refinement loop.
    ///
    /// Called from the coordinator's event handler when a spike task closes.
    pub(super) async fn resume_needs_evidence_refinement(
        &mut self,
        proposal_id: &str,
        spike_task_id: &str,
    ) {
        // Read the spike task to get its resolution.
        let task_repo = self.task_repo();
        let spike_task = match task_repo.get(spike_task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::warn!(
                    spike_task_id = %spike_task_id,
                    "Spike task not found for needs-evidence resume"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    spike_task_id = %spike_task_id,
                    error = %e,
                    "DB error reading spike task for needs-evidence resume"
                );
                return;
            }
        };

        // Get the current state.
        let state = match self.active_refinements.get(proposal_id).cloned() {
            Some(s) => s,
            None => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    "No active refinement found for needs-evidence resume"
                );
                return;
            }
        };

        let round = state.current_round;
        let revision_seq = state.current_revision_seq;

        // Build a debate-trail entry with the spike finding.
        let finding_body = format!(
            "Spike finding (task {}): {}\n\n\
             Close reason: {}",
            spike_task.short_id,
            spike_task.description,
            spike_task.close_reason.as_deref().unwrap_or("completed"),
        );

        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let model_id = self
            .refinement_sessions
            .get(proposal_id)
            .map(|s| s.model_id.clone());

        if let Err(e) = proposal_repo
            .add_debate_trail_entry(djinn_db::ProposalDebateTrailCreateInput {
                proposal_id,
                kind: "rebuttal",
                body: &finding_body,
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: model_id.as_deref(),
                source_task_id: Some(spike_task_id),
                against_revision_seq: revision_seq,
                round,
            })
            .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to record spike finding in debate trail"
            );
        }

        // Clear the parking state on the proposal.
        let event_bus2 = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
        if let Err(e) = proposal_repo2.clear_needs_evidence_spike(proposal_id).await {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to clear needs-evidence spike linkage"
            );
        }

        // Record a lifecycle event for the resume.
        let event_bus3 = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo3 = ProposalRepository::new(self.db.clone(), event_bus3);
        let resume_meta = serde_json::json!({
            "source": "refinement_loop",
            "event": "needs_evidence_resumed",
            "spike_task_id": spike_task_id,
            "round": round,
        });
        if let Err(e) = proposal_repo3
            .record_refinement_lifecycle(proposal_id, "needs_evidence_resumed", Some(&resume_meta))
            .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to record needs_evidence_resumed lifecycle"
            );
        }

        // Resume the refinement loop.
        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            state.resume_from_needs_evidence();
        }

        tracing::info!(
            proposal_id = %proposal_id,
            spike_task_id = %spike_task_id,
            "Needs-evidence spike closed — refinement resumed"
        );
    }
}
