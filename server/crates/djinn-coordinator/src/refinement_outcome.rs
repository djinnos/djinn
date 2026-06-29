// Outcome processing, lifecycle management, and helper methods for the
// refinement tribunal dispatch loop.
//
// Complements `refinement_dispatch.rs` which owns the dispatch loop and
// per-user/model cap admission. Split to keep both files under the
// size-guard threshold.

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
use super::refinement_dispatch::RefinementSession;

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
            RefinementPhase::AwaitingHumanReview | RefinementPhase::Complete => {}
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
            RefinementPhase::AwaitingHumanReview | RefinementPhase::Complete => {
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
