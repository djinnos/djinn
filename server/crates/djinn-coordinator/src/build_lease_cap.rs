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
}
