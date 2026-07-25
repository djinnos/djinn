//! The single build-slot capacity authority, over the v1 lease FIFO.
//!
//! # Why this adapter exists
//!
//! `DJINN_MAX_BUILD_TASKRUNS` used to be read by two independent authorities
//! that each enforced it over a DISJOINT population: the v0 admission journal
//! governed task-run dispatch, and the v1 build lease governed graph warming.
//! At `enforce` the node therefore ran 2x the operator's configured
//! concurrency. They were also structurally blind to each other -- a leased
//! warm Job wrote no journal row at all -- so neither could have observed the
//! other's occupancy even if it had wanted to.
//!
//! Capacity is now decided in exactly one place: `build_leases`, where every
//! population is a row and occupancy is one weighted `SUM` inside one
//! advisory-locked transaction. This adapter is how layer-1 dispatch admission
//! reaches it, so `BuildAdmissionController` never learns what a lease is and
//! never learns where the cap came from.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_supervisor::services::{
    LeaseAbandonRequest, LeaseDeadlines, LeaseGrantRequest, LeaseIdentity, LeaseReleaseRequest,
    LeaseResult, LeaseState, LeaseStatusRequest, TaskDispatchLeaseIdentity,
};

use crate::build_admission::{BuildSlotAuthority, DispatchSlotOutcome};
use crate::build_lease::BuildLeaseService;

/// Layer-1 dispatch admission over the one v1 lease FIFO.
pub struct BuildLeaseDispatchAuthority {
    service: Arc<BuildLeaseService>,
}

impl BuildLeaseDispatchAuthority {
    #[must_use]
    pub fn new(service: Arc<BuildLeaseService>) -> Self {
        Self { service }
    }

    fn identity(task_id: &str, generation: i64) -> LeaseIdentity {
        LeaseIdentity::TaskDispatch(TaskDispatchLeaseIdentity {
            task_id: task_id.to_owned(),
            generation,
        })
    }
}

#[async_trait]
impl BuildSlotAuthority for BuildLeaseDispatchAuthority {
    async fn acquire_dispatch_slot(&self, task_id: &str, generation: i64) -> DispatchSlotOutcome {
        if !self.service.is_ready() {
            // Recovery has not completed, so occupancy is unknown. Unknown is
            // never permission.
            return DispatchSlotOutcome::Unavailable;
        }

        // ── Shadow ──────────────────────────────────────────────────────────
        //
        // Until the durable epoch arms the v1 authority, dispatch must not
        // change behaviour: landing the unified pool cannot itself flip
        // enforcement on. Probe occupancy read-only and report what enforcement
        // WOULD have done. Deliberately no row is inserted -- a shadow row
        // would occupy real capacity and start denying graph warming, which is
        // exactly the silent behaviour change this branch is avoiding.
        if !self.service.dispatch_enforcing() {
            let would_defer = match self.service.recovery_snapshot().await {
                Ok(snapshot) => {
                    let weight = self.service.slot_weights().dispatch().slots();
                    snapshot.occupied.saturating_add(weight) > snapshot.cap
                }
                Err(()) => false,
            };
            return DispatchSlotOutcome::Observed { would_defer };
        }

        let identity = Self::identity(task_id, generation);
        let grant = match self
            .service
            .queue(LeaseQueueRequestShim::build(identity.clone()))
            .await
        {
            LeaseResult::Granted(grant) => grant,
            // Queued means the pool is full and this attempt now holds a FIFO
            // position. The task stays queued and the dispatcher retries; the
            // row is reclaimed by its queue deadline if the task never returns.
            LeaseResult::Queued(_) => {
                return match self.occupancy().await {
                    Some(occupancy) => DispatchSlotOutcome::AtCapacity {
                        occupancy,
                        cap: self.cap(),
                    },
                    None => DispatchSlotOutcome::Unavailable,
                };
            }
            // A grant that arrived earlier and is already launching/bound is
            // this same attempt's capacity, replayed. It is held, not lost.
            LeaseResult::Status(status)
                if matches!(
                    status.state,
                    LeaseState::Granted
                        | LeaseState::Launching
                        | LeaseState::Bound
                        | LeaseState::Active
                ) =>
            {
                return DispatchSlotOutcome::Granted;
            }
            LeaseResult::LeaseWaitTimeout { .. } => {
                return match self.occupancy().await {
                    Some(occupancy) => DispatchSlotOutcome::AtCapacity {
                        occupancy,
                        cap: self.cap(),
                    },
                    None => DispatchSlotOutcome::Unavailable,
                };
            }
            _ => return DispatchSlotOutcome::Unavailable,
        };

        // Acknowledge the grant so a recovered coordinator can tell a granted
        // slot from an attempted spawn, exactly as the warm path does.
        match self
            .service
            .grant(LeaseGrantRequest {
                identity,
                fencing_token: grant.fencing_token,
            })
            .await
        {
            LeaseResult::Status(status)
                if matches!(status.state, LeaseState::Launching | LeaseState::Bound) =>
            {
                DispatchSlotOutcome::Granted
            }
            _ => DispatchSlotOutcome::Unavailable,
        }
    }

    async fn release_dispatch_slot(&self, task_id: &str, generation: i64) {
        if !self.service.is_ready() {
            return;
        }
        let identity = Self::identity(task_id, generation);

        // The row's durable state decides how it is retired: a still-queued
        // attempt is abandoned (it never occupied), while an occupying one is
        // released under its own fencing token. Guessing wrong either leaks a
        // slot or -- far worse -- releases one under a token that is not ours.
        let status = match self
            .service
            .status(LeaseStatusRequest {
                identity: identity.clone(),
            })
            .await
        {
            LeaseResult::Status(status) => status,
            // No row (or unreadable). Nothing acquired, nothing to hand back.
            _ => return,
        };

        match (status.state, status.fencing_token) {
            (LeaseState::Queued, _) => {
                let _ = self
                    .service
                    .abandon(LeaseAbandonRequest {
                        identity,
                        candidate_cleanup: false,
                    })
                    .await;
            }
            (
                LeaseState::Granted
                | LeaseState::Launching
                | LeaseState::Bound
                | LeaseState::Active
                | LeaseState::Suspect,
                Some(fencing_token),
            ) => {
                let _ = self
                    .service
                    .release(LeaseReleaseRequest {
                        identity,
                        fencing_token,
                        candidate_cleanup: false,
                    })
                    .await;
            }
            // Already terminal, or occupying without a token (which the schema
            // forbids). Either way there is nothing safe to release.
            _ => {}
        }
    }

    async fn occupancy(&self) -> Option<i64> {
        self.service
            .recovery_snapshot()
            .await
            .ok()
            .map(|snapshot| snapshot.occupied)
    }

    fn cap(&self) -> i64 {
        self.service.cap()
    }
}

/// A dispatch slot carries no wall-clock deadline of its own.
///
/// The queue deadline exists to stop a queued row outliving the work that
/// wanted it; for dispatch that job is already done by the coordinator, which
/// releases the slot when the task-run terminalizes and reconciles orphans on
/// restart. Giving it a short queue deadline instead would silently drop a
/// task's FIFO position while it waited, which is the starvation this design
/// exists to avoid.
struct LeaseQueueRequestShim;

impl LeaseQueueRequestShim {
    fn build(identity: LeaseIdentity) -> djinn_supervisor::services::LeaseQueueRequest {
        djinn_supervisor::services::LeaseQueueRequest {
            identity,
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        }
    }
}
