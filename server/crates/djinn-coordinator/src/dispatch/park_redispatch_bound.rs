//! Cumulative bound on the `no_attempted_remediation` park-rung decline.
//!
//! Extracted next to [`super::cycling_intervention`] (and for the same reason:
//! `crate::types` and `super::retry` both sit at their byte ceilings) so the
//! bound, its threshold, and the marker accounting it reads live in one place.
//! `crate::types` re-exports the names, so `use super::*` call sites are
//! unchanged.
//!
//! ## Why this exists (task 6tlg, 2026-07-27/28)
//!
//! The attempted-remediation gate on the second-strike park rung
//! (`super::retry`, the `NON_ATTEMPT_PARK_THRESHOLD` branch) refuses to park
//! while fewer than [`crate::types::NON_ATTEMPT_PARK_THRESHOLD`] DISTINCT
//! MODELS have terminated pre-submission, and redispatches with forced model
//! rotation instead. Two properties of the quantity it measures
//! (`PostInterventionHistory::non_attempt_models`) make that count unable to
//! grow for a repeating infrastructure failure:
//!
//! 1. Infra outcomes (`crashed` / `timed_out` / `spawn_failed` / `interrupted`,
//!    per `TaskAttemptOutcome::is_infra`) are deliberately excluded from it — a
//!    worker session that exits `failed` is booked `crashed`, so it contributes
//!    nothing.
//! 2. It is deduplicated by MODEL label, so N identical failures on one model
//!    count once.
//!
//! Task 6tlg rode exactly this: 33 worker sessions all died at ~9m40s on the
//! same launcher lease error, every one booked `crashed`, and the rung recorded
//! ten consecutive `park_attempted_remediation_redispatch` markers between
//! 21:16Z and 06:07Z whose payload read `"non_attempt_count": 1` every single
//! time — pinned one below a threshold of 2, forever.
//!
//! `super::retry`'s own cumulative bound (`MAX_ARBITER_HOLD_CYCLES`, incident
//! gy53) does not catch this: it reads the arbitration ledger's prospective
//! hold cycle, and a guard that declines BEFORE any arbitration row is created
//! never advances that cycle. The bound below is keyed on the durable decline
//! markers themselves, which the rung writes on every decline, so it advances
//! precisely when the loop it bounds goes around.

use djinn_core::models::ActivityEntry;

/// How many times the park rung may decline to park via the
/// `no_attempted_remediation` sub-guard, within one intervention epoch, before
/// the decline is no longer honored and the rung parks.
///
/// Rationale for `3`: the guard exists so a task that has never had a
/// post-intervention session reach `submit_work` gets real remediation
/// attempts before a park consumes its strike — with forced model rotation,
/// three redispatches exhaust a typical creator's model list. Past that the
/// premise ("rotation has not been tried") is no longer true regardless of what
/// the model-deduplicated counter reads, and every further decline is another
/// ~10-minute session with nobody deciding anything.
pub(crate) const MAX_PARK_REDISPATCH_DECLINES: usize = 3;

/// The `kind` written into a [`crate::types::PARK_REDISPATCH_MARKER`] payload
/// by the attempted-remediation sub-guard.
pub(crate) const NO_ATTEMPTED_REMEDIATION_KIND: &str = "no_attempted_remediation";

/// Count the `no_attempted_remediation` declines already recorded for the
/// CURRENT intervention epoch.
///
/// 4etb: scoping is on the CANONICAL EVIDENCE EPOCH the marker payload carries
/// (`evidence_floor`), not on `intervention_count`. Rung 1 is retired, so
/// nothing advances that counter on this path and an intervention-keyed budget
/// could never re-arm. The epoch advances exactly when an adjudication or human
/// review resets the floor, which is the property the old counter stood in for:
/// a fresh epoch starts from zero declines.
///
/// Entries whose payload is missing, unparseable, or carries a different
/// `kind`/epoch are ignored. That fails OPEN (toward the existing decline
/// behavior) on payload drift rather than parking a task on evidence that
/// could not be read.
pub(crate) fn no_attempted_remediation_declines(
    entries: &[ActivityEntry],
    evidence_floor: &str,
) -> usize {
    entries
        .iter()
        .filter(|entry| entry.event_type == crate::types::PARK_REDISPATCH_MARKER)
        .filter_map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).ok())
        .filter(|payload| {
            payload["kind"] == NO_ATTEMPTED_REMEDIATION_KIND
                && payload["evidence_floor"].as_str() == Some(evidence_floor)
        })
        .count()
}

/// Should the park rung decline to park because remediation was never actually
/// attempted?
///
/// `true` reproduces the pre-existing behavior (record a decline marker and
/// redispatch with forced model rotation). `false` lets the rung proceed to the
/// park it was already about to perform.
///
/// The `prior_declines` bound is evaluated LAST so it can only ever REMOVE a
/// decline, never add one: a task that has submitted post-intervention work, or
/// whose non-attempt count genuinely reached the threshold, is unaffected.
pub(crate) fn should_decline_no_attempted_remediation_park(
    any_submitted: bool,
    non_attempt_model_count: usize,
    prior_declines: usize,
) -> bool {
    !any_submitted
        && non_attempt_model_count < crate::types::NON_ATTEMPT_PARK_THRESHOLD
        && prior_declines < MAX_PARK_REDISPATCH_DECLINES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NON_ATTEMPT_PARK_THRESHOLD, PARK_REDISPATCH_MARKER};

    fn marker(kind: &str, non_attempt_count: usize, evidence_floor: &str) -> ActivityEntry {
        ActivityEntry {
            id: String::new(),
            task_id: None,
            actor_id: "coordinator".to_owned(),
            actor_role: "system".to_owned(),
            event_type: PARK_REDISPATCH_MARKER.to_owned(),
            payload: serde_json::json!({
                "kind": kind,
                "fingerprint": serde_json::Value::Null,
                "non_attempt_count": non_attempt_count,
                "evidence_floor": evidence_floor,
            })
            .to_string(),
            created_at: String::new(),
        }
    }

    /// Replay of the exact 6tlg shape: the rung declined ten consecutive times
    /// between 21:16Z and 06:07Z, every marker's payload reading
    /// `"non_attempt_count": 1` against a `NON_ATTEMPT_PARK_THRESHOLD` of 2,
    /// because the model-deduplicated, infra-excluding counter cannot grow when
    /// every session dies the same infrastructure death. Feed the gate that
    /// unchanging evidence and it must stop honoring the decline; without the
    /// bound every one of the ten rounds redispatches and the loop is infinite.
    ///
    /// The round count is the OBSERVED incident length, deliberately not derived
    /// from [`MAX_PARK_REDISPATCH_DECLINES`], so tuning the bound cannot make
    /// this assertion vacuous.
    #[test]
    fn repeated_identical_declines_stop_being_honored() {
        const OBSERVED_6TLG_DECLINES: usize = 10;
        const {
            assert!(
                MAX_PARK_REDISPATCH_DECLINES < OBSERVED_6TLG_DECLINES,
                "the bound must trip inside the observed incident, not after it"
            );
        }

        let mut recorded: Vec<ActivityEntry> = Vec::new();
        let mut redispatched = 0usize;

        for _ in 0..OBSERVED_6TLG_DECLINES {
            // `non_attempt_count` is pinned at 1 every round — the 6tlg payload.
            if !should_decline_no_attempted_remediation_park(
                false,
                1,
                no_attempted_remediation_declines(&recorded, "2026-08-06T00:00:00.000Z"),
            ) {
                break;
            }
            redispatched += 1;
            recorded.push(marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "2026-08-06T00:00:00.000Z"));
        }

        assert_eq!(
            redispatched, MAX_PARK_REDISPATCH_DECLINES,
            "the rung must decline exactly {MAX_PARK_REDISPATCH_DECLINES} times and then park; \
             declining all {OBSERVED_6TLG_DECLINES} observed rounds is the 6tlg wedge"
        );
    }

    /// The bound may only remove a decline, never create one. A post-intervention
    /// submit, or a non-attempt count that genuinely reached the threshold, must
    /// keep behaving exactly as before regardless of how many declines were
    /// recorded.
    #[test]
    fn bound_never_turns_a_park_into_a_redispatch() {
        let saturated: Vec<ActivityEntry> = (0..MAX_PARK_REDISPATCH_DECLINES + 5)
            .map(|_| marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E0"))
            .collect();
        let declines = no_attempted_remediation_declines(&saturated, "E0");

        assert!(!should_decline_no_attempted_remediation_park(true, 0, 0));
        assert!(!should_decline_no_attempted_remediation_park(
            true, 0, declines
        ));
        assert!(!should_decline_no_attempted_remediation_park(
            false,
            NON_ATTEMPT_PARK_THRESHOLD,
            0
        ));
    }

    /// Declines are scoped to the intervention epoch recorded in the payload, so
    /// a genuine Planner intervention re-arms the budget. Markers of another
    /// `kind` (the first-occurrence CI fingerprint guard, which has its own
    /// novelty bound) and unparseable payloads are not counted.
    #[test]
    fn declines_are_scoped_to_the_epoch_and_the_kind() {
        let entries = vec![
            marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E1"),
            marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E1"),
            marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E2"),
            marker("first_occurrence_fingerprint", 0, "E2"),
            ActivityEntry {
                payload: "not json".to_owned(),
                ..marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E2")
            },
            ActivityEntry {
                event_type: "status_changed".to_owned(),
                ..marker(NO_ATTEMPTED_REMEDIATION_KIND, 1, "E2")
            },
        ];

        assert_eq!(no_attempted_remediation_declines(&entries, "E1"), 2);
        assert_eq!(no_attempted_remediation_declines(&entries, "E2"), 1);
        assert_eq!(no_attempted_remediation_declines(&entries, "E3"), 0);
    }
}
