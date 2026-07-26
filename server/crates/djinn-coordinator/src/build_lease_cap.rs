//! Cap resolution for the one v1 build-slot FIFO.
//!
//! Split out of `build_lease.rs` for the file-size guard; this is the same
//! `BuildLeaseService` and the same private state, in a child module.
//!
//! Everything here answers one question: what cap is enforced RIGHT NOW. The
//! answer has a strict precedence — the durable admission-epoch reference cap,
//! then a positive `build_lease_caps` row, then the process configuration —
//! and exactly one place that writes the enforced value
//! ([`BuildLeaseService::read_handoff_epoch`]).
//!
//! That single writer is the fix for a real production defect. `set-cap` used
//! to be inert against a running process because the resolved cap was only
//! STORED by `recover()`; see `read_handoff_epoch` for the full account.

use std::sync::atomic::Ordering;

use super::BuildLeaseService;

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
        self.configured_cap
    }

    /// Read the durable admission-handoff epoch and apply its reference cap.
    ///
    /// Returns the reference cap to enforce, defaulting to `fallback` (the
    /// lease-table cap) when no handoff reader is installed or the row carries
    /// no cap. An unreadable epoch clears the observed epoch (fail closed) and
    /// retains the fallback cap. The handoff reference cap is authoritative for
    /// the v1 authority when set, so a restart converges on the epoch's cap
    /// rather than a stale lease-table value.
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
    /// the epoch converges the enforced value, and
    /// [`Self::refresh_epoch_cap`] performs that read on the coordinator's
    /// handoff tick.
    ///
    /// Storing here is deliberately the ONLY write path besides `set_cap`, so
    /// there is one rule for what the enforced cap is: whatever the last
    /// successful epoch read resolved. An unreadable or capless epoch stores
    /// the fallback rather than widening the cap.
    pub(super) async fn read_handoff_epoch(&self, fallback: i64) -> i64 {
        let cap = self.resolve_handoff_epoch(fallback).await;
        self.cap.store(cap, Ordering::Release);
        cap
    }

    async fn resolve_handoff_epoch(&self, fallback: i64) -> i64 {
        let fallback = self.armed_fallback(fallback);
        let Some(handoff) = self.handoff.as_ref() else {
            return fallback;
        };
        match handoff.read().await {
            Ok(Some(row)) => {
                self.observed_epoch.store(row.epoch, Ordering::Release);
                self.dispatch_enforcing
                    .store(row.v1_mode.is_enforcing(), Ordering::Release);
                row.cap.unwrap_or(fallback)
            }
            Ok(None) => {
                // No durable epoch row: nothing to observe, keep the fallback.
                self.observed_epoch.store(-1, Ordering::Release);
                self.dispatch_enforcing.store(false, Ordering::Release);
                fallback
            }
            Err(_) => {
                // Unreadable epoch: fail closed on the observed epoch, and do
                // NOT enforce a cap we could not confirm was armed.
                self.observed_epoch.store(-1, Ordering::Release);
                self.dispatch_enforcing.store(false, Ordering::Release);
                fallback
            }
        }
    }

    /// Re-read the durable admission epoch and adopt its reference cap, without
    /// a restart.
    ///
    /// This is what makes `djinn-server epoch set-cap` a live control rather
    /// than a value that takes effect at the next rollout. It is called on the
    /// coordinator's periodic handoff tick — the same tick that already
    /// re-evaluates the epoch's v0/v1 modes — so the cap and the modes it is
    /// paired with can never be observed from different epochs for long.
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
        let cap = self.read_handoff_epoch(durable).await;
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
}
