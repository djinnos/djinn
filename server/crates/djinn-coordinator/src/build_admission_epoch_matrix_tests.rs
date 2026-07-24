//! Versioned admission-epoch transition matrix (ujvz): the rollout and rollback
//! state machine driven through the merged operator executor.
//!
//! Every scenario drives [`crate::build_admission_transition::AdmissionTransitionExecutor`]
//! — the same safe-ordering executor operators use — and asserts, at every
//! committed durable state, that at least one authority still enforces and that
//! the epoch's reference cap is never exceeded at full speed. See
//! [`crate::build_admission_epoch_support`] for the two invariants.

use djinn_db::{AdmissionHandoffPhase, V0Mode, V1Mode};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

use crate::build_admission::BuildAdmissionMode;
use crate::build_admission_epoch_support::{EpochWorld, assert_restart_is_fail_closed};
use crate::build_admission_handoff::HandoffState;
use crate::build_admission_transition::TransitionError;

fn armed_generations() -> Vec<String> {
    vec![
        crate::build_admission::task_run_generation_key("task-alpha", 0),
        crate::build_admission::task_run_generation_key("task-beta", 2),
    ]
}

/// The launcher-side projection of the durable row *without* a leader tick, so
/// the window between two transactions can be inspected exactly as a running
/// agent would see it.
async fn raw_lift(world: &EpochWorld) -> InvocationLiftDecision {
    evaluate_invocation_lift(Ok(Some(world.row().await)))
}

/// Shadow → overlap → cutover, observing after every durable transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_cutover_holds_an_enforcing_authority_and_the_cap_at_every_state() {
    let world = EpochWorld::new();
    let cap = 3;
    let armed = armed_generations();

    // ── Baseline: v0 alone, v1 off. ───────────────────────────────────────
    let baseline = world.observe("baseline").await;
    assert_eq!(baseline.row.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(baseline.row.v0_mode, V0Mode::Enforce);
    assert_eq!(baseline.row.v1_mode, V1Mode::Off);
    assert_eq!(baseline.snapshot.state, HandoffState::EmergencyPrimary);
    assert_eq!(baseline.lift, InvocationLiftDecision::Unleased);
    assert!(baseline.v0_enforcing && !baseline.v1_enforcing);

    // ── Shadow: v1 observes, never lifts; v0 remains the sole authority. ──
    let epoch = world.row().await.epoch;
    world.executor.arm_shadow(epoch, cap).await.unwrap();
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "the freshly armed epoch has no acknowledgement yet, so nothing lifts"
    );
    let shadow = world.observe("shadow").await;
    assert_eq!(shadow.row.v1_mode, V1Mode::Shadow);
    assert_eq!(shadow.row.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(shadow.snapshot.state, HandoffState::Shadow);
    assert_eq!(
        shadow.lift,
        InvocationLiftDecision::Shadow,
        "shadow observes what v1 would do and never lifts the launcher quota"
    );
    assert!(shadow.v0_enforcing && !shadow.v1_enforcing);
    assert_eq!(shadow.cap, cap);

    // ── Overlap modes armed, phase still v0-primary: v1 still cannot lift. ─
    let epoch = world.row().await.epoch;
    world.executor.arm_overlap(epoch, cap).await.unwrap();
    let armed_modes = world.observe("overlap_modes_armed").await;
    assert_eq!(armed_modes.row.v1_mode, V1Mode::Enforce);
    assert_eq!(
        armed_modes.row.phase,
        AdmissionHandoffPhase::EmergencyPrimary
    );
    assert_eq!(
        armed_modes.lift,
        InvocationLiftDecision::Unleased,
        "arming v1 enforce alone never lifts: the overlap phase is not committed"
    );
    assert!(armed_modes.v0_enforcing && !armed_modes.v1_enforcing);

    // ── Forward overlap: both authorities enforce. ────────────────────────
    let epoch = world.row().await.epoch;
    world.executor.enter_forward_overlap(epoch).await.unwrap();
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "the overlap epoch does not lift before v0 acknowledges it"
    );
    let overlap = world.observe("forward_overlap").await;
    assert_eq!(overlap.row.phase, AdmissionHandoffPhase::ForwardOverlap);
    assert_eq!(overlap.snapshot.state, HandoffState::ForwardOverlap);
    assert!(
        overlap.v0_enforcing && overlap.v1_enforcing,
        "the forward overlap is the both-enforcing state"
    );

    // ── The invocation-primary edge is blocked by a missing generation ack. ─
    let epoch = world.row().await.epoch;
    let blocked = world
        .executor
        .commit_invocation_primary(epoch, &armed)
        .await
        .unwrap_err();
    assert!(
        matches!(
            blocked,
            TransitionError::AwaitingGenerationAcks { missing: 2, .. }
        ),
        "a missing live-generation acknowledgement blocks invocation_primary: {blocked:?}"
    );
    let still_overlap = world.observe("blocked_on_generation_acks").await;
    assert_eq!(
        still_overlap.row.phase,
        AdmissionHandoffPhase::ForwardOverlap
    );
    assert!(still_overlap.v0_enforcing);

    // One of two generations acknowledges: still blocked, still both-enforcing.
    world.generations_ack(&armed[..1]).await;
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world
            .executor
            .commit_invocation_primary(epoch, &armed)
            .await
            .unwrap_err(),
        TransitionError::AwaitingGenerationAcks { missing: 1, .. }
    ));
    let partial = world.observe("one_generation_acknowledged").await;
    assert_eq!(partial.row.phase, AdmissionHandoffPhase::ForwardOverlap);
    assert!(partial.v0_enforcing && partial.v1_enforcing);

    // ── Cutover: every armed generation acknowledges, v0 may be released. ──
    world.generations_ack(&armed[1..]).await;
    let epoch = world.row().await.epoch;
    world
        .executor
        .commit_invocation_primary(epoch, &armed)
        .await
        .unwrap();
    let primary = world.observe("invocation_primary").await;
    assert_eq!(primary.row.phase, AdmissionHandoffPhase::InvocationPrimary);
    assert_eq!(primary.snapshot.state, HandoffState::InvocationPrimary);
    assert!(
        !primary.v0_enforcing && primary.v1_enforcing,
        "v0 is released exactly once v1 is the committed, lifting authority"
    );
    assert_eq!(world.leader_mode(), BuildAdmissionMode::Off);

    // ── Terminal forward step: v0 observes, v1 keeps enforcing the cap. ────
    let epoch = world.row().await.epoch;
    world.executor.observe_v0(epoch).await.unwrap();
    let observed = world.observe("v0_observe").await;
    assert_eq!(observed.row.v0_mode, V0Mode::Observe);
    assert_eq!(observed.row.v1_mode, V1Mode::Enforce);
    assert!(!observed.v0_enforcing && observed.v1_enforcing);
    assert_eq!(observed.cap, cap);
}

/// Drive the forward cutover the same way the operator runbook does, leaving
/// the world at the terminal `v0 = observe` state.
async fn drive_forward_cutover(world: &EpochWorld, cap: i64, armed: &[String]) {
    world.observe("cutover_baseline").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_shadow(epoch, cap).await.unwrap();
    world.observe("cutover_shadow").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_overlap(epoch, cap).await.unwrap();
    world.observe("cutover_overlap_modes").await;
    let epoch = world.row().await.epoch;
    world.executor.enter_forward_overlap(epoch).await.unwrap();
    world.observe("cutover_forward_overlap").await;
    world.generations_ack(armed).await;
    let epoch = world.row().await.epoch;
    world
        .executor
        .commit_invocation_primary(epoch, armed)
        .await
        .unwrap();
    world.observe("cutover_invocation_primary").await;
}

/// The rollback ordering proof: v0 is enforcing *and* acknowledged before any
/// v1 quota is lifted at the rollback epoch, and v1 is never disabled while v0
/// is unconfirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_confirms_v0_before_any_v1_quota_lift_and_never_raises_the_cap() {
    let world = EpochWorld::new();
    let armed = armed_generations();
    drive_forward_cutover(&world, 3, &armed).await;
    let epoch = world.row().await.epoch;
    world.executor.observe_v0(epoch).await.unwrap();
    let released = world.observe("v1_primary").await;
    assert!(!released.v0_enforcing && released.v1_enforcing);

    // ── Arm the rollback at a same-or-lower cap: v0 re-enforces. ──────────
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.arm_rollback(epoch, 4).await,
        Err(TransitionError::CapNotSameOrLower {
            requested: 4,
            current: 3
        })
    ));
    let armed_row = world.executor.arm_rollback(epoch, 2).await.unwrap();
    assert_eq!(armed_row.v0_mode, V0Mode::Enforce);
    assert_eq!(armed_row.v1_mode, V1Mode::Enforce);
    assert_eq!(armed_row.cap, Some(2));
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "the rollback epoch lifts nothing until it is acknowledged"
    );

    // ── v0 unconfirmed: the rollback halts, mutating nothing. ─────────────
    let halted = world
        .executor
        .enter_rollback_overlap(armed_row.epoch)
        .await
        .unwrap_err();
    assert!(matches!(
        halted,
        TransitionError::HaltedV0Unconfirmed { .. }
    ));
    let after_halt = world.row().await;
    assert_eq!(
        after_halt.epoch, armed_row.epoch,
        "the halt mutated nothing"
    );
    assert_eq!(after_halt.v1_mode, V1Mode::Enforce, "v1 was never disabled");

    // The leader tick confirms v0 (enforcing, healthy, acknowledged).
    let confirmed = world.observe("rollback_armed").await;
    assert!(confirmed.v0_enforcing, "v0 re-enforces before the rollback");
    assert_eq!(confirmed.row.emergency_ack_epoch, Some(confirmed.row.epoch));
    assert_eq!(confirmed.cap, 2, "the rollback lowered the reference cap");

    // ── Rollback overlap: v1 lifts only after v0 acknowledges this epoch. ─
    let epoch = world.row().await.epoch;
    let overlap = world.executor.enter_rollback_overlap(epoch).await.unwrap();
    assert_eq!(overlap.phase, AdmissionHandoffPhase::RollbackOverlap);
    assert_eq!(overlap.invocation_ack_epoch, Some(overlap.epoch));
    assert_ne!(
        overlap.emergency_ack_epoch,
        Some(overlap.epoch),
        "the advance cleared the v0 acknowledgement"
    );
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "no v1 quota is lifted at the rollback overlap before v0 acknowledges it"
    );
    let rollback_overlap = world.observe("rollback_overlap").await;
    assert_eq!(
        rollback_overlap.snapshot.state,
        HandoffState::RollbackOverlap
    );
    assert_eq!(
        rollback_overlap.row.emergency_ack_epoch,
        Some(rollback_overlap.row.epoch),
        "v0 is acknowledged BEFORE the epoch lifts v1"
    );
    assert_eq!(rollback_overlap.lift, InvocationLiftDecision::Lift);
    assert!(rollback_overlap.v0_enforcing && rollback_overlap.v1_enforcing);

    // ── Complete: v1 is disabled only after v0 is re-confirmed. ───────────
    let epoch = world.row().await.epoch;
    let baseline = world.executor.complete_rollback(epoch).await.unwrap();
    assert_eq!(baseline.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(baseline.v0_mode, V0Mode::Enforce);
    assert_eq!(baseline.v1_mode, V1Mode::Off);
    assert_eq!(baseline.cap, Some(2), "the rollback never raised the cap");
    let final_state = world.observe("rollback_complete").await;
    assert_eq!(final_state.snapshot.state, HandoffState::EmergencyPrimary);
    assert_eq!(final_state.lift, InvocationLiftDecision::Unleased);
    assert!(final_state.v0_enforcing && !final_state.v1_enforcing);
    assert_eq!(world.leader_mode(), BuildAdmissionMode::Enforce);
}

/// The kill switch runs the same reverse ordering straight from
/// invocation-primary, where v0 is still durably `Enforce`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_switch_from_invocation_primary_keeps_an_enforcing_authority_throughout() {
    let world = EpochWorld::new();
    let armed = armed_generations();
    drive_forward_cutover(&world, 3, &armed).await;

    let epoch = world.row().await.epoch;
    world.executor.arm_rollback(epoch, 3).await.unwrap();
    let armed_state = world.observe("kill_switch_armed").await;
    assert!(armed_state.v0_enforcing);
    assert_eq!(armed_state.cap, 3);

    let epoch = world.row().await.epoch;
    world.executor.enter_rollback_overlap(epoch).await.unwrap();
    let overlap = world.observe("kill_switch_overlap").await;
    assert!(overlap.v0_enforcing && overlap.v1_enforcing);

    let epoch = world.row().await.epoch;
    world.executor.complete_rollback(epoch).await.unwrap();
    let done = world.observe("kill_switch_complete").await;
    assert_eq!(done.row.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(done.row.v1_mode, V1Mode::Off);
    assert!(done.v0_enforcing && !done.v1_enforcing);
}

/// The durable transactions of one complete forward + rollback cycle, in order.
const CYCLE_STEPS: usize = 15;

/// The phase that must be authoritative after `executed` transactions.
fn expected_phase(executed: usize) -> AdmissionHandoffPhase {
    match executed {
        0..=5 => AdmissionHandoffPhase::EmergencyPrimary,
        6..=8 => AdmissionHandoffPhase::ForwardOverlap,
        9..=12 => AdmissionHandoffPhase::InvocationPrimary,
        13..=14 => AdmissionHandoffPhase::RollbackOverlap,
        _ => AdmissionHandoffPhase::EmergencyPrimary,
    }
}

/// Apply the first `stop_after` durable transactions of the cycle.
async fn drive_cycle(world: &EpochWorld, stop_after: usize, cap: i64, armed: &[String]) {
    for step in 0..stop_after {
        let epoch = world.row().await.epoch;
        match step {
            // The leader's live handoff tick is itself a durable transaction.
            0 | 2 | 4 | 6 | 11 | 13 => {
                world.leader_tick().await;
            }
            1 => world
                .executor
                .arm_shadow(epoch, cap)
                .await
                .map(drop)
                .unwrap(),
            3 => world
                .executor
                .arm_overlap(epoch, cap)
                .await
                .map(drop)
                .unwrap(),
            5 => world
                .executor
                .enter_forward_overlap(epoch)
                .await
                .map(drop)
                .unwrap(),
            7 => world.generations_ack(armed).await,
            8 => world
                .executor
                .commit_invocation_primary(epoch, armed)
                .await
                .map(drop)
                .unwrap(),
            9 => world.executor.observe_v0(epoch).await.map(drop).unwrap(),
            10 => world
                .executor
                .arm_rollback(epoch, cap - 1)
                .await
                .map(drop)
                .unwrap(),
            12 => world
                .executor
                .enter_rollback_overlap(epoch)
                .await
                .map(drop)
                .unwrap(),
            _ => world
                .executor
                .complete_rollback(epoch)
                .await
                .map(drop)
                .unwrap(),
        }
    }
}

/// Crash and restart between EVERY handoff transaction of the full cycle: the
/// earlier safe phase stays authoritative, no state is torn, and the restarted
/// process still holds an enforcing authority under its cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_restart_between_every_transaction_keeps_the_earlier_phase_authoritative() {
    let armed = armed_generations();
    for executed in 0..=CYCLE_STEPS {
        let world = EpochWorld::new();
        drive_cycle(&world, executed, 3, &armed).await;
        let before = world.row().await;
        assert_eq!(
            before.phase,
            expected_phase(executed),
            "after {executed} transactions the durable phase is the committed one"
        );

        // Forced process loss: nothing cooperative runs. A replacement leader
        // starts fail-closed and reads the durable epoch.
        let restarted = world.restart();
        let after = restarted.row().await;
        assert_eq!(
            after, before,
            "a crash between transactions leaves the durable row exactly as committed"
        );
        let observation = restarted
            .observe(&format!("restart_after_{executed}_transactions"))
            .await;
        assert_eq!(observation.row.phase, expected_phase(executed));
        assert_restart_is_fail_closed(&observation);
    }
}

/// The operator steps that issue more than one durable write have an internal
/// window. A crash inside it must still leave the earlier, safe authority in
/// charge — even when the durable v0 mode has already been relaxed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_inside_a_multi_write_operator_step_leaves_the_safe_authority_in_charge() {
    let armed = armed_generations();

    // Window 1: `observe_v0` writes the relaxed modes, then re-acknowledges the
    // invocation authority. A crash between the two leaves v0 = observe on an
    // incomplete epoch.
    let world = EpochWorld::new();
    drive_forward_cutover(&world, 3, &armed).await;
    let epoch = world.row().await.epoch;
    world
        .handoff
        .set_modes_and_cap(epoch, V0Mode::Observe, V1Mode::Enforce, Some(3))
        .await
        .unwrap();
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "the half-applied step lifts nothing"
    );
    let restarted = world.restart();
    let observation = restarted.observe("crash_inside_observe_v0").await;
    assert_eq!(observation.row.v0_mode, V0Mode::Observe);
    assert!(
        observation.v0_enforcing,
        "an incomplete epoch keeps the emergency authority enforcing even when the \
         durable v0 mode has been relaxed"
    );
    assert!(!observation.v1_enforcing);

    // Window 2: `complete_rollback` advances to the v0 baseline and only then
    // disables v1. A crash between the two leaves both authorities armed.
    let world = EpochWorld::new();
    drive_forward_cutover(&world, 3, &armed).await;
    let epoch = world.row().await.epoch;
    world.executor.arm_rollback(epoch, 3).await.unwrap();
    world.observe("rollback_armed_for_window").await;
    let epoch = world.row().await.epoch;
    world.executor.enter_rollback_overlap(epoch).await.unwrap();
    world.observe("rollback_overlap_for_window").await;
    let epoch = world.row().await.epoch;
    world
        .handoff
        .advance(epoch, AdmissionHandoffPhase::EmergencyPrimary, &[])
        .await
        .unwrap();
    assert_eq!(
        raw_lift(&world).await,
        InvocationLiftDecision::Unleased,
        "the v0 baseline stops v1 lifting the moment the phase commits"
    );
    let restarted = world.restart();
    let observation = restarted.observe("crash_inside_complete_rollback").await;
    assert_eq!(
        observation.row.phase,
        AdmissionHandoffPhase::EmergencyPrimary
    );
    assert_eq!(
        observation.row.v1_mode,
        V1Mode::Enforce,
        "v1 is still durably armed; only the phase stopped it lifting"
    );
    assert!(observation.v0_enforcing && !observation.v1_enforcing);
}
