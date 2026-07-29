//! Periodic build-admission reconciliation — the pass that retires occupying
//! journal rows whose Kubernetes objects are gone.
//!
//! ## The wedge this exists to prevent
//!
//! [`BuildAdmissionReconciler`](djinn_coordinator::build_admission_inventory::BuildAdmissionReconciler)
//! is what terminalizes an occupying admission row whose object the API server
//! no longer has. Until this loop existed it ran **only** from
//! [`AppState::become_leader`](crate::server::AppState::become_leader) and the
//! startup path, so a row that was not reclaimable during that single pass
//! stayed occupying until the next process restart.
//!
//! One clause of `is_reclaimable` makes that a live production hazard rather
//! than a theoretical one: a row is only a reclamation candidate once it has
//! **settled** past
//! [`DEFAULT_RECLAIM_SETTLE_WINDOW`](djinn_coordinator::build_admission_inventory::DEFAULT_RECLAIM_SETTLE_WINDOW)
//! (300s), because before then the API server could still be admitting a create
//! the dead process POSTed. A rolling deploy produces exactly such a row — the
//! outgoing pod is killed mid-create, its row is marked `CreateUnknown` with a
//! null object uid — and the incoming pod's one and only reconciliation runs
//! seconds later, well inside the settle window. The row is correctly skipped,
//! and then nothing ever looks at it again.
//!
//! A single such row is enough to halt the entire board. `CreateUnknown` rows
//! feed `create_unknown_pending`, which `readiness()` reports as
//! `CreateUnknownHealth`, which fails Enforce closed for **every** admission —
//! not at capacity, but before any capacity is measured. Observed in production
//! on 2026-07-28: one orphaned row from a rolled pod denied every dispatch for
//! ~20 minutes until the server was restarted by hand.
//!
//! This loop closes that gap. It is the same reconciliation pass on a timer, so
//! a row that was too young to reclaim during the startup pass is reclaimed by
//! the first tick after it settles.
//!
//! ## Why leader-only
//!
//! Reconciliation writes to the durable admission journal (retiring rows,
//! adopting live objects). The single-active-writer invariant that the topology
//! gate exists to enforce is exactly what would break if standby HTTP-only pods
//! ran this concurrently, so — like [`crate::git_maintenance`] and
//! [`crate::graph_retention`] — it is started exclusively from `become_leader`
//! and runs until the process-wide `CancellationToken` fires.
//!
//! ## Fail-closed semantics are unchanged
//!
//! A pass that cannot prove the inventory marks the controller
//! `InventoryPending`, and Enforce denies until a later pass succeeds. That is
//! the pre-existing contract of the reconciler and this loop does not soften
//! it — it only makes it recoverable. Before this loop a failed inventory at
//! startup was permanent; now the next tick clears it.
//!
//! ## Why the loop is supervised rather than merely spawned
//!
//! This loop IS the recovery machinery, so its own failure modes are the ones
//! that turn a five-minute event into a five-hour one. As originally written it
//! was a bare `tokio::spawn` with the `JoinHandle` dropped, calling an
//! unbounded Kubernetes LIST, and logging its exit at `debug!`. All three
//! failure modes were silent:
//!
//! * a panic anywhere in a pass killed the loop permanently, with no log that
//!   any operator would ever look at and no restart;
//! * one hung API call parked the pass forever while every process-level
//!   liveness signal still read healthy;
//! * an exit — for any reason — produced a single `debug!` line.
//!
//! Each pass now runs as its own supervised child task under a bounded budget:
//! a panic is contained to that pass and reported at `error!`, an overrunning
//! pass is aborted so the next tick still happens, and any exit of the loop
//! itself is reported at `warn!`/`error!` and restarted a bounded number of
//! times. Alongside that, every completed pass advances the durable-ish
//! "last successful reconcile" stamp on the controller, which
//! [`crate::server::AppState::publish_build_admission_metrics`] exports and the
//! `build_admission_health` doctor check reads. Nothing before this asserted
//! anywhere that a pass had completed within the last N seconds, and that
//! missing assertion is the whole reason the outage lasted as long as it did.

use std::future::Future;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;

/// Environment variable overriding the reconciliation cadence, in seconds.
pub const INTERVAL_ENV: &str = "DJINN_BUILD_ADMISSION_RECONCILE_INTERVAL_SECS";
/// Environment variable overriding the per-pass budget, in seconds.
pub const PASS_TIMEOUT_ENV: &str = "DJINN_BUILD_ADMISSION_RECONCILE_PASS_TIMEOUT_SECS";

/// Default cadence: 120s. Deliberately shorter than the 300s settle window so
/// that a row becomes reclaimable and is then reclaimed within roughly one
/// window rather than two — the failure this loop repairs is a total board
/// halt, so time-to-heal is the property being tuned.
const DEFAULT_INTERVAL_SECS: u64 = 120;
/// Floor, so a misconfigured tiny value cannot hot-loop the API server with
/// LIST/GET traffic.
const MIN_INTERVAL_SECS: u64 = 30;
/// Ceiling, so a misconfigured huge value cannot silently restore the
/// startup-only behaviour this loop exists to remove.
const MAX_INTERVAL_SECS: u64 = 60 * 60;

/// Default per-pass budget: 300s. A pass is a bounded number of Kubernetes
/// reads plus journal writes; anything past five minutes is a hang, not slow
/// work, and the next tick is more useful than the stuck one.
const DEFAULT_PASS_TIMEOUT_SECS: u64 = 300;
const MIN_PASS_TIMEOUT_SECS: u64 = 30;
const MAX_PASS_TIMEOUT_SECS: u64 = 30 * 60;

/// How many times the supervisor rebuilds the loop after the loop task itself
/// dies. Bounded so a deterministically-panicking loop cannot spin forever;
/// exhausting it is reported at `error!` and is a genuine "reconciliation is
/// gone" condition, which the `build_admission_health` doctor check surfaces
/// through the reconcile-age signal.
const MAX_LOOP_RESTARTS: u32 = 8;

/// Why one reconciliation pass stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PassOutcome {
    /// The pass ran to completion. Says nothing about whether reconciliation
    /// SUCCEEDED — that verdict belongs to the controller's readiness gates —
    /// only that the loop is alive and making calls.
    Completed,
    /// The pass panicked. Contained to the pass; the loop keeps ticking.
    Panicked,
    /// The pass exceeded its budget and was aborted.
    TimedOut,
}

/// Why the reconciliation loop stopped. Every variant is worth a log line: this
/// loop stopping means occupying rows stop being reclaimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopExit {
    /// The process-wide cancellation token fired (normal shutdown).
    Cancelled,
}

/// Run one pass as a supervised child task under `budget`.
///
/// Spawning rather than `catch_unwind`-ing is deliberate: a `JoinHandle`
/// reports a panic as data without requiring the pass future to be
/// `UnwindSafe`, and it gives the timeout something it can actually `abort()`
/// instead of leaking a stuck future into the loop's own stack forever.
async fn run_pass<Fut>(budget: Duration, pass: Fut) -> PassOutcome
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(pass);
    let abort = handle.abort_handle();
    match tokio::time::timeout(budget, handle).await {
        Ok(Ok(())) => PassOutcome::Completed,
        Ok(Err(join_error)) if join_error.is_panic() => {
            tracing::error!(
                error = %join_error,
                "build_admission_reconcile pass PANICKED; the loop survives and will \
                 retry on the next tick, but this pass reclaimed nothing"
            );
            PassOutcome::Panicked
        }
        // A cancelled (not panicked) join can only come from an abort we did
        // not issue; treat it as a non-completion rather than a success.
        Ok(Err(join_error)) => {
            tracing::error!(
                error = %join_error,
                "build_admission_reconcile pass ended without completing"
            );
            PassOutcome::Panicked
        }
        Err(_) => {
            abort.abort();
            tracing::error!(
                budget_secs = budget.as_secs(),
                "build_admission_reconcile pass exceeded its budget and was aborted; \
                 an unbounded Kubernetes call is the usual cause. Reconciliation \
                 resumes on the next tick rather than stopping here."
            );
            PassOutcome::TimedOut
        }
    }
}

/// The reconciliation loop proper, parameterised over the pass so its
/// supervision properties are testable without an `AppState`.
///
/// Returns only when `cancel` fires. Every other way a pass can end — panic,
/// hang — is absorbed here rather than ending the loop.
pub(crate) async fn run_loop<F, Fut>(
    interval: Duration,
    pass_budget: Duration,
    cancel: CancellationToken,
    pass: F,
) -> LoopExit
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first tick fires immediately; consume it. `AppState::initialize` runs
    // a reconciliation pass on every pod before `become_leader` is even called,
    // and the leader's own promotion path can run another, so reconciling again
    // in the same breath would only duplicate API-server traffic during the
    // leadership transition. This suppression is what makes it safe to spawn
    // this loop as early as we do — see `become_leader`.
    ticker.tick().await;
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                return LoopExit::Cancelled;
            }
            _ = ticker.tick() => {
                // Off (no controller) and non-Kubernetes runtimes make this
                // a cheap no-op inside the call itself; there is no
                // separate gate here to drift out of sync with that one.
                let _ = run_pass(pass_budget, pass()).await;
            }
        }
    }
}

/// Spawn the periodic build-admission reconciliation task. Leader-only
/// (started from `become_leader`), runs until `state.cancel()` fires.
pub fn spawn(state: AppState) {
    let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref());
    let pass_budget = parse_pass_timeout(std::env::var(PASS_TIMEOUT_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(
            ?interval,
            ?pass_budget,
            "build_admission_reconcile loop starting (leader-only)"
        );
        // Outer supervision: the loop body already contains per-pass panics, so
        // reaching this handler means the SUPERVISION code itself died. Rebuild
        // it rather than leaving the board with no reconciler, and say so at
        // `error!` — the original code dropped this handle entirely.
        for restart in 0..=MAX_LOOP_RESTARTS {
            let loop_state = state.clone();
            let loop_cancel = cancel.clone();
            let handle = tokio::spawn(async move {
                run_loop(interval, pass_budget, loop_cancel, move || {
                    let state = loop_state.clone();
                    async move {
                        state.reconcile_build_admission_inventory().await;
                        state.publish_build_admission_metrics().await;
                    }
                })
                .await
            });
            match handle.await {
                Ok(LoopExit::Cancelled) => {
                    // Not an error, but never `debug!`: "reconciliation has
                    // stopped" is exactly the fact an operator needs when the
                    // board goes quiet, and a debug line is invisible in
                    // production.
                    tracing::warn!(
                        restarts = restart,
                        "build_admission_reconcile loop exited on cancellation; occupying \
                         admission rows are no longer being reclaimed by this process"
                    );
                    return;
                }
                Err(join_error) => {
                    tracing::error!(
                        error = %join_error,
                        restart = restart + 1,
                        max_restarts = MAX_LOOP_RESTARTS,
                        "build_admission_reconcile LOOP died; restarting it"
                    );
                }
            }
        }
        tracing::error!(
            max_restarts = MAX_LOOP_RESTARTS,
            "build_admission_reconcile loop exhausted its restart budget and is GONE; \
             occupying admission rows will not be reclaimed until this process restarts. \
             Run the stale-admission-occupancy runbook."
        );
    });
}

/// Parse and bound the configured interval. Anything unparseable or out of
/// range falls back to the default rather than disabling the loop: a typo in an
/// env var must not be able to reinstate the startup-only wedge.
fn parse_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| (MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(secs))
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Parse and bound the configured per-pass budget. Same fallback discipline as
/// [`parse_interval`]: a typo must not be able to remove the deadline.
fn parse_pass_timeout(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| (MIN_PASS_TIMEOUT_SECS..=MAX_PASS_TIMEOUT_SECS).contains(secs))
        .unwrap_or(DEFAULT_PASS_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn unset_interval_uses_the_default() {
        assert_eq!(
            parse_interval(None),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn a_valid_interval_is_adopted() {
        assert_eq!(parse_interval(Some("300")), Duration::from_secs(300));
        assert_eq!(
            parse_interval(Some(" 45 ")),
            Duration::from_secs(45),
            "surrounding whitespace should not defeat a valid value"
        );
    }

    #[test]
    fn out_of_range_and_unparseable_values_fall_back_to_the_default() {
        let default = Duration::from_secs(DEFAULT_INTERVAL_SECS);
        for raw in ["0", "1", "99999", "not-a-number", "", "-5"] {
            assert_eq!(
                parse_interval(Some(raw)),
                default,
                "{raw:?} must fall back to the default rather than disable the loop"
            );
        }
    }

    #[test]
    fn out_of_range_and_unparseable_pass_budgets_fall_back_to_the_default() {
        let default = Duration::from_secs(DEFAULT_PASS_TIMEOUT_SECS);
        for raw in ["0", "5", "99999", "forever", "", "-5"] {
            assert_eq!(
                parse_pass_timeout(Some(raw)),
                default,
                "{raw:?} must fall back to the default rather than remove the pass deadline"
            );
        }
        assert_eq!(parse_pass_timeout(Some("120")), Duration::from_secs(120));
    }

    /// A pass that panics must not take the loop with it. Before the loop was
    /// supervised, the very first panic ended reconciliation permanently — no
    /// log an operator would see, no restart, and the board stayed wedged until
    /// somebody restarted the process by hand.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_pass_does_not_end_the_loop() {
        let passes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let loop_passes = Arc::clone(&passes);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_loop(
                Duration::from_secs(60),
                Duration::from_secs(300),
                loop_cancel,
                move || {
                    let passes = Arc::clone(&loop_passes);
                    async move {
                        passes.fetch_add(1, Ordering::SeqCst);
                        panic!("pass exploded");
                    }
                },
            )
            .await
        });

        // Three ticks past the suppressed first one.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        let observed = passes.load(Ordering::SeqCst);
        cancel.cancel();
        assert_eq!(
            handle.await.expect("the loop task itself must not panic"),
            LoopExit::Cancelled
        );
        assert!(
            observed >= 3,
            "every tick after a panicking pass must still run a pass; observed {observed}"
        );
    }

    /// A pass that hangs forever must be abandoned, not allowed to park the
    /// loop. `KubeWorkloadInventory::list` had no client-side deadline at all,
    /// so exactly one unanswered LIST stopped reconciliation for five hours
    /// while every process-level health signal still read fine.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_pass_is_abandoned_and_the_loop_keeps_ticking() {
        let started = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let loop_started = Arc::clone(&started);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_loop(
                Duration::from_secs(60),
                Duration::from_secs(120),
                loop_cancel,
                move || {
                    let started = Arc::clone(&loop_started);
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<()>().await;
                    }
                },
            )
            .await
        });

        // Enough virtual time for the first pass to blow its 120s budget and
        // for several further ticks to fire.
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        let observed = started.load(Ordering::SeqCst);
        cancel.cancel();
        assert_eq!(
            handle.await.expect("the loop task itself must not panic"),
            LoopExit::Cancelled
        );
        assert!(
            observed >= 2,
            "a hung pass must be aborted so later ticks still run; only {observed} pass(es) started"
        );
    }

    /// The pass budget is the thing that turns a hang into a bounded failure.
    #[tokio::test(start_paused = true)]
    async fn run_pass_classifies_completion_panic_and_timeout() {
        assert_eq!(
            run_pass(Duration::from_secs(60), async {}).await,
            PassOutcome::Completed
        );
        assert_eq!(
            run_pass(Duration::from_secs(60), async { panic!("boom") }).await,
            PassOutcome::Panicked
        );
        assert_eq!(
            run_pass(Duration::from_secs(60), std::future::pending::<()>()).await,
            PassOutcome::TimedOut
        );
    }

    /// The loop suppresses its very first tick because a reconciliation pass
    /// has just run elsewhere. Pin that, because `become_leader` now spawns
    /// this loop immediately after the topology gate opens and the safety of
    /// that move rests on this suppression.
    #[tokio::test(start_paused = true)]
    async fn the_first_immediate_tick_is_suppressed() {
        let passes = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let loop_passes = Arc::clone(&passes);
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_loop(
                Duration::from_secs(60),
                Duration::from_secs(300),
                loop_cancel,
                move || {
                    let passes = Arc::clone(&loop_passes);
                    async move {
                        passes.fetch_add(1, Ordering::SeqCst);
                    }
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        let observed = passes.load(Ordering::SeqCst);
        cancel.cancel();
        let _ = handle.await;
        assert_eq!(
            observed, 0,
            "the immediate first tick must be consumed without running a pass"
        );
    }

    #[test]
    fn the_default_cadence_is_shorter_than_the_reclaim_settle_window() {
        // The loop's whole purpose is to reclaim rows that the startup pass had
        // to skip because they had not settled yet. A cadence at or above the
        // settle window would let a row wait two full windows before any pass
        // looked at it again.
        let settle =
            djinn_coordinator::build_admission_inventory::DEFAULT_RECLAIM_SETTLE_WINDOW.as_secs();
        assert!(
            DEFAULT_INTERVAL_SECS < settle,
            "default cadence {DEFAULT_INTERVAL_SECS}s must be shorter than the {settle}s settle window"
        );
    }
}
