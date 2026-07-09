// Startup recovery for refinement loops interrupted by a coordinator restart.
//
// Split out of `refinement_outcome.rs` to keep that file under the
// size-guard byte threshold. Owns the restart reconciliation pass: dangling
// refinements that legitimately converged and parked awaiting the human's
// accept/reject review are restored from durable lifecycle data; refinements
// genuinely lost mid-tribunal are stamped `refinement_stop`/`Interrupted`.

use djinn_db::ProposalRepository;

use super::actor::CoordinatorActor;
use super::refinement::{RefinementLoopState, StopReason};
use super::refinement_outcome::entry_in_current_run;

impl CoordinatorActor {
    /// Startup reconciliation for refinements interrupted by a restart.
    ///
    /// Runs once before the message loop. Every DB-dangling refinement (more
    /// `refinement_start` than `refinement_stop` lifecycle rows) either:
    ///
    /// - is restored to its parked `AwaitingHumanReview` state — when the
    ///   tribunal had legitimately converged and written a durable
    ///   `refinement_awaiting_review` lifecycle row, and nobody has edited the
    ///   spec since (the head revision still equals the parked refined seq). A
    ///   converged park is a valid, human-actionable result; stamping it
    ///   `Interrupted` on every deploy would silently destroy the judge's work
    ///   and force a full re-run; or
    /// - is stamped `refinement_stop` with [`StopReason::Interrupted`] — for a
    ///   refinement genuinely lost mid-tribunal (no awaiting-review park after
    ///   the latest start), leaving the proposal restartable.
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
            if self.try_restore_awaiting_review(&proposal_id).await {
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

    /// Attempt to restore a dangling refinement that was parked awaiting the
    /// human's accept/reject review from durable lifecycle data. Returns `true`
    /// when the parked state was rebuilt into `self.active_refinements` (so the
    /// caller must NOT stamp it interrupted), `false` otherwise.
    ///
    /// Falls back to `false` (interrupted stamp) when the proposal is not parked
    /// awaiting review, or when the live spec has moved on since the park (head
    /// `latest_revision_seq` no longer equals the parked `refined_revision_seq`),
    /// because a human/agent edit invalidates the converged result.
    async fn try_restore_awaiting_review(&mut self, proposal_id: &str) -> bool {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = ProposalRepository::new(self.db.clone(), event_bus);

        let park = match proposal_repo.parked_awaiting_review(proposal_id).await {
            Ok(Some(park)) => park,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read awaiting-review park during startup recovery; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        let Some(refined_revision_seq) = park.refined_revision_seq else {
            tracing::warn!(
                proposal_id = %proposal_id,
                "Awaiting-review park missing refined_revision_seq; \
                 falling back to interrupted stamp"
            );
            return false;
        };

        let proposal = match proposal_repo.get(proposal_id).await {
            Ok(Some(p)) => p,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to load proposal during awaiting-review restore; \
                     falling back to interrupted stamp"
                );
                return false;
            }
        };

        // If the spec moved on since the park, the converged result no longer
        // matches the live head — treat as interrupted rather than restoring a
        // stale review.
        if proposal.latest_revision_seq != refined_revision_seq {
            tracing::info!(
                proposal_id = %proposal_id,
                parked_seq = refined_revision_seq,
                head_seq = proposal.latest_revision_seq,
                "Awaiting-review park is stale (spec advanced); stamping interrupted"
            );
            return false;
        }

        // The snapshot seq is the pre-refinement baseline (revert target on
        // reject). Fall back to the refined seq if the park predates snapshot
        // recording — a reject then reverts to the refined head, a no-op.
        let snapshot_revision_seq = park.snapshot_revision_seq.unwrap_or(refined_revision_seq);

        // Restore the round counter from the debate trail so a reject-with-
        // feedback re-loop resumes at the correct round. Scoped to the current
        // run (see `entry_in_current_run`) so a prior interrupted run's
        // round-numbered entries don't inflate the restored round. Best-effort:
        // defaults to round 1 when the trail is empty or unreadable.
        let run_start = self.latest_refinement_run_start(proposal_id).await;
        let current_round = match proposal_repo.debate_trail(proposal_id).await {
            Ok(entries) => entries
                .iter()
                .filter(|e| entry_in_current_run(e, run_start.as_deref()))
                .map(|e| e.round)
                .max()
                .unwrap_or(1),
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to read debate trail for round restore; defaulting to round 1"
                );
                1
            }
        };

        let stop_reason = park.stop_reason.as_deref().and_then(StopReason::from_tag);

        let state = RefinementLoopState::restored_awaiting_review(
            proposal_id,
            refined_revision_seq,
            snapshot_revision_seq,
            current_round,
            proposal.author_user_id.clone(),
            stop_reason,
        );
        self.active_refinements
            .insert(proposal_id.to_string(), state);

        tracing::info!(
            proposal_id = %proposal_id,
            refined_revision_seq,
            snapshot_revision_seq,
            current_round,
            "Restored refinement parked awaiting human review across restart"
        );
        true
    }
}
