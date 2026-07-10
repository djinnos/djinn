// Objection collection for the adversary pass of the refinement tribunal loop.
//
// Split from `refinement_outcome.rs` (which drives the outcome-processing loop)
// to keep both files under the size-guard threshold. The run-scoping predicate
// lives in `refinement_outcome::entry_in_current_run`.

use djinn_core::models::ProposalDebateTrail;

use super::refinement::ObjectionRecord;
use super::refinement_outcome::entry_in_current_run;

/// Collect the adversary objections that count toward `round` of the current
/// refinement run.
///
/// The current-run, current-round objection set is the normal source. When that
/// set is empty AND this is round 1 (a fresh or restarted run's opener), fall
/// back to the prior run's *unresolved blocking* objections — entries the dead
/// run's adversary filed but whose Judge never resolved. Only the Judge resolves
/// entries, so a dead run's objections stay `resolved_at == None`. The adversary
/// agent reads the full unscoped trail and is told not to re-file already-filed
/// objections, so without this fallback the coordinator counts zero objections,
/// never dispatches the Advocate, and the restarted run starves at revision 1.
///
/// Only round 1 carries them over; rounds >= 2 keep strict current-run scoping
/// so repeat-objection escalation is not distorted by recounting old entries
/// every round. When `run_start` is `None` every entry is in-run, so the
/// carry-over set is empty and behavior is unchanged. Returns the objections and
/// whether the carry-over fallback engaged.
pub(super) fn collect_adversary_round_objections(
    entries: &[ProposalDebateTrail],
    round: i32,
    run_start: Option<&str>,
) -> (Vec<ObjectionRecord>, bool) {
    let to_record = |e: &ProposalDebateTrail| ObjectionRecord {
        body: e.body.clone(),
        blocking: e.blocking,
        author_model: e.author_model.clone(),
        entry_id: Some(e.id.clone()),
    };

    let current_run: Vec<ObjectionRecord> = entries
        .iter()
        .filter(|e| {
            e.agent_role == "adversary"
                && e.kind == "objection"
                && e.round == round
                && entry_in_current_run(e, run_start)
        })
        .map(&to_record)
        .collect();

    if !current_run.is_empty() || round != 1 {
        return (current_run, false);
    }

    // Round 1 with an empty current-run set: carry the prior (interrupted) run's
    // standing objections — blocking, still unresolved, and NOT in the current
    // run — so the Advocate is dispatched to revise against them.
    let carried: Vec<ObjectionRecord> = entries
        .iter()
        .filter(|e| {
            e.agent_role == "adversary"
                && e.kind == "objection"
                && e.blocking
                && e.resolved_at.is_none()
                && !entry_in_current_run(e, run_start)
        })
        .map(&to_record)
        .collect();

    let carried_over = !carried.is_empty();
    (carried, carried_over)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refinement::{
        AdversaryPassOutcome, AdversaryPassResult, RefinementConfig, RefinementLoopState,
        RefinementPhase,
    };

    fn test_config() -> RefinementConfig {
        RefinementConfig::default()
    }

    /// Build an adversary objection debate-trail entry with an explicit
    /// `created_at`, `blocking`, and `resolved_at` so tests can reproduce a
    /// trail that spans an interrupted run and a fresh restart.
    fn objection_entry(
        round: i32,
        blocking: bool,
        resolved_at: Option<&str>,
        created_at: &str,
    ) -> ProposalDebateTrail {
        ProposalDebateTrail {
            id: format!("objection/{created_at}"),
            proposal_id: "p1".into(),
            kind: "objection".into(),
            body: format!("Missing section (created {created_at})"),
            blocking,
            agent_role: "adversary".into(),
            author_kind: "agent".into(),
            author_user_id: None,
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round,
            body_metadata: None,
            resolved_at: resolved_at.map(str::to_owned),
            resolved_by_user_id: None,
            reopened_at: None,
            reopened_by_user_id: None,
            created_at: created_at.into(),
            updated_at: created_at.into(),
        }
    }

    /// Interrupted-then-restarted refinement: the prior run's round-1 adversary
    /// filed blocking objections that its dead Judge never resolved. The fresh
    /// run's round-1 current-run set is empty (its adversary won't re-file), so
    /// the standing prior-run objections are carried over and the loop routes to
    /// the Advocate instead of starving.
    #[test]
    fn round1_empty_current_run_carries_prior_run_objections() {
        let entries = vec![
            // Prior (interrupted) run, round 1: unresolved blocking objections.
            objection_entry(1, true, None, "2026-07-09T10:00:00.000Z"),
            objection_entry(1, true, None, "2026-07-09T10:00:05.000Z"),
        ];
        // The fresh run started after the prior run's objections were filed.
        let run_start = Some("2026-07-09T12:00:00.000Z");

        let (objections, carried_over) = collect_adversary_round_objections(&entries, 1, run_start);
        assert!(carried_over, "must flag the carry-over fallback");
        assert_eq!(objections.len(), 2, "both standing objections carry over");

        // Feeding them to the state machine must route to the Advocate.
        let mut state = RefinementLoopState::with_config("p1", 1, test_config());
        let result = AdversaryPassResult {
            objections,
            explicit_dry: false,
        };
        let outcome = state.process_adversary_pass(&result);
        assert_eq!(outcome, AdversaryPassOutcome::Continue);
        assert_eq!(state.phase, RefinementPhase::AdvocateRevision);
    }

    /// When the fresh run's round-1 adversary DID file objections, only the
    /// current-run entries are returned — no mixing with the prior run's — and
    /// the carry-over fallback stays disengaged.
    #[test]
    fn round1_current_run_objections_do_not_mix_with_prior_run() {
        let entries = vec![
            // Prior run, round 1: unresolved blocking objection.
            objection_entry(1, true, None, "2026-07-09T10:00:00.000Z"),
            // Fresh run, round 1: the current adversary filed its own objection.
            objection_entry(1, true, None, "2026-07-09T12:00:30.000Z"),
        ];
        let run_start = Some("2026-07-09T12:00:00.000Z");

        let (objections, carried_over) = collect_adversary_round_objections(&entries, 1, run_start);
        assert!(!carried_over, "current-run set is non-empty: no carry-over");
        assert_eq!(
            objections.len(),
            1,
            "only the current-run objection returns"
        );
        assert_eq!(
            objections[0].entry_id.as_deref(),
            Some("objection/2026-07-09T12:00:30.000Z")
        );
    }

    /// Prior-run entries that are already resolved or non-blocking are never
    /// carried: the round stays dry exactly as today.
    #[test]
    fn round1_resolved_or_nonblocking_prior_run_stays_dry() {
        let entries = vec![
            // Resolved blocking objection — not carried.
            objection_entry(
                1,
                true,
                Some("2026-07-09T10:30:00.000Z"),
                "2026-07-09T10:00:00.000Z",
            ),
            // Unresolved but non-blocking objection — not carried.
            objection_entry(1, false, None, "2026-07-09T10:05:00.000Z"),
        ];
        let run_start = Some("2026-07-09T12:00:00.000Z");

        let (objections, carried_over) = collect_adversary_round_objections(&entries, 1, run_start);
        assert!(!carried_over);
        assert!(objections.is_empty(), "nothing to carry → dry round");
    }

    /// Carry-over is round-1 only. At round >= 2 an empty current-run set stays
    /// dry even when unresolved prior-run blocking objections exist, so the
    /// repeat-objection escalation semantics are not distorted.
    #[test]
    fn round2_empty_current_run_does_not_carry_over() {
        let entries = vec![
            objection_entry(1, true, None, "2026-07-09T10:00:00.000Z"),
            objection_entry(2, true, None, "2026-07-09T10:00:05.000Z"),
        ];
        let run_start = Some("2026-07-09T12:00:00.000Z");

        let (objections, carried_over) = collect_adversary_round_objections(&entries, 2, run_start);
        assert!(!carried_over, "rounds >= 2 never carry over");
        assert!(objections.is_empty(), "round 2 stays dry");
    }

    /// With no `refinement_start` boundary (`run_start == None`) every entry is
    /// in-run: the round-1 entries count as current-run objections and the
    /// carry-over fallback (which requires out-of-run entries) never engages.
    #[test]
    fn round1_no_boundary_counts_all_as_current_run() {
        let entries = vec![
            objection_entry(1, true, None, "2026-07-09T10:00:00.000Z"),
            objection_entry(1, true, None, "2026-07-09T10:00:05.000Z"),
        ];

        let (objections, carried_over) = collect_adversary_round_objections(&entries, 1, None);
        assert!(
            !carried_over,
            "no boundary → everything is in-run, no carry-over"
        );
        assert_eq!(
            objections.len(),
            2,
            "both round-1 entries count as current-run"
        );
    }
}
