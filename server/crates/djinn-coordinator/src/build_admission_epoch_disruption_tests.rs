//! Versioned admission-epoch transition matrix (ujvz): disruptions.
//!
//! Stale and reordered watch deliveries, partial configuration updates, and
//! missing acknowledgements. Each disruption is applied to a real durable epoch
//! and asserted against the same two invariants the cutover scenarios use (see
//! [`crate::build_admission_epoch_support`]), plus the bounded-label contract
//! for the telemetry this rollout emits.

use std::collections::BTreeSet;

use djinn_db::{
    AdmissionHandoffAuthority, AdmissionHandoffPhase, AdmissionHandoffRow, V0Mode, V1Mode,
};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};

use crate::build_admission::{
    BuildAdmissionMode, BuildAdmissionReadiness, MAX_ADMISSION_CAP, MIN_ADMISSION_CAP,
    task_run_generation_key,
};
use crate::build_admission_epoch_support::EpochWorld;
use crate::build_admission_handoff::{
    EmergencyAuthorityDecision, HandoffState, HandoffWarningReason, InvocationAuthorityObservation,
    evaluate_handoff,
};
use crate::build_admission_transition::TransitionError;

fn armed_generations() -> Vec<String> {
    vec![
        task_run_generation_key("task-alpha", 0),
        task_run_generation_key("task-beta", 2),
    ]
}

/// Reach a committed forward overlap: both authorities enforce, both
/// acknowledged, with a concrete reference cap.
async fn overlapping_world(cap: i64) -> EpochWorld {
    let world = EpochWorld::new();
    world.observe("disruption_baseline").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_shadow(epoch, cap).await.unwrap();
    world.observe("disruption_shadow").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_overlap(epoch, cap).await.unwrap();
    world.observe("disruption_overlap_modes").await;
    let epoch = world.row().await.epoch;
    world.executor.enter_forward_overlap(epoch).await.unwrap();
    world.observe("disruption_forward_overlap").await;
    world
}

/// A watch consumer that cached an older row cannot act on it: every mutation
/// carrying a stale epoch is fenced, and re-reading is the only way forward.
/// Reordered deliveries therefore cannot commit a state the durable row does
/// not already permit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_and_reordered_watch_deliveries_are_fenced_and_never_release_v0() {
    let world = overlapping_world(3).await;

    // A watch delivery captured while the shadow was armed, replayed late.
    let stale_shadow = AdmissionHandoffRow {
        phase: AdmissionHandoffPhase::EmergencyPrimary,
        epoch: 1,
        emergency_ack_epoch: Some(1),
        invocation_ack_epoch: None,
        v0_mode: V0Mode::Enforce,
        v1_mode: V1Mode::Shadow,
        cap: Some(3),
        updated_at: "stale-delivery".into(),
    };
    // A delivery from the future that this process has not committed.
    let current = world.row().await;
    let unseen_future = AdmissionHandoffRow {
        phase: AdmissionHandoffPhase::InvocationPrimary,
        epoch: current.epoch + 5,
        emergency_ack_epoch: None,
        invocation_ack_epoch: Some(current.epoch + 5),
        v0_mode: V0Mode::Observe,
        v1_mode: V1Mode::Enforce,
        cap: Some(3),
        updated_at: "reordered-delivery".into(),
    };

    // Read in isolation, a reordered delivery can look permissive…
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(unseen_future.clone()))),
        InvocationLiftDecision::Lift
    );
    // …but every mutation it could motivate is fenced against the durable row.
    for epoch in [stale_shadow.epoch, unseen_future.epoch] {
        assert!(matches!(
            world.executor.set_cap(epoch, 2).await,
            Err(TransitionError::InvalidTransition(_))
        ));
        assert!(matches!(
            world.executor.arm_overlap(epoch, 2).await,
            Err(TransitionError::InvalidTransition(_))
        ));
        assert!(matches!(
            world.executor.commit_invocation_primary(epoch, &[]).await,
            Err(TransitionError::InvalidTransition(_))
        ));
        for authority in [
            AdmissionHandoffAuthority::Emergency,
            AdmissionHandoffAuthority::Invocation,
        ] {
            assert!(
                world.handoff.acknowledge(authority, epoch).await.is_err(),
                "an acknowledgement for epoch {epoch} is not current"
            );
        }
        assert!(
            world
                .handoff
                .record_generation_ack(epoch, &armed_generations()[0])
                .await
                .is_err(),
            "a generation acknowledgement for epoch {epoch} is not current"
        );
        assert!(
            world
                .handoff
                .advance(epoch, AdmissionHandoffPhase::InvocationPrimary, &[])
                .await
                .is_err()
        );
    }

    // Nothing moved, and the committed state still holds both invariants.
    let after = world.observe("after_reordered_deliveries").await;
    assert_eq!(after.row, current, "no fenced attempt mutated the row");
    assert!(after.v0_enforcing && after.v1_enforcing);

    // The v1 authority binds to the epoch it actually observed, not to any
    // delivery: a stale delivery cannot make it act on a superseded cap.
    assert_eq!(after.row.cap, Some(3));
    assert_ne!(after.row.epoch, stale_shadow.epoch);
    assert_ne!(after.row.epoch, unseen_future.epoch);
}

/// Applying acknowledgements out of order (invocation before emergency, or an
/// authority re-acknowledging a superseded epoch) never opens an edge early.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reordered_acknowledgements_never_open_an_edge_early() {
    let world = EpochWorld::new();
    world.observe("reorder_baseline").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_overlap(epoch, 2).await.unwrap();

    // The invocation authority acknowledges first; the emergency ack the
    // EmergencyPrimary edge actually requires is still missing.
    world.invocation_acks().await;
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.enter_forward_overlap(epoch).await,
        Err(TransitionError::AwaitingEmergencyAck { .. })
    ));
    assert!(
        world
            .handoff
            .advance(epoch, AdmissionHandoffPhase::ForwardOverlap, &[])
            .await
            .is_err(),
        "the durable edge requires the outgoing primary authority"
    );
    let blocked = world.observe("invocation_acked_first").await;
    assert_eq!(blocked.row.phase, AdmissionHandoffPhase::EmergencyPrimary);
    assert!(blocked.v0_enforcing);

    // Now the edge opens, and the superseded epoch's acknowledgements are dead.
    let epoch = world.row().await.epoch;
    let overlap = world.executor.enter_forward_overlap(epoch).await.unwrap();
    assert!(
        world
            .handoff
            .acknowledge(AdmissionHandoffAuthority::Invocation, epoch)
            .await
            .is_err(),
        "the pre-advance epoch can no longer be acknowledged"
    );
    assert_eq!(overlap.phase, AdmissionHandoffPhase::ForwardOverlap);
    let committed = world.observe("reordered_overlap").await;
    assert!(committed.v0_enforcing && committed.v1_enforcing);
}

/// A configuration update that lands partially — modes written without the
/// acknowledgements that make them authoritative, or an illegal combination
/// written outside the operator executor — always fails closed onto v0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_config_updates_fail_closed_onto_the_emergency_authority() {
    let world = overlapping_world(3).await;

    // 1. An out-of-range cap never reaches the durable row.
    let before = world.row().await;
    for cap in [0, MAX_ADMISSION_CAP + 1] {
        assert!(matches!(
            world.executor.set_cap(before.epoch, cap).await,
            Err(TransitionError::InvalidConfig(_))
        ));
    }
    assert_eq!(world.row().await, before, "a rejected cap changed nothing");

    // 2. A legal cap change re-collects acknowledgements; the window between
    //    the write and the acks is an incomplete epoch that lifts nothing.
    let recapped = world.executor.set_cap(before.epoch, 2).await.unwrap();
    assert_eq!(recapped.cap, Some(2));
    assert_ne!(
        recapped.emergency_ack_epoch,
        Some(recapped.epoch),
        "the mode/cap write cleared the v0 acknowledgement"
    );
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(recapped.clone()))),
        InvocationLiftDecision::Unleased,
        "a partially acknowledged epoch never lifts v1"
    );
    let recapped_state = world.observe("cap_changed").await;
    assert_eq!(recapped_state.cap, 2, "the lowered cap is now in force");
    assert!(recapped_state.v0_enforcing);

    // 3. The illegal both-non-enforcing combination written straight to the
    //    durable row (an operator editing modes outside the executor) fails
    //    closed in BOTH projections.
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.arm_rollback(epoch, 2).await,
        Err(TransitionError::InvalidTransition(_))
    ));
    world
        .handoff
        .set_modes_and_cap(epoch, V0Mode::Observe, V1Mode::Shadow, Some(2))
        .await
        .unwrap();
    let illegal = world.row().await;
    let snapshot = evaluate_handoff(
        Ok(Some(illegal.clone())),
        BuildAdmissionMode::Enforce,
        true,
        BuildAdmissionReadiness::Healthy,
        InvocationAuthorityObservation::default(),
    );
    assert_eq!(snapshot.state, HandoffState::IllegalModeCombo);
    assert_eq!(
        snapshot.emergency,
        EmergencyAuthorityDecision::RequiredFailClosed
    );
    assert_eq!(
        evaluate_invocation_lift(Ok(Some(illegal))),
        InvocationLiftDecision::Unleased
    );
    let observed = world.observe("illegal_mode_combo").await;
    assert!(
        observed.v0_enforcing && !observed.v1_enforcing,
        "an illegal combination leaves the emergency authority in charge"
    );
    assert_eq!(world.leader_mode(), BuildAdmissionMode::Enforce);
}

/// Every phase's required acknowledgements gate its edge, and the durable state
/// stays safe while the edge is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_acknowledgements_block_every_edge_including_invocation_primary() {
    let armed = armed_generations();

    // ── EmergencyPrimary needs the v0 acknowledgement. ────────────────────
    let world = EpochWorld::new();
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.enter_forward_overlap(epoch).await,
        Err(TransitionError::AwaitingEmergencyAck { epoch: 0 })
    ));

    // ── ForwardOverlap needs both authorities plus every live generation. ─
    let world = overlapping_world(3).await;
    let epoch = world.row().await.epoch;
    // The armed live set has not acknowledged: the edge is closed and reports
    // only how many are missing, never which.
    let error = world
        .executor
        .commit_invocation_primary(epoch, &armed)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        TransitionError::AwaitingGenerationAcks { missing: 2, .. }
    ));
    let message = error.to_string();
    for key in &armed {
        assert!(
            !message.contains(key),
            "a blocked-edge error must not carry generation identities"
        );
    }
    // The durable edge is closed too, not merely the executor's guard.
    assert!(
        world
            .handoff
            .advance(epoch, AdmissionHandoffPhase::InvocationPrimary, &armed)
            .await
            .is_err()
    );
    let blocked = world.observe("missing_generation_acks").await;
    assert_eq!(blocked.row.phase, AdmissionHandoffPhase::ForwardOverlap);
    assert!(blocked.v0_enforcing && blocked.v1_enforcing);

    // A live generation that acknowledges a superseded epoch does not count.
    world
        .handoff
        .record_generation_ack(epoch, &armed[0])
        .await
        .unwrap();
    assert!(
        !world
            .handoff
            .generation_ack_complete(epoch, &armed)
            .await
            .unwrap()
    );
    world.generations_ack(&armed[1..]).await;
    assert!(
        world
            .handoff
            .generation_ack_complete(epoch, &armed)
            .await
            .unwrap()
    );

    // ── InvocationPrimary needs the v1 acknowledgement to leave. ──────────
    let epoch = world.row().await.epoch;
    world
        .executor
        .commit_invocation_primary(epoch, &armed)
        .await
        .unwrap();
    world.observe("committed_primary").await;
    let epoch = world.row().await.epoch;
    world.executor.arm_rollback(epoch, 3).await.unwrap();
    // v0 has not re-acknowledged the rollback epoch yet.
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.enter_rollback_overlap(epoch).await,
        Err(TransitionError::HaltedV0Unconfirmed { .. })
    ));
    let halted = world.observe("rollback_awaiting_v0").await;
    assert!(halted.v0_enforcing, "the halt kept v0 enforcing");

    // ── RollbackOverlap needs both authorities to leave. ──────────────────
    let epoch = world.row().await.epoch;
    world.executor.enter_rollback_overlap(epoch).await.unwrap();
    let epoch = world.row().await.epoch;
    assert!(matches!(
        world.executor.complete_rollback(epoch).await,
        Err(TransitionError::AwaitingEmergencyAck { .. })
    ));
    let overlap = world.observe("rollback_overlap_awaiting_v0").await;
    assert!(overlap.v0_enforcing && overlap.v1_enforcing);
    let epoch = world.row().await.epoch;
    world.executor.complete_rollback(epoch).await.unwrap();
    let done = world.observe("rollback_done").await;
    assert!(done.v0_enforcing && !done.v1_enforcing);
}

/// Extract the label block of every rendered sample line for `metric`.
fn label_blocks(rendered: &str, metric: &str) -> Vec<String> {
    rendered
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter(|line| line.starts_with(metric))
        .map(|line| {
            line.split_once('{')
                .and_then(|(_, rest)| rest.split_once('}'))
                .map_or_else(String::new, |(labels, _)| labels.to_owned())
        })
        .collect()
}

/// Split one rendered label block into `(key, value)` pairs.
fn label_pairs(block: &str) -> Vec<(String, String)> {
    if block.is_empty() {
        return Vec::new();
    }
    block
        .split(',')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap();
            (key.trim().to_owned(), value.trim_matches('"').to_owned())
        })
        .collect()
}

/// Every metric this rollout emits carries only closed-enumeration labels, and
/// identifiers stay in logs and traces.
///
/// The v1 lease family (`operation` / `consumer` / `state` / `outcome`, which is
/// where abandon, warm bind, timeout, and occupancy live) has its own
/// exhaustive contract test beside the service:
/// `crate::build_lease::tests::production_metrics_use_only_bounded_typed_labels`.
/// This test covers the epoch/handoff, shadow-escalation, admission-health, and
/// run-dir reclamation families, which had no label contract.
#[test]
fn epoch_and_admission_telemetry_labels_stay_bounded_and_carry_no_identifiers() {
    // Identity-bearing inputs held by the code that emits, so a leak would show.
    const WORK_ID: &str = "ujvz-work-secret-7f91";
    const POD_UID: &str = "ujvz-pod-secret-7f91";
    let generation = task_run_generation_key("ujvz-task-secret-7f91", 4);

    let (reasons, rendered) = djinn_telemetry::render_isolated(|| {
        // Drive the handoff warning family through the coordinator's own
        // bounded projection rather than through literal strings.
        let mut reasons = BTreeSet::new();
        for row in [
            Err(()),
            Ok(None),
            Ok(Some(AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::ForwardOverlap,
                epoch: 41,
                emergency_ack_epoch: Some(40),
                invocation_ack_epoch: Some(41),
                v0_mode: V0Mode::Enforce,
                v1_mode: V1Mode::Enforce,
                cap: Some(3),
                updated_at: WORK_ID.into(),
            })),
            Ok(Some(AdmissionHandoffRow {
                phase: AdmissionHandoffPhase::EmergencyPrimary,
                epoch: 41,
                emergency_ack_epoch: Some(41),
                invocation_ack_epoch: None,
                v0_mode: V0Mode::Enforce,
                v1_mode: V1Mode::Off,
                cap: Some(3),
                updated_at: POD_UID.into(),
            })),
        ] {
            let snapshot = evaluate_handoff(
                row,
                BuildAdmissionMode::Enforce,
                true,
                BuildAdmissionReadiness::Healthy,
                InvocationAuthorityObservation { enforcing: true },
            );
            let reason =
                snapshot.warning_reason(true, InvocationAuthorityObservation { enforcing: true });
            djinn_telemetry::build_admission::set_handoff_warning(
                reason.map(HandoffWarningReason::as_str),
            );
            if let Some(reason) = reason {
                reasons.insert(reason.as_str());
            }
        }

        // Shadow-mode escalation/throttle observations. Only `would_escalate`
        // has a production caller today (the complementary `would_throttle`
        // branch is deferred with the shadow broker check); both label values
        // are asserted so the family stays closed when it is wired.
        djinn_telemetry::build_admission::record_shadow_invocation(true);
        djinn_telemetry::build_admission::record_shadow_invocation(false);

        for mode in ["enforce", "observe", "off"] {
            djinn_telemetry::build_admission::set_health(mode, 3, true, true, true);
            djinn_telemetry::build_admission::increment_would_defer(mode, 3);
            djinn_telemetry::build_admission::increment_unknown_classification(mode, 3);
        }

        // Occupancy and queue-wait telemetry for the same admissions.
        djinn_telemetry::build_slot_occupancy::set_slots_in_use(2);
        djinn_telemetry::build_slot_occupancy::set_slots_queued(1);
        for outcome in [
            djinn_telemetry::build_slot_queue::OUTCOME_ADMITTED,
            djinn_telemetry::build_slot_queue::OUTCOME_CANCELLED,
            djinn_telemetry::build_slot_queue::OUTCOME_SHUTDOWN,
        ] {
            djinn_telemetry::build_slot_queue::record_wait_seconds(
                outcome,
                std::time::Duration::from_millis(5),
            );
        }

        // Reclamation / unleased run-dir families.
        for state in [
            djinn_telemetry::run_dir::STATE_RESERVED,
            djinn_telemetry::run_dir::STATE_READY_ACTIVE,
            djinn_telemetry::run_dir::STATE_RECLAIMABLE,
            djinn_telemetry::run_dir::STATE_QUARANTINED_UNOWNED,
        ] {
            djinn_telemetry::run_dir::set_state_count(state, 1);
        }
        for tier in [
            djinn_telemetry::run_dir::RECLAIM_TIER_RECLAIMABLE,
            djinn_telemetry::run_dir::RECLAIM_TIER_READY_IDLE,
            djinn_telemetry::run_dir::RECLAIM_TIER_WARM_BASE_AUX,
        ] {
            djinn_telemetry::run_dir::increment_reclaim(tier, 1, 1024);
        }
        djinn_telemetry::run_dir::increment_queue_reason(
            djinn_telemetry::run_dir::QUEUE_REASON_DISK_PRESSURE,
        );
        djinn_telemetry::run_dir::increment_quota_failure(
            djinn_telemetry::run_dir::QUOTA_FAILURE_PROBE_UNAVAILABLE,
        );
        reasons
    });

    // The bounded projection produced exactly the contract reasons.
    assert_eq!(
        reasons,
        BTreeSet::from(["unexpected_overlap", "stale_epoch", "epoch_unreadable"])
    );

    let bounded: [(&str, &[&str]); 9] = [
        (
            "reason",
            &[
                "unexpected_overlap",
                "stale_epoch",
                "epoch_unreadable",
                "disk_pressure",
                "probe_unavailable",
            ],
        ),
        ("effective_mode", &["enforce", "observe", "off"]),
        ("decision", &["would_escalate", "would_throttle"]),
        (
            "outcome",
            &["admitted", "cancelled", "shutdown", "seeded", "reseeded"],
        ),
        (
            "state",
            &[
                "absent",
                "reserved",
                "seeding",
                "ready_active",
                "ready_idle",
                "reclaimable",
                "reclaiming",
                "quarantined_unowned",
            ],
        ),
        ("tier", &["reclaimable", "ready_idle", "warm_base_aux"]),
        // Numeric, not identities: histogram bucket bounds and quantiles.
        ("le", &[]),
        ("quantile", &[]),
        ("effective_cap", &[]),
    ];
    let allowed_keys: BTreeSet<&str> = bounded.iter().map(|(key, _)| *key).collect();

    let mut seen_keys = BTreeSet::new();
    for line in rendered.lines() {
        if line.starts_with('#') || !line.starts_with("djinn_") {
            continue;
        }
        let Some((_, rest)) = line.split_once('{') else {
            continue;
        };
        let Some((block, _)) = rest.split_once('}') else {
            continue;
        };
        for (key, value) in label_pairs(block) {
            assert!(
                allowed_keys.contains(key.as_str()),
                "unbounded label key `{key}` in `{line}`"
            );
            seen_keys.insert(key.clone());
            match key.as_str() {
                // Histogram bucket boundaries and quantiles are numeric.
                "le" | "quantile" => {
                    assert!(value == "+Inf" || value.parse::<f64>().is_ok());
                }
                // The reference cap is bounded by the configuration validator.
                "effective_cap" => {
                    let cap: i64 = value.parse().unwrap();
                    assert!((MIN_ADMISSION_CAP..=MAX_ADMISSION_CAP).contains(&cap));
                }
                other => {
                    let (_, allowed) = bounded.iter().find(|(key, _)| *key == other).unwrap();
                    assert!(
                        allowed.contains(&value.as_str()),
                        "unbounded value `{value}` for label `{other}`"
                    );
                }
            }
        }
    }
    assert!(
        seen_keys.contains("reason") && seen_keys.contains("decision"),
        "the handoff and shadow families must actually have rendered"
    );

    // No identifier reaches a label, by value or by key.
    for identifier in [WORK_ID, POD_UID, generation.as_str()] {
        assert!(
            !rendered.contains(identifier),
            "identifier `{identifier}` leaked into telemetry"
        );
    }
    for key in [
        "work_id=",
        "uid=",
        "epoch=",
        "task_id=",
        "session_id=",
        "project_id=",
        "user_id=",
        "generation=",
        "pod_uid=",
    ] {
        assert!(
            !rendered.contains(key),
            "identity label `{key}` was emitted"
        );
    }
    // The handoff warning family is exactly three series, all present.
    assert_eq!(
        label_blocks(&rendered, "djinn_build_admission_handoff_warning").len(),
        3
    );
}
