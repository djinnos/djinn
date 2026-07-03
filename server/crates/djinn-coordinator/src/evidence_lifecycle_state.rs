// Evidence lifecycle state derivation for refinement proposals.
//
// The coordinator uses [`EvidenceLifecycleState`] to decide whether the
// refinement dispatcher may create further tribunal tasks.  The state is
// derived entirely from persisted proposal fields, lifecycle events, task
// status, and dispatch-pause data — there are no in-memory counters.
//
// Precedence (highest → lowest):
//   Terminal → PausedOrFrozen → AwaitingEvidence → EvidenceFailed →
//   EvidenceReady → Active
//
// `PausedOrFrozen` takes precedence over both `Active` and `EvidenceReady`
// dispatch decisions: even when evidence findings are available, an
// administrative dispatch pause or proposal build freeze prevents automatic
// resume.

use djinn_core::models::Proposal;

use crate::actor::CoordinatorActor;

// ── Terminal proposal statuses ──────────────────────────────────────────────

/// Proposal statuses that mean the proposal has reached a final state and
/// refinement should never dispatch again.
const TERMINAL_PROPOSAL_STATUSES: &[&str] = &["done", "rejected", "archived", "superseded"];

// ── Evidence lifecycle state ────────────────────────────────────────────────

/// Durable evidence lifecycle state for a refinement proposal.
///
/// Derived from persisted data, not from in-memory bookkeeping.  The
/// dispatcher consults this to decide whether to skip, park, or resume
/// tribunal phase dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EvidenceLifecycleState {
    /// Refinement is proceeding normally — no evidence demand is active.
    Active,
    /// An evidence spike is linked and still running.  Refinement is parked.
    AwaitingEvidence,
    /// The evidence spike closed with valid findings and refinement can
    /// resume (once dispatch is not paused/frozen).
    EvidenceReady,
    /// The evidence spike failed (cancelled, errored, force-closed, or
    /// closed without valid findings).  Refinement remains blocked.
    EvidenceFailed,
    /// Administrative gate: a manual dispatch pause or proposal build freeze
    /// prevents any dispatch, regardless of evidence state.  Has precedence
    /// over `Active` and `EvidenceReady`.
    PausedOrFrozen,
    /// The proposal has reached a terminal status.  Refinement is finished.
    Terminal,
}

// ── Snapshot input ──────────────────────────────────────────────────────────

/// Read-only snapshot of persisted data used to derive the evidence lifecycle
/// state.  Every field corresponds to a DB column, lifecycle event, or
/// pre-resolved helper that the coordinator already knows how to query.
///
/// The struct is intentionally flat and `Clone`-friendly so tests can
/// construct fixtures without touching any repository or database.
#[derive(Debug, Clone)]
pub(super) struct EvidenceLifecycleSnapshot {
    /// `proposals.status` — e.g. `"in_review"`, `"done"`, `"archived"`.
    pub proposal_status: String,
    /// `proposals.build_frozen` — when `true`, dispatch is frozen.
    pub build_frozen: bool,
    /// Pre-resolved administrative dispatch-pause flag (global or
    /// project-scoped).  The coordinator calls
    /// `refinement_dispatch_paused()` before building the snapshot.
    pub dispatch_paused: bool,
    /// `proposals.linked_spike_task_id` — `None` when no evidence demand
    /// is linked.
    pub linked_spike_task_id: Option<String>,
    /// `proposals.needs_evidence_claim` — the structured claim JSON.
    /// `None` when no evidence demand is linked.
    /// Kept in the snapshot for downstream sibling tasks (spike completion
    /// processing, resume-with-findings) that need to read the claim.
    #[allow(dead_code)]
    pub needs_evidence_claim: Option<String>,
    /// Status of the linked spike task row (`"open"`, `"in_progress"`,
    /// `"closed"`, etc.).  `None` when no spike is linked or the task
    /// row was hard-deleted.
    pub spike_task_status: Option<String>,
    /// `close_reason` of the linked spike task when it is closed.
    /// `None` when the spike is still open or no spike is linked.
    /// Kept in the snapshot for downstream sibling tasks that need to
    /// distinguish failure modes (e.g. force-closed vs completed).
    #[allow(dead_code)]
    pub spike_task_close_reason: Option<String>,
    /// Whether a `refinement_evidence_received` lifecycle event exists for
    /// the most recent evidence cycle (after the latest
    /// `refinement_awaiting_evidence_started`).
    pub has_evidence_received_event: bool,
    /// Whether a `refinement_evidence_failed` lifecycle event exists for
    /// the most recent evidence cycle.
    pub has_evidence_failed_event: bool,
}

// ── Pure derivation function ────────────────────────────────────────────────

/// Derive the evidence lifecycle state from a persisted-data snapshot.
///
/// This is a **pure function** — it reads no databases, holds no locks, and
/// performs no I/O.  All inputs must be resolved before calling.
pub(super) fn derive_evidence_lifecycle_state(
    snapshot: &EvidenceLifecycleSnapshot,
) -> EvidenceLifecycleState {
    // 1. Terminal — proposal has reached a final status.
    if TERMINAL_PROPOSAL_STATUSES.contains(&snapshot.proposal_status.as_str()) {
        return EvidenceLifecycleState::Terminal;
    }

    // 2. PausedOrFrozen — administrative gate.
    //    Takes precedence over Active and EvidenceReady so that dispatch
    //    never auto-resumes while a human gate is in place.
    if snapshot.dispatch_paused || snapshot.build_frozen {
        return EvidenceLifecycleState::PausedOrFrozen;
    }

    // 3. AwaitingEvidence — linked spike is still open (running).
    if snapshot.linked_spike_task_id.is_some() {
        // Determine whether the spike task row is open.
        let spike_is_open = match snapshot.spike_task_status.as_deref() {
            // Explicit closed status → spike is done.
            Some("closed") => false,
            // Missing task row (hard-deleted) → treat as closed.
            None => false,
            // Any other status (open, in_progress, …) → still running.
            Some(_) => true,
        };

        if spike_is_open {
            return EvidenceLifecycleState::AwaitingEvidence;
        }

        // Spike is linked but the task is closed.  Check lifecycle events
        // to distinguish EvidenceFailed from EvidenceReady.
        if snapshot.has_evidence_failed_event {
            return EvidenceLifecycleState::EvidenceFailed;
        }
        if snapshot.has_evidence_received_event {
            return EvidenceLifecycleState::EvidenceReady;
        }

        // Spike closed but no lifecycle event recorded yet — the
        // completion processor hasn't run.  Stay in AwaitingEvidence so
        // the re-drive path picks it up.
        return EvidenceLifecycleState::AwaitingEvidence;
    }

    // 4. No linked spike — evidence cycle may have already been processed.
    //    Check lifecycle events for the most recent cycle.
    if snapshot.has_evidence_failed_event {
        return EvidenceLifecycleState::EvidenceFailed;
    }
    if snapshot.has_evidence_received_event {
        return EvidenceLifecycleState::EvidenceReady;
    }

    // 5. Active — normal refinement, no evidence demand.
    EvidenceLifecycleState::Active
}

// ── Coordinator integration ─────────────────────────────────────────────────

impl CoordinatorActor {
    /// Load a proposal by id for the evidence lifecycle state check.
    ///
    /// Returns `None` when the proposal does not exist or a DB error
    /// occurs — callers should fail closed (skip dispatch) on `None`.
    pub(super) async fn load_proposal_for_lifecycle(&self, proposal_id: &str) -> Option<Proposal> {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = djinn_db::ProposalRepository::new(self.db.clone(), event_bus);
        match proposal_repo.get(proposal_id).await {
            Ok(proposal) => proposal,
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    error = %e,
                    "Failed to load proposal for evidence lifecycle check"
                );
                None
            }
        }
    }

    /// Build an [`EvidenceLifecycleSnapshot`] from persisted DB data for
    /// the given proposal and derive the [`EvidenceLifecycleState`].
    ///
    /// Reads the proposal, dispatch-pause state, and (when a spike is
    /// linked) the spike task status and evidence lifecycle events.
    /// Returns the derived state.
    pub(super) async fn derive_proposal_evidence_lifecycle(
        &self,
        proposal: &Proposal,
    ) -> EvidenceLifecycleState {
        let snapshot = self.build_evidence_lifecycle_snapshot(proposal).await;
        derive_evidence_lifecycle_state(&snapshot)
    }

    /// Build the snapshot of persisted data needed for evidence lifecycle
    /// state derivation.
    pub(super) async fn build_evidence_lifecycle_snapshot(
        &self,
        proposal: &Proposal,
    ) -> EvidenceLifecycleSnapshot {
        let dispatch_paused = self.refinement_dispatch_paused(&proposal.id).await;

        let mut spike_task_status: Option<String> = None;
        let mut spike_task_close_reason: Option<String> = None;
        let mut has_evidence_received_event = false;
        let mut has_evidence_failed_event = false;

        if proposal.linked_spike_task_id.is_some() {
            // Look up the spike task row to determine open/closed status.
            let event_bus = crate::events::event_bus_for(&self.events_tx);
            let task_repo = djinn_db::TaskRepository::new(self.db.clone(), event_bus);

            if let Some(ref spike_id) = proposal.linked_spike_task_id {
                match task_repo.get(spike_id).await {
                    Ok(Some(task)) => {
                        spike_task_status = Some(task.status.clone());
                        spike_task_close_reason = task.close_reason.clone();
                    }
                    Ok(None) => {
                        // Task was hard-deleted — leave status as None.
                    }
                    Err(e) => {
                        tracing::warn!(
                            proposal_id = %proposal.id,
                            spike_task_id = %spike_id,
                            error = %e,
                            "Failed to read spike task for evidence lifecycle; \
                             assuming open"
                        );
                        // Fail open: assume the spike is still running so
                        // we don't accidentally resume.
                        spike_task_status = Some("open".to_string());
                    }
                }
            }
        }

        // Check for evidence lifecycle events even after the link/claim has
        // been cleared: a successful in-process receipt clears the durable
        // evidence block before the next Advocate dispatch, but the latest
        // `refinement_evidence_received` event must still derive EvidenceReady.
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let proposal_repo = djinn_db::ProposalRepository::new(self.db.clone(), event_bus);

        match proposal_repo.revisions(&proposal.id).await {
            Ok(revisions) => {
                // Walk revisions in reverse to find the latest evidence
                // lifecycle event.
                for rev in revisions.iter().rev() {
                    match rev.event_kind.as_str() {
                        "refinement_evidence_received" => {
                            has_evidence_received_event = true;
                            break;
                        }
                        "refinement_evidence_failed" => {
                            has_evidence_failed_event = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal.id,
                    error = %e,
                    "Failed to read revisions for evidence lifecycle; \
                     assuming no evidence events"
                );
            }
        }

        EvidenceLifecycleSnapshot {
            proposal_status: proposal.status.clone(),
            build_frozen: proposal.build_frozen,
            dispatch_paused,
            linked_spike_task_id: proposal.linked_spike_task_id.clone(),
            needs_evidence_claim: proposal.needs_evidence_claim.clone(),
            spike_task_status,
            spike_task_close_reason,
            has_evidence_received_event,
            has_evidence_failed_event,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a snapshot with sensible defaults.  Individual fields
    /// can be overridden for each test case.
    fn base_snapshot() -> EvidenceLifecycleSnapshot {
        EvidenceLifecycleSnapshot {
            proposal_status: "in_review".to_string(),
            build_frozen: false,
            dispatch_paused: false,
            linked_spike_task_id: None,
            needs_evidence_claim: None,
            spike_task_status: None,
            spike_task_close_reason: None,
            has_evidence_received_event: false,
            has_evidence_failed_event: false,
        }
    }

    // ── Active ───────────────────────────────────────────────────────

    #[test]
    fn active_when_no_evidence_demand_and_in_review() {
        let snap = base_snapshot();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Active
        );
    }

    #[test]
    fn active_when_no_evidence_demand_and_building() {
        let mut snap = base_snapshot();
        snap.proposal_status = "building".to_string();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Active
        );
    }

    // ── Terminal ─────────────────────────────────────────────────────

    #[test]
    fn terminal_when_status_done() {
        let mut snap = base_snapshot();
        snap.proposal_status = "done".to_string();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn terminal_when_status_rejected() {
        let mut snap = base_snapshot();
        snap.proposal_status = "rejected".to_string();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn terminal_when_status_archived() {
        let mut snap = base_snapshot();
        snap.proposal_status = "archived".to_string();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn terminal_when_status_superseded() {
        let mut snap = base_snapshot();
        snap.proposal_status = "superseded".to_string();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn terminal_takes_precedence_over_dispatch_pause() {
        let mut snap = base_snapshot();
        snap.proposal_status = "done".to_string();
        snap.dispatch_paused = true;
        snap.build_frozen = true;
        snap.linked_spike_task_id = Some("spike-1".to_string());
        snap.spike_task_status = Some("open".to_string());
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn terminal_takes_precedence_over_evidence_ready() {
        let mut snap = base_snapshot();
        snap.proposal_status = "archived".to_string();
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    // ── PausedOrFrozen ───────────────────────────────────────────────

    #[test]
    fn paused_or_frozen_when_dispatch_paused() {
        let mut snap = base_snapshot();
        snap.dispatch_paused = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );
    }

    #[test]
    fn paused_or_frozen_when_build_frozen() {
        let mut snap = base_snapshot();
        snap.build_frozen = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );
    }

    #[test]
    fn paused_or_frozen_takes_precedence_over_active() {
        let mut snap = base_snapshot();
        snap.dispatch_paused = true;
        // No evidence demand → would be Active without the pause.
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );
    }

    #[test]
    fn paused_or_frozen_takes_precedence_over_evidence_ready() {
        let mut snap = base_snapshot();
        snap.build_frozen = true;
        snap.has_evidence_received_event = true;
        // Would be EvidenceReady without the freeze.
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );
    }

    #[test]
    fn paused_or_frozen_does_not_hide_evidence_failed() {
        // PausedOrFrozen takes precedence over EvidenceFailed for
        // dispatch decisions, but the derivation still checks
        // PausedOrFrozen first.  This is correct: the dispatcher sees
        // "paused" and does not dispatch; the evidence-failed state is
        // visible once the pause clears.
        let mut snap = base_snapshot();
        snap.dispatch_paused = true;
        snap.has_evidence_failed_event = true;
        // PausedOrFrozen wins over EvidenceFailed.
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );
    }

    // ── AwaitingEvidence ─────────────────────────────────────────────

    #[test]
    fn awaiting_evidence_when_spike_linked_and_open() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-1".to_string());
        snap.needs_evidence_claim = Some(r#"{"question":"Is X feasible?"}"#.to_string());
        snap.spike_task_status = Some("open".to_string());
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );
    }

    #[test]
    fn awaiting_evidence_when_spike_linked_and_in_progress() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-2".to_string());
        snap.needs_evidence_claim = Some(r#"{"question":"Is Y feasible?"}"#.to_string());
        snap.spike_task_status = Some("in_progress".to_string());
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );
    }

    #[test]
    fn awaiting_evidence_when_spike_linked_but_task_deleted() {
        // Spike task was hard-deleted.  We stay in AwaitingEvidence so
        // the re-drive path can detect the orphan and escalate.
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-deleted".to_string());
        snap.needs_evidence_claim = Some(r#"{"question":"Is Z feasible?"}"#.to_string());
        snap.spike_task_status = None; // task row gone
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );
    }

    #[test]
    fn awaiting_evidence_when_spike_closed_but_no_lifecycle_event() {
        // Spike closed but the completion processor hasn't written a
        // lifecycle event yet.  Stay parked so re-drive picks it up.
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-3".to_string());
        snap.needs_evidence_claim = Some(r#"{"question":"Is W feasible?"}"#.to_string());
        snap.spike_task_status = Some("closed".to_string());
        snap.spike_task_close_reason = Some("completed".to_string());
        // No lifecycle events yet.
        snap.has_evidence_received_event = false;
        snap.has_evidence_failed_event = false;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );
    }

    // ── EvidenceFailed ───────────────────────────────────────────────

    #[test]
    fn evidence_failed_when_lifecycle_event_present_with_linked_closed_spike() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-4".to_string());
        snap.spike_task_status = Some("closed".to_string());
        snap.spike_task_close_reason = Some("force_closed".to_string());
        snap.has_evidence_failed_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceFailed
        );
    }

    #[test]
    fn evidence_failed_when_lifecycle_event_present_and_link_cleared() {
        // The link has been cleared after the failure was recorded.
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = None;
        snap.has_evidence_failed_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceFailed
        );
    }

    #[test]
    fn evidence_failed_distinguishable_from_active() {
        let active = base_snapshot();
        let mut failed = base_snapshot();
        failed.has_evidence_failed_event = true;

        assert_ne!(
            derive_evidence_lifecycle_state(&active),
            derive_evidence_lifecycle_state(&failed)
        );
        assert_eq!(
            derive_evidence_lifecycle_state(&active),
            EvidenceLifecycleState::Active
        );
        assert_eq!(
            derive_evidence_lifecycle_state(&failed),
            EvidenceLifecycleState::EvidenceFailed
        );
    }

    #[test]
    fn evidence_failed_distinguishable_from_terminal() {
        let mut terminal = base_snapshot();
        terminal.proposal_status = "done".to_string();
        terminal.has_evidence_failed_event = true;

        let mut failed = base_snapshot();
        failed.has_evidence_failed_event = true;

        // Terminal wins over EvidenceFailed.
        assert_eq!(
            derive_evidence_lifecycle_state(&terminal),
            EvidenceLifecycleState::Terminal
        );
        assert_eq!(
            derive_evidence_lifecycle_state(&failed),
            EvidenceLifecycleState::EvidenceFailed
        );
    }

    // ── EvidenceReady ────────────────────────────────────────────────

    #[test]
    fn evidence_ready_when_received_event_and_link_cleared() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = None;
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceReady
        );
    }

    #[test]
    fn evidence_ready_when_received_event_and_spike_closed() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-task-5".to_string());
        snap.spike_task_status = Some("closed".to_string());
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceReady
        );
    }

    #[test]
    fn evidence_failed_takes_precedence_over_evidence_ready() {
        // Both events present (e.g. a retry cycle).  The failed event
        // should take precedence because the latest event wins.
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = None;
        snap.has_evidence_received_event = true;
        snap.has_evidence_failed_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceFailed
        );
    }

    // ── Precedence matrix ────────────────────────────────────────────

    #[test]
    fn full_precedence_matrix() {
        // Verify the precedence order: Terminal > PausedOrFrozen >
        // AwaitingEvidence > EvidenceFailed > EvidenceReady > Active.

        // Terminal overrides everything.
        let mut snap = base_snapshot();
        snap.proposal_status = "done".to_string();
        snap.dispatch_paused = true;
        snap.build_frozen = true;
        snap.linked_spike_task_id = Some("s".to_string());
        snap.spike_task_status = Some("open".to_string());
        snap.has_evidence_failed_event = true;
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );

        // PausedOrFrozen overrides evidence states.
        let mut snap = base_snapshot();
        snap.proposal_status = "in_review".to_string();
        snap.dispatch_paused = true;
        snap.linked_spike_task_id = Some("s".to_string());
        snap.spike_task_status = Some("open".to_string());
        snap.has_evidence_failed_event = true;
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::PausedOrFrozen
        );

        // AwaitingEvidence overrides EvidenceFailed/Ready.
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("s".to_string());
        snap.spike_task_status = Some("open".to_string());
        snap.has_evidence_failed_event = true;
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );

        // EvidenceFailed overrides EvidenceReady.
        let mut snap = base_snapshot();
        snap.has_evidence_failed_event = true;
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceFailed
        );

        // EvidenceReady overrides Active.
        let mut snap = base_snapshot();
        snap.has_evidence_received_event = true;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::EvidenceReady
        );

        // Active is the default.
        let snap = base_snapshot();
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Active
        );
    }

    // ── Existing terminal proposal ───────────────────────────────────

    #[test]
    fn existing_terminal_proposal_reports_terminal() {
        // Simulate a proposal that has been through evidence, received
        // findings, and eventually graduated to "done".
        let mut snap = base_snapshot();
        snap.proposal_status = "done".to_string();
        snap.linked_spike_task_id = None;
        snap.needs_evidence_claim = None;
        snap.has_evidence_received_event = true;
        snap.has_evidence_failed_event = false;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    #[test]
    fn superseded_proposal_is_terminal_even_with_active_spike() {
        let mut snap = base_snapshot();
        snap.proposal_status = "superseded".to_string();
        snap.linked_spike_task_id = Some("spike-orphan".to_string());
        snap.spike_task_status = Some("open".to_string());
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Terminal
        );
    }

    // ── Edge cases ───────────────────────────────────────────────────

    #[test]
    fn needs_evidence_claim_without_linked_spike_is_active() {
        // Claim is set but link is cleared (shouldn't normally happen,
        // but the derivation should be safe).
        let mut snap = base_snapshot();
        snap.needs_evidence_claim = Some(r#"{"question":"stale claim"}"#.to_string());
        snap.linked_spike_task_id = None;
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::Active
        );
    }

    #[test]
    fn linked_spike_with_unknown_status_treated_as_open() {
        let mut snap = base_snapshot();
        snap.linked_spike_task_id = Some("spike-unknown".to_string());
        snap.spike_task_status = Some("in_review".to_string());
        assert_eq!(
            derive_evidence_lifecycle_state(&snap),
            EvidenceLifecycleState::AwaitingEvidence
        );
    }
}
