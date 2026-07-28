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

use std::time::Duration;

use tokio::time::MissedTickBehavior;

use crate::server::AppState;

/// Environment variable overriding the reconciliation cadence, in seconds.
pub const INTERVAL_ENV: &str = "DJINN_BUILD_ADMISSION_RECONCILE_INTERVAL_SECS";

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

/// Spawn the periodic build-admission reconciliation task. Leader-only
/// (started from `become_leader`), runs until `state.cancel()` fires.
pub fn spawn(state: AppState) {
    let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(
            ?interval,
            "build_admission_reconcile loop starting (leader-only)"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it. `become_leader` has
        // just run a reconciliation pass of its own, so reconciling again in
        // the same breath would only duplicate API-server traffic during the
        // leadership transition.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("build_admission_reconcile loop cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    // Off (no controller) and non-Kubernetes runtimes make this
                    // a cheap no-op inside the call itself; there is no
                    // separate gate here to drift out of sync with that one.
                    state.reconcile_build_admission_inventory().await;
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
