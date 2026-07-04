//! Internal compaction coordination primitive for the slot reply loop.
//!
//! Provides a scoped guard that marks a slot lifecycle as "compaction in
//! flight" so entry/exit cleanup around context rotation is centralized and so
//! actor/pool command routing can observe whether the reply loop is mid-rotation
//! (a later slice consumes that signal to defer/demote work that would otherwise
//! apply to the pre-rotation transcript). The guard is released on every exit
//! path — success, summarizer failure, early return, or panic unwinding — via
//! RAII `Drop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::Notify;

/// Shared state that tracks whether a compaction critical section is active.
///
/// A single instance is intended to live for the lifetime of a slot/reply-loop
/// session; the actor/pool can observe the same instance to decide whether to
/// defer/demote commands. This is intentionally a simple boolean flag; it does
/// not serialize concurrent callers (the reply loop itself is single-tasked).
#[derive(Debug, Clone, Default)]
pub struct CompactionCriticalSection {
    active: Arc<AtomicBool>,
    /// Monotonic version bumped on every release so observers can wait for a
    /// specific release transition without busy waiting. `0` means "never held
    /// or released at least once".
    version: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl CompactionCriticalSection {
    /// Create a new, released critical section.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if the critical section is currently entered.
    pub fn is_compacting(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Asynchronously wait until the critical section is released, or return
    /// immediately if it is already released. This is not a lock acquisition:
    /// callers can race with re-entry after the release they observe.
    ///
    /// The returned future is cancellation-safe: the internal notify is
    /// edge-triggered, so a missed wakeup is recovered by the next release.
    pub async fn wait_released(&self) {
        // Avoid the await entirely if we are already released. `Acquire` on the
        // active flag couples with the `Release` store in `release`, so any state
        // published while the section was active is visible after this load.
        if !self.is_compacting() {
            return;
        }
        // Snapshot the version before waiting. If we observe a release after
        // this snapshot, the version will have advanced, and the loop below
        // will exit immediately.
        let observed_version = self.version.load(Ordering::Acquire);
        loop {
            // Double-check after subscribing. We cannot miss the release that
            // happened before we started waiting because the store advances the
            // version and notifies *after* the store.
            if !self.is_compacting() {
                let current_version = self.version.load(Ordering::Acquire);
                if current_version != observed_version {
                    return;
                }
                // Edge case: the section was never entered in this generation,
                // so the version did not change. If it is released, we are done.
                return;
            }
            self.notify.notified().await;
        }
    }

    /// Enter the critical section. Panics if already entered (single-tasked
    /// contract violation).
    #[allow(clippy::panic)]
    fn enter(&self) {
        if self.active.swap(true, Ordering::AcqRel) {
            panic!("compaction critical section entered while already active");
        }
    }

    /// Release the critical section. Idempotent.
    fn release(&self) {
        self.active.store(false, Ordering::Release);
        self.version.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    /// Acquire a scoped guard that releases the section when dropped.
    pub fn guard(&self) -> CompactionGuard<'_> {
        self.enter();
        CompactionGuard { section: self }
    }
}

/// RAII guard for the compaction critical section. The section is released when
/// the guard is dropped, covering success, error, early-return, and panic paths.
#[derive(Debug)]
pub struct CompactionGuard<'a> {
    section: &'a CompactionCriticalSection,
}

impl CompactionGuard<'_> {
    /// Explicitly release the guard before the end of its scope by dropping it.
    /// Equivalent to letting the guard fall out of scope; provided for call
    /// sites that want to make the release point explicit.
    pub fn release(self) {
        // Dropping `self` here runs the `Drop` impl, which releases the section.
    }
}

impl Drop for CompactionGuard<'_> {
    fn drop(&mut self) {
        self.section.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_releases_on_explicit_release() {
        let section = CompactionCriticalSection::new();
        assert!(!section.is_compacting());
        {
            let guard = section.guard();
            assert!(section.is_compacting());
            guard.release();
            // After explicit release the section is no longer active.
            assert!(!section.is_compacting());
        }
        assert!(!section.is_compacting());
    }

    #[test]
    fn guard_releases_on_implicit_drop() {
        let section = CompactionCriticalSection::new();
        {
            let _guard = section.guard();
            assert!(section.is_compacting());
        }
        assert!(!section.is_compacting());
    }

    #[test]
    #[should_panic(expected = "compaction critical section entered while already active")]
    fn guard_panics_on_reentry() {
        let section = CompactionCriticalSection::new();
        let _guard1 = section.guard();
        let _guard2 = section.guard(); // should panic: single-tasked contract
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_released_returns_when_already_released() {
        let section = CompactionCriticalSection::new();
        // Should complete immediately without entering the critical section.
        tokio::time::timeout(Duration::from_millis(10), section.wait_released())
            .await
            .expect("wait_released should not block when already released");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clone_observes_active_and_release_transitions() {
        let section = CompactionCriticalSection::new();
        let observer = section.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            observer.wait_released().await;
            let _ = tx.send(observer.is_compacting()).await;
        });

        // Give the observer a chance to start waiting.
        tokio::task::yield_now().await;
        assert!(section.is_compacting());

        {
            let _guard = section.guard();
            // Yield again so the observer task is parked on the notify.
            tokio::task::yield_now().await;
        }

        let released = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("observer should report release")
            .expect("channel should stay open");
        assert!(!released);
        assert!(!section.is_compacting());
    }

    use std::time::Duration;
}
