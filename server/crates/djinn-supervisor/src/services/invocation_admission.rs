//! Agent/launcher-side projection of the durable admission epoch.
//!
//! The v0 (emergency) side of the handoff is interpreted by the coordinator's
//! `evaluate_handoff`. This module is the v1 (invocation) counterpart read by
//! the agent/launcher before it decides whether to lift the reserved cpu.max
//! quota. It lives here — beside the [`InvocationLiftDecision`] contract and
//! above `djinn-db` — so BOTH the host path (`DirectServices`) and the pod path
//! (`WorkerSupervisorServices`) can read it without depending on the
//! coordinator crate.

use djinn_db::{AdmissionHandoffPhase, AdmissionHandoffRow, V1Mode};

use crate::services::lease::InvocationLiftDecision;

/// Project the durable epoch row into whether a bound v1 invocation may lift the
/// launcher quota.
///
/// It is intentionally independent of the v0 emergency gates and fails closed on
/// every uncertain input:
///
/// - An unreadable (`Err`) or missing (`None`) row keeps the quota unleased.
/// - The illegal both-non-enforcing combo keeps the quota unleased.
/// - An incomplete epoch (the current phase's required acknowledgements are not
///   at the current epoch) keeps the quota unleased — a stale epoch never lifts.
/// - `v1 = off` keeps the quota unleased; `v1 = shadow` observes only.
/// - `v1 = enforce` lifts only once the handoff has actually entered an overlap
///   or invocation-primary phase; a `v1 = enforce` row still parked in the
///   emergency-primary phase has not armed the overlap and stays unleased.
///
/// The caller additionally requires a matching durable fencing token before it
/// acts on a [`InvocationLiftDecision::Lift`]; this function never authorizes a
/// lift on epoch alone.
///
/// # Shadow CLAMPS — it does not speed anything up
///
/// Read this before arming `v1 = shadow` in production. Only
/// [`InvocationLiftDecision::Lift`] ever raises `cpu.max`. `Shadow` binds the
/// invocation and records telemetry (the "would throttle" arms) and then leaves
/// the leaf pinned at the broker's unleased quota —
/// `UnleasedQuota::DEFAULT_MILLICORES`, i.e. **250m** — for the whole command.
/// So a rollout that seeds the epoch and arms shadow makes every leased build
/// slower, not faster: it is an observation mode whose entire purpose is to
/// measure what enforcement *would* do. This is correct by design and asserted by
/// `shadow_epoch_binds_but_never_lifts` in `djinn-agent`'s
/// `process/tests/process_lease_tests.rs` — do not "fix" it. Only `v1 = enforce`
/// with a fully acknowledged
/// `ForwardOverlap`/`InvocationPrimary`/`RollbackOverlap` phase lifts the quota.
///
/// # `Unleased` does NOT clamp (goxi launcher blocker 11)
///
/// This used to read "`Unleased` is a literal no-op with the same effect [as
/// shadow]". Both halves of that were wrong in the same way. It was not a no-op:
/// the leaf had already been born at the 250m unleased quota before this decision
/// was ever read, so `Unleased` pinned every command there for its whole life.
/// And it was not "the same effect as shadow" in intent — shadow clamps
/// deliberately to observe, whereas `Unleased` means *no admission authority
/// exists for this invocation at all*.
///
/// Production ran the launcher armed with the `admission_handoff` row ABSENT
/// (`djinn-server epoch show` → `admission handoff row: <absent>`), which is this
/// function's `Ok(None)` arm. A measured leaf reached 21,130,868 usec of CPU —
/// 84x the 250,000 usec escalation threshold — with `cpu.max` never leaving
/// `25000 100000`, making an armed launcher ~16x slower than a disabled one.
///
/// `Unleased` now selects `LeaseAuthority::Unarmed` at leaf creation (see
/// `djinn-agent`'s `process_broker::birth_authority`), so the leaf is born with no
/// quota of its own and inherits the Pod's budget. It is still contained, still
/// killable, still measurable — it just costs nothing. Asserted behaviourally on
/// a real cgroup2 hierarchy by step 7 of
/// `djinn-cgroup-launcher/tests/delegated_cpu_lease_lifecycle.rs`, and on the
/// production composition by
/// `unleased_epoch_is_born_unclamped_because_no_grant_can_ever_lift_it`.
#[must_use]
pub fn evaluate_invocation_lift(
    row: Result<Option<AdmissionHandoffRow>, ()>,
) -> InvocationLiftDecision {
    let Ok(Some(row)) = row else {
        return InvocationLiftDecision::Unleased;
    };
    // Neither authority enforces: no admission control at all. Fail closed.
    if !row.v0_mode.is_enforcing() && !row.v1_mode.is_enforcing() {
        return InvocationLiftDecision::Unleased;
    }
    // The current phase's required acknowledgements must be at the current epoch
    // before the row's modes are authoritative. Anything else is a stale epoch.
    let emergency_current = row.emergency_ack_epoch == Some(row.epoch);
    let invocation_current = row.invocation_ack_epoch == Some(row.epoch);
    let complete = match row.phase {
        AdmissionHandoffPhase::EmergencyPrimary => emergency_current,
        AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => {
            emergency_current && invocation_current
        }
        AdmissionHandoffPhase::InvocationPrimary => invocation_current,
    };
    if !complete {
        return InvocationLiftDecision::Unleased;
    }
    match row.v1_mode {
        V1Mode::Off => InvocationLiftDecision::Unleased,
        V1Mode::Shadow => InvocationLiftDecision::Shadow,
        V1Mode::Enforce => match row.phase {
            AdmissionHandoffPhase::ForwardOverlap
            | AdmissionHandoffPhase::InvocationPrimary
            | AdmissionHandoffPhase::RollbackOverlap => InvocationLiftDecision::Lift,
            // v1 enforce configured but the overlap has not been armed yet.
            AdmissionHandoffPhase::EmergencyPrimary => InvocationLiftDecision::Unleased,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::V0Mode;

    fn row(
        phase: AdmissionHandoffPhase,
        emergency: bool,
        invocation: bool,
        v0_mode: V0Mode,
        v1_mode: V1Mode,
    ) -> AdmissionHandoffRow {
        AdmissionHandoffRow {
            phase,
            epoch: 7,
            emergency_ack_epoch: emergency.then_some(7),
            invocation_ack_epoch: invocation.then_some(7),
            v0_mode,
            v1_mode,
            cap: None,
            updated_at: "now".into(),
        }
    }

    #[test]
    fn lifts_only_in_committed_v1_overlap_or_primary() {
        // Unreadable / missing epochs keep the quota unleased.
        assert_eq!(
            evaluate_invocation_lift(Err(())),
            InvocationLiftDecision::Unleased
        );
        assert_eq!(
            evaluate_invocation_lift(Ok(None)),
            InvocationLiftDecision::Unleased
        );

        // Baseline (v1 off) never lifts even with a complete emergency ack.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Off,
            )))),
            InvocationLiftDecision::Unleased
        );

        // Shadow observes but never lifts.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Shadow,
            )))),
            InvocationLiftDecision::Shadow
        );

        // v1 enforce still parked in emergency-primary has not armed the overlap.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Unleased
        );

        // A committed forward overlap with v1 enforcing lifts.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                true,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );
        // Invocation-primary lifts (v0 disabled, v1 enforcing).
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::InvocationPrimary,
                false,
                true,
                V0Mode::Disabled,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );
        // Rollback overlap still has v1 enforcing, so it may still lift.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::RollbackOverlap,
                true,
                true,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Lift
        );

        // An INCOMPLETE overlap epoch (missing the invocation ack) is stale and
        // must not lift.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::ForwardOverlap,
                true,
                false,
                V0Mode::Enforce,
                V1Mode::Enforce,
            )))),
            InvocationLiftDecision::Unleased
        );

        // The illegal both-non-enforcing combo fails closed.
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(row(
                AdmissionHandoffPhase::EmergencyPrimary,
                true,
                false,
                V0Mode::Observe,
                V1Mode::Shadow,
            )))),
            InvocationLiftDecision::Unleased
        );
    }
}
