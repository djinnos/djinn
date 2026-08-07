//! Cap resolution for the one build-slot FIFO.
//!
//! Split out of `build_lease.rs` for the file-size guard; this is the same
//! `BuildLeaseService` and the same private state, in a child module.
//!
//! Everything here answers one question: what cap is enforced RIGHT NOW. The
//! answer has a strict precedence — the durable invocation-lease authority's
//! reference cap, then a positive `build_lease_caps` row, then the process
//! configuration — and exactly one place that writes the enforced value
//! ([`BuildLeaseService::adopt_authority_cap`]).
//!
//! That single writer is the fix for a real production defect. `set-cap` used
//! to be inert against a running process because the resolved cap was only
//! STORED by `recover()`; see `adopt_authority_cap` for the full account.
//!
//! # This module survives the Kueue cutover deliberately
//!
//! 9oga's file map lists it as delete-entirely while listing `build_lease.rs`,
//! which calls it, as a retain. That is a contradiction in the campaign's own
//! map, and this is the side of it that is correct: the reference cap is the
//! invocation lease's, not the deleted pods-quota reservation's, and this is the
//! only path that adopts it at runtime.

use std::sync::atomic::Ordering;

use super::BuildLeaseService;
use djinn_db::repositories::invocation_lease_authority::InvocationLeaseMode;
use djinn_k8s::launcher::CgroupLauncherMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileBoundPrecondition {
    Armed,
    Disarmed,
}

/// Fail-closed reusable predicate consumed by the capacity controller.
#[must_use]
pub fn compile_bound_precondition(
    launcher: Option<CgroupLauncherMode>,
    authority: Result<Option<InvocationLeaseMode>, ()>,
) -> CompileBoundPrecondition {
    if launcher == Some(CgroupLauncherMode::Required)
        && matches!(authority, Ok(Some(InvocationLeaseMode::Enforce)))
    {
        CompileBoundPrecondition::Armed
    } else {
        CompileBoundPrecondition::Disarmed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DampedCapacitySnapshot {
    Capacity { compile_slots: i64 },
    Conservative { fail_safe_compile_slots: i64 },
}

impl BuildLeaseService {
    /// The reference cap this service is currently enforcing.
    ///
    /// Meaningful only after [`Self::recover`]; before that it is the
    /// constructor's configured value. A zero cap denies every consumer
    /// unconditionally, so composition uses this to refuse to wire a consumer
    /// behind a gate that can never open.
    #[must_use]
    pub fn cap(&self) -> i64 {
        self.cap.load(Ordering::Acquire)
    }

    /// Resolve the durable lease-table cap against the process configuration.
    ///
    /// A positive durable cap has been armed by a real writer (`set_cap`, or a
    /// previous `grant_next` converging the table on this process's cap) and is
    /// kept verbatim. A durable `0` is the migration-seeded, never-armed state
    /// and yields to [`Self::configured_cap`]; see that field for why `0` can
    /// never be a deliberate durable policy on the production path.
    fn armed_fallback(&self, durable: i64) -> i64 {
        if durable > 0 {
            return durable;
        }
        if self.configured_cap > 0 {
            tracing::info!(
                configured_cap = self.configured_cap,
                "build lease: durable cap is unarmed (0); adopting the configured build-slot cap"
            );
        }
        self.derived_fallback.load(Ordering::Acquire)
    }

    /// Read the durable invocation-lease authority and apply its reference cap.
    ///
    /// Returns the reference cap to enforce, defaulting to `fallback` (the
    /// lease-table cap) when no authority reader is installed or the row carries
    /// no cap. An unreadable authority clears the observed epoch (fail closed)
    /// and retains the fallback cap. The authority's reference cap is
    /// authoritative when set, so a restart converges on it rather than on a
    /// stale lease-table value.
    ///
    /// The resolved cap is STORED, not merely returned. It used to be returned
    /// only, and `recover()` was the sole caller that wrote it to `self.cap` —
    /// which is what made `djinn-server epoch set-cap` inert against a running
    /// process. Production on 2026-07-25: `set-cap --cap 12` reported success
    /// and `epoch show` read back `cap 12`, while every subsequent denial still
    /// said `occupancy=3 cap=3` because the live `grant_next` read a cached
    /// atomic that only a restart could refresh. An operator's only cap knob
    /// silently doing nothing during an incident is worse than refusing the
    /// write, so the durable cap is now authoritative at runtime: every read of
    /// the authority converges the enforced value, and
    /// [`Self::refresh_epoch_cap`] performs that read on the server's periodic
    /// authority pass.
    ///
    /// Storing here is deliberately the ONLY write path besides `set_cap`, so
    /// there is one rule for what the enforced cap is: whatever the last
    /// successful authority read resolved. An unreadable or capless authority
    /// stores the fallback rather than widening the cap.
    pub(super) async fn adopt_authority_cap(&self, fallback: i64) -> i64 {
        let cap = self.resolve_authority_cap(fallback).await;
        self.cap.store(cap, Ordering::Release);
        cap
    }

    async fn resolve_authority_cap(&self, fallback: i64) -> i64 {
        let fallback = self.armed_fallback(fallback);
        let Some(authority) = self.authority.as_ref() else {
            return fallback;
        };
        match authority.read().await {
            Ok(Some(row)) => {
                self.observed_epoch.store(row.epoch, Ordering::Release);
                self.dispatch_enforcing
                    .store(row.mode.is_enforcing(), Ordering::Release);
                row.cap.unwrap_or(fallback)
            }
            Ok(None) => {
                // No durable authority row: nothing to observe, keep the fallback.
                self.observed_epoch.store(-1, Ordering::Release);
                self.dispatch_enforcing.store(false, Ordering::Release);
                fallback
            }
            Err(_) => {
                // Unreadable authority: fail closed on the observed epoch, and
                // do NOT enforce a cap we could not confirm was armed.
                self.observed_epoch.store(-1, Ordering::Release);
                self.dispatch_enforcing.store(false, Ordering::Release);
                fallback
            }
        }
    }

    /// Re-read the durable invocation-lease authority and adopt its reference
    /// cap, without a restart.
    ///
    /// This is what makes `djinn-server epoch set-cap` a live control rather
    /// than a value that takes effect at the next rollout. It is called on the
    /// server's periodic authority pass — the same pass that already
    /// re-evaluates the arming mode — so the cap and the mode it is paired with
    /// can never be observed from different epochs for long.
    ///
    /// A RAISED cap must also drain: `grant_next` runs only when someone
    /// queues, so without this an operator who raises the cap to unwedge a
    /// stalled board still waits for the next dispatch attempt before anything
    /// moves. Lowering never revokes an occupying row; it simply stops granting.
    ///
    /// Returns the cap now in force, or `None` before recovery has opened the
    /// service (occupancy is unknown then, and unknown is never a reason to
    /// change what is enforced).
    pub async fn refresh_epoch_cap(&self) -> Option<i64> {
        if !self.is_ready() {
            return None;
        }
        let _guard = self.operation.lock().await;
        let previous = self.cap.load(Ordering::Acquire);
        let durable = self.repository.snapshot().await.map(|s| s.cap).ok()?;
        let cap = self.adopt_authority_cap(durable).await;
        if cap == previous {
            return Some(cap);
        }
        tracing::info!(
            previous_cap = previous,
            cap,
            "build lease: adopted the durable admission-epoch cap without a restart"
        );
        if cap > previous {
            let _ = self.drain().await;
        }
        Some(cap)
    }

    /// Accept only a damped snapshot and converge through the one enforced-cap
    /// writer. A durable authority cap remains authoritative.
    pub async fn adopt_capacity_snapshot(&self, snapshot: DampedCapacitySnapshot) -> Option<i64> {
        if !self.is_ready() {
            return None;
        }
        let fallback = match snapshot {
            DampedCapacitySnapshot::Capacity { compile_slots } => compile_slots,
            DampedCapacitySnapshot::Conservative {
                fail_safe_compile_slots,
            } => fail_safe_compile_slots,
        }
        .max(0);
        self.derived_fallback.store(fallback, Ordering::Release);
        let _guard = self.operation.lock().await;
        let previous = self.cap.load(Ordering::Acquire);
        let adopted = self.adopt_authority_cap(fallback).await;
        if adopted > previous {
            let _ = self.drain().await;
        }
        Some(adopted)
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn compile_bound_precondition_exhausts_launcher_and_authority_modes() {
        for launcher in [
            None,
            Some(CgroupLauncherMode::Disabled),
            Some(CgroupLauncherMode::Required),
        ] {
            for authority in [
                Ok(None),
                Ok(Some(InvocationLeaseMode::Off)),
                Ok(Some(InvocationLeaseMode::Shadow)),
                Ok(Some(InvocationLeaseMode::Enforce)),
                Err(()),
            ] {
                let expected = launcher == Some(CgroupLauncherMode::Required)
                    && matches!(authority, Ok(Some(InvocationLeaseMode::Enforce)));
                assert_eq!(
                    compile_bound_precondition(launcher, authority),
                    if expected {
                        CompileBoundPrecondition::Armed
                    } else {
                        CompileBoundPrecondition::Disarmed
                    }
                );
            }
        }
    }

    /// Wall-clock bound on the compile-fail visibility probe below.
    ///
    /// The probe shells out to `rustc`. It used to do so with no bound at all,
    /// which is how it turned into a HARD CI failure rather than a flake: in
    /// run 30727348884 nextest's `slow-timeout = { period = "30s",
    /// terminate-after = 3 }` (`.config/nextest.toml`) terminated it on all
    /// three attempts, 90s apiece, and reported a bare TMT with no stderr, no
    /// exit status, and nothing to distinguish "the visibility property
    /// regressed" from "the toolchain was starved for disk". Retries cannot
    /// help — every attempt re-runs the same cold-I/O work under the same
    /// load — so the useful thing a retry budget can buy is not another
    /// attempt but an ATTRIBUTABLE failure, which is what this bound produces.
    ///
    /// 60s sits below nextest's 90s termination point on purpose, so this
    /// assertion is what fires first and the panic message is what the lane
    /// reports.
    const VISIBILITY_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn build_lease_cap_api_visibility() {
        use std::{
            fs::{self, File},
            process::{Command, Stdio},
            thread::sleep,
            time::{Instant, SystemTime},
        };

        let deps = std::env::current_exe()
            .expect("current test executable")
            .parent()
            .expect("target deps directory")
            .to_owned();
        let coordinator_rlib = fs::read_dir(&deps)
            .expect("read target deps")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("libdjinn_coordinator-")
                    && entry.path().extension().is_some_and(|ext| ext == "rlib")
            })
            .max_by_key(|entry| {
                entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH)
            })
            .expect("djinn-coordinator rlib must accompany its unit tests")
            .path();
        let source = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ui/build_lease_cap_private.rs"
        );
        let output_dir = tempfile::tempdir().expect("compile-fail output directory");
        let stderr_path = output_dir.path().join("probe.stderr");
        let stderr_sink = File::create(&stderr_path).expect("compile-fail stderr sink");

        // `--emit=metadata` is the cost fix, and it is safe precisely BECAUSE
        // of what this probe asserts. Every diagnostic below is a name
        // resolution / field privacy error, all of which are produced during
        // type checking; nothing downstream of typeck is needed to observe
        // them, and the probe never links because it never compiles.
        //
        // What the flag buys is the crate loader's behaviour. With no object
        // code requested, `-L dependency` lookups for `djinn-coordinator`'s
        // transitive crates are satisfied from their `.rmeta` instead of being
        // decoded out of their `.rlib`. That directory is the whole workspace's
        // `target/debug/deps`: thousands of entries, several stale ~113MB
        // `libdjinn_coordinator-*.rlib` candidates among them, cold-restored by
        // `rust-cache` on CI while the sibling test processes in the same shard
        // compete for the same disk. Reading metadata sidecars instead of
        // fully-linked archives is the difference between the probe being
        // cheap and the probe being the reason a shard goes red.
        //
        // Diagnostics are unchanged: a rustc that rejected the flag, or a probe
        // that stopped emitting these errors, fails the assertions below with
        // the captured stderr attached rather than passing vacuously.
        //
        // Stderr goes to a file rather than a pipe so this cannot deadlock on a
        // full pipe buffer while the test is polling for exit.
        let mut probe = Command::new("rustc")
            .args(["--edition=2021", "--crate-type=bin", "--emit=metadata"])
            .arg(source)
            .arg("--extern")
            .arg(format!("djinn_coordinator={}", coordinator_rlib.display()))
            .arg("-L")
            .arg(format!("dependency={}", deps.display()))
            .arg("--out-dir")
            .arg(output_dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_sink))
            .spawn()
            .expect("spawn rustc visibility probe");

        let started = Instant::now();
        let timed_out = loop {
            if probe
                .try_wait()
                .expect("poll rustc visibility probe")
                .is_some()
            {
                break false;
            }
            if started.elapsed() >= VISIBILITY_PROBE_TIMEOUT {
                let _ = probe.kill();
                break true;
            }
            sleep(Duration::from_millis(25));
        };
        // Unconditional, on both paths: `Child::wait` returns the status
        // `try_wait` already cached, so this reaps the killed probe without
        // changing what the successful path observes.
        let status = probe.wait().expect("reap rustc visibility probe");
        let budget = VISIBILITY_PROBE_TIMEOUT;
        let elapsed = started.elapsed();
        let search_path = deps.display();
        assert!(
            !timed_out,
            "rustc visibility probe did not finish within {budget:?} (elapsed {elapsed:?}) and \
             was killed. The API-visibility property was NOT evaluated: this is a toolchain or \
             disk-contention stall reading {search_path}, not a visibility regression, and \
             re-running will reproduce it."
        );

        let stderr = fs::read_to_string(&stderr_path).expect("read visibility probe stderr");
        assert!(
            !status.success(),
            "visibility probe unexpectedly compiled: BuildLeaseService's private cap state is \
             reachable from outside the crate"
        );
        assert!(
            stderr.contains("field `cap` of struct `BuildLeaseService` is private"),
            "visibility probe stderr did not report `cap` as private:\n{stderr}"
        );
        assert!(
            stderr.contains("field `derived_fallback` of struct `BuildLeaseService` is private"),
            "visibility probe stderr did not report `derived_fallback` as private:\n{stderr}"
        );
        assert!(
            stderr.contains("no method named `set_cap_directly`"),
            "visibility probe stderr did not reject `set_cap_directly`:\n{stderr}"
        );
    }
}
