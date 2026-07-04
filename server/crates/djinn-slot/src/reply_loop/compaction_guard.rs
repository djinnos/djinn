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
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared state that tracks whether a compaction critical section is active.
///
/// A single instance is intended to live for the lifetime of a slot/reply-loop
/// session; the actor/pool can observe the same instance to decide whether to
/// defer/demote commands. This is intentionally a simple boolean flag; it does
/// not serialize concurrent callers (the reply loop itself is single-tasked).
#[derive(Debug, Clone, Default)]
pub struct CompactionCriticalSection {
    active: Arc<AtomicBool>,
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
}
