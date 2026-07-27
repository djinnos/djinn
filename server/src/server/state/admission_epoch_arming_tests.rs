//! End-to-end reachability of the v1 invocation-lift authority.
//!
//! Every other test around the admission epoch drives a *stand-in* for one of
//! the two production seams:
//!
//! * `djinn-coordinator`'s `EpochWorld::leader_tick` **re-implements**
//!   `finalize_build_admission_handoff` rather than calling it, so it cannot
//!   catch a divergence between the policy and the seam that applies it.
//! * `djinn-agent`'s `process_lease_tests` hold the lift decision in a
//!   `Mutex<InvocationLiftDecision>`, so they prove what the launcher does
//!   *given* a decision, never that the decision is reachable.
//! * `admin.rs`'s own tests drive the operator CLI against a bare repository
//!   with a hand-written `acknowledge` standing in for the coordinator.
//!
//! Nothing joined them, so "a fresh deployment can reach a lift-granting phase"
//! was asserted nowhere. These tests close that gap by composing only real
//! seams against a real Postgres database:
//!
//! * the real migration-seeded row (no hand-built fixture),
//! * the real ordered startup gates `AppState::initialize` runs,
//! * the real `confirm_build_admission_topology` that `become_leader` calls,
//! * the real `finalize_build_admission_handoff` the periodic leader loop ticks,
//! * the real `crate::admin::run_admin_command` operator CLI, and
//! * the real `evaluate_invocation_lift` the launcher reads.
//!
//! # The trap these tests are shaped to avoid
//!
//! Blocker eleven shipped because its test asserted only `lifts.is_empty()` — a
//! property the defect satisfied. Asserting `Unleased` everywhere would repeat
//! exactly that mistake, because `Unleased` is also what a permanently broken
//! rollout returns. So every stage here asserts the *positive* reachable
//! outcome, and the intermediate `Unleased`/`Shadow` stages each assert a
//! **distinct, separately identified reason** for not lifting, so they fail
//! independently rather than all passing for one wrong reason.

use super::build_admission_config_tests::{
    BUILD_ADMISSION_TELEMETRY_LOCK, admission, handoff_repository,
    state_for_admission_config_with_db,
};
use super::*;
use crate::admin::{AdminCommand, EpochAction, run_admin_command};
use djinn_agent::actors::coordinator::BuildAdmissionReadiness;
use djinn_coordinator::build_admission::{BuildAdmissionDecision, DenialCause};
use djinn_db::{AdmissionDomain, AdmissionHandoffPhase, V0Mode, V1Mode};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

/// Run one real operator CLI step and return its rendered output.
async fn epoch_cli(state: &AppState, action: EpochAction) -> String {
    run_admin_command(state.db(), AdminCommand::Epoch { action })
        .await
        .expect("operator epoch command")
}

/// The launcher-side decision, read from the durable row exactly as
/// `DirectServices::invocation_lift_decision` and
/// `WorkerSupervisorServices::invocation_lift_decision` read it.
async fn launcher_lift(state: &AppState) -> InvocationLiftDecision {
    evaluate_invocation_lift(handoff_repository(state).read().await.map_err(|_| ()))
}

/// One tick of the periodic leader handoff loop
/// (`start_handoff_warning_loop` → `finalize_build_admission_handoff`). This is
/// the production writer of the v0 acknowledgement, and the only thing that
/// re-completes an epoch after an operator step bumps it.
async fn leader_handoff_tick(state: &AppState) {
    let controller = admission(state).clone();
    state.finalize_build_admission_handoff(&controller).await;
}

/// Drive the ordered startup gates in the exact sequence `AppState::initialize`
/// runs them (handoff → recovery → deferred recovery → inventory), then the
/// topology confirmation that `become_leader` performs on winning the
/// coordinator advisory lock.
async fn boot_through_leadership(state: &AppState) {
    boot_to_topology_gate(state).await;
    state.confirm_build_admission_topology().await;
}

/// The same ordered startup gates, stopping *before* leadership is acquired —
/// i.e. what a standby pod (or a leader that never won the lock) reaches.
async fn boot_to_topology_gate(state: &AppState) {
    state.initialize_build_admission_handoff().await;
    state.initialize_build_admission_recovery().await;
    state.initialize_build_admission_deferred_recovery().await;
    *state.inner.graph_warmer.write().await =
        Some(Arc::new(build_in_process_graph_warmer(state.clone())));
    state.initialize_build_admission_inventory().await;
}

/// Ask the real emergency controller to admit a real task run.
async fn admit(state: &AppState, work_id: &str) -> BuildAdmissionDecision {
    admission(state)
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            work_id.to_owned(),
            0,
            format!("{work_id}-job"),
        )
        .await
        .expect("admission decision")
}

fn observe_config() -> BuildAdmissionConfig {
    BuildAdmissionConfig {
        mode: BuildAdmissionMode::Observe,
        cap: 3,
    }
}

/// A fresh deployment reaches a lift-granting phase through nothing but the real
/// startup seams and the real operator CLI, and admits work at every step.
///
/// This is the property the eleven previous blockers left unproven: `Lift` is
/// *reachable*. It also pins the exact operator sequence the production runbook
/// uses, so a change that silently lengthens or breaks that sequence fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn fresh_deployment_reaches_lift_through_real_startup_and_operator_cli() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db.clone(), observe_config());

    // ── Stage 0: the migration-seeded baseline opens by itself. ───────────
    boot_through_leadership(&state).await;
    let controller = admission(&state).clone();
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::Healthy,
        "coordinator leadership must complete the startup gates"
    );
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Enforce,
        "a durable emergency-primary row promotes the configured Observe mode"
    );
    let baseline = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(baseline.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(
        baseline.emergency_ack_epoch,
        Some(baseline.epoch),
        "leadership acknowledged the seeded epoch with no operator action"
    );
    assert!(
        matches!(
            admit(&state, "baseline-task").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the v0 baseline admits work"
    );
    // Reason #1 for not lifting: v1 is off. Distinct from every stage below.
    assert_eq!(baseline.v1_mode, V1Mode::Off);
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Unleased,
        "v1 is off at the baseline, so the launcher must not lift"
    );

    // `epoch seed` is idempotent and must never disturb a live rollout.
    let rendered = epoch_cli(&state, EpochAction::Seed).await;
    assert!(
        rendered.starts_with("seed: already present"),
        "seed must not touch an existing row: {rendered}"
    );

    // ── Stage 1: arm shadow. v1 observes; it must NOT lift. ───────────────
    epoch_cli(
        &state,
        EpochAction::Advance {
            cap: Some(3),
            generations: vec![],
        },
    )
    .await;
    let shadow = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(shadow.v1_mode, V1Mode::Shadow);
    assert_eq!(shadow.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert!(
        shadow.epoch > baseline.epoch,
        "arming bumps the epoch and clears both acknowledgements"
    );
    // Reason #2: shadow observes. A DISTINCT decision from Unleased, so a
    // regression collapsing shadow into either neighbour fails right here.
    leader_handoff_tick(&state).await;
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Shadow,
        "shadow must bind-and-observe, never lift"
    );

    // ── Stage 2: arm the overlap modes. v1 enforces but the phase has not ──
    // ── advanced, so it still must not lift. ──────────────────────────────
    epoch_cli(
        &state,
        EpochAction::Advance {
            cap: Some(3),
            generations: vec![],
        },
    )
    .await;
    let armed = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(armed.v1_mode, V1Mode::Enforce);
    assert_eq!(
        armed.phase,
        AdmissionHandoffPhase::EmergencyPrimary,
        "arming modes never advances the phase on its own"
    );
    // Reason #3: v1 enforcing but the overlap is not armed. Independent of the
    // acknowledgement state, which stage 3 covers.
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Unleased,
        "v1=enforce parked in emergency-primary must not lift"
    );

    // ── Stage 3: the overlap edge waits for the leader's v0 ack. ──────────
    let rendered = epoch_cli(
        &state,
        EpochAction::Advance {
            cap: None,
            generations: vec![],
        },
    )
    .await;
    assert!(
        rendered.contains("awaiting v0 emergency acknowledgement"),
        "arming cleared the v0 ack, so the overlap edge must wait: {rendered}"
    );
    assert_eq!(
        handoff_repository(&state)
            .read()
            .await
            .expect("read")
            .expect("row")
            .phase,
        AdmissionHandoffPhase::EmergencyPrimary,
        "a waiting step mutates nothing"
    );

    // The periodic leader loop is what unblocks it — no restart involved.
    leader_handoff_tick(&state).await;
    assert_eq!(
        handoff_repository(&state)
            .read()
            .await
            .expect("read")
            .expect("row")
            .emergency_ack_epoch,
        Some(armed.epoch),
        "the leader handoff tick re-acknowledges the bumped epoch"
    );

    // ── Stage 4: enter the forward overlap. ───────────────────────────────
    epoch_cli(
        &state,
        EpochAction::Advance {
            cap: None,
            generations: vec![],
        },
    )
    .await;
    let overlap = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(overlap.phase, AdmissionHandoffPhase::ForwardOverlap);
    assert_eq!(
        overlap.invocation_ack_epoch,
        Some(overlap.epoch),
        "the executor records the v1 authority ack on the new epoch"
    );
    // Reason #4: the advance cleared v0's ack, and an overlap needs BOTH. This
    // is the ordering guarantee that v0 is confirmed before v1 ever lifts.
    assert_ne!(overlap.emergency_ack_epoch, Some(overlap.epoch));
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Unleased,
        "an overlap with no v0 acknowledgement must not lift"
    );

    // ── Stage 5: the leader completes the overlap epoch → LIFT. ───────────
    leader_handoff_tick(&state).await;
    let lifting = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(lifting.emergency_ack_epoch, Some(lifting.epoch));
    assert_eq!(lifting.invocation_ack_epoch, Some(lifting.epoch));
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Lift,
        "a fresh deployment MUST be able to reach a lift-granting phase"
    );
    // Both authorities enforce across the overlap, and work is still admitted:
    // reaching Lift is not paid for with a fail-closed admission gate.
    assert_eq!(lifting.v0_mode, V0Mode::Enforce);
    assert_eq!(lifting.v1_mode, V1Mode::Enforce);
    assert_eq!(controller.mode(), BuildAdmissionMode::Enforce);
    assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
    assert!(
        matches!(
            admit(&state, "overlap-task").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the lifting overlap admits work rather than denying it"
    );

    // ── Stage 6: a restart keeps lifting. ────────────────────────────────
    let restarted = state_for_admission_config_with_db(db, observe_config());
    boot_through_leadership(&restarted).await;
    assert_eq!(
        admission(&restarted).readiness(),
        BuildAdmissionReadiness::Healthy
    );
    assert_eq!(
        launcher_lift(&restarted).await,
        InvocationLiftDecision::Lift,
        "a restart must re-open on the committed overlap, not fall back to unleased"
    );
    assert!(matches!(
        admit(&restarted, "restarted-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
}

/// The 2026-07-19 wedge shape still fails closed.
///
/// This protection is load-bearing — it is what stops a second active admission
/// writer — so it must keep denying. The test pins the *exact* self-contradictory
/// denial from the incident (`occupancy 0 reached cap 3`) and then proves the
/// denial is attributable specifically to the missing topology gate by opening
/// only that gate and watching the same request be admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn unconfirmed_topology_still_fails_closed_and_never_lifts() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, observe_config());

    // Every startup gate except coordinator leadership.
    boot_to_topology_gate(&state).await;
    let controller = admission(&state).clone();
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Enforce,
        "the durable row promotes the configured Observe mode"
    );
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::TopologyPending,
        "leadership was never acquired"
    );

    // The incident signature: a denial whose occupancy is nowhere near the cap.
    assert_eq!(
        admit(&state, "wedged-task").await,
        BuildAdmissionDecision::Denied {
            occupancy: None,
            cap: 3,
            cause: DenialCause::ControllerNotAdmitting
        },
        "an unconfirmed topology MUST fail closed"
    );

    // No gate short of topology may complete the epoch, and nothing lifts.
    assert_eq!(
        handoff_repository(&state)
            .read()
            .await
            .expect("read")
            .expect("row")
            .emergency_ack_epoch,
        None,
        "only a healthy controller may acknowledge the durable epoch"
    );
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Unleased,
        "an incomplete epoch never lifts"
    );

    // Ticking the leader loop must not launder the missing gate into an ack.
    leader_handoff_tick(&state).await;
    assert_eq!(
        handoff_repository(&state)
            .read()
            .await
            .expect("read")
            .expect("row")
            .emergency_ack_epoch,
        None,
        "the handoff loop must not acknowledge from an unready controller"
    );
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::TopologyPending
    );

    // The control: opening ONLY the topology gate admits the same request, so
    // the denial above is attributable to that gate and nothing else.
    state.confirm_build_admission_topology().await;
    assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
    assert!(
        matches!(
            admit(&state, "unwedged-task").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "confirming topology is what opens admission"
    );
}

/// The documented 2026-07-19 remediation (deleting the row) stays safe, and
/// `epoch seed` restores the rollout without re-arming the wedge.
///
/// Production is in exactly this state right now: the row is absent because an
/// operator deleted it. This pins both halves of getting back — that the absent
/// state is not itself a wedge, and that the restore lands on a *complete*
/// baseline rather than the unacknowledged epoch the incident was about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn absent_row_keeps_configured_mode_and_seed_restores_a_complete_baseline() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, observe_config());
    handoff_repository(&state)
        .delete_for_test()
        .await
        .expect("delete the row exactly as the 2026-07-19 remediation did");

    boot_through_leadership(&state).await;
    let controller = admission(&state).clone();
    // Absence is mapped to the CONFIGURED standalone mode — it does not invent
    // rollout state and does not promote to a fail-closed Enforce.
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Observe,
        "an absent row preserves the configured standalone mode"
    );
    assert!(
        matches!(
            admit(&state, "absent-row-task").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the remediated deployment admits work"
    );
    assert_eq!(
        launcher_lift(&state).await,
        InvocationLiftDecision::Unleased,
        "an absent row never lifts, so arming is useless until it is restored"
    );

    // Restore. The seed is born acknowledged, so it lands directly on a
    // COMPLETE emergency-primary baseline: the deployment never has to climb
    // out of the unacknowledged epoch that started the incident.
    let rendered = epoch_cli(&state, EpochAction::Seed).await;
    assert!(rendered.starts_with("seed: applied"), "{rendered}");
    let seeded = handoff_repository(&state)
        .read()
        .await
        .expect("read")
        .expect("row");
    assert_eq!(seeded.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert_eq!(seeded.v0_mode, V0Mode::Enforce);
    assert_eq!(seeded.v1_mode, V1Mode::Off);
    assert_eq!(
        seeded.emergency_ack_epoch,
        Some(seeded.epoch),
        "the restored row must be complete, NOT the unacknowledged incident shape"
    );

    // A restart on the restored row opens without operator intervention.
    let restarted = state_for_admission_config_with_db(state.db().clone(), observe_config());
    boot_through_leadership(&restarted).await;
    assert_eq!(
        admission(&restarted).readiness(),
        BuildAdmissionReadiness::Healthy,
        "the restored baseline self-opens"
    );
    assert!(matches!(
        admit(&restarted, "restored-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
}

/// Re-seeding the row into an ALREADY-RUNNING deployment must not re-arm the
/// 2026-07-19 outage.
///
/// Production is configured `mode: observe` with `maxBuildTaskRuns: 3` (the Helm
/// chart's `buildAdmission`), and its row is currently absent, so its controller
/// is a benign standalone Observe that never denies. The moment an operator
/// restores the row, the durable state reads `RequiredFailClosed`, and the
/// periodic handoff loop promotes the Observe controller through
/// `require_enforcement()` — which resets EVERY startup gate.
///
/// Those gates are otherwise walked only by `initialize()` and by
/// `become_leader()`, neither of which runs again inside a live process. So
/// without the re-establishment this test pins, the promoted controller parks
/// fail-closed forever and denies every admission with the incident's exact
/// self-contradictory signature, recoverable only by restarting the pod.
///
/// This is why the incident happened even though #2264 had already wired
/// `mark_topology_ready` to leadership the day before: the missing piece was
/// never the call site, it was that the LIVE promotion path resets the gates with
/// nothing left to reopen them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn seeding_a_live_observe_deployment_does_not_wedge_admission() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, observe_config());
    // Production's current state: the row was deleted by the 2026-07-19
    // remediation, so the deployment booted and won leadership as Observe.
    handoff_repository(&state)
        .delete_for_test()
        .await
        .expect("delete the row");
    boot_through_leadership(&state).await;
    let controller = admission(&state).clone();
    assert_eq!(controller.mode(), BuildAdmissionMode::Observe);
    assert!(matches!(
        admit(&state, "pre-seed-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));

    // The operator restores the row against the LIVE deployment.
    let rendered = epoch_cli(&state, EpochAction::Seed).await;
    assert!(rendered.starts_with("seed: applied"), "{rendered}");

    // The next periodic handoff tick promotes Observe → Enforce. This is the
    // exact moment the outage began.
    leader_handoff_tick(&state).await;
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Enforce,
        "a durable emergency-primary row must promote the configured Observe mode"
    );

    // The promotion must NOT leave the controller parked behind its own reset
    // gates. Nothing restarts this process, so readiness restored here or never.
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::Healthy,
        "promoting a live leader must re-establish the startup gates it reset"
    );
    assert_ne!(
        admit(&state, "post-seed-task").await,
        BuildAdmissionDecision::Denied {
            occupancy: None,
            cap: 3,
            cause: DenialCause::ControllerNotAdmitting
        },
        "re-seeding must not reproduce the incident's self-contradictory denial"
    );
    assert!(
        matches!(
            admit(&state, "post-seed-task-2").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the restored deployment must keep admitting work"
    );

    // A further tick is stable — the promotion is not a flapping loop.
    leader_handoff_tick(&state).await;
    assert_eq!(controller.mode(), BuildAdmissionMode::Enforce);
    assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
}

/// A STANDBY must not re-open its own topology gate.
///
/// This is the load-bearing half of re-establishing the gates after a live
/// promotion. The journal and Kubernetes gates are re-derived from real checks,
/// but the topology gate encodes "this process holds the coordinator advisory
/// lock" — which cannot be re-checked, only remembered. If re-establishment
/// asserted it unconditionally, every standby pod would promote itself into a
/// healthy Enforce writer and the single-active invariant would be gone, which is
/// precisely the corruption the gate exists to prevent.
///
/// So a pod that never won leadership must stay fail-closed at `TopologyPending`
/// even though it ran the same promotion path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn a_standby_promotion_never_re_asserts_the_topology_gate() {
    let _telemetry_guard = BUILD_ADMISSION_TELEMETRY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let db = Database::open_in_memory().expect("test database");
    let state = state_for_admission_config_with_db(db, observe_config());
    handoff_repository(&state)
        .delete_for_test()
        .await
        .expect("delete the row");

    // A standby: every startup gate EXCEPT coordinator leadership.
    boot_to_topology_gate(&state).await;
    let controller = admission(&state).clone();
    assert_eq!(controller.mode(), BuildAdmissionMode::Observe);

    // Restore the row, then run the same promotion path the leader ran.
    epoch_cli(&state, EpochAction::Seed).await;
    leader_handoff_tick(&state).await;
    assert_eq!(
        controller.mode(),
        BuildAdmissionMode::Enforce,
        "the durable row promotes a standby's controller too"
    );

    // The journal and inventory gates were legitimately re-derived, so the
    // remaining closed gate must be exactly topology — not an earlier one.
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::TopologyPending,
        "a standby must NOT re-assert a topology gate it never held"
    );
    assert_eq!(
        admit(&state, "standby-task").await,
        BuildAdmissionDecision::Denied {
            occupancy: None,
            cap: 3,
            cause: DenialCause::ControllerNotAdmitting
        },
        "a promoted standby must fail closed"
    );
    assert_eq!(
        handoff_repository(&state)
            .read()
            .await
            .expect("read")
            .expect("row")
            .invocation_ack_epoch,
        None,
        "a standby writes no authority acknowledgement"
    );

    // Winning the lock is the only thing that opens it.
    state.confirm_build_admission_topology().await;
    assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
    assert!(matches!(
        admit(&state, "promoted-leader-task").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
}
