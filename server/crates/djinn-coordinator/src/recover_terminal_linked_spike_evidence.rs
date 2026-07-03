use super::actor::CoordinatorActor;
use djinn_db::ProposalRepository;
use djinn_db::repositories::proposal::{
    TerminalLinkedEvidenceSpikeOutcome, evidence_spike_task_is_terminal,
};

impl CoordinatorActor {
    /// Startup recovery pass for terminal linked evidence spikes that may have
    /// closed while the coordinator was down. Delegates lifecycle classification,
    /// findings validation, and idempotency to the repository primitive.
    ///
    /// When the spike closed with valid findings (`EvidenceReceived`) or the
    /// lifecycle was already recorded by a prior pass (`AlreadyRecorded`), the
    /// recovery clears the durable evidence link/claim and resumes the in-memory
    /// refinement loop so the next Advocate can dispatch. For failed spikes the
    /// proposal remains blocked — no link clearing or automatic resume.
    pub(super) async fn recover_terminal_linked_spike_evidence(&mut self) {
        let repo = ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let candidates = match repo.list_linked_evidence_spike_recovery_candidates().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: failed to query linked evidence spike recovery candidates"
                );
                return;
            }
        };
        if candidates.is_empty() {
            return;
        }
        tracing::info!(
            count = candidates.len(),
            "CoordinatorActor: recovering linked evidence spike lifecycle state at startup"
        );
        for candidate in candidates {
            let proposal_id = &candidate.proposal_id;
            let spike_task_id = &candidate.linked_spike_task_id;
            let task_status = &candidate.linked_spike_task_status;
            let close_reason = candidate.linked_spike_task_close_reason.as_deref();
            if !evidence_spike_task_is_terminal(task_status) {
                tracing::debug!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    status = %task_status,
                    outcome = "NotTerminal",
                    "CoordinatorActor: recovery skipped; linked evidence spike is not terminal"
                );
                continue;
            }
            match repo
                .persist_terminal_linked_spike_evidence_lifecycle(
                    proposal_id,
                    spike_task_id,
                    task_status,
                    close_reason,
                )
                .await
            {
                Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived) => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        outcome = "EvidenceReceived",
                        "CoordinatorActor: recorded linked evidence spike receipt"
                    );
                    // Clear the durable evidence block and resume the
                    // in-memory refinement loop (if one exists). The resume
                    // helper is idempotent — clearing an already-cleared link
                    // is safe, and dispatch only proceeds when the lifecycle
                    // state and loop phase are correct.
                    self.resume_refinement_after_evidence_received(proposal_id, spike_task_id)
                        .await;
                }
                Ok(TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded { event_kind }) => {
                    tracing::debug!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        event_kind = %event_kind,
                        outcome = "AlreadyRecorded",
                        "CoordinatorActor: linked evidence spike terminal lifecycle already recorded"
                    );
                    // The lifecycle event was already recorded (e.g. by the
                    // event-driven path during startup). Still call resume
                    // so that the link/claim is cleared if a prior pass
                    // crashed between recording the lifecycle and clearing,
                    // and the in-memory loop phase is advanced.  The resume
                    // helper is idempotent: link clearing is an unconditional
                    // NULL UPDATE, loop phase update is idempotent, and
                    // dispatch only proceeds when the loop phase permits it.
                    self.resume_refinement_after_evidence_received(proposal_id, spike_task_id)
                        .await;
                }
                Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed { reason }) => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        reason = %reason,
                        outcome = "EvidenceFailed",
                        "CoordinatorActor: recorded linked evidence spike failure"
                    );
                    // Evidence failed — the proposal remains blocked. No
                    // link clearing or automatic resume.
                }
                Ok(TerminalLinkedEvidenceSpikeOutcome::NotLinked) => tracing::debug!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    outcome = "NotLinked",
                    "CoordinatorActor: linked evidence spike lifecycle skipped; proposal no longer linked"
                ),
                Ok(TerminalLinkedEvidenceSpikeOutcome::NotTerminal) => tracing::debug!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    status = %task_status,
                    outcome = "NotTerminal",
                    "CoordinatorActor: linked evidence spike lifecycle skipped; task is not terminal"
                ),
                Err(e) => tracing::warn!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    error = %e,
                    "CoordinatorActor: failed to persist linked evidence spike terminal lifecycle"
                ),
            }
        }
    }
}
