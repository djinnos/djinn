//! Regression coverage for the `TemplateCloneDatabase` runtime-ownership bug.
//!
//! `Database::open_in_memory` constructs a SQLx pool that spawns maintenance
//! tasks (`PoolInner::new` → `spawn_maintenance_tasks`). Those tasks call
//! `crate::rt::spawn`, which requires a live Tokio runtime handle via
//! `Handle::try_current` — otherwise sqlx panics with
//! `this functionality requires a Tokio context`.
//!
//! The original bug created the pool inside a temporary runtime that was
//! dropped immediately, leaving an orphaned pool. The fix owns the runtime in
//! the returned guard. This test reproduces the CI path: multiple concurrently
//! held and dropped guards inside scoped threads (matching the
//! `execute_selected` runner), and confirms no panic occurs.

// `eprintln!` is used for skip-reason diagnostics, matching the pattern in
// `djinn-k8s/tests/kind_smoke.rs`.
#![allow(clippy::print_stderr)]

use std::thread;

use djinn_qa::run::{DatabaseAcquirer, TemplateCloneDatabase};

#[test]
fn concurrent_guard_acquisition_and_drop_does_not_panic_without_tokio_context() {
    // The regression reproduces the CI path only when the Postgres
    // template-clone test database is reachable. When the backing service is
    // unavailable, `acquire` fails with a connection error (not the bug), so
    // skip cleanly rather than failing for the wrong reason.
    let acquirer = TemplateCloneDatabase;
    if let Err(reason) = acquirer.acquire() {
        eprintln!(
            "skipping template-clone database runtime regression: \
             Postgres/template prerequisite unavailable ({reason})"
        );
        return;
    }

    // Acquire, hold, and drop several guards concurrently inside scoped
    // threads — exactly the `execute_selected` runner path. Each thread
    // intentionally has no ambient Tokio runtime, so the guard's owned runtime
    // is the only thing keeping the pool's maintenance tasks alive.
    let guards = thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    // `acquire` must succeed without a Tokio context present on
                    // this thread.
                    acquirer.acquire().expect("acquire isolated database guard")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("scoped thread did not panic"))
            .collect::<Vec<_>>()
    });

    // All guards drop here while still on threads without an ambient Tokio
    // runtime. Before the fix, the pool's Drop would try to spawn cleanup work
    // and panic with "this functionality requires a Tokio context".
    drop(guards);
}
