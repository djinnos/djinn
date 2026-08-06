//! Exact-run startup recovery.
//!
//! The coordinator registry is only a disposable projection. Startup therefore
//! enumerates durable nonterminal runs, observes each exact run in a
//! repeatable-read snapshot, and uses the shared liveness evaluator as the only
//! authority for rehydration or stale reaping. Proposal lifecycle rows, task
//! descriptions, and timestamps are deliberately not consulted here.

use djinn_core::refinement_liveness::{
    RefinementIntentState, RefinementLivenessResult, RefinementParkKind,
    RefinementPhase as DurablePhase, RefinementStopReason, evaluate_refinement_liveness,
};
use djinn_db::{ProposalRepository, TerminalRefinementRunRequest};

use super::actor::CoordinatorActor;
use super::refinement::{RefinementLoopState, RefinementPhase};

const HEARTBEAT_GRACE_MILLIS: i64 = 60_000;

impl CoordinatorActor {
    /// Rebuild the run-keyed projection from exact durable run snapshots.
    ///
    /// This path is read-only for live runs: replaying it, or rebuilding after
    /// the projection has been cleared, cannot create lifecycle, task, or intent
    /// rows. A stale run is the sole exception and is terminalized with its exact
    /// `(run_id, generation)` CAS fence.
    pub(super) async fn recover_interrupted_refinements(&mut self) {
        let proposal_repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let runs = match proposal_repo.load_recoverable_refinement_runs().await {
            Ok(runs) => runs,
            Err(error) => {
                tracing::warn!(%error, "failed to enumerate durable refinement runs for startup recovery");
                return;
            }
        };

        for run in runs {
            let exact = match proposal_repo
                .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                    run_id: run.run_id.clone(),
                    heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
                })
                .await
            {
                Ok(Some(exact)) if exact.generation == run.generation => exact,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(run_id = %run.run_id, %error, "failed exact refinement recovery observation");
                    continue;
                }
            };

            // Do not trust a repository-local cached result: startup has one
            // shared, pure liveness authority evaluated at the DB observation.
            match evaluate_refinement_liveness(&exact.snapshot, exact.observed_at) {
                RefinementLivenessResult::Terminal { .. } => continue,
                RefinementLivenessResult::Stale { reason } => {
                    self.try_reap_exact_stale_run(
                        &exact.proposal_id,
                        &exact.snapshot.run.run_id,
                        exact.generation,
                        reason,
                    )
                    .await;
                    continue;
                }
                RefinementLivenessResult::Live { .. } => {}
            }

            if self.active_refinements.contains_key(&run.run_id) {
                continue;
            }
            let proposal = match proposal_repo.get(&exact.proposal_id).await {
                Ok(Some(proposal)) => proposal,
                Ok(None) | Err(_) => continue,
            };
            let captured_snapshot_seq = proposal_repo
                .refinement_run_captured_snapshot_seq(&run.run_id)
                .await
                .unwrap_or_default();
            // Rebuilt projections seed `pending_blocking_verdict` false. A run
            // recovered while parked at `AdversaryAttack` after a needs-work
            // verdict would then send its next dry pass back to the Judge and
            // re-strand exactly the verdict the restart interrupted. The durable
            // authority is the debate trail.
            let pending_blocking_verdict =
                CoordinatorActor::outstanding_blocking_verdict(&proposal_repo, &exact.proposal_id)
                    .await;
            let mut state =
                RefinementLoopState::new(&exact.proposal_id, proposal.latest_revision_seq)
                    .with_run_identity(run.run_id.clone(), exact.generation)
                    .with_recovered_snapshot_seq(captured_snapshot_seq)
                    .with_pending_blocking_verdict(pending_blocking_verdict)
                    .with_attributed_user(proposal.refinement_owner_user_id.clone());

            if let Some(park) = exact.snapshot.park.as_ref() {
                state.phase = match park.kind {
                    RefinementParkKind::AwaitingReview => RefinementPhase::AwaitingHumanReview,
                    RefinementParkKind::AwaitingEvidence => RefinementPhase::AwaitingEvidence,
                };
            } else if let Some(intent) = exact.snapshot.intents.iter().find(|intent| {
                intent.run_id == run.run_id
                    && matches!(
                        intent.state,
                        RefinementIntentState::Pending
                            | RefinementIntentState::Claimed
                            | RefinementIntentState::Materialized
                    )
            }) {
                state.phase = match intent.phase {
                    DurablePhase::AdversaryAttack => RefinementPhase::AdversaryAttack,
                    DurablePhase::AdvocateRevision => RefinementPhase::AdvocateRevision,
                    DurablePhase::JudgeAdjudication => RefinementPhase::JudgeAdjudication,
                };
                state.current_round = intent.round;
            } else {
                // A live task/session can outlast a materialized intent. Its
                // phase/role identity stays durable in the task/intent ledger;
                // there is no safe legacy reconstruction to perform here.
                continue;
            }
            self.active_refinements.insert(run.run_id, state);
        }
    }

    /// Terminalize only the exact snapshot that the evaluator declared stale.
    async fn try_reap_exact_stale_run(
        &self,
        proposal_id: &str,
        run_id: &str,
        generation: i32,
        stale_reason: djinn_core::refinement_liveness::RefinementStaleReason,
    ) {
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let reason = RefinementStopReason::ReapedPhantom {
            prior_run_id: run_id.to_owned(),
            generation: generation as u64,
            evidence_summary: format!(
                "startup recovery evaluator classified exact snapshot stale: {stale_reason:?}"
            ),
        };
        match repo
            .terminal_refinement_run(TerminalRefinementRunRequest {
                run_id: run_id.to_owned(),
                generation,
                reason,
            })
            .await
        {
            Ok(true) | Ok(false) => {}
            Err(error) => {
                tracing::warn!(proposal_id, run_id, generation, %error, "exact stale refinement recovery terminal CAS failed")
            }
        }
    }
}
