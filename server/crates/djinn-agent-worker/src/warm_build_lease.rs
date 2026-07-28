//! Handing the warm Pod's build slot back at the cargo→graph boundary.
//!
//! # Why the warm Job should not hold a build slot through SCIP
//!
//! MEASURED from `build_leases` over 6h48m on 2026-07-27: the warm held **1 of
//! only 3 build slots for 6h44m — a 98.9% duty cycle** — and 60.7% of that hold
//! was the SCIP/graph phase. 48.5% of the whole Job was ONE single-threaded
//! rust-analyzer process averaging ~0.82 cores (2899s CPU / 3520s elapsed, in a
//! cgroup allowed 4). The lease is weighted for 4000 millicores.
//!
//! The cargo phase genuinely is a build and genuinely needs that weight. The
//! graph phase is not a build: it is one mostly-serial indexer plus
//! serialization and publication. Releasing between them recovers ~0.6 of 3
//! slots — **~20% of total build capacity** — with no change to graph freshness,
//! because the graph phase keeps running exactly as before; it simply stops
//! charging build capacity for time it does not use.
//!
//! # This is not a second lease authority
//!
//! Queue, grant and bind remain host-owned in `djinn-coordinator`. The Pod does
//! exactly one thing: a **fenced, idempotent release of a slot it already
//! holds**, using the same `BuildLeaseRepository::release` primitive the
//! coordinator uses, against the same row.
//!
//! ## Exactly-once, and leak-free
//!
//! Three independent layers, because "release the slot twice" and "leak the
//! slot" are both production incidents:
//!
//! 1. **In-process**: [`WarmBuildLease::release_once`] latches an
//!    [`AtomicBool`], so a second call on the same handle performs no database
//!    work at all and reports [`WarmLeaseRelease::AlreadyReleasedInPod`].
//! 2. **Durable**: `release` is a compare-and-set on the fencing token inside a
//!    locked transaction, and a row that is ALREADY terminal is replayed rather
//!    than re-terminalised. So the host's own release when the Job finishes —
//!    and the reclaim sweep, and a restarted coordinator's recovery — all land
//!    on an idempotent no-op. Nothing double-releases, and nothing can release
//!    somebody else's slot: a stale token is rejected outright.
//! 3. **Crash-safe**: a crash AFTER the release cannot leak the slot, because
//!    the row is already terminal and terminal rows occupy nothing. A crash
//!    BEFORE the release (anywhere in the cargo phase) leaves behaviour exactly
//!    as it is today — the host-side absence proofs in
//!    `djinn_coordinator::build_lease_reclaim` and
//!    `K8sGraphWarmer::reconcile_durable_warm_leases` still own that row, and
//!    this module changes neither.
//!
//! A failed release is *also* safe: it is logged and the warm proceeds, and the
//! slot is reclaimed on the existing host path. The only cost of a failure is
//! the capacity win, never correctness.
//!
//! ## Relationship to the host-side terminal release (#2688)
//!
//! #2688 made `K8sGraphWarmer::reconcile_durable_warm_leases` release the slot
//! once `WarmCandidateInventory::workload_finished()` holds — every Job carrying
//! `Complete`/`Failed` and every Pod in a terminal phase — instead of waiting
//! for the API server's garbage collector. The two changes compose, in both
//! orders, for reasons that are properties of the ledger rather than of timing:
//!
//! * **This release first (the normal case, mid-SCIP).** The row becomes
//!   terminal, and `BuildLeaseGraphWarmAdapter::recoverable` filters
//!   `state != Terminal`, so the row never reaches #2688's path at all. Nothing
//!   double-releases because there is nothing left to reconcile.
//! * **#2688 first.** It cannot fire while this Pod is warming: a Pod running
//!   SCIP is `Running`, not terminal, which is exactly the safety property
//!   #2688 preserves. If it somehow did, `release_once` reports
//!   [`WarmLeaseRelease::AlreadyTerminal`] — logged, and explicitly NOT counted
//!   as a freed slot, so the capacity accounting stays honest.
//!
//! #2688 stays load-bearing for every warm that never reaches this boundary —
//! one that dies during the cargo phase, or an unleased/failed-early path. What
//! changes for a normally-completing warm is that its slot is already free by
//! the time #2688 looks, roughly 60% of the Job earlier.

use std::sync::atomic::{AtomicBool, Ordering};

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
};

/// Durable build-lease consumer id projected into the warm Pod.
///
/// Must match `djinn_k8s::warm_job::ENV_WARM_LEASE_CONSUMER_ID`; the
/// `rendered_job_env_contract` suite asserts the two against the REAL rendered
/// manifest so this cannot drift silently.
pub const ENV_LEASE_CONSUMER_ID: &str = "DJINN_WARM_LEASE_CONSUMER_ID";
/// Fencing token for [`ENV_LEASE_CONSUMER_ID`]. Must match
/// `djinn_k8s::warm_job::ENV_WARM_LEASE_FENCING_TOKEN`.
pub const ENV_LEASE_FENCING_TOKEN: &str = "DJINN_WARM_LEASE_FENCING_TOKEN";

/// What one call to [`WarmBuildLease::release_once`] actually did.
///
/// Deliberately distinguishes the durable transition from every kind of no-op:
/// a release that reports success must have moved a row that was occupying
/// capacity, or the capacity win is imaginary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmLeaseRelease {
    /// The row was occupying capacity and is now terminal. A slot was freed.
    Released,
    /// The row was already terminal before this call — the host released it
    /// first, or reclaimed it. Nothing was occupying, nothing was freed.
    AlreadyTerminal,
    /// This handle already performed its release; no database work was done.
    AlreadyReleasedInPod,
    /// The release was attempted and the ledger refused or was unreachable.
    /// The slot stays occupying and the existing host-side reclaim owns it.
    Failed(String),
}

/// One warm Pod's own build lease, resolved from the projected environment.
#[derive(Debug)]
pub struct WarmBuildLease {
    key: BuildLeaseKey,
    fencing_token: i64,
    released: AtomicBool,
}

impl WarmBuildLease {
    /// Resolve from the process environment.
    ///
    /// `None` means this Pod holds no durable lease it may release — an
    /// unleased warm (legacy path), or a malformed projection. Returning `None`
    /// rather than guessing is deliberate: releasing a slot we cannot prove we
    /// hold is exactly the double-release this module exists to avoid.
    pub fn from_env() -> Option<Self> {
        Self::from_parts(
            std::env::var(ENV_LEASE_CONSUMER_ID).ok().as_deref(),
            std::env::var(ENV_LEASE_FENCING_TOKEN).ok().as_deref(),
        )
    }

    /// Pure seam for [`Self::from_env`].
    pub fn from_parts(consumer_id: Option<&str>, fencing_token: Option<&str>) -> Option<Self> {
        let consumer_id = consumer_id?.trim();
        if consumer_id.is_empty() {
            return None;
        }
        let fencing_token: i64 = fencing_token?.trim().parse().ok()?;
        if fencing_token <= 0 {
            return None;
        }
        Some(Self {
            key: BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::GraphWarm,
                consumer_id: consumer_id.to_string(),
            },
            fencing_token,
            released: AtomicBool::new(false),
        })
    }

    /// The ledger row this handle may release.
    pub fn key(&self) -> &BuildLeaseKey {
        &self.key
    }

    /// Release this Pod's build slot, at most once per handle.
    ///
    /// Never returns an error: the caller is the warm pipeline, and a warm that
    /// cannot give its slot back early must still produce a graph.
    pub async fn release_once(&self, db: &Database) -> WarmLeaseRelease {
        // `swap` is the exactly-once latch: whoever observes `false` owns the
        // single durable attempt, and every later caller short-circuits before
        // touching the database.
        if self.released.swap(true, Ordering::SeqCst) {
            return WarmLeaseRelease::AlreadyReleasedInPod;
        }
        let repository = BuildLeaseRepository::new(db.clone());
        // Read first ONLY to classify the outcome honestly. The release below
        // is still a locked compare-and-set, so this read races nothing that
        // matters: a row that goes terminal between the two is reported as
        // `AlreadyTerminal` by the second look, never as a freed slot.
        let was_occupying = match repository.get(&self.key).await {
            Ok(Some(row)) => row.state != BuildLeaseState::Terminal,
            Ok(None) => {
                return WarmLeaseRelease::Failed(format!(
                    "no build lease row for consumer {}",
                    self.key.consumer_id
                ));
            }
            Err(error) => return WarmLeaseRelease::Failed(error.to_string()),
        };

        match repository
            .release(
                &self.key,
                self.fencing_token,
                Some(serde_json::json!({
                    "released_by": "warm_pod_cargo_graph_boundary",
                })),
            )
            .await
        {
            Ok(_) if was_occupying => WarmLeaseRelease::Released,
            Ok(_) => WarmLeaseRelease::AlreadyTerminal,
            Err(error) => WarmLeaseRelease::Failed(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::{
        BuildLeaseTerminalReason, GrantNextBuildLeaseResult, QueueBuildLeaseInput,
        QueueBuildLeaseResult,
    };

    const NOW: &str = "2026-07-27T22:12:30Z";

    /// The warm slot is weighted for a full build-capable pod; two of them do
    /// not fit a cap of 4.
    const WARM_WEIGHT: i64 = 4;

    fn queue_input(kind: BuildLeaseConsumerKind, id: &str, weight: i64) -> QueueBuildLeaseInput {
        QueueBuildLeaseInput {
            key: BuildLeaseKey {
                consumer_kind: kind,
                consumer_id: id.into(),
            },
            immutable_identity: format!("warm:proj:{id}:rev"),
            queue_deadline: None,
            launch_deadline: None,
            weight,
        }
    }

    async fn queue_and_grant(
        repository: &BuildLeaseRepository,
        kind: BuildLeaseConsumerKind,
        id: &str,
        weight: i64,
        cap: i64,
    ) -> i64 {
        assert!(matches!(
            repository
                .queue(&queue_input(kind, id, weight))
                .await
                .unwrap(),
            QueueBuildLeaseResult::Queued { .. }
        ));
        match repository.grant_next(cap, NOW, None).await.unwrap() {
            GrantNextBuildLeaseResult::Granted(row) => {
                row.fencing_token.expect("granted rows carry a token")
            }
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    // --- Environment resolution -------------------------------------------

    #[test]
    fn an_unleased_or_malformed_projection_yields_no_releasable_lease() {
        // Releasing a slot we cannot prove we hold is the double-release this
        // module exists to prevent, so every ambiguous input is `None`.
        assert!(WarmBuildLease::from_parts(None, Some("7")).is_none());
        assert!(WarmBuildLease::from_parts(Some("warm-1"), None).is_none());
        assert!(WarmBuildLease::from_parts(Some(""), Some("7")).is_none());
        assert!(WarmBuildLease::from_parts(Some("   "), Some("7")).is_none());
        assert!(WarmBuildLease::from_parts(Some("warm-1"), Some("")).is_none());
        assert!(WarmBuildLease::from_parts(Some("warm-1"), Some("abc")).is_none());
        assert!(WarmBuildLease::from_parts(Some("warm-1"), Some("0")).is_none());
        assert!(WarmBuildLease::from_parts(Some("warm-1"), Some("-3")).is_none());

        let lease = WarmBuildLease::from_parts(Some(" warm-1 "), Some(" 7 "))
            .expect("a well-formed projection resolves");
        assert_eq!(lease.key().consumer_id, "warm-1");
        assert_eq!(lease.key().consumer_kind, BuildLeaseConsumerKind::GraphWarm);
    }

    // --- The capacity win, asserted as a capacity side effect --------------

    /// THE point of the change: after the release, the freed slot is actually
    /// grantable to somebody else. Asserted by granting it — not by reading a
    /// state column back.
    #[tokio::test]
    async fn releasing_at_the_boundary_frees_a_slot_another_consumer_can_take() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;

        // A second build-capable consumer arrives while the warm holds the cap.
        repository
            .queue(&queue_input(
                BuildLeaseConsumerKind::TaskDispatch,
                "task-1:1",
                WARM_WEIGHT,
            ))
            .await
            .unwrap();
        assert!(
            matches!(
                repository.grant_next(CAP, NOW, None).await.unwrap(),
                GrantNextBuildLeaseResult::Empty { occupancy, cap } if occupancy == WARM_WEIGHT && cap == CAP
            ),
            "the warm must be holding the whole cap before the release"
        );

        // …the warm finishes cargo and hands its slot back.
        let lease = WarmBuildLease::from_parts(Some("warm-1"), Some(&token.to_string())).unwrap();
        assert_eq!(lease.release_once(&db).await, WarmLeaseRelease::Released);

        // The side effect: the waiting consumer is now GRANTED. Under the old
        // behaviour it stays queued for the entire SCIP phase.
        match repository.grant_next(CAP, NOW, None).await.unwrap() {
            GrantNextBuildLeaseResult::Granted(row) => {
                assert_eq!(row.key.consumer_id, "task-1:1");
            }
            other => panic!("released capacity was not re-grantable: {other:?}"),
        }
    }

    // --- Exactly-once ------------------------------------------------------

    /// A second release from the same Pod must do NO database work, and the
    /// host's own release afterwards must be a harmless replay rather than a
    /// second capacity event.
    #[tokio::test]
    async fn release_is_exactly_once_in_pod_and_idempotent_against_the_host() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;
        let lease = WarmBuildLease::from_parts(Some("warm-1"), Some(&token.to_string())).unwrap();

        assert_eq!(lease.release_once(&db).await, WarmLeaseRelease::Released);
        let after_first = repository.get(lease.key()).await.unwrap().unwrap();
        assert_eq!(after_first.state, BuildLeaseState::Terminal);

        // Second in-pod call: short-circuited before any database work.
        assert_eq!(
            lease.release_once(&db).await,
            WarmLeaseRelease::AlreadyReleasedInPod
        );

        // The host's release when the Job terminates: an idempotent replay that
        // neither errors nor re-terminalises.
        let replayed = repository
            .release(lease.key(), token, None)
            .await
            .expect("the host's release must remain idempotent");
        assert_eq!(replayed.state, BuildLeaseState::Terminal);
        assert_eq!(
            replayed.terminal_reason.as_deref(),
            Some(BuildLeaseTerminalReason::Released.as_str())
        );
        assert_eq!(
            replayed.terminal_at, after_first.terminal_at,
            "a replay must not re-stamp the terminal transition"
        );
    }

    /// A fresh handle for the SAME row (a would-be second release from another
    /// code path) must be reported as a no-op, not as a freed slot: the
    /// capacity accounting has to stay honest even when the latch is bypassed.
    #[tokio::test]
    async fn a_second_handle_reports_a_no_op_rather_than_a_second_freed_slot() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;
        let token_text = token.to_string();

        let first = WarmBuildLease::from_parts(Some("warm-1"), Some(&token_text)).unwrap();
        let second = WarmBuildLease::from_parts(Some("warm-1"), Some(&token_text)).unwrap();

        assert_eq!(first.release_once(&db).await, WarmLeaseRelease::Released);
        assert_eq!(
            second.release_once(&db).await,
            WarmLeaseRelease::AlreadyTerminal
        );
    }

    /// A crash during SCIP — after the release — cannot leak the slot: the row
    /// is already terminal, so it occupies nothing and the FIFO keeps draining
    /// with no host intervention at all.
    #[tokio::test]
    async fn a_crash_after_the_release_leaks_nothing() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;
        let lease = WarmBuildLease::from_parts(Some("warm-1"), Some(&token.to_string())).unwrap();
        assert_eq!(lease.release_once(&db).await, WarmLeaseRelease::Released);

        // Simulate the Pod dying here: the handle is gone, nothing else runs.
        drop(lease);

        // Occupancy is zero, so an arriving consumer is granted immediately —
        // no reclaim sweep, no settle window, no operator.
        repository
            .queue(&queue_input(
                BuildLeaseConsumerKind::TaskDispatch,
                "task-1:1",
                WARM_WEIGHT,
            ))
            .await
            .unwrap();
        match repository.grant_next(CAP, NOW, None).await.unwrap() {
            GrantNextBuildLeaseResult::Granted(row) => {
                assert_eq!(row.key.consumer_id, "task-1:1");
            }
            other => panic!("a crash after release left capacity stranded: {other:?}"),
        }
    }

    /// A crash BEFORE the release must change nothing: the row keeps occupying
    /// and the existing host-side absence proofs still own it. Asserted as the
    /// capacity side effect — the slot is NOT grantable — so a regression that
    /// released early would be caught here.
    #[tokio::test]
    async fn a_crash_before_the_release_leaves_the_slot_to_the_host_reclaim() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;
        // The Pod dies mid-cargo: the handle is dropped without ever releasing.
        drop(WarmBuildLease::from_parts(Some("warm-1"), Some(&token.to_string())).unwrap());

        repository
            .queue(&queue_input(
                BuildLeaseConsumerKind::TaskDispatch,
                "task-1:1",
                WARM_WEIGHT,
            ))
            .await
            .unwrap();
        assert!(
            matches!(
                repository.grant_next(CAP, NOW, None).await.unwrap(),
                GrantNextBuildLeaseResult::Empty { .. }
            ),
            "a Pod that never reached the boundary must NOT have freed its slot"
        );
        assert_eq!(
            repository
                .get(&BuildLeaseKey {
                    consumer_kind: BuildLeaseConsumerKind::GraphWarm,
                    consumer_id: "warm-1".into(),
                })
                .await
                .unwrap()
                .unwrap()
                .state,
            BuildLeaseState::Granted,
        );
    }

    /// A stale token can never release the current holder's slot — the fence
    /// that stops a Pod outlived by a newer grant from freeing capacity that is
    /// no longer its own.
    #[tokio::test]
    async fn a_stale_fencing_token_cannot_release_the_slot() {
        let db = Database::open_in_memory().unwrap();
        let repository = BuildLeaseRepository::new(db.clone());
        const CAP: i64 = 4;

        let token = queue_and_grant(
            &repository,
            BuildLeaseConsumerKind::GraphWarm,
            "warm-1",
            WARM_WEIGHT,
            CAP,
        )
        .await;
        let stale = WarmBuildLease::from_parts(Some("warm-1"), Some(&(token + 1).to_string()))
            .expect("a stale token still parses");

        assert!(matches!(
            stale.release_once(&db).await,
            WarmLeaseRelease::Failed(_)
        ));
        assert_eq!(
            repository.get(stale.key()).await.unwrap().unwrap().state,
            BuildLeaseState::Granted,
            "a fenced-out release must leave the row occupying"
        );
    }

    /// A ledger that refuses the release must never fail the warm: the graph
    /// still has to be produced, and the host reclaim still owns the slot.
    #[tokio::test]
    async fn a_missing_row_is_reported_without_failing_the_warm() {
        let db = Database::open_in_memory().unwrap();
        let lease = WarmBuildLease::from_parts(Some("warm-does-not-exist"), Some("7")).unwrap();
        assert!(matches!(
            lease.release_once(&db).await,
            WarmLeaseRelease::Failed(_)
        ));
    }
}
