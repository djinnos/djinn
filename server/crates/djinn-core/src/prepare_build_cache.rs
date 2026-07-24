//! Stack-neutral admission/classification core for the `prepare_build_cache`
//! worker tool (epic 1bnj, proposal jvpm).
//!
//! This module owns the pure decision logic that maps a resolved platform
//! warm-cache descriptor plus the current admission signals onto a single
//! typed outcome. It performs no filesystem work and allocates no run-dir
//! bytes: queued outcomes (`disk_pressure`, `disk_capacity_unknown`) are
//! returned WITHOUT touching the target run-dir, preserving the seed
//! single-flight and disk-reservation invariants owned by the worker startup
//! seed path and the future nquz first-use client.
//!
//! The public tool schema is stack-neutral; the Rust cargo target run-dir is
//! the only platform cache that exists today, but the classifier is written so
//! any future stack cache slots in without changing the tool surface.
//!
//! ## Compatibility no-op while eager seeding is active
//!
//! nquz first-use / enforce is not yet live on `main` (the observe-mode core
//! landed in #2537). While the worker still eagerly seeds the cargo target
//! run-dir at startup, this tool must NOT duplicate that work: it returns a
//! compatible `Noop` (ready) outcome pointing at the already-seeded run-dir.
//! Only once eager seeding is retired does the classifier route through the
//! disk-admission seam and return `Ready`/`Queued`.

/// A resolved platform warm-cache descriptor for the current stack.
///
/// Stack-neutral by construction: adding a new stack cache is a new variant,
/// not a change to the tool schema or the worker-facing outcome contract.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PlatformCache {
    /// Rust cargo warm-target per-run seed directory. `cache_path` is the
    /// worker's private `CARGO_TARGET_DIR` (a run-dir under the cargo target
    /// runs root), which is the cache-path contract returned to the agent.
    CargoTargetRunDir { cache_path: String },
    /// The stack has no platform warm cache; preparation is not applicable.
    None,
}

/// Disk-admission signal consulted only when eager seeding is retired.
///
/// The classifier never allocates run-dir bytes to compute this; callers pass
/// the already-observed admission state. `CapacityUnknown` is distinct from
/// `Pressure` so operators can tell a measured-full disk apart from a disk
/// whose capacity could not be measured.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DiskAdmission {
    /// Enough headroom to seed a fresh run-dir.
    Ok,
    /// Measured disk pressure: seeding is deferred (queued) without allocating.
    Pressure,
    /// Disk capacity could not be determined: fail closed to queued without
    /// allocating.
    CapacityUnknown,
}

/// Reason a preparation was queued rather than made ready.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum QueuedReason {
    DiskPressure,
    DiskCapacityUnknown,
}

impl QueuedReason {
    /// Stable wire label for the queued reason (audit / structured fields).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DiskPressure => "disk_pressure",
            Self::DiskCapacityUnknown => "disk_capacity_unknown",
        }
    }
}

/// The single typed outcome returned per `prepare_build_cache` call.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PrepareBuildCacheOutcome {
    /// The platform cache was seeded/admitted and is ready. `cache_path` is the
    /// cache-path contract the agent may rely on.
    Ready { cache_path: String },
    /// Compatibility no-op: eager seeding already prepared the cache at worker
    /// startup, so this call did no work. `cache_path` still returns the
    /// ready cache-path contract.
    Noop { cache_path: String },
    /// Preparation was deferred without allocating any run-dir bytes.
    Queued { reason: QueuedReason },
    /// The stack has no platform warm cache.
    NotApplicable,
}

impl PrepareBuildCacheOutcome {
    /// Bounded telemetry label for this outcome. Exactly one of these is
    /// emitted per call; `error` is emitted by the handler for internal
    /// failures and is intentionally not represented here.
    pub const fn telemetry_label(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Noop { .. } => "noop",
            Self::Queued { .. } => "queued",
            Self::NotApplicable => "not-applicable",
        }
    }

    /// Machine-readable `outcome` field for the tool result payload.
    pub const fn outcome_field(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::Noop { .. } => "noop",
            Self::Queued { .. } => "queued",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// Classify a `prepare_build_cache` call into exactly one typed outcome.
///
/// Pure: performs no filesystem work and allocates no run-dir bytes. The
/// queued branches return without side effects, preserving the disk-reservation
/// and seed single-flight invariants (the actual reservation/seed is owned by
/// the worker startup path today, and by the nquz first-use client once eager
/// seeding is retired).
///
/// - No platform cache -> [`PrepareBuildCacheOutcome::NotApplicable`].
/// - Platform cache present while eager seeding is active ->
///   [`PrepareBuildCacheOutcome::Noop`] (do not duplicate seed work).
/// - Platform cache present, eager seeding retired -> route through the
///   disk-admission seam: `Ok` -> `Ready`, `Pressure`/`CapacityUnknown` ->
///   `Queued` (no allocation).
pub fn classify_prepare_build_cache(
    cache: &PlatformCache,
    eager_seed_active: bool,
    disk: DiskAdmission,
) -> PrepareBuildCacheOutcome {
    let cache_path = match cache {
        PlatformCache::None => return PrepareBuildCacheOutcome::NotApplicable,
        PlatformCache::CargoTargetRunDir { cache_path } => cache_path.clone(),
    };

    if eager_seed_active {
        // Eager startup seeding already prepared the run-dir; returning a
        // compatible ready no-op avoids duplicating (and re-locking) the seed.
        return PrepareBuildCacheOutcome::Noop { cache_path };
    }

    match disk {
        DiskAdmission::Ok => PrepareBuildCacheOutcome::Ready { cache_path },
        DiskAdmission::Pressure => PrepareBuildCacheOutcome::Queued {
            reason: QueuedReason::DiskPressure,
        },
        DiskAdmission::CapacityUnknown => PrepareBuildCacheOutcome::Queued {
            reason: QueuedReason::DiskCapacityUnknown,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_cache() -> PlatformCache {
        PlatformCache::CargoTargetRunDir {
            cache_path: "/cache/cargo-target-runs/task-run-123".to_string(),
        }
    }

    #[test]
    fn no_platform_cache_is_not_applicable_regardless_of_signals() {
        for eager in [true, false] {
            for disk in [
                DiskAdmission::Ok,
                DiskAdmission::Pressure,
                DiskAdmission::CapacityUnknown,
            ] {
                let outcome = classify_prepare_build_cache(&PlatformCache::None, eager, disk);
                assert_eq!(outcome, PrepareBuildCacheOutcome::NotApplicable);
                assert_eq!(outcome.telemetry_label(), "not-applicable");
                assert_eq!(outcome.outcome_field(), "not_applicable");
            }
        }
    }

    #[test]
    fn eager_seeding_active_returns_ready_noop_without_consulting_disk() {
        // Even under disk pressure, the eager-seed compatibility path is a
        // ready no-op: the startup seed already reserved and prepared the dir.
        for disk in [
            DiskAdmission::Ok,
            DiskAdmission::Pressure,
            DiskAdmission::CapacityUnknown,
        ] {
            let outcome = classify_prepare_build_cache(&cargo_cache(), true, disk);
            assert_eq!(
                outcome,
                PrepareBuildCacheOutcome::Noop {
                    cache_path: "/cache/cargo-target-runs/task-run-123".to_string(),
                }
            );
            assert_eq!(outcome.telemetry_label(), "noop");
            assert_eq!(outcome.outcome_field(), "noop");
        }
    }

    #[test]
    fn eager_retired_ok_disk_is_ready_with_cache_path_contract() {
        let outcome = classify_prepare_build_cache(&cargo_cache(), false, DiskAdmission::Ok);
        assert_eq!(
            outcome,
            PrepareBuildCacheOutcome::Ready {
                cache_path: "/cache/cargo-target-runs/task-run-123".to_string(),
            }
        );
        assert_eq!(outcome.telemetry_label(), "ready");
        assert_eq!(outcome.outcome_field(), "ready");
    }

    #[test]
    fn eager_retired_disk_pressure_queues_without_allocation() {
        let outcome = classify_prepare_build_cache(&cargo_cache(), false, DiskAdmission::Pressure);
        assert_eq!(
            outcome,
            PrepareBuildCacheOutcome::Queued {
                reason: QueuedReason::DiskPressure,
            }
        );
        assert_eq!(outcome.telemetry_label(), "queued");
        assert_eq!(outcome.outcome_field(), "queued");
        if let PrepareBuildCacheOutcome::Queued { reason } = outcome {
            assert_eq!(reason.as_str(), "disk_pressure");
        }
    }

    #[test]
    fn eager_retired_capacity_unknown_queues_fail_closed() {
        let outcome =
            classify_prepare_build_cache(&cargo_cache(), false, DiskAdmission::CapacityUnknown);
        assert_eq!(
            outcome,
            PrepareBuildCacheOutcome::Queued {
                reason: QueuedReason::DiskCapacityUnknown,
            }
        );
        assert_eq!(outcome.telemetry_label(), "queued");
        if let PrepareBuildCacheOutcome::Queued { reason } = outcome {
            assert_eq!(reason.as_str(), "disk_capacity_unknown");
        }
    }
}
