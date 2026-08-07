//! The leader-scoped provider-action registry (proposal `nafu`, wave 3).
//!
//! # Why a registry exists at all
//!
//! A CI route row in `calling` means "some process is inside a provider
//! mutation for this evidence, and nobody else may touch the row". Recovering
//! such a row is only safe once that process's future is provably gone. The
//! repository already refuses to infer that from elapsed time — it demands a
//! [`CiQuiescenceProof`], and the graceful arm of that proof is a
//! `provider_actions_drained_at` stamp on the former owner's incarnation.
//!
//! Before this module, that stamp had **no producer**. Leadership cancellation
//! released the advisory lock in the same `select!` arm that observed the
//! token, while the coordinator's current tick — provider calls included — kept
//! running detached. A new leader could therefore acquire the lock, see a
//! `calling` row, and find nothing that said "do not touch"; the only thing
//! preventing a duplicate provider episode was that startup recovery also
//! demands the 300-second floor.
//!
//! # What it guarantees
//!
//! Two things, and deliberately not a third:
//!
//! 1. **Admission is closable.** Once [`ProviderActionScope::close_admission`]
//!    returns, [`ProviderActionScope::admit`] returns `None` forever, so no
//!    *new* route can enter `calling`.
//! 2. **In-flight work is joinable.** [`ProviderActionScope::wait_until_empty`]
//!    resolves only when every admitted guard has been dropped.
//!
//! It does **not** promise that an admitted call finished with a known result.
//! A call whose outcome is unknown leaves its charged row in `calling` for
//! startup recovery; that is the proposal's `outcome_unknown` path and it is
//! correct. What the scope buys is that the *future* is gone before the lock is
//! released — which is the only thing exclusion needs.
//!
//! # The ordering this enforces
//!
//! ```text
//! cancellation  ->  close admission  ->  join in-flight  ->  stamp drained
//!               ->  release advisory lock  ->  new acquisition
//! ```
//!
//! Nothing here releases the lock; leadership does. The scope's job is to make
//! "is it safe to release yet?" an answerable question, and
//! [`ProviderActionScope::wait_until_drained`] is where leadership asks it.
//!
//! [`CiQuiescenceProof`]: (see `djinn_db::CiQuiescenceProof`)

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct ScopeState {
    /// Whether a new provider action may still enter `calling`.
    admission_closed: bool,
    /// Admitted, not-yet-dropped guards.
    in_flight: usize,
    /// Set once the owner has joined its futures *and* stamped the
    /// incarnation. Leadership waits on this, not on `in_flight == 0`, because
    /// an empty scope that has not been stamped proves nothing to a future
    /// recovering incarnation.
    drained: bool,
    /// How many actions were ever admitted. Reporting only.
    admitted_total: u64,
    /// How many admissions were refused after the gate closed. Reporting only:
    /// a non-zero count is the quiescing state doing its job.
    refused_total: u64,
}

/// A cloneable handle to the leader's provider-action scope.
#[derive(Clone, Debug)]
pub struct ProviderActionScope {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<ScopeState>,
    /// Woken on every guard drop and on `mark_drained`.
    notify: Notify,
}

impl Default for ProviderActionScope {
    fn default() -> Self {
        Self::new()
    }
}

/// A snapshot for the rollback quiescence report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderActionScopeCounts {
    pub admission_closed: bool,
    pub in_flight: usize,
    pub drained: bool,
    pub admitted_total: u64,
    pub refused_total: u64,
}

impl ProviderActionScope {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(ScopeState::default()),
                notify: Notify::new(),
            }),
        }
    }

    /// Try to enter the scope.
    ///
    /// `None` means admission is closed and the caller **must not** perform the
    /// provider mutation — nor advance its route row to `calling`, since a
    /// charged `calling` row with no future behind it is the worst of both
    /// worlds: it holds its charge and nothing will ever finalize it except
    /// startup recovery.
    ///
    /// The returned guard must be held across the whole provider call. It is
    /// deliberately not `Clone`.
    #[must_use]
    pub fn admit(&self) -> Option<ProviderActionGuard> {
        let mut state = self.inner.state.lock().expect("scope mutex poisoned");
        if state.admission_closed {
            state.refused_total += 1;
            return None;
        }
        state.in_flight += 1;
        state.admitted_total += 1;
        Some(ProviderActionGuard {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Close admission. Idempotent, and irreversible for this leadership term —
    /// a scope that could reopen would let a route enter `calling` after the
    /// drain stamp was written, which is precisely the state the stamp claims
    /// cannot exist.
    pub fn close_admission(&self) {
        let mut state = self.inner.state.lock().expect("scope mutex poisoned");
        state.admission_closed = true;
        drop(state);
        self.inner.notify.notify_waiters();
    }

    #[must_use]
    pub fn is_admission_closed(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("scope mutex poisoned")
            .admission_closed
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("scope mutex poisoned")
            .in_flight
    }

    #[must_use]
    pub fn counts(&self) -> ProviderActionScopeCounts {
        let state = self.inner.state.lock().expect("scope mutex poisoned");
        ProviderActionScopeCounts {
            admission_closed: state.admission_closed,
            in_flight: state.in_flight,
            drained: state.drained,
            admitted_total: state.admitted_total,
            refused_total: state.refused_total,
        }
    }

    /// Resolve once no admitted guard remains.
    ///
    /// Note the subscribe-then-check order. Subscribing first is what closes
    /// the window in which the last guard drops between the check and the
    /// wait — a lost notification there would hang shutdown until the process
    /// was killed, which is exactly the "abrupt death" arm this path exists to
    /// avoid needing.
    pub async fn wait_until_empty(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.in_flight() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Record that the owner has closed admission, joined every future, and
    /// written its incarnation's drain stamp.
    pub fn mark_drained(&self) {
        let mut state = self.inner.state.lock().expect("scope mutex poisoned");
        debug_assert!(
            state.admission_closed,
            "a scope may not be marked drained while it still admits actions"
        );
        debug_assert_eq!(
            state.in_flight, 0,
            "a scope may not be marked drained with futures still in flight"
        );
        state.drained = true;
        drop(state);
        self.inner.notify.notify_waiters();
    }

    #[must_use]
    pub fn is_drained(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("scope mutex poisoned")
            .drained
    }

    /// Wait, up to `timeout`, for the owner to finish draining.
    ///
    /// Returns `true` when the drain completed. On `false` the caller is
    /// releasing the advisory lock **without** a graceful proof — which is a
    /// legal, if worse, outcome: no `provider_actions_drained_at` stamp is
    /// written, so a future incarnation reading the row finds
    /// [`CiQuiescenceProof::GracefulDrain`] unsatisfied and defers rather than
    /// racing a possibly-live future. The timeout degrades safety of *recovery
    /// latency*, never of exclusion.
    pub async fn wait_until_drained(&self, timeout: std::time::Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.inner.notify.notified();
                if self.is_drained() {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

/// Proof that one provider action is inside the scope. Dropping it leaves.
#[derive(Debug)]
pub struct ProviderActionGuard {
    inner: Arc<Inner>,
}

impl Drop for ProviderActionGuard {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("scope mutex poisoned");
        state.in_flight = state.in_flight.saturating_sub(1);
        let empty = state.in_flight == 0;
        drop(state);
        if empty {
            self.inner.notify.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn an_open_scope_admits_and_an_empty_scope_resolves_immediately() {
        let scope = ProviderActionScope::new();
        assert!(!scope.is_admission_closed());
        let guard = scope.admit().expect("an open scope admits");
        assert_eq!(scope.in_flight(), 1);
        drop(guard);
        assert_eq!(scope.in_flight(), 0);
        scope.wait_until_empty().await;
    }

    /// The gate is what makes "no new route enters `calling`" true. A refusal is
    /// counted rather than silent, because the quiescing state is reported.
    #[tokio::test]
    async fn a_closed_scope_refuses_every_further_admission() {
        let scope = ProviderActionScope::new();
        let held = scope.admit().expect("admitted before closing");
        scope.close_admission();

        assert!(scope.admit().is_none());
        assert!(scope.admit().is_none());
        assert_eq!(scope.counts().refused_total, 2);
        assert_eq!(scope.counts().in_flight, 1, "the held guard is unaffected");

        drop(held);
        scope.wait_until_empty().await;
    }

    /// The join is real: `wait_until_empty` must not resolve while a guard is
    /// outstanding, and must resolve once it is dropped.
    #[tokio::test]
    async fn waiting_joins_an_outstanding_action() {
        let scope = ProviderActionScope::new();
        let guard = scope.admit().unwrap();

        let waiter = {
            let scope = scope.clone();
            tokio::spawn(async move { scope.wait_until_empty().await })
        };

        // Still in flight: the waiter must not have resolved.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), async {})
                .await
                .is_ok()
        );
        assert!(!waiter.is_finished());

        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter must resolve once the scope empties")
            .expect("waiter task panicked");
    }

    /// Leadership waits on the *stamp*, not on emptiness. An empty scope whose
    /// owner has not written `provider_actions_drained_at` proves nothing to a
    /// future recovering incarnation, so releasing the lock there would hand
    /// out an unprovable handoff.
    #[tokio::test]
    async fn drained_is_a_stronger_signal_than_empty() {
        let scope = ProviderActionScope::new();
        scope.close_admission();
        scope.wait_until_empty().await;

        assert!(!scope.is_drained(), "empty is not drained");
        assert!(
            !scope.wait_until_drained(Duration::from_millis(50)).await,
            "leadership must not treat an unstamped scope as a graceful handoff"
        );

        scope.mark_drained();
        assert!(scope.wait_until_drained(Duration::from_secs(5)).await);
    }

    /// A timeout is a degraded outcome, not a corrupt one: the caller learns it
    /// has no graceful proof and the scope never claims otherwise.
    #[tokio::test]
    async fn a_timed_out_drain_reports_no_graceful_proof() {
        let scope = ProviderActionScope::new();
        let _held = scope.admit().unwrap();
        scope.close_admission();

        assert!(!scope.wait_until_drained(Duration::from_millis(50)).await);
        assert!(!scope.is_drained());
    }
}
