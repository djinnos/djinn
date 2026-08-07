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
//! The mutex is `tokio::sync::Mutex` because every holder awaits while holding
//! it — that is the whole point, the capture spans the awaited work.

use tokio::sync::{Mutex, MutexGuard};

fn cell() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Install a permissive process-global default subscriber, once.
///
/// Serializing the capturing tests is not sufficient on its own. `tracing`'s
/// interest cache is keyed per callsite and shared by the whole process, and
/// the default when no global subscriber is installed is `NoSubscriber`, which
/// answers `Interest::never()` for everything. Any thread running
/// non-capturing test code can therefore re-cache a callsite as permanently
/// disabled between our `rebuild_interest_cache` and the event we are waiting
/// for — and then the WARN under test is never emitted at all, the capture
/// comes back empty, and the test fails with nothing wrong in the code.
///
/// A global subscriber that ENABLES every callsite and writes to a sink fixes
/// that at the root: interest can no longer be cached as `never`, so every
/// event is dispatched to whatever subscriber is current, which for a
/// capturing test is its own thread-local one.
fn enable_all_callsites_globally() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let sink = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        // Fails only if a global default is already installed, which is fine:
        // something else already decided interest for the process.
        let _ = tracing::subscriber::set_global_default(sink);
    });
}

/// Acquire exclusive access to the process's log-capture seam and re-ask the
/// (now single) default dispatcher about every callsite. Hold the returned
/// guard until the capture has been read.
pub(crate) async fn lock() -> MutexGuard<'static, ()> {
    enable_all_callsites_globally();
    let guard = cell().lock().await;
    tracing::callsite::rebuild_interest_cache();
    guard
}
