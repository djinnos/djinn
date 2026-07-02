use super::actor::CoordinatorActor;
use djinn_db::ProposalRepository;
use djinn_db::repositories::proposal::{evidence_spike_task_is_terminal, TerminalLinkedEvidenceSpikeOutcome};

impl CoordinatorActor {
    /// Startup recovery pass for terminal linked evidence spikes that may have
    /// closed while the coordinator was down. Delegates lifecycle classification,
    /// findings validation, and idempotency to the repository primitive; does not
    /// clear evidence links or resume tribunal work.
    pub(super) async fn recover_terminal_linked_spike_evidence(&self) {
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
                Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceReceived) => tracing::info!(
                    proposal_id = %proposal_id,
                    spike_task_id = %spike_task_id,
                    outcome = "EvidenceReceived",
                    "CoordinatorActor: recorded linked evidence spike receipt"
                ),
                Ok(TerminalLinkedEvidenceSpikeOutcome::EvidenceFailed { reason }) => {
                    tracing::info!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        reason = %reason,
                        outcome = "EvidenceFailed",
                        "CoordinatorActor: recorded linked evidence spike failure"
                    )
                }
                Ok(TerminalLinkedEvidenceSpikeOutcome::AlreadyRecorded { event_kind }) => {
                    tracing::debug!(
                        proposal_id = %proposal_id,
                        spike_task_id = %spike_task_id,
                        event_kind = %event_kind,
                        outcome = "AlreadyRecorded",
                        "CoordinatorActor: linked evidence spike terminal lifecycle already recorded"
                    );
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
