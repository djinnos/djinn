//! Durable FIFO, weighting, cap reconciliation and fencing tests for
//! `build_lease`.
//!
//! Split out of `build_lease.rs`, which is at the 51200-byte `Server Guards`
//! ceiling. Included with `#[path]` so it stays a child of the module under
//! test and keeps its `use super::*` access.

use std::sync::Arc;

use super::*;

const NOW: &str = "2026-01-01T00:00:00Z";
const LATER: &str = "2026-01-01T01:00:00Z";

fn input(kind: BuildLeaseConsumerKind, id: &str, identity: &str) -> QueueBuildLeaseInput {
    weighted_input(kind, id, identity, 1)
}

fn weighted_input(
    kind: BuildLeaseConsumerKind,
    id: &str,
    identity: &str,
    weight: i64,
) -> QueueBuildLeaseInput {
    QueueBuildLeaseInput {
        key: BuildLeaseKey {
            consumer_kind: kind,
            consumer_id: id.into(),
        },
        immutable_identity: identity.into(),
        queue_deadline: None,
        launch_deadline: None,
        weight,
    }
}

async fn grant(repo: &BuildLeaseRepository, cap: i64) -> BuildLeaseRow {
    match repo.grant_next(cap, NOW, Some(LATER)).await.unwrap() {
        GrantNextBuildLeaseResult::Granted(row) => row,
        GrantNextBuildLeaseResult::Empty { .. } => panic!("expected queued lease to grant"),
    }
}

#[tokio::test]
async fn fifo_is_global_across_task_and_graph_warm_consumers() {
    let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
    let task = input(BuildLeaseConsumerKind::TaskInvocation, "task", "task-v1");
    let warm = input(BuildLeaseConsumerKind::GraphWarm, "warm", "warm-v1");
    repo.queue(&task).await.unwrap();
    repo.queue(&warm).await.unwrap();

    let first = grant(&repo, 1).await;
    assert_eq!(first.key, task.key);
    repo.release(&first.key, first.fencing_token.unwrap(), None)
        .await
        .unwrap();
    let second = grant(&repo, 1).await;
    assert_eq!(second.key, warm.key);
    assert!(first.enqueue_sequence < second.enqueue_sequence);
}

#[tokio::test]
async fn cap_zero_and_cap_reconciliation_preserve_occupied_work() {
    let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
    let first = input(BuildLeaseConsumerKind::TaskInvocation, "first", "first-v1");
    let second = input(BuildLeaseConsumerKind::GraphWarm, "second", "second-v1");
    repo.queue(&first).await.unwrap();
    repo.queue(&second).await.unwrap();
    assert!(matches!(
        repo.grant_next(0, NOW, None).await.unwrap(),
        GrantNextBuildLeaseResult::Empty {
            occupancy: 0,
            cap: 0
        }
    ));

    let granted = grant(&repo, 1).await;
    let snapshot = repo.set_cap(0).await.unwrap();
    assert_eq!((snapshot.cap, snapshot.occupied), (0, 1));
    assert!(matches!(
        repo.grant_next(2, NOW, None).await.unwrap(),
        GrantNextBuildLeaseResult::Granted(ref row) if row.key == second.key
    ));
    assert_eq!(repo.snapshot().await.unwrap().occupied, 2);
    repo.release(&granted.key, granted.fencing_token.unwrap(), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn restart_snapshot_orders_nonterminal_rows_by_fifo_sequence() {
    let db = Database::open_in_memory().unwrap();
    let repo = BuildLeaseRepository::new(db.clone());
    let first = input(BuildLeaseConsumerKind::GraphWarm, "first", "first-v1");
    let second = input(
        BuildLeaseConsumerKind::TaskInvocation,
        "second",
        "second-v1",
    );
    repo.queue(&first).await.unwrap();
    repo.queue(&second).await.unwrap();
    let first_row = grant(&repo, 2).await;

    let recovered = BuildLeaseRepository::new(db).snapshot().await.unwrap();
    assert_eq!(recovered.occupied, 1);
    assert_eq!(
        recovered
            .rows
            .iter()
            .map(|row| &row.key)
            .collect::<Vec<_>>(),
        vec![&first.key, &second.key]
    );
    assert_eq!(recovered.rows[0].fencing_token, first_row.fencing_token);
}

#[tokio::test]
async fn queue_replay_pod_binding_and_status_validation_are_idempotent_and_fenced() {
    let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
    let request = input(
        BuildLeaseConsumerKind::TaskInvocation,
        "stable",
        "identity-v1",
    );
    assert!(matches!(
        repo.queue(&request).await.unwrap(),
        QueueBuildLeaseResult::Queued {
            idempotent: false,
            ..
        }
    ));
    assert!(matches!(
        repo.queue(&request).await.unwrap(),
        QueueBuildLeaseResult::Queued {
            idempotent: true,
            ..
        }
    ));
    assert!(matches!(
        repo.queue(&input(
            BuildLeaseConsumerKind::TaskInvocation,
            "stable",
            "identity-v2"
        ))
        .await
        .unwrap(),
        QueueBuildLeaseResult::LeaseIdentityConflict { .. }
    ));

    let granted = grant(&repo, 1).await;
    let token = granted.fencing_token.unwrap();
    assert_eq!(
        repo.status(&request.key, token, BuildLeaseState::Launching, None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Launching
    );
    // A stale Granted report after Launching is rejected rather than
    // reaching transition's state-to-SQL conversion.
    assert!(matches!(
        repo.status(&request.key, token, BuildLeaseState::Granted, None)
            .await,
        Err(DbError::InvalidData(_))
    ));
    let bound = repo.bind(&request.key, token, "pod-a", None).await.unwrap();
    assert_eq!(bound.bound_pod_uid.as_deref(), Some("pod-a"));
    let delayed_grant = repo
        .status(&request.key, token, BuildLeaseState::Launching, None)
        .await
        .unwrap();
    assert_eq!(delayed_grant.state, BuildLeaseState::Bound);
    assert_eq!(delayed_grant.bound_pod_uid.as_deref(), Some("pod-a"));
    assert_eq!(
        repo.bind(&request.key, token, "pod-a", None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Bound
    );
    assert!(matches!(
        repo.bind(&request.key, token, "pod-b", None).await,
        Err(DbError::InvalidTransition(_))
    ));

    repo.status(&request.key, token, BuildLeaseState::Active, None)
        .await
        .unwrap();
    assert_eq!(
        repo.status(&request.key, token, BuildLeaseState::Launching, None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Active
    );
    assert_eq!(
        repo.bind(&request.key, token, "pod-a", None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Active
    );
    assert!(
        repo.bind(&request.key, token + 1, "pod-a", None)
            .await
            .is_err()
    );
    assert!(repo.bind(&request.key, token, "pod-b", None).await.is_err());
    assert_eq!(
        repo.get(&request.key).await.unwrap().unwrap().state,
        BuildLeaseState::Active
    );

    repo.status(&request.key, token, BuildLeaseState::Suspect, None)
        .await
        .unwrap();
    assert_eq!(
        repo.status(&request.key, token, BuildLeaseState::Launching, None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Suspect
    );
    assert_eq!(
        repo.bind(&request.key, token, "pod-a", None)
            .await
            .unwrap()
            .state,
        BuildLeaseState::Suspect
    );
    repo.release(&request.key, token, None).await.unwrap();
    let terminal = repo.bind(&request.key, token, "pod-a", None).await.unwrap();
    assert_eq!(terminal.state, BuildLeaseState::Terminal);
    assert_eq!(terminal.terminal_reason.as_deref(), Some("released"));
    let delayed_grant = repo
        .status(&request.key, token, BuildLeaseState::Launching, None)
        .await
        .unwrap();
    assert_eq!(delayed_grant.state, BuildLeaseState::Terminal);
    assert_eq!(delayed_grant.terminal_reason.as_deref(), Some("released"));
    assert!(repo.bind(&request.key, token, "pod-b", None).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_terminal_attempts_return_capacity_exactly_once() {
    let repo = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let first = input(BuildLeaseConsumerKind::TaskInvocation, "first", "first-v1");
    let second = input(BuildLeaseConsumerKind::GraphWarm, "second", "second-v1");
    repo.queue(&first).await.unwrap();
    repo.queue(&second).await.unwrap();
    let granted = grant(&repo, 1).await;
    let token = granted.fencing_token.unwrap();

    let release_repo = Arc::clone(&repo);
    let release_key = first.key.clone();
    let release =
        tokio::spawn(async move { release_repo.release(&release_key, token, None).await });
    let cancel_repo = Arc::clone(&repo);
    let cancel_key = first.key.clone();
    let cancel = tokio::spawn(async move { cancel_repo.cancel(&cancel_key, None).await });
    for result in [release.await.unwrap(), cancel.await.unwrap()] {
        assert_eq!(result.unwrap().state, BuildLeaseState::Terminal);
    }
    assert_eq!(repo.snapshot().await.unwrap().occupied, 0);
    let next = grant(&repo, 1).await;
    assert_eq!(next.key, second.key);
    assert_eq!(repo.snapshot().await.unwrap().occupied, 1);
}

/// A spent `task_dispatch` attempt must be retired, not replayed forever.
///
/// This is the 2026-07-27 board wedge, at the ledger. The key
/// `task_dispatch/{task_id}:{generation}` is a COORDINATE the dispatcher asks
/// for on every tick, not an attempt. Once a terminal row sat under it,
/// `queue()` — which had no state filter — returned that row unchanged forever,
/// and layer-1 admission turned the replay into a denial *before* the journal
/// write that would have advanced the generation. So the key never changed, the
/// tombstone was read again next tick, and the loop closed on itself: six
/// dispatchable tasks, zero sessions, 78 denials in five minutes at occupancy
/// 0-1 of cap 3.
///
/// Every terminal signature a dispatch row can reach is covered, each produced
/// by the production path that produces it. The load-bearing assertion in each
/// round is the last one: the fresh attempt is actually GRANTED against a cap
/// of 1, which the tombstone could never be.
#[tokio::test]
async fn a_spent_task_dispatch_attempt_is_retired_rather_than_replayed_forever() {
    let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());

    for (task_id, reason) in [
        ("expired-task", BuildLeaseTerminalReason::DeadlineExpired),
        ("reclaimed-task", BuildLeaseTerminalReason::ReclaimedAbsent),
        ("abandoned-task", BuildLeaseTerminalReason::Abandoned),
        ("released-task", BuildLeaseTerminalReason::Released),
        ("cancelled-task", BuildLeaseTerminalReason::Cancelled),
    ] {
        let consumer_id = format!("{task_id}:0");
        let request = input(
            BuildLeaseConsumerKind::TaskDispatch,
            &consumer_id,
            &format!("dispatch:{task_id}:0"),
        );

        // ── Produce the tombstone, only through production paths ────────────
        match reason {
            BuildLeaseTerminalReason::DeadlineExpired => {
                // The coordinator stamps an absolute queue deadline on every
                // dispatch attempt; the tick sweep retires it once passed.
                let stamped = QueueBuildLeaseInput {
                    queue_deadline: Some(NOW.to_owned()),
                    ..request.clone()
                };
                repo.queue(&stamped).await.unwrap();
                let expired = repo.expire_deadlines(LATER).await.unwrap();
                assert_eq!(expired.len(), 1);
            }
            BuildLeaseTerminalReason::ReclaimedAbsent => {
                repo.queue(&request).await.unwrap();
                let granted = grant(&repo, 1).await;
                assert!(matches!(
                    repo.reclaim_absent_object(&ReclaimAbsentBuildLeaseInput {
                        key: request.key.clone(),
                        observed_state: granted.state,
                        observed_immutable_identity: granted.immutable_identity.clone(),
                        observed_fencing_token: granted.fencing_token,
                        observed_bound_pod_uid: granted.bound_pod_uid.clone(),
                        observed_updated_at: granted.updated_at.clone(),
                    })
                    .await
                    .unwrap(),
                    ReclaimAbsentBuildLeaseOutcome::Reclaimed(_)
                ));
            }
            BuildLeaseTerminalReason::Abandoned => {
                repo.queue(&request).await.unwrap();
                assert_eq!(repo.abandon_queued_dispatch(task_id).await.unwrap(), 1);
            }
            BuildLeaseTerminalReason::Released => {
                repo.queue(&request).await.unwrap();
                let granted = grant(&repo, 1).await;
                repo.release(&request.key, granted.fencing_token.unwrap(), None)
                    .await
                    .unwrap();
            }
            BuildLeaseTerminalReason::Cancelled => {
                repo.queue(&request).await.unwrap();
                repo.cancel(&request.key, None).await.unwrap();
            }
        }

        let tombstone = repo.get(&request.key).await.unwrap().unwrap();
        assert_eq!(tombstone.state, BuildLeaseState::Terminal, "{task_id}");
        assert_eq!(
            tombstone.terminal_reason.as_deref(),
            Some(reason.as_str()),
            "{task_id}"
        );
        assert_eq!(
            repo.snapshot().await.unwrap().occupied,
            0,
            "a terminal row occupies nothing, so retiring it can free nothing"
        );

        // ── The tick that used to wedge: the same key, asked for again ──────
        let fresh = match repo.queue(&request).await.unwrap() {
            QueueBuildLeaseResult::Queued {
                row,
                idempotent,
                superseded,
            } => {
                assert!(
                    !idempotent,
                    "{task_id}: a spent attempt must not replay as an idempotent hit"
                );
                assert_eq!(
                    superseded.as_deref(),
                    Some(reason.as_str()),
                    "{task_id}: the retirement must be reported, so an operator \
                     can see a tombstone was cleared"
                );
                row
            }
            other => panic!("{task_id}: the fresh attempt must queue, got {other:?}"),
        };
        assert_eq!(fresh.state, BuildLeaseState::Queued);
        assert_eq!(fresh.terminal_reason, None);
        assert_eq!(fresh.fencing_token, None);
        assert!(
            fresh.enqueue_sequence > tombstone.enqueue_sequence,
            "{task_id}: a fresh attempt joins the BACK of the FIFO rather than \
             inheriting the retired attempt's queue position"
        );

        // The side effect that is the whole point: capacity is obtainable.
        let granted = grant(&repo, 1).await;
        assert_eq!(granted.key, request.key, "{task_id}");
        assert_ne!(
            granted.fencing_token, tombstone.fencing_token,
            "{task_id}: the retired attempt's token must be fenced out"
        );
        assert_eq!(repo.snapshot().await.unwrap().occupied, 1);

        // Hand the slot back so the next round starts from an empty pool.
        repo.release(&request.key, granted.fencing_token.unwrap(), None)
            .await
            .unwrap();
        assert_eq!(repo.snapshot().await.unwrap().occupied, 0);
    }
}

/// The retirement above is scoped to `task_dispatch` alone.
///
/// Retained terminal replay is load-bearing for the other two populations: a
/// warm Job whose response was lost must be told it was `Released`, not handed
/// a fresh grant that re-runs a finished compile. Widening the fix to every
/// consumer kind would break exactly that, and would break it silently.
#[tokio::test]
async fn a_terminal_warm_or_invocation_lease_is_still_replayed_verbatim() {
    let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
    for (kind, id) in [
        (BuildLeaseConsumerKind::GraphWarm, "warm-request"),
        (BuildLeaseConsumerKind::TaskInvocation, "invocation-id"),
    ] {
        let request = input(kind, id, &format!("{id}-v1"));
        repo.queue(&request).await.unwrap();
        let granted = grant(&repo, 1).await;
        repo.release(&request.key, granted.fencing_token.unwrap(), None)
            .await
            .unwrap();

        match repo.queue(&request).await.unwrap() {
            QueueBuildLeaseResult::Queued {
                row,
                idempotent,
                superseded,
            } => {
                assert_eq!(
                    row.state,
                    BuildLeaseState::Terminal,
                    "{kind:?} must replay its retained outcome"
                );
                assert_eq!(
                    row.terminal_reason.as_deref(),
                    Some(BuildLeaseTerminalReason::Released.as_str())
                );
                assert!(idempotent, "{kind:?}");
                assert_eq!(superseded, None, "{kind:?}");
            }
            other => panic!("{kind:?} replay must return the retained row, got {other:?}"),
        }
        assert_eq!(
            repo.snapshot().await.unwrap().occupied,
            0,
            "and the replay must not re-buy capacity"
        );
    }
}
