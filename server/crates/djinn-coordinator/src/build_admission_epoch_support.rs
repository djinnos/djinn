//! Shared harness for the versioned admission-epoch transition matrix (ujvz).
//!
//! The matrix drives the *merged* pieces of the rollout and re-implements none
//! of them:
//!
//! - [`AdmissionTransitionExecutor`] (twqh) is the only operator-facing writer
//!   of durable phase/mode/cap transitions.
//! - [`evaluate_handoff`] (icvs) is the v0 (emergency) side projection the
//!   coordinator leader applies on every live handoff tick.
//! - [`evaluate_invocation_lift`] (keoq) is the v1 (invocation) side projection
//!   the agent/launcher applies before it lifts the reserved quota.
//! - [`BuildAdmissionController`] and [`BuildLeaseService`] are the two real
//!   cap-enforcing authorities.
//!
//! # The two invariants asserted at every observed committed state
//!
//! 1. **At least one enforcing authority.** `v0_enforcing || v1_enforcing`,
//!    where `v0_enforcing` is the emergency controller's state *after* one
//!    leader tick (the tick is what releases or re-closes v0) and
//!    `v1_enforcing` is the launcher projection actually authorizing the v1
//!    regime for this epoch. The sharp edge of the proof is the companion
//!    assertion that [`EmergencyAuthorityDecision::MayDisable`] — the sole v0
//!    release point — never occurs unless v1 is genuinely lifting.
//! 2. **The cap is never exceeded at full speed.** Every enforcing authority is
//!    driven with a concurrent burst larger than the epoch's reference cap, and
//!    warm consumers plus above-unleased task trees
//!    (`count_task_or_warm_occupancy`, which deliberately excludes the
//!    below-the-lease invocation children) never exceed it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use djinn_db::{
    AdmissionDomain, AdmissionHandoffAuthority, AdmissionHandoffRepository, AdmissionHandoffRow,
    AdmissionJournalRepository, BuildLeaseRepository, Database,
};
use djinn_k8s::{WarmAdmission, WarmAdmissionRequest, WarmAdmissionTransition};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, InvocationLiftDecision, LeaseAbandonRequest, LeaseDeadlines,
    LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult, LeaseState,
    LeaseStatusRequest, evaluate_invocation_lift,
};
use tokio::sync::Barrier;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode,
};
use crate::build_admission_handoff::{
    EmergencyAuthorityDecision, HandoffSnapshot, InvocationAuthorityObservation, evaluate_handoff,
};
use crate::build_admission_transition::AdmissionTransitionExecutor;
use crate::build_lease::{
    BuildLeaseService, ManualLeaseClock, NoopLeaseTelemetry, NoopLeaseTransactionPause,
};

/// Reference cap used when the durable row inherits the controller default
/// (`cap = NULL`). The seeded row carries no cap, so the baseline observation
/// still needs a concrete number to burst against.
pub(crate) const INHERITED_CAP: i64 = 3;

/// One observed committed durable state plus both authority projections.
#[derive(Clone, Debug)]
pub(crate) struct EpochObservation {
    pub(crate) label: String,
    pub(crate) row: AdmissionHandoffRow,
    pub(crate) snapshot: HandoffSnapshot,
    pub(crate) lift: InvocationLiftDecision,
    /// The emergency controller still enforces after the leader tick.
    pub(crate) v0_enforcing: bool,
    /// The invocation authority is live for this epoch (v1 mode enforcing and
    /// the launcher projection authorizes the lift).
    pub(crate) v1_enforcing: bool,
    /// Reference cap in force for this epoch.
    pub(crate) cap: i64,
}

/// A deterministic world holding one durable epoch row, one admission journal,
/// one lease ledger, and the leader's in-memory emergency controller.
pub(crate) struct EpochWorld {
    db: Database,
    pub(crate) handoff: Arc<AdmissionHandoffRepository>,
    pub(crate) journal: Arc<AdmissionJournalRepository>,
    pub(crate) leases: Arc<BuildLeaseRepository>,
    pub(crate) executor: AdmissionTransitionExecutor,
    /// The coordinator leader's emergency controller. Its mode is driven only
    /// by [`Self::leader_tick`], exactly as `finalize_build_admission_handoff`
    /// drives it in the server.
    leader: BuildAdmissionController,
}

/// Process-global burst counter. Worlds share the journal across a restart, so
/// burst identities must stay unique for the whole test binary or a replayed
/// index would collide with a retained terminal row.
static BURST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn next_burst() -> usize {
    BURST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
}

impl EpochWorld {
    pub(crate) fn new() -> Self {
        let db = Database::open_in_memory().unwrap();
        let handoff = Arc::new(AdmissionHandoffRepository::new(db.clone()));
        Self {
            journal: Arc::new(AdmissionJournalRepository::new(db.clone())),
            leases: Arc::new(BuildLeaseRepository::new(db.clone())),
            executor: AdmissionTransitionExecutor::new(Arc::clone(&handoff)),
            leader: BuildAdmissionController::new(
                Arc::new(AdmissionJournalRepository::new(db.clone())),
                BuildAdmissionMode::Enforce,
                INHERITED_CAP,
                "ujvz-leader",
            ),
            handoff,
            db,
        }
    }

    /// Simulate a coordinator restart: the durable state is untouched and a new
    /// process comes up with a fresh, fail-closed emergency controller.
    pub(crate) fn restart(&self) -> Self {
        let handoff = Arc::new(AdmissionHandoffRepository::new(self.db.clone()));
        let leader = BuildAdmissionController::new_closed(
            Arc::new(AdmissionJournalRepository::new(self.db.clone())),
            INHERITED_CAP,
            "ujvz-leader-restarted",
        );
        // Journal recovery, Kubernetes inventory, and the single-active
        // topology gate all complete before the leader reads the epoch.
        leader.mark_ready();
        Self {
            db: self.db.clone(),
            journal: Arc::clone(&self.journal),
            leases: Arc::clone(&self.leases),
            executor: AdmissionTransitionExecutor::new(Arc::clone(&handoff)),
            leader,
            handoff,
        }
    }

    pub(crate) async fn row(&self) -> AdmissionHandoffRow {
        self.handoff.read().await.unwrap().unwrap()
    }

    pub(crate) fn leader_mode(&self) -> BuildAdmissionMode {
        self.leader.mode()
    }

    /// One live handoff tick of the coordinator leader, mirroring
    /// `finalize_build_admission_handoff`: read the durable row, project it,
    /// release or re-close the emergency controller, and acknowledge the exact
    /// current epoch when the policy allows it.
    pub(crate) async fn leader_tick(&self) -> HandoffSnapshot {
        let snapshot = self.project().await;
        match snapshot.emergency {
            EmergencyAuthorityDecision::MayDisable => {
                self.leader.disable();
                return snapshot;
            }
            EmergencyAuthorityDecision::RequiredFailClosed
                if self.leader.mode() != BuildAdmissionMode::Enforce =>
            {
                // A durable row that still requires v0 re-closes a released
                // controller and re-runs its startup gates before it may
                // acknowledge anything.
                self.leader.require_enforcement();
                self.leader.mark_ready();
            }
            EmergencyAuthorityDecision::RequiredFailClosed
            | EmergencyAuthorityDecision::ConfiguredStandalone => {}
        }
        let snapshot = self.project().await;
        if snapshot.emergency_acknowledgement_allowed
            && let Some(row) = snapshot
                .row
                .as_ref()
                .filter(|row| row.emergency_ack_epoch != Some(row.epoch))
        {
            self.handoff
                .acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
                .await
                .unwrap();
            return self.project().await;
        }
        snapshot
    }

    /// Project the durable row through the emergency-side policy with the
    /// invocation observation taken from the launcher-side projection, so the
    /// two independent authority readings are never invented by the test.
    async fn project(&self) -> HandoffSnapshot {
        let row = self.handoff.read().await.unwrap();
        let lift = evaluate_invocation_lift(Ok(row.clone()));
        evaluate_handoff(
            Ok(row),
            self.leader.mode(),
            self.leader.mode() == BuildAdmissionMode::Enforce,
            self.leader.readiness(),
            InvocationAuthorityObservation {
                enforcing: lift == InvocationLiftDecision::Lift,
            },
        )
    }

    /// Observe one committed durable state and assert both invariants.
    pub(crate) async fn observe(&self, label: &str) -> EpochObservation {
        let snapshot = self.leader_tick().await;
        let row = self.row().await;
        let lift = evaluate_invocation_lift(Ok(Some(row.clone())));
        let v0_enforcing = self.leader.mode() == BuildAdmissionMode::Enforce;
        let v1_enforcing = row.v1_mode.is_enforcing() && lift == InvocationLiftDecision::Lift;
        let cap = row.cap.unwrap_or(INHERITED_CAP);

        assert_eq!(
            snapshot.emergency == EmergencyAuthorityDecision::MayDisable,
            !v0_enforcing,
            "{label}: the emergency controller is released exactly at MayDisable"
        );
        if snapshot.emergency == EmergencyAuthorityDecision::MayDisable {
            assert!(
                v1_enforcing,
                "{label}: v0 was released while v1 was not enforcing (lift={lift:?}, \
                 v1_mode={:?})",
                row.v1_mode
            );
        }
        assert!(
            v0_enforcing || v1_enforcing,
            "{label}: no enforcing authority at phase {:?} epoch {} (v0={:?}, v1={:?}, \
             state={:?}, lift={lift:?})",
            row.phase,
            row.epoch,
            row.v0_mode,
            row.v1_mode,
            snapshot.state,
        );
        // Bounded telemetry: the warning family is the three contract reasons
        // and nothing derived from an identifier or an epoch number.
        let gauges = snapshot.warning_gauges(
            v0_enforcing,
            InvocationAuthorityObservation {
                enforcing: v1_enforcing,
            },
        );
        assert!(
            gauges.unexpected_overlap + gauges.stale_epoch + gauges.epoch_unreadable <= 1,
            "{label}: at most one bounded handoff warning reason is ever active"
        );

        if v0_enforcing {
            self.assert_v0_cap_holds_at_full_speed(label, cap).await;
        }
        if v1_enforcing {
            self.assert_v1_cap_holds_at_full_speed(label, cap, row.epoch)
                .await;
        }

        EpochObservation {
            label: label.to_owned(),
            row,
            snapshot,
            lift,
            v0_enforcing,
            v1_enforcing,
            cap,
        }
    }

    /// Drive the v0 emergency controller with a concurrent burst larger than
    /// the epoch cap and assert warm consumers plus above-unleased task trees
    /// never exceed it. The journal is returned to zero occupancy afterwards so
    /// a later, lower cap starts from a clean ledger.
    async fn assert_v0_cap_holds_at_full_speed(&self, label: &str, cap: i64) {
        let burst = next_burst();
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&self.journal),
            BuildAdmissionMode::Enforce,
            cap,
            format!("ujvz-burst-{burst}"),
        ));
        let contenders = usize::try_from(cap).unwrap() + 2;
        let barrier = Arc::new(Barrier::new(contenders + 1));
        let mut attempts = Vec::new();
        for index in 0..contenders {
            let controller = Arc::clone(&controller);
            let barrier = Arc::clone(&barrier);
            attempts.push(tokio::spawn(async move {
                barrier.wait().await;
                if index % 2 == 0 {
                    let work_id = format!("burst-{burst}-task-{index}");
                    match controller
                        .admit_task_run(
                            Some("worker"),
                            AdmissionDomain::TaskObservation,
                            work_id,
                            0,
                            format!("burst-{burst}-run-{index}"),
                        )
                        .await
                        .unwrap()
                    {
                        BuildAdmissionDecision::Permitted { permit, .. } => Some(permit),
                        BuildAdmissionDecision::Denied { .. } => None,
                        BuildAdmissionDecision::Unclassified => {
                            panic!("classified worker role must not be unclassified")
                        }
                    }
                } else {
                    WarmAdmission::admit(
                        controller.as_ref(),
                        WarmAdmissionRequest {
                            domain: "ignored".into(),
                            work_id: format!("burst-{burst}-warm-{index}"),
                            generation: 0,
                            object_name: format!("burst-{burst}-warm-object-{index}"),
                        },
                    )
                    .await
                    .ok()
                }
            }));
        }
        barrier.wait().await;
        let mut permits = Vec::new();
        for attempt in attempts {
            if let Some(permit) = attempt.await.unwrap() {
                permits.push(permit);
            }
        }
        let occupancy = self.journal.count_task_or_warm_occupancy().await.unwrap();
        assert!(
            occupancy <= cap,
            "{label}: v0 admitted {occupancy} above the epoch cap {cap}"
        );
        assert_eq!(
            occupancy, cap,
            "{label}: the burst exceeded the cap in contenders, so a healthy \
             enforcing v0 must fill it exactly — never fewer, never more"
        );
        assert_eq!(
            i64::try_from(permits.len()).unwrap(),
            occupancy,
            "{label}: every v0 permit is durably accounted"
        );

        // A below-the-lease invocation child never counts against the parent's
        // cap, which is what "above-unleased task trees" means here.
        let BuildAdmissionDecision::Permitted { permit: child, .. } = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::InvocationBuild,
                format!("burst-{burst}-child"),
                0,
                format!("burst-{burst}-child-job"),
            )
            .await
            .unwrap()
        else {
            panic!("{label}: an invocation child must not be blocked by the parent cap");
        };
        assert_eq!(
            self.journal.count_task_or_warm_occupancy().await.unwrap(),
            occupancy,
            "{label}: invocation children stay below the lease and outside the cap"
        );
        permits.push(child);

        for permit in permits {
            controller
                .transition(
                    &permit,
                    WarmAdmissionTransition::DefinitiveFailure {
                        diagnostic: "burst teardown".into(),
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(
            self.journal.count_task_or_warm_occupancy().await.unwrap(),
            0,
            "{label}: the burst released every slot it took"
        );
    }

    /// Drive the v1 lease authority the same way. Recovery reads the durable
    /// epoch (and its reference cap) before the service opens, so the assertion
    /// also proves the observed epoch is the committed one.
    async fn assert_v1_cap_holds_at_full_speed(&self, label: &str, cap: i64, epoch: i64) {
        let burst = next_burst();
        let service = BuildLeaseService::with_seams(
            Arc::clone(&self.leases),
            0,
            Arc::new(ManualLeaseClock::new(100)),
            Arc::new(NoopLeaseTransactionPause),
            Arc::new(NoopLeaseTelemetry),
        )
        .with_handoff_epoch(Arc::clone(&self.handoff));
        assert!(matches!(service.recover().await, LeaseResult::Status(_)));
        assert_eq!(
            service.observed_epoch(),
            Some(epoch),
            "{label}: the v1 service observes the committed epoch before it opens"
        );

        let contenders = usize::try_from(cap).unwrap() + 2;
        let mut queued = Vec::new();
        let mut granted = 0_i64;
        for index in 0..contenders {
            let identity = LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
                project_id: "ujvz-project".into(),
                warm_request_id: format!("lease-burst-{burst}-{index}"),
                graph_revision: "ujvz-revision".into(),
            });
            let result = service
                .queue(LeaseQueueRequest {
                    identity: identity.clone(),
                    deadlines: LeaseDeadlines {
                        queue_deadline_ms: 0,
                        launch_deadline_ms: 0,
                    },
                })
                .await;
            if matches!(result, LeaseResult::Granted(_)) {
                granted += 1;
            }
            queued.push(identity);
        }
        let snapshot = self.leases.snapshot().await.unwrap();
        assert!(
            snapshot.occupied <= cap,
            "{label}: v1 occupied {} above the epoch cap {cap}",
            snapshot.occupied
        );
        assert_eq!(
            granted, snapshot.occupied,
            "{label}: every v1 grant is durably accounted"
        );
        assert_eq!(
            snapshot.occupied, cap,
            "{label}: the FIFO fills the epoch cap exactly — never fewer, never more"
        );

        // Releasing an occupied lease drains the FIFO into the next queued row,
        // so teardown repeats until every contender is terminal. An occupied
        // lease needs its fencing token; the abandon contract only removes a
        // still-queued row.
        for _ in 0..=contenders {
            let mut remaining = false;
            for identity in &queued {
                let LeaseResult::Status(status) = service
                    .status(LeaseStatusRequest {
                        identity: identity.clone(),
                    })
                    .await
                else {
                    continue;
                };
                if matches!(status.state, LeaseState::Cancelled | LeaseState::Released) {
                    continue;
                }
                remaining = true;
                let identity = identity.clone();
                if let Some(fencing_token) = status.fencing_token {
                    let _ = service
                        .release(LeaseReleaseRequest {
                            identity,
                            fencing_token,
                            candidate_cleanup: false,
                        })
                        .await;
                } else {
                    let _ = service
                        .abandon(LeaseAbandonRequest {
                            identity,
                            candidate_cleanup: false,
                        })
                        .await;
                }
            }
            if !remaining {
                break;
            }
        }
        assert_eq!(
            self.leases.snapshot().await.unwrap().occupied,
            0,
            "{label}: the lease burst released every slot it took"
        );
    }

    /// The invocation authority acknowledges the current epoch, as the operator
    /// executor does while it drives a transition.
    pub(crate) async fn invocation_acks(&self) {
        let row = self.row().await;
        self.handoff
            .acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
            .await
            .unwrap();
    }

    /// Every live generation acknowledges the current epoch.
    pub(crate) async fn generations_ack(&self, generations: &[String]) {
        let row = self.row().await;
        for key in generations {
            self.handoff
                .record_generation_ack(row.epoch, key)
                .await
                .unwrap();
        }
    }
}

/// Assert that a controller reconstructed from the durable state agrees with
/// the harness reading — used by the crash/restart scenarios to prove the
/// earlier safe phase stays authoritative across process loss.
pub(crate) fn assert_restart_is_fail_closed(observation: &EpochObservation) {
    let label = &observation.label;
    assert!(
        observation.v0_enforcing || observation.v1_enforcing,
        "{label}: a restart left no enforcing authority"
    );
    if !observation.v0_enforcing {
        assert_eq!(
            observation.lift,
            InvocationLiftDecision::Lift,
            "{label}: v0 stays released only while v1 is genuinely lifting"
        );
    }
    assert_ne!(
        observation.snapshot.state,
        crate::build_admission_handoff::HandoffState::EpochUnreadable,
        "{label}: the durable epoch is readable after a restart"
    );
}
