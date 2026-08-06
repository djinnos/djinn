//! Serialization for tests that capture `tracing` output.
//!
//! A test that asserts on captured log lines installs a THREAD-LOCAL subscriber
//! (`tracing::dispatcher::set_default`) and then reads what it wrote. That is
//! sound in isolation and racy in a parallel suite, because `tracing` caches
//! per-callsite interest **globally**: whichever subscriber evaluates a
//! callsite first decides, for the whole process, whether that callsite is
//! enabled. A concurrently running test whose subscriber declines the callsite
//! leaves it cached as `never`, and the next test's WARN is never emitted at
//! all — its capture comes back empty and it fails with nothing wrong in the
//! code under test.
//!
//! Every log-capturing test in this crate therefore holds [`lock`] for the
//! duration of its capture and rebuilds the interest cache inside the critical
//! section, so exactly one capturing subscriber is ever live at a time.
//!
//! The mutex is deliberately poison-tolerant: one panicking capture test must
//! not cascade into every other capture test failing on `PoisonError`.

use std::sync::{Mutex, MutexGuard, OnceLock};

fn cell() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Acquire exclusive access to the process's log-capture seam and re-ask the
/// (now single) default dispatcher about every callsite. Hold the returned
/// guard until the capture has been read.
#[must_use]
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    let guard = cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tracing::callsite::rebuild_interest_cache();
    guard
}
