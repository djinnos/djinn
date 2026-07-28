//! Which child PIDs are already owned by an in-process waiter.
//!
//! # The bug this exists to prevent
//!
//! `djinn-agent-worker` runs as a Linux subreaper (`PR_SET_CHILD_SUBREAPER`)
//! with one background thread calling `waitpid(-1, WNOHANG)` in a loop, so that
//! processes reparented onto the worker are reaped instead of accumulating as
//! zombies (see `djinn_graph::child_reaper`).
//!
//! `waitpid(-1, ...)` does not distinguish "an orphan that was reparented onto
//! me" from "a child *I* spawned and am already waiting on". A process may have
//! only one consumer of a given child's terminal status, and `tokio::process`
//! is such a consumer: on Linux it opens a `pidfd` for every child it spawns,
//! and once that fd signals readiness it calls `waitpid(pid, WNOHANG)` to
//! collect the status. If the reaper's `waitpid(-1, ...)` gets there first, the
//! zombie is gone and tokio's call fails with **`ECHILD` ("No child
//! processes")** — surfacing as an I/O error on a subprocess that in fact ran
//! perfectly.
//!
//! Measured on this codebase's reaper against `git rev-parse HEAD`:
//!
//! ```text
//! reaper idle-polling at 10ms   ~0.2% of spawns stolen  (≈10 in 6000)
//! reaper not sleeping           100%  of spawns stolen  (200 in 200)
//! ```
//!
//! The reaper skips its sleep whenever the previous pass reaped something, so
//! *while it is draining adopted orphans it is effectively spinning* — and any
//! subprocess that exits inside that window is stolen with near-certainty. That
//! is why this is not a benign flake: it is quiet when the process is idle and
//! near-deterministic in exactly the moments a process tree is tearing down.
//!
//! # The contract
//!
//! A spawner that intends to wait on its own child registers the PID here for
//! as long as it holds wait rights. The reaper consults [`is_owned`] and
//! refuses to consume a registered PID, leaving it for its rightful waiter.
//!
//! Registration must be atomic with respect to a reaper pass, or a child that
//! forks and exits before its PID lands in the set is still stealable.
//! [`spawn_owned`] therefore performs the spawn *and* the registration while
//! holding [`SPAWN_GATE`] shared, and the reaper takes it exclusively around
//! one wait pass. The gate is uncontended in practice: a reaper pass is a
//! couple of syscalls.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock, RwLock, RwLockWriteGuard};

/// PIDs whose terminal status belongs to an in-process waiter.
fn owned() -> &'static Mutex<HashSet<u32>> {
    static OWNED: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    OWNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Serializes "spawn + register" against a reaper pass. Held **shared** by
/// spawners and **exclusively** by the reaper, so a forked-but-unregistered
/// child cannot be observed by a wait pass.
fn spawn_gate() -> &'static RwLock<()> {
    static GATE: OnceLock<RwLock<()>> = OnceLock::new();
    GATE.get_or_init(|| RwLock::new(()))
}

/// Exclusive access for the duration of one reaper wait pass.
///
/// Held by `djinn_graph::child_reaper`'s Linux waiter so that no spawn can
/// interleave between another thread's `fork` and its [`spawn_owned`]
/// registration.
pub fn reaper_pass() -> RwLockWriteGuard<'static, ()> {
    spawn_gate().write().unwrap_or_else(|e| e.into_inner())
}

/// Whether `pid`'s terminal status belongs to an in-process waiter, meaning a
/// reaper must not consume it.
#[must_use]
pub fn is_owned(pid: u32) -> bool {
    owned()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&pid)
}

/// Number of currently registered PIDs. Diagnostics and tests only.
#[must_use]
pub fn owned_count() -> usize {
    owned().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// Wait rights over one child PID. Deregisters on drop, so a panic or an early
/// return cannot strand a PID that the reaper would then never collect.
#[derive(Debug)]
pub struct OwnedChild {
    pid: Option<u32>,
}

impl OwnedChild {
    /// A guard over no PID. Used when a spawn produced no usable PID (the
    /// child was already reaped by the platform layer), which is not an error —
    /// there is simply nothing to protect.
    #[must_use]
    pub fn none() -> Self {
        Self { pid: None }
    }

    /// The protected PID, if any.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Release wait rights early. Equivalent to dropping the guard.
    pub fn release(mut self) {
        self.deregister();
    }

    fn deregister(&mut self) {
        if let Some(pid) = self.pid.take() {
            owned()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid);
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.deregister();
    }
}

/// Spawn a child and claim wait rights over it in one atomic step.
///
/// `spawn` is invoked while the reaper is held out, and `pid_of` extracts the
/// new child's PID from whatever handle `spawn` produced (`std::process::Child`
/// and `tokio::process::Child` both expose one). The returned [`OwnedChild`]
/// must be held until the child has been waited on.
///
/// Callers that spawn a child and then wait on it themselves **must** go
/// through this function; a bare `Command::spawn` in a process that also runs a
/// `waitpid(-1, ...)` reaper is racing that reaper for its own child's exit
/// status.
pub fn spawn_owned<T, E>(
    spawn: impl FnOnce() -> Result<T, E>,
    pid_of: impl FnOnce(&T) -> Option<u32>,
) -> Result<(T, OwnedChild), E> {
    let _gate = spawn_gate().read().unwrap_or_else(|e| e.into_inner());
    let child = spawn()?;
    let pid = pid_of(&child);
    if let Some(pid) = pid {
        owned()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid);
    }
    Ok((child, OwnedChild { pid }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_registers_and_deregisters() {
        let (pid, guard) = spawn_owned(|| Ok::<_, ()>(4_242_001_u32), |p| Some(*p)).unwrap();
        assert!(is_owned(pid), "pid must be protected while the guard lives");
        drop(guard);
        assert!(!is_owned(pid), "dropping the guard must release the pid");
    }

    #[test]
    fn release_is_equivalent_to_drop() {
        let (pid, guard) = spawn_owned(|| Ok::<_, ()>(4_242_002_u32), |p| Some(*p)).unwrap();
        assert!(is_owned(pid));
        guard.release();
        assert!(!is_owned(pid));
    }

    #[test]
    fn a_spawn_failure_registers_nothing() {
        let before = owned_count();
        let result = spawn_owned(|| Err::<u32, _>("boom"), |p| Some(*p));
        assert!(result.is_err());
        assert_eq!(owned_count(), before);
    }

    #[test]
    fn a_pidless_child_yields_an_inert_guard() {
        let (_child, guard) = spawn_owned(|| Ok::<_, ()>(()), |()| None).unwrap();
        assert_eq!(guard.pid(), None);
    }
}
