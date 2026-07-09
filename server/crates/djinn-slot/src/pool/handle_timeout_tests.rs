//! Coordinator→pool ask-timeout regression tests (2026-07-09 whole-board
//! freeze). A single-mailbox pool actor that stalls must not wedge its
//! caller: an un-timed `rx.await` on a stalled pool froze the coordinator's
//! own single `select!` loop for 11 minutes. `SlotPoolHandle::request` now
//! bounds every ask with [`POOL_ASK_TIMEOUT`], returning `PoolError::Timeout`.

use std::collections::HashMap;

use tokio::sync::mpsc;

use super::POOL_ASK_TIMEOUT;
use crate::pool::types::{PoolError, PoolMessage, PoolStatus};

/// A pool whose mailbox is never drained (receiver kept alive so the channel
/// is not `Closed`, but no reply is ever sent) must elapse into
/// `PoolError::Timeout` rather than hang forever. `start_paused` auto-advances
/// the clock past the timeout so the test does not sleep for real.
#[tokio::test(start_paused = true)]
async fn get_status_times_out_when_pool_never_replies() {
    let (tx, _rx) = mpsc::channel::<PoolMessage>(64);
    let handle = super::SlotPoolHandle::from_raw_sender(tx);

    let result = handle.get_status().await;

    match result {
        Err(PoolError::Timeout { timeout_secs }) => {
            assert_eq!(timeout_secs, POOL_ASK_TIMEOUT.as_secs());
        }
        other => panic!("expected PoolError::Timeout, got {other:?}"),
    }
    // Keep the receiver alive to the end so `send` saw an open channel (i.e. the
    // failure was the reply timeout, not an `ActorDead` from a closed channel).
    drop(_rx);
}

#[tokio::test(start_paused = true)]
async fn has_session_times_out_when_pool_never_replies() {
    let (tx, _rx) = mpsc::channel::<PoolMessage>(64);
    let handle = super::SlotPoolHandle::from_raw_sender(tx);

    let result = handle.has_session("task-1").await;

    assert!(
        matches!(result, Err(PoolError::Timeout { .. })),
        "expected PoolError::Timeout, got {result:?}"
    );
    drop(_rx);
}

/// Positive control: a pool that replies promptly returns `Ok` and the
/// watchdog never fires.
#[tokio::test(start_paused = true)]
async fn get_status_returns_ok_when_pool_replies_fast() {
    let (tx, mut rx) = mpsc::channel::<PoolMessage>(64);
    tokio::spawn(async move {
        if let Some(PoolMessage::GetStatus { respond_to }) = rx.recv().await {
            let _ = respond_to.send(Ok(PoolStatus {
                active_slots: 0,
                total_slots: 0,
                per_model: HashMap::new(),
                running_tasks: Vec::new(),
            }));
        }
    });
    let handle = super::SlotPoolHandle::from_raw_sender(tx);

    let result = handle.get_status().await;

    assert!(result.is_ok(), "expected Ok, got {result:?}");
}
