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
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read proposal after advocate session"
                );
                return;
            }
        };

        let revision = proposal.latest_revision_seq;

        // If the judge is not the current agent, this is the first pass; just
        // note the revision and move to the adversary phase.
        if state.judge_agent.is_none() {
            self.advance_to_phase(
                proposal_id,
                RefinementPhase::AdversaryAttack,
                state.with_revision(revision),
            )
            .await;
            return;
        }

        // Otherwise this is an adversary-driven revision: update the adversary
        // record with the new revision and clear the objection flag so the loop
        // can re-evaluate.
        let mut next_state = state.clone();
        next_state.adversary_pass_outcome = Some(AdversaryPassOutcome::Revised {
            new_revision: revision,
        });
        next_state.objection = None;
        self.advance_to_phase(proposal_id, RefinementPhase::AdversaryAttack, next_state)
            .await;
    }

    /// Process an adversary session outcome: read the objection record (if any),
    /// decide whether to continue, escalate, or end refinement.
    async fn process_adversary_outcome(&mut self, proposal_id: &str, state: &RefinementLoopState) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(proposal_id = %proposal_id, "Proposal not found after adversary");
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "adversary".into(),
                        error: "proposal not found after session".into(),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read proposal after adversary session"
                );
                return;
            }
        };

        // Read the most recent objection record for this proposal and revision.
        let objection = match ObjectionRecord::latest_for_revision(
            self.db.clone(),
            proposal_id,
            state.focused_revision,
        )
        .await
        {
            Ok(Some(obj)) => Some(obj),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    revision = state.focused_revision,
                    error = %e,
                    "Failed to read objection record after adversary session"
                );
                None
            }
        };

        let mut next_state = state.clone();
        next_state.objection = objection.clone();

        let Some(obj) = objection else {
            // No objection: refinement is complete.
            self.advance_to_phase(proposal_id, RefinementPhase::Complete, next_state)
                .await;
            return;
        };

        if obj.blocking {
            // Blocking objection: count it and decide whether to escalate or
            // continue revising.
            next_state.blocking_objection_count += 1;
            if next_state.blocking_objection_count >= 3 {
                self.terminate_refinement(proposal_id, StopReason::EscalatedToHuman)
                    .await;
                return;
            }
            // Need a revision from the advocate.
            self.advance_to_phase(proposal_id, RefinementPhase::AdvocateRevision, next_state)
                .await;
            return;
        }

        // Non-blocking objection: just note it and move to the judge.
        self.advance_to_phase(proposal_id, RefinementPhase::JudgeAdjudication, next_state)
            .await;
    }

    /// Process a judge session outcome: read the verdict, update the
    /// loop state, and either continue refining or finish.
    async fn process_judge_outcome(&mut self, proposal_id: &str, state: &RefinementLoopState) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                tracing::warn!(proposal_id = %proposal_id, "Proposal not found after judge");
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: "judge".into(),
                        error: "proposal not found after session".into(),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read proposal after judge session"
                );
                return;
            }
        };

        let verdict = match &state.objection {
            Some(obj) => JudgeVerdictResult::from_objection(&proposal, obj).await,
            None => {
                // No objection to judge — shouldn't happen, but treat as
                // complete rather than crash.
                self.advance_to_phase(proposal_id, RefinementPhase::Complete, state.clone())
                    .await;
                return;
            }
        };

        let mut next_state = state.clone();

        match verdict {
            JudgeVerdictResult::Sustained => {
                // Sustained objection: the adversary wins; require a revision.
                next_state.adversary_pass_outcome = Some(AdversaryPassOutcome::Sustained);
                self.advance_to_phase(proposal_id, RefinementPhase::AdvocateRevision, next_state)
                    .await;
            }
            JudgeVerdictResult::Overruled => {
                // Overruled objection: the advocate wins; clear the objection
                // and continue the adversary pass.
                next_state.adversary_pass_outcome = Some(AdversaryPassOutcome::Overruled);
                next_state.objection = None;
                self.advance_to_phase(proposal_id, RefinementPhase::AdversaryAttack, next_state)
                    .await;
            }
            JudgeVerdictResult::NeedsHuman => {
                self.terminate_refinement(proposal_id, StopReason::EscalatedToHuman)
                    .await;
            }
            JudgeVerdictResult::Error { error } => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %error,
                    "Judge verdict parse failed; continuing adversary pass"
                );
                next_state.objection = None;
                self.advance_to_phase(proposal_id, RefinementPhase::AdversaryAttack, next_state)
                    .await;
            }
        }
    }

    /// Advance the refinement loop to the next phase.
    async fn advance_to_phase(
        &mut self,
        proposal_id: &str,
        phase: RefinementPhase,
        mut next_state: RefinementLoopState,
    ) {
        // Keep a note of the previous phase for logging.
        let previous_phase = next_state.phase;
        next_state.phase = phase;
        self.active_refinements
            .insert(proposal_id.to_string(), next_state.clone());

        // Record a phase transition if the phase actually changed.
        if previous_phase != phase {
            if let Err(e) = self.record_phase_transition(proposal_id, previous_phase, phase).await {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to record refinement phase transition"
                );
            }
        }
    }

    /// Record a phase transition in the debate trail.
    async fn record_phase_transition(
        &mut self,
        proposal_id: &str,
        from: RefinementPhase,
        to: RefinementPhase,
    ) -> djinn_db::Result<()> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        proposal_repo
            .create_debate_trail(
                proposal_id,
                "system",
                "refinement",
                "system",
                "",
                false,
                Some(
                    serde_json::json!({
                        "from": from.as_str(),
                        "to": to.as_str(),
                        "kind": "phase_transition",
                    })
                    .to_string(),
                ),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
    }

    /// Terminate the refinement loop for a proposal, recording the reason and
    /// cleaning up any active session state.
    async fn terminate_refinement(&mut self, proposal_id: &str, reason: StopReason) {
        // Record terminal reason for observability.
        tracing::info!(proposal_id = %proposal_id, reason = ?reason, "Refinement loop terminated");
        let mut next_state = match self.active_refinements.remove(proposal_id) {
            Some(s) => s,
            None => RefinementLoopState::new(proposal_id, 1),
        };
        next_state.phase = RefinementPhase::Complete;
        next_state.stop_reason = Some(reason);
        self.active_refinements
            .insert(proposal_id.to_string(), next_state);
    }

    /// Resolve a candidate model list for refinement dispatch.
    ///
    /// If `user_models` is non-empty, use those exactly; otherwise fall back to
    /// the configured model priorities.
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
        self.close_refinement_task_with_result(
            task_id,
            reason,
            self.run_close_refinement_task_transition(task_id, reason).await,
        )
        .await;
    }

    async fn run_close_refinement_task_transition(
        &self,
        task_id: &str,
        reason: &str,
    ) -> Result<(), djinn_db::Error> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = TaskRepository::new(self.db.clone(), event_bus);
        task_repo
            .transition(
                task_id,
                TransitionAction::ForceClose,
                "coordinator",
                "system",
                Some(reason),
                None,
            )
            .await
    }

    async fn close_refinement_task_with_result(
        &self,
        task_id: &str,
        _reason: &str,
        result: Result<(), djinn_db::Error>,
    ) {
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

    #[cfg(test)]
    async fn close_refinement_task_for_test(
        &self,
        task_id: &str,
        reason: &str,
        simulated_result: Result<(), djinn_db::Error>,
    ) {
        self.close_refinement_task_with_result(task_id, reason, simulated_result)
            .await;
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

        let agent_type_label = match agent_type {
            "advocate" => "advocate revision",
            "adversary" => "adversary review",
            "judge" => "judge adjudication",
            _ => agent_type,
        };

        let title = format!(
            "Refinement {} round {} for proposal {}",
            agent_type_label, round, proposal_label
        );

        let body = {
            let proposal = proposal?;
            let mut context = String::new();
            context.push_str(&format!(
                "Proposal title: {}\nCurrent revision: {}\n\n",
                proposal.title, proposal.latest_revision_seq
            ));
            context.push_str(&format!(
                "Refinement readiness context (round {}, revision {}):\n{}\n\n",
                round, against_revision_seq, readiness_context
            ));
            if let Some(feedback) = reviewer_feedback {
                context.push_str("Human reviewer feedback for this round:\n");
                context.push_str(feedback);
            }
            context
        };

        let metadata = {
            let mut m = serde_json::Map::new();
            m.insert(
                "refinement".to_string(),
                serde_json::json!({
                    "proposal_id": proposal_id,
                    "agent_type": agent_type,
                    "round": round,
                    "against_revision_seq": against_revision_seq,
                }),
            );
            if let Some(user_id) = attributed_user_id {
                m.insert(
                    "attributed_user_id".to_string(),
                    serde_json::Value::String(user_id.to_string()),
                );
            }
            serde_json::Value::Object(m)
        };

        let task = task_repo
            .create(djinn_db::CreateTaskParams {
                title: &title,
                body: &body,
                status: Some("open"),
                attributed_user_id,
                metadata: Some(metadata.to_string()),
            })
            .await;

        match task {
            Ok(t) => Some(t.id),
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    agent_type = %agent_type,
                    error = %e,
                    "Failed to create refinement task"
                );
                None
            }
        }
    }

    /// Build a short readiness summary for the current proposal revision.
    ///
    /// Used as a context string for the refinement prompt and stored in the
    /// created task body.
    pub(super) async fn build_refinement_readiness_context(
        &self,
        proposal_id: &str,
        target_count: usize,
    ) -> Option<String> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);
        let proposal = proposal_repo.get(proposal_id).await.ok().flatten()?;

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

    // ---- close_refinement_task idempotency regression ----

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn close_refinement_task_already_closed_emits_no_warning() {
        let actor = actor_for_test().await;

        // Simulate the exact idempotent error the repository boundary returns.
        let already_closed =
            djinn_db::Error::InvalidTransition("task is already closed".to_owned());
        actor
            .close_refinement_task_for_test("task/abc", "already closed", Err(already_closed))
            .await;

        assert!(
            !logs_contain("Failed to close completed refinement task"),
            "already-closed close should not emit a warning"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn close_refinement_task_other_invalid_transition_emits_warning() {
        let actor = actor_for_test().await;

        let other = djinn_db::Error::InvalidTransition(
            "release is only valid from in_progress".to_owned(),
        );
        actor
            .close_refinement_task_for_test("task/xyz", "other transition", Err(other))
            .await;

        assert!(
            logs_contain("Failed to close completed refinement task"),
            "non-idempotent InvalidTransition must still warn"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn close_refinement_task_internal_error_emits_warning() {
        let actor = actor_for_test().await;

        let internal = djinn_db::Error::Internal("database connection lost".to_owned());
        actor
            .close_refinement_task_for_test("task/123", "internal error", Err(internal))
            .await;

        assert!(
            logs_contain("Failed to close completed refinement task"),
            "internal/repository errors must still warn"
        );
    }

    // ---- test helpers ----

    async fn actor_for_test() -> CoordinatorActor {
        let db = djinn_db::Database::open_in_memory().expect("open in-memory db");
        let (events_tx, _) = tokio::sync::broadcast::channel(16);
        CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: events_tx.subscribe(),
            cancel: tokio_util::sync::CancellationToken::new(),
            tick: tokio::time::interval(crate::types::STUCK_INTERVAL),
            db: db.clone(),
            events_tx: events_tx.clone(),
            pool: djinn_slot::SlotPoolHandle::spawn(
                crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
                CancellationToken::new(),
                djinn_slot::SlotPoolConfig {
                    models: vec![],
                    role_priorities: std::collections::HashMap::new(),
                },
            ),
            catalog: djinn_provider::catalog::CatalogService::new(),
            health: djinn_provider::catalog::health::HealthTracker::default(),
            role_registry: Arc::new(crate::roles::RoleRegistry::new()),
            lsp: djinn_lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx: tokio::sync::watch::channel(crate::SharedCoordinatorState {
                dispatched: 0,
                recovered: 0,
                epic_throughput: std::collections::HashMap::new(),
                pr_errors: std::collections::HashMap::new(),
                rate_limited_until: None,
            })
            .0,
            dispatch_limit: 50,
            model_priorities: std::collections::HashMap::new(),
            pr_errors: std::collections::HashMap::new(),
            last_dispatched: std::collections::HashMap::new(),
            inflight_dispatches: std::collections::HashMap::new(),
            provisional_admissions: std::collections::HashMap::new(),
            dispatch_cooldowns: std::collections::HashMap::new(),
            dispatch_failure_streak: std::collections::HashMap::new(),
            background_work_tracker: crate::types::BackgroundWorkTracker::default(),
            auto_merge_tracker: crate::types::AutoMergeTracker::default(),
            consolidation_runner: Arc::new(
                crate::consolidation::DbConsolidationRunner::new(db.clone()),
            ),
            last_stale_sweep: std::time::Instant::now(),
            last_auto_dispatch_sweep: std::time::Instant::now(),
            last_proposal_review_sweep: std::time::Instant::now(),
            last_graph_refresh: std::time::Instant::now(),
            graph_warmer: None,
            mirror: None,
            runtime_ops: None,
            rpc_registry: None,
            prune_tick_counter: 0,
            throughput_events: std::collections::HashMap::new(),
            escalation_counts: std::collections::HashMap::new(),
            pr_status_cache: std::collections::HashMap::new(),
            pr_draft_first_seen: std::collections::HashMap::new(),
            review_stuck_sha_first_seen: std::collections::HashMap::new(),
            merge_fail_count: std::collections::HashMap::new(),
            auto_approve_attempted: std::collections::HashMap::new(),
            delegated_to_github: std::collections::HashMap::new(),
            conversations_resolved: std::collections::HashMap::new(),
            handled_dequeues: std::collections::HashMap::new(),
            stall_killed: std::collections::HashSet::new(),
            stall_progress_watermark: std::collections::HashMap::new(),
            stall_cancel_streak: std::collections::HashMap::new(),
            provider_failure_streak: std::collections::HashMap::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            pr_cleanup_config: crate::types::PrCleanupConfig::default(),
            worker_lifecycle_config: crate::types::WorkerLifecycleConfig::default(),
            active_refinements: std::collections::HashMap::new(),
            refinement_sessions: std::collections::HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    fn logs_contain(needle: &str) -> bool {
        tracing_test::logs_with_scope_contain(None, needle)
    }
}


