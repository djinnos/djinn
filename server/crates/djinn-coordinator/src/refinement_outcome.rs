// Outcome processing, lifecycle management, and helper methods for the
// refinement tribunal dispatch loop.
//
// Complements `refinement_dispatch.rs` which owns the dispatch loop and
// per-user/model cap admission. Split to keep both files under the
// size-guard threshold.

use djinn_control_plane::tools::epic_ops::{
    AcceptanceCriterionItem, parse_acceptance_criteria_array,
};
use djinn_control_plane::tools::proposal_readiness::evaluate_proposal_readiness;
use djinn_core::models::{Proposal, ProposalDebateTrail, TransitionAction};
use djinn_db::{ProposalRepository, TaskRepository, UserSettingsRepository};

use super::refinement::{
    AdversaryPassOutcome, AdversaryPassResult, JudgeVerdictResult, ObjectionRecord,
    RefinementLoopState, RefinementPhase, StopReason, build_revision_event_metadata,
    select_refinement_model,
};

use super::actor::CoordinatorActor;
use super::refinement_dispatch::RefinementSession;

fn format_debate_context_entry(entry: &ProposalDebateTrail) -> String {
    format!(
        "- round {}, revision {}, {} by {} (blocking={}): {}",
        entry.round,
        entry.against_revision_seq,
        entry.kind,
        entry.agent_role,
        entry.blocking,
        entry.body
    )
}

impl CoordinatorActor {
    /// Process the outcome of a completed refinement session.
    pub(super) async fn process_refinement_outcome(
        &mut self,
        proposal_id: &str,
        session: &RefinementSession,
    ) {
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
            RefinementPhase::AwaitingHumanReview
            | RefinementPhase::AwaitingEvidence
            | RefinementPhase::Complete => {}
        }
    }

    /// Process an advocate session outcome: read the latest revision,
    /// patch event_metadata with refinement attribution, and advance.
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

        if advanced {
            let model_id = self
                .refinement_sessions
                .get(proposal_id)
                .map(|s| s.model_id.clone());
            let event_meta =
                build_revision_event_metadata(state.current_round, model_id.as_deref());
            let event_bus2 = crate::events::event_bus_for(&self.events_tx);
            let proposal_repo2 = ProposalRepository::new(self.db.clone(), event_bus2);
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

    /// Process an adversary session outcome: read debate-trail objections
    /// and feed them to the state machine.
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

        let Some(state) = self.active_refinements.get_mut(proposal_id) else {
            tracing::warn!(
                proposal_id = %proposal_id,
                "adversary outcome arrived for missing refinement state"
            );
            return;
        };
        let outcome = state.process_adversary_pass(&adversary_result);

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
                if let Some(state) = self.active_refinements.get(proposal_id).cloned() {
                    let summary = format!("Escalated to human review: {reason:?}");
                    self.persist_awaiting_review(proposal_id, &summary, &state)
                        .await;
                }
            }
        }
    }

    /// Persist that the tribunal has parked for human accept/reject review.
    pub(super) async fn persist_awaiting_review(
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

        // Surface converged-awaiting-review proposals in the standard status
        // grouping: a `draft` proposal that the tribunal parks for human review
        // advances to `in_review`. Idempotent and status-scoped (draft-only);
        // any other status is left untouched.
        match proposal_repo.advance_draft_to_in_review(proposal_id).await {
            Ok(true) => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    "Tribunal converged — advanced draft proposal to in_review"
                );
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to advance draft proposal to in_review on awaiting-review park"
                );
            }
        }
    }

    /// Process a judge session outcome: read the verdict from the debate
    /// trail and advance the state machine.
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

        // ── Check for an accepted needs-evidence demand first ───────────
        //
        // If the Judge called `proposal_refinement_demand_evidence` and the
        // demand was accepted, a `kind = "needs_evidence"` entry with valid
        // metadata exists for the current round. This takes priority over a
        // normal verdict: the loop parks in AwaitingEvidence and no further
        // tribunal phases are dispatched until the evidence spike produces
        // findings.
        let needs_evidence_entry = entries.iter().find(|e| {
            e.agent_role == "judge"
                && e.kind == "needs_evidence"
                && e.round == round
                && e.body_metadata.is_some()
        });

        if let Some(_entry) = needs_evidence_entry {
            if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                state.record_needs_evidence();
            }
            tracing::info!(
                proposal_id = %proposal_id,
                round,
                "Judge demanded evidence — refinement parked (AwaitingEvidence)"
            );
            return;
        }

        // ── Normal verdict path ─────────────────────────────────────────
        let verdict_entry = entries
            .iter()
            .find(|e| e.agent_role == "judge" && e.kind == "verdict" && e.round == round);

        let verdict = if let Some(entry) = verdict_entry {
            JudgeVerdictResult {
                body: entry.body.clone(),
                blocking: entry.blocking,
            }
        } else {
            // No verdict entry: treat as not-ready (non-decision).
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
    pub(super) async fn terminate_refinement(&mut self, proposal_id: &str, reason: StopReason) {
        if let Some(state) = self.active_refinements.get_mut(proposal_id) {
            state.terminate(reason.clone());
        }
        self.persist_refinement_stop(proposal_id, &reason).await;
        self.refinement_sessions.remove(proposal_id);
    }

    /// Resolve the human's single accept/reject review of a converged
    /// refinement. Returns `Err` if no refinement is parked for review.
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

        if !accept
            && let Err(e) = self
                .reset_live_spec_to_revision(proposal_id, state.snapshot_revision_seq)
                .await
        {
            tracing::warn!(
                proposal_id = %proposal_id,
                error = %e,
                "Failed to revert spec to snapshot on reject"
            );
        }

        if let Some(s) = self.active_refinements.get_mut(proposal_id) {
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

    /// Reset the live proposal spec to the state at `target_revision_seq`.
    /// Best-effort: logs a warning and continues on failure.
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

        let target_rev = revisions
            .iter()
            .rev()
            .find(|r| r.event_kind == "spec_revision" && r.seq <= target_revision_seq);

        let Some(rev) = target_rev else {
            return Err(format!(
                "no spec_revision found at or before seq {target_revision_seq}"
            ));
        };

        if rev.body.is_empty() && rev.title.is_empty() {
            return Ok(());
        }

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
    /// Runs once before the message loop; records `refinement_stop` for
    /// every DB-dangling refinement so the proposal is restartable.
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
    pub(super) async fn persist_refinement_stop(&self, proposal_id: &str, reason: &StopReason) {
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
    pub(super) async fn read_diverse_refinement_setting(&self, proposal_id: &str) -> bool {
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
    pub(super) async fn resolve_refinement_dispatch_params(
        &self,
        phase: RefinementPhase,
        diverse_refinement: bool,
        attributed_user_id: Option<&str>,
    ) -> (String, String) {
        let agent_type = match phase {
            RefinementPhase::AdvocateRevision => "advocate",
            RefinementPhase::AdversaryAttack => "adversary",
            RefinementPhase::JudgeAdjudication => "judge",
            RefinementPhase::AwaitingHumanReview
            | RefinementPhase::AwaitingEvidence
            | RefinementPhase::Complete => {
                return ("advocate".into(), String::new());
            }
        };

        let user_models = self
            .resolve_dispatch_models_for_role("planner", attributed_user_id)
            .await;

        let primary_model = self.resolve_refinement_primary_model(&user_models);
        let candidates = self.resolve_refinement_model_candidates(&user_models);

        let (model_id, _same_fallback) =
            select_refinement_model(diverse_refinement, &primary_model, &candidates);

        (agent_type.to_string(), model_id)
    }

    /// Resolve the primary model for a refinement session.
    pub(super) fn resolve_refinement_primary_model(&self, user_models: &[String]) -> String {
        if let Some(first) = user_models.first() {
            return first.clone();
        }
        self.model_priorities
            .values()
            .next()
            .and_then(|models| models.first().cloned())
            .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".to_string())
    }

    /// Resolve candidate models for diverse-refinement selection.
    pub(super) fn resolve_refinement_model_candidates(
        &self,
        user_models: &[String],
    ) -> Vec<String> {
        if !user_models.is_empty() {
            return user_models.to_vec();
        }
        self.model_priorities
            .values()
            .flat_map(|models| models.iter().cloned())
            .collect()
    }

    /// Resolve the user a refinement run is attributed to.
    pub(super) async fn resolve_refinement_attributed_user(
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

    /// Force-close a finished refinement task. Best-effort.
    pub(super) async fn close_refinement_task(&self, task_id: &str, reason: &str) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus);
        let result = task_repo
            .transition(
                task_id,
                TransitionAction::ForceClose,
                "coordinator",
                "system",
                Some(reason),
                None,
            )
            .await
            .map(|_| ());
        handle_close_refinement_task_result(task_id, result);
    }

    /// Resolve the project path for the slot pool dispatch.
    pub(super) async fn resolve_refinement_project_path(&self, proposal_id: &str) -> String {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        match proposal_repo.targets(proposal_id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => "default".to_string(),
        }
    }

    /// Whether refinement dispatch is halted by an administrative pause.
    pub(super) async fn refinement_dispatch_paused(&self, proposal_id: &str) -> bool {
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

    /// Create a refinement task in the DB, enriched with DoR context and any
    /// current-revision human reviewer feedback from the latest demand round.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn create_refinement_task_with_context(
        &self,
        proposal_id: &str,
        agent_type: &str,
        round: i32,
        against_revision_seq: i32,
        readiness_context: &str,
        reviewer_feedback: Option<&str>,
        attributed_user_id: Option<&str>,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus.clone());

        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let proposal = proposal_repo.get(proposal_id).await.ok().flatten();
        let proposal_label = proposal
            .as_ref()
            .map(|p| format!("\"{}\"", p.title))
            .unwrap_or_else(|| proposal_id.to_string());

        let title = format!("Refinement {agent_type} — {proposal_label} (round {round})");
        let mut description = format!(
            "Proposal refinement session: {agent_type} role for proposal {proposal_id}, \
             round {round}, against revision {against_revision_seq}.\n\n\
             Current DoR status: {readiness_context}"
        );

        if agent_type.eq_ignore_ascii_case("advocate")
            && let Some(evidence_context) = self
                .build_advocate_evidence_findings_context(proposal.as_ref(), proposal_id)
                .await
        {
            description.push_str("\n\n");
            description.push_str(&evidence_context);
        }

        // Inject current human reviewer feedback near the DoR status so the
        // tribunal agent sees the exact feedback string for this round. The
        // caller is responsible for ensuring the feedback belongs to the
        // proposal's current revision (see
        // `ProposalRepository::latest_current_revision_reviewer_feedback`).
        if let Some(feedback) = reviewer_feedback.filter(|s| !s.is_empty()) {
            description.push_str("\n\nHuman reviewer feedback for this round: ");
            description.push_str(feedback);
        }

        let project_id = match proposal_repo.targets(proposal_id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => return None,
        };

        match task_repo
            .create_in_project(
                &project_id,
                None,
                &title,
                &description,
                "",
                "refinement",
                0,
                "system",
                None,
                None,
            )
            .await
        {
            Ok(task) => {
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

    async fn build_advocate_evidence_findings_context(
        &self,
        proposal: Option<&Proposal>,
        proposal_id: &str,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let entries = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read debate trail for evidence findings context"
                );
                return None;
            }
        };

        let findings = entries
            .iter()
            .rev()
            .find(|entry| entry.kind == "evidence_findings" && entry.agent_role == "spike")?;

        let mut context = String::from(
            "Evidence findings received for this Advocate round. Use these findings to respond to the current proposal and objections before Judge adjudication resumes.\n",
        );
        if let Some(proposal) = proposal {
            context.push_str("\nCurrent proposal body:\n");
            context.push_str(&proposal.body);
            context.push('\n');
        }

        let current_debate = entries
            .iter()
            .filter(|entry| matches!(entry.kind.as_str(), "objection" | "rebuttal"))
            .map(format_debate_context_entry)
            .collect::<Vec<_>>();
        if !current_debate.is_empty() {
            context.push_str("\nCurrent objections/rebuttals:\n");
            context.push_str(&current_debate.join("\n"));
            context.push('\n');
        }

        context.push_str("\nEvidence findings:\n");
        context.push_str(&format_debate_context_entry(findings));
        if let Some(metadata) = findings.body_metadata.as_deref().filter(|s| !s.is_empty()) {
            context.push_str("\nStructured findings metadata:\n");
            context.push_str(metadata);
        }
        Some(context)
    }

    /// Evaluate proposal readiness using the deterministic P1 DoR evaluator.
    pub(super) async fn evaluate_proposal_readiness(
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

        // Parse ACs with the tolerant, structure-aware parser that every read
        // path uses (proposal_show et al.). `djinn_core::models::parse_json_array`
        // deserializes to `Vec<String>` and therefore fails wholesale on
        // structured `{ "criterion", "met" }` ACs, silently yielding an empty
        // vec — which made the injected "Current DoR status" report "At least
        // one acceptance criterion is required" even when the live proposal head
        // already had structured ACs attached (proposal 019f0c32 rounds 1–3).
        let ac_items: Vec<AcceptanceCriterionItem> =
            parse_acceptance_criteria_array(&proposal.acceptance_criteria);

        Some(evaluate_proposal_readiness(
            &proposal.body,
            &ac_items,
            target_count,
        ))
    }
}

/// Classify and handle the result of a `close_refinement_task` transition.
///
/// The already-closed case (`"task is already closed"`) is treated as an
/// idempotent no-op — no warning is emitted.  All other errors (other
/// [`djinn_db::Error::InvalidTransition`] messages and repository/internal
/// failures) surface as a `WARN` so operators can investigate.
fn handle_close_refinement_task_result(task_id: &str, result: Result<(), djinn_db::Error>) {
    if let Err(e) = result {
        if is_already_closed_refinement_close_error(&e) {
            // Idempotent no-op: the task was already closed — nothing to do.
            return;
        }
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "Failed to close completed refinement task"
        );
    }
}

/// Returns `true` when the repository error indicates the task was already closed
/// at the time `ForceClose` was attempted. This is the only idempotent close case
/// — all other [`djinn_db::Error::InvalidTransition`] messages remain real failures
/// that must surface as warnings.
fn is_already_closed_refinement_close_error(error: &djinn_db::Error) -> bool {
    matches!(
        error,
        djinn_db::Error::InvalidTransition(msg) if msg == "task is already closed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_already_closed_refinement_close_error ----

    #[test]
    fn force_close_already_closed_returns_true() {
        let error = djinn_db::Error::InvalidTransition("task is already closed".to_owned());
        assert!(is_already_closed_refinement_close_error(&error));
    }

    #[test]
    fn force_close_other_invalid_transition_returns_false() {
        let error =
            djinn_db::Error::InvalidTransition("release is only valid from in_progress".to_owned());
        assert!(!is_already_closed_refinement_close_error(&error));
    }

    #[test]
    fn force_close_non_transition_error_returns_false() {
        let error = djinn_db::Error::Internal("something broke".to_owned());
        assert!(!is_already_closed_refinement_close_error(&error));
    }

    // ---- handle_close_refinement_task_result regression tests ----

    #[test]
    #[tracing_test::traced_test]
    fn close_already_closed_emits_no_warning() {
        let already_closed =
            djinn_db::Error::InvalidTransition("task is already closed".to_owned());
        handle_close_refinement_task_result("task/abc", Err(already_closed));

        assert!(
            !logs_contain("Failed to close completed refinement task"),
            "already-closed close should not emit a warning"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn close_other_invalid_transition_emits_warning() {
        let other =
            djinn_db::Error::InvalidTransition("release is only valid from in_progress".to_owned());
        handle_close_refinement_task_result("task/xyz", Err(other));

        assert!(
            logs_contain("Failed to close completed refinement task"),
            "non-idempotent InvalidTransition must still warn"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn close_internal_error_emits_warning() {
        let internal = djinn_db::Error::Internal("database connection lost".to_owned());
        handle_close_refinement_task_result("task/123", Err(internal));

        assert!(
            logs_contain("Failed to close completed refinement task"),
            "internal/repository errors must still warn"
        );
    }

    // `logs_contain` is injected by the `#[tracing_test::traced_test]` macro
    // into each test function scope; no module-level helper is needed.
}
