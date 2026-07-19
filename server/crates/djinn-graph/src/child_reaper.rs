//! Worker-scoped child lifecycle registry.
//!
//! On Linux this module owns the worker's `waitpid(-1, ...)` loop.  Code which
//! registers a direct child must never call `Child::{wait,try_wait}` itself:
//! the terminal status is delivered through the [`DirectChild`] receiver.
//! Unrecognised statuses are retained as adopted-child records, rather than
//! being silently discarded because they do not belong to a known process
//! group.  This matters when the warm worker is a Linux subreaper.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};

/// Identifier supplied by the command supervisor when it reserves admission.
pub type CommandId = String;

/// A reaped terminal child status.  This intentionally does not use
/// `std::process::ExitStatus`: the reaper receives raw wait statuses and must
/// not manufacture a platform-specific `ExitStatus` for a child it did not
/// spawn through `std::process::Child`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalStatus {
    /// PID returned by `waitpid`.
    pub pid: u32,
    /// Raw status word returned by `waitpid`.
    pub raw_status: i32,
}

impl TerminalStatus {
    /// Normal process exit code, if the process exited normally.
    #[must_use]
    pub fn code(self) -> Option<i32> {
        if self.raw_status & 0x7f == 0 {
            Some((self.raw_status >> 8) & 0xff)
        } else {
            None
        }
    }

    /// Terminating signal, if the process was signalled.
    #[must_use]
    pub fn signal(self) -> Option<i32> {
        let signal = self.raw_status & 0x7f;
        (signal != 0 && signal != 0x7f).then_some(signal)
    }
}

/// Direct-child registration metadata visible to shutdown orchestration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisorRecord {
    pub command_id: CommandId,
    pub pid: u32,
    pub pgid: i32,
}

/// An adopted or otherwise unregistered child after its authoritative terminal
/// status has been reaped. Records are retained until [`ChildReaper::drain`]
/// takes them, so unexpected descendants are observable rather than dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptedChildRecord {
    pub status: TerminalStatus,
}

#[derive(Debug)]
struct SupervisorEntry {
    record: SupervisorRecord,
    status_tx: mpsc::Sender<TerminalStatus>,
}

#[derive(Debug, Default)]
struct Registry {
    admission_open: bool,
    /// In-flight admission reservations obtained via [`ChildReaper::admit`]
    /// that have not yet been resolved by [`Admission::register_direct`] or
    /// released.  This gives [`ChildReaper::close_admission`], supervisor-idle,
    /// and adopted-idle hooks unambiguous race behaviour: shutdown cannot
    /// observe an empty registry while a reservation is still outstanding.
    pending_admissions: usize,
    supervisors: HashMap<u32, SupervisorEntry>,
    adopted: Vec<AdoptedChildRecord>,
}

#[derive(Debug)]
struct Inner {
    registry: Mutex<Registry>,
    changed: Condvar,
}

/// Cloneable worker-wide owner for Linux child-status consumption.
///
/// Construct exactly one instance during warm-worker startup, before commands
/// are admitted. Clones are cheap handles to that same registry and never
/// create another wait consumer.
#[derive(Clone, Debug)]
pub struct ChildReaper {
    inner: Arc<Inner>,
}

/// A command has passed the admission gate but has not yet exposed its spawned
/// child to the caller. Calling [`Admission::register_direct`] installs the PID
/// route before any code receives wait control for that child.
///
/// If the admission is neither registered nor explicitly released, dropping it
/// decrements the in-flight reservation counter so shutdown accounting stays
/// deterministic.
#[derive(Debug)]
pub struct Admission {
    reaper: ChildReaper,
    command_id: CommandId,
    /// Whether the reservation has been resolved.  Set to `true` inside
    /// [`Admission::register_direct`] after the pending counter is decremented
    /// so that [`Drop`] does not double-release.
    resolved: bool,
}

/// The sole terminal-status receiver for one registered direct child.
#[derive(Debug)]
pub struct DirectChild {
    record: SupervisorRecord,
    status_rx: mpsc::Receiver<TerminalStatus>,
}

impl ChildReaper {
    /// Start a worker-scoped lifecycle registry.
    ///
    /// Linux starts one background `waitpid(-1, WNOHANG)` consumer. Other
    /// platforms deliberately provide the same registration seam without a
    /// wait consumer so existing non-Linux process behavior remains available
    /// to callers until a platform implementation is introduced.
    #[must_use]
    pub fn new() -> Self {
        let reaper = Self::new_inner();
        #[cfg(target_os = "linux")]
        reaper.start_linux_waiter();
        reaper
    }

    fn new_inner() -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(Registry {
                    admission_open: true,
                    ..Registry::default()
                }),
                changed: Condvar::new(),
            }),
        }
    }

    // Unit tests exercise registry transitions without installing a second
    // process-wide wait consumer that could reap a fixture belonging to a
    // neighbouring test. Linux integration tests should use `new`.
    #[cfg(test)]
    fn inert_for_test() -> Self {
        Self::new_inner()
    }

    /// Reserve command admission before spawning. Once closed, no new command
    /// can obtain a reservation. The returned [`Admission`] holds one
    /// in-flight reservation that is resolved when [`Admission::register_direct`]
    /// is called, or released when the [`Admission`] is dropped.
    pub fn admit(&self, command_id: impl Into<CommandId>) -> io::Result<Admission> {
        let command_id = command_id.into();
        let mut registry = self.inner.registry.lock().expect("child registry poisoned");
        if !registry.admission_open {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child lifecycle admission is closed",
            ));
        }
        registry.pending_admissions += 1;
        drop(registry);
        Ok(Admission {
            reaper: self.clone(),
            command_id,
            resolved: false,
        })
    }

    /// Prevent new command admission. Existing reservations remain valid and
    /// can still register a direct child; the pending counter tracks them until
    /// resolved or dropped.
    pub fn close_admission(&self) {
        let mut registry = self.inner.registry.lock().expect("child registry poisoned");
        registry.admission_open = false;
        self.inner.changed.notify_all();
    }

    /// Whether shutdown has closed command admission.
    #[must_use]
    pub fn admission_open(&self) -> bool {
        self.inner
            .registry
            .lock()
            .expect("child registry poisoned")
            .admission_open
    }

    /// Number of in-flight admission reservations not yet resolved by
    /// registration or release.
    #[must_use]
    pub fn pending_admissions(&self) -> usize {
        self.inner
            .registry
            .lock()
            .expect("child registry poisoned")
            .pending_admissions
    }

    /// Snapshot currently registered direct-child supervisors.
    #[must_use]
    pub fn supervisors(&self) -> Vec<SupervisorRecord> {
        self.inner
            .registry
            .lock()
            .expect("child registry poisoned")
            .supervisors
            .values()
            .map(|entry| entry.record.clone())
            .collect()
    }

    /// Snapshot terminal statuses of unknown/adopted children retained so far.
    #[must_use]
    pub fn adopted_children(&self) -> Vec<AdoptedChildRecord> {
        self.inner
            .registry
            .lock()
            .expect("child registry poisoned")
            .adopted
            .clone()
    }

    /// Deterministically wait for all registered direct-child routes to empty
    /// and all in-flight admission reservations to be resolved.
    pub fn wait_for_supervisors_idle(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |registry| {
            registry.supervisors.is_empty() && registry.pending_admissions == 0
        })
    }

    /// Deterministic drain hook for adopted-child accounting. This becomes idle
    /// only after callers have acknowledged records through [`Self::drain`] and
    /// every pre-closure admission reservation has resolved. A pending
    /// reservation may still claim a retained status through
    /// [`Admission::register_direct`], so shutdown must not treat adopted
    /// accounting as idle before that claim window closes.
    pub fn wait_for_adopted_idle(&self, timeout: Duration) -> bool {
        self.wait_until(timeout, |registry| {
            registry.adopted.is_empty() && registry.pending_admissions == 0
        })
    }

    /// Take all recorded adopted-child terminal statuses. This is the explicit
    /// acknowledgement point used by shutdown/tests; records are never silently
    /// discarded by the wait loop. While an admission reservation is pending,
    /// retained statuses remain claimable by `register_direct`, so this returns
    /// no records rather than racing registration and stranding its PID route.
    pub fn drain(&self) -> Vec<AdoptedChildRecord> {
        let mut registry = self.inner.registry.lock().expect("child registry poisoned");
        if registry.pending_admissions != 0 {
            return Vec::new();
        }
        let records = std::mem::take(&mut registry.adopted);
        self.inner.changed.notify_all();
        records
    }

    fn wait_until(&self, timeout: Duration, done: impl Fn(&Registry) -> bool) -> bool {
        let clock = SystemClock::new();
        let deadline = clock.now_instant() + timeout;
        let mut registry = self.inner.registry.lock().expect("child registry poisoned");
        while !done(&registry) {
            let remaining = deadline.saturating_duration_since(clock.now_instant());
            if remaining.is_zero() {
                return false;
            }
            let (next, result) = self
                .inner
                .changed
                .wait_timeout(registry, remaining)
                .expect("child registry poisoned");
            registry = next;
            if result.timed_out() && !done(&registry) {
                return false;
            }
        }
        true
    }

    /// Route a reaped terminal status. If the PID belongs to a registered
    /// supervisor, the status is delivered exactly once through the channel and
    /// the supervisor entry is removed. Otherwise the status is retained as an
    /// adopted-child record for later [`Self::drain`].
    fn record_terminal(&self, status: TerminalStatus) {
        let delivery = {
            let mut registry = self.inner.registry.lock().expect("child registry poisoned");
            let direct = registry.supervisors.remove(&status.pid);
            let delivery = direct.map(|entry| entry.status_tx);
            if delivery.is_none() {
                tracing::debug!(
                    pid = status.pid,
                    raw_status = status.raw_status,
                    "reaped adopted child"
                );
                registry.adopted.push(AdoptedChildRecord { status });
            }
            self.inner.changed.notify_all();
            delivery
        };
        // Send after releasing the registry lock. Removal above makes routing
        // exactly-once even if duplicate status injection is attempted in tests.
        if let Some(tx) = delivery {
            let _ = tx.send(status);
        }
    }

    #[cfg(target_os = "linux")]
    fn start_linux_waiter(&self) {
        let reaper = self.clone();
        std::thread::Builder::new()
            .name("djinn-child-reaper".to_owned())
            .spawn(move || {
                loop {
                    let mut reaped_any = false;
                    loop {
                        let mut raw_status = 0;
                        // SAFETY: `raw_status` is valid writable memory and -1 with
                        // WNOHANG asks the kernel for any child of this worker.
                        let pid = unsafe { libc::waitpid(-1, &mut raw_status, libc::WNOHANG) };
                        if pid > 0 {
                            reaped_any = true;
                            reaper.record_terminal(TerminalStatus {
                                pid: pid as u32,
                                raw_status,
                            });
                            continue;
                        }
                        if pid < 0 {
                            let err = io::Error::last_os_error();
                            if err.raw_os_error() != Some(libc::ECHILD)
                                && err.raw_os_error() != Some(libc::EINTR)
                            {
                                tracing::warn!(error = %err, "child reaper waitpid failed");
                            }
                        }
                        break;
                    }
                    // `waitpid` has no portable notification fd here. A short sleep
                    // avoids SIGCHLD-handler races while keeping registry hooks
                    // deterministic and shutdown polling bounded.
                    if !reaped_any {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            })
            .expect("failed to start Linux child reaper");
    }
}

impl Default for ChildReaper {
    fn default() -> Self {
        Self::new()
    }
}

impl Admission {
    /// Install the PID/PGID status route immediately after successful spawn and
    /// before returning any spawn control to the command supervisor.
    ///
    /// # Reconciliation
    ///
    /// If the child terminated *before* this call (a fast direct child reaped
    /// by the Linux waiter before the PID route existed), a retained
    /// [`AdoptedChildRecord`] for the same PID is atomically reconciled while
    /// holding the registry lock: the record is removed from adopted
    /// accounting, no supervisor entry is inserted, and the terminal status is
    /// delivered exactly once through the returned [`DirectChild`] after
    /// unlocking. The supervisor registry is left idle — no stranded route.
    pub fn register_direct(mut self, pid: u32, pgid: i32) -> io::Result<DirectChild> {
        let (status_tx, status_rx) = mpsc::channel();
        let record = SupervisorRecord {
            command_id: self.command_id.clone(),
            pid,
            pgid,
        };

        // Holder for the sender: taken when inserting the supervisor route in
        // the normal path, or retained for immediate delivery in the
        // reconciliation path.
        let mut tx_holder = Some(status_tx);

        let reconciled = {
            let mut registry = self
                .reaper
                .inner
                .registry
                .lock()
                .expect("child registry poisoned");

            if registry.supervisors.contains_key(&pid) {
                // Error path: Drop releases the pending reservation.
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("direct child PID {pid} is already registered"),
                ));
            }

            // Atomically reconcile any pre-registration adopted terminal status
            // for this PID. This closes the window where a fast direct child is
            // classified as adopted before its PID route is inserted.
            let recon = registry
                .adopted
                .iter()
                .position(|r| r.status.pid == pid)
                .map(|idx| registry.adopted.remove(idx).status);

            if recon.is_none() {
                // Normal case: no prior status. Insert the supervisor route so
                // the future terminal status is delivered exactly once.
                registry.supervisors.insert(
                    pid,
                    SupervisorEntry {
                        record: record.clone(),
                        status_tx: tx_holder
                            .take()
                            .expect("sender available when no reconciliation"),
                    },
                );
            }
            // If recon is Some, the child is already dead. We do NOT insert a
            // supervisor entry (it would never receive a status) and instead
            // deliver the reconciled status after unlocking.

            registry.pending_admissions = registry.pending_admissions.saturating_sub(1);
            self.reaper.inner.changed.notify_all();
            recon
        };

        // Reservation resolved; prevent Drop from double-releasing the counter.
        self.resolved = true;

        // Deliver the reconciled terminal status after releasing the registry
        // lock, so no code holds the lock while crossing a channel boundary.
        if let Some(status) = reconciled
            && let Some(tx) = tx_holder.take()
        {
            let _ = tx.send(status);
        }

        Ok(DirectChild { record, status_rx })
    }

    /// Explicitly release an admission reservation that will not produce a
    /// registered direct child (e.g. spawn failed). Decrements the in-flight
    /// admission counter deterministically.
    pub fn release(self) {
        // Drop handles the pending-admission decrement when resolved is false.
        drop(self);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        // The admission was neither registered nor explicitly released (e.g.
        // spawn failed, caller dropped it, or register_direct hit an error).
        // Release the pending reservation so shutdown accounting is exact.
        let mut registry = self
            .reaper
            .inner
            .registry
            .lock()
            .expect("child registry poisoned");
        registry.pending_admissions = registry.pending_admissions.saturating_sub(1);
        self.reaper.inner.changed.notify_all();
    }
}

impl DirectChild {
    /// Registered direct-child metadata.
    #[must_use]
    pub fn record(&self) -> &SupervisorRecord {
        &self.record
    }

    /// Wait for the one terminal status routed by the worker-wide reaper.
    pub fn recv_timeout(&self, timeout: Duration) -> io::Result<TerminalStatus> {
        self.status_rx
            .recv_timeout(timeout)
            .map_err(|err| match err {
                mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "child status timed out")
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "child reaper stopped")
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_decodes_exit_code_and_signal() {
        let exited = TerminalStatus {
            pid: 1,
            raw_status: 7 << 8,
        };
        assert_eq!(exited.code(), Some(7));
        assert_eq!(exited.signal(), None);

        let signalled = TerminalStatus {
            pid: 2,
            raw_status: 9, // SIGKILL
        };
        assert_eq!(signalled.code(), None);
        assert_eq!(signalled.signal(), Some(9));

        // Linux WIFSTOPPED uses 0x7f in the low byte; not signalled, not exited.
        let stopped = TerminalStatus {
            pid: 3,
            raw_status: 0x7f,
        };
        assert_eq!(stopped.code(), None);
        assert_eq!(stopped.signal(), None);
    }

    #[test]
    fn admission_closure_rejects_new_commands() {
        let reaper = ChildReaper::inert_for_test();
        let _before = reaper.admit("before-close").unwrap();
        // The reservation is held alive; pending count reflects it.
        assert_eq!(reaper.pending_admissions(), 1);

        reaper.close_admission();
        assert!(!reaper.admission_open());
        let err = reaper
            .admit("after-close")
            .expect_err("admission must close");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn dropped_admission_releases_pending_reservation() {
        let reaper = ChildReaper::inert_for_test();
        {
            let _admission = reaper.admit("ephemeral").unwrap();
            assert_eq!(reaper.pending_admissions(), 1);
        }
        // The admission was dropped without registering; pending count returns
        // to zero deterministically.
        assert_eq!(reaper.pending_admissions(), 0);
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));
    }

    #[test]
    fn released_admission_releases_pending_reservation() {
        let reaper = ChildReaper::inert_for_test();
        let admission = reaper.admit("explicit-release").unwrap();
        assert_eq!(reaper.pending_admissions(), 1);
        admission.release();
        assert_eq!(reaper.pending_admissions(), 0);
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));
    }

    #[test]
    fn admission_interleaving_with_closure() {
        // A reservation legitimately obtained *before* closure can still
        // register a direct child after admission is closed. This is the
        // deterministic pre-closure-reservation completion semantics.
        let reaper = ChildReaper::inert_for_test();
        let admission = reaper.admit("pre-close-cmd").unwrap();
        reaper.close_admission();

        // Registry is not idle while the reservation is outstanding.
        assert!(!reaper.wait_for_supervisors_idle(Duration::ZERO));

        let child = admission
            .register_direct(999_995, 42)
            .expect("pre-closure reservation can still register");
        assert_eq!(child.record().command_id, "pre-close-cmd");

        // The reservation resolved into a supervisor route.
        assert_eq!(reaper.pending_admissions(), 0);
        assert!(!reaper.wait_for_supervisors_idle(Duration::ZERO));

        reaper.record_terminal(TerminalStatus {
            pid: 999_995,
            raw_status: 0,
        });
        let status = child.recv_timeout(Duration::from_millis(50)).unwrap();
        assert_eq!(status.code(), Some(0));

        // Now the supervisor registry is idle.
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));
    }

    #[test]
    fn direct_status_is_routed_once_and_registry_becomes_idle() {
        // Do not use a real PID: this tests registry transitions without racing
        // the process-wide Linux wait loop used by other tests.
        let reaper = ChildReaper::inert_for_test();
        let child = reaper
            .admit("command-a")
            .unwrap()
            .register_direct(999_991, 12)
            .unwrap();
        assert_eq!(reaper.supervisors().len(), 1);
        assert_eq!(reaper.pending_admissions(), 0);

        reaper.record_terminal(TerminalStatus {
            pid: 999_991,
            raw_status: 7 << 8,
        });
        assert_eq!(
            child
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .code(),
            Some(7)
        );
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));

        // A duplicate terminal report cannot be delivered to the completed
        // direct route and is explicitly accounted as an unexpected child.
        reaper.record_terminal(TerminalStatus {
            pid: 999_991,
            raw_status: 7 << 8,
        });
        assert_eq!(reaper.adopted_children().len(), 1);
        // Supervisor registry is still idle (the duplicate went to adopted).
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));
    }

    #[test]
    fn duplicate_registration_is_rejected_and_reservation_released() {
        let reaper = ChildReaper::inert_for_test();
        let first = reaper.admit("first").unwrap();
        let _child = first.register_direct(999_996, 1).unwrap();
        assert_eq!(reaper.pending_admissions(), 0);

        let second = reaper.admit("second").unwrap();
        assert_eq!(reaper.pending_admissions(), 1);
        let err = second
            .register_direct(999_996, 1)
            .expect_err("duplicate PID rejected");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // The failed registration releases the reservation via Drop.
        assert_eq!(reaper.pending_admissions(), 0);
    }

    #[test]
    fn direct_child_reconciles_pre_registration_status() {
        // THE KEY RACE FIX: a terminal status arrives for a PID *before*
        // register_direct is called. Registration must atomically reconcile
        // the retained adopted record, deliver the status exactly once through
        // the DirectChild, remove the record from adopted accounting, and leave
        // the supervisor registry idle — no stranded route.
        let reaper = ChildReaper::inert_for_test();

        // Status arrives first (fast direct child or race with the waiter).
        reaper.record_terminal(TerminalStatus {
            pid: 999_993,
            raw_status: 42 << 8,
        });

        // The status is retained as an adopted record, not silently discarded.
        assert_eq!(reaper.adopted_children().len(), 1);
        assert_eq!(reaper.adopted_children()[0].status.pid, 999_993);

        // Registration atomically reconciles the retained status.
        let child = reaper
            .admit("reconcile-cmd")
            .unwrap()
            .register_direct(999_993, 99)
            .unwrap();
        assert_eq!(child.record().pid, 999_993);

        // The status is delivered exactly once through the DirectChild.
        let status = child.recv_timeout(Duration::from_millis(50)).unwrap();
        assert_eq!(status.pid, 999_993);
        assert_eq!(status.code(), Some(42));

        // Adopted accounting is empty — the record was removed during
        // reconciliation.
        assert!(reaper.adopted_children().is_empty());
        assert!(reaper.wait_for_adopted_idle(Duration::ZERO));

        // No supervisor route was left stranded: the registry is idle.
        assert!(reaper.supervisors().is_empty());
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));

        // A duplicate status injection after reconciliation goes to adopted
        // (it cannot be re-delivered to the already-consumed DirectChild).
        reaper.record_terminal(TerminalStatus {
            pid: 999_993,
            raw_status: 42 << 8,
        });
        assert_eq!(reaper.adopted_children().len(), 1);
    }

    #[test]
    fn drain_preserves_status_claimable_by_pending_admission() {
        // Shutdown can close admission and attempt an adopted drain while a
        // command that was admitted before closure is between spawn and direct
        // registration. Its already-reaped status must remain claimable: if
        // drain removed it, registration would install a route which can never
        // receive another kernel status.
        let reaper = ChildReaper::inert_for_test();
        let admission = reaper.admit("shutdown-race-cmd").unwrap();
        reaper.record_terminal(TerminalStatus {
            pid: 999_994,
            raw_status: 23 << 8,
        });
        reaper.close_admission();

        // Pending admission reservations block acknowledgement, preserving the
        // status for the legitimate pre-closure register_direct call.
        assert_eq!(reaper.drain(), Vec::<AdoptedChildRecord>::new());
        assert_eq!(reaper.adopted_children().len(), 1);
        assert!(!reaper.wait_for_adopted_idle(Duration::ZERO));

        let child = admission.register_direct(999_994, 17).unwrap();
        let status = child.recv_timeout(Duration::from_millis(50)).unwrap();
        assert_eq!(status.code(), Some(23));

        // Reconciliation removed the retained record and did not insert a
        // dead-end supervisor route.
        assert!(reaper.adopted_children().is_empty());
        assert!(reaper.wait_for_adopted_idle(Duration::ZERO));
        assert!(reaper.wait_for_supervisors_idle(Duration::ZERO));
    }

    #[test]
    fn adopted_records_remain_until_explicitly_drained() {
        let reaper = ChildReaper::inert_for_test();
        reaper.record_terminal(TerminalStatus {
            pid: 999_992,
            raw_status: 0,
        });
        assert!(!reaper.wait_for_adopted_idle(Duration::ZERO));
        let records = reaper.drain();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status.pid, 999_992);
        assert!(reaper.wait_for_adopted_idle(Duration::ZERO));
    }

    #[test]
    fn multiple_adopted_records_drain_independently_of_pgid_or_session() {
        // The reaper retains every unknown/adopted terminal status regardless of
        // which process group or session the child belonged to. This is the
        // registry foundation for descendants that call setpgid()/setsid().
        let reaper = ChildReaper::inert_for_test();
        reaper.record_terminal(TerminalStatus {
            pid: 888_001,
            raw_status: 0,
        });
        reaper.record_terminal(TerminalStatus {
            pid: 888_002,
            raw_status: 9,
        });
        reaper.record_terminal(TerminalStatus {
            pid: 888_003,
            raw_status: 1 << 8,
        });

        let records = reaper.drain();
        assert_eq!(records.len(), 3);
        // Records are retained and observable in arrival order.
        assert_eq!(records[0].status.pid, 888_001);
        assert_eq!(records[1].status.pid, 888_002);
        assert_eq!(records[2].status.pid, 888_003);
        assert!(reaper.wait_for_adopted_idle(Duration::ZERO));
    }

    #[test]
    fn supervisor_idle_reflects_pending_admissions() {
        // wait_for_supervisors_idle must not report idle while an admission
        // reservation is outstanding, even if no supervisor is registered yet.
        let reaper = ChildReaper::inert_for_test();
        let _admission = reaper.admit("pending-cmd").unwrap();

        assert!(!reaper.wait_for_supervisors_idle(Duration::ZERO));
        assert_eq!(reaper.supervisors().len(), 0);
        assert_eq!(reaper.pending_admissions(), 1);
    }
}
