//! Observe-only disk-admission dimension for build workloads (proposal nquz,
//! phase 1 — DARK/OBSERVE).
//!
//! This module is the capacity-snapshot adapter and the typed `disk_pressure` /
//! `disk_capacity_unknown` queue-reason evaluator. It NEVER denies: it computes
//! only what disk admission WOULD do given a capacity sample and the run-dir
//! ledger totals, so telemetry can measure the future policy before it is armed.
//!
//! It is deliberately pure and side-effect-free (no filesystem writes, no DB
//! access, no deletion). `build_admission.rs` remains the policy authority; this
//! module supplies a self-contained decision that a later phase will consume.

use std::path::Path;
use std::time::Duration;

/// Bounded classification of a node-local cache volume's free space.
///
/// Supplied by the capacity observer (`1c9q`); `Unknown` covers a missing or
/// unclassifiable sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskCapacityState {
    Healthy,
    Warning,
    Critical,
    Unknown,
}

impl DiskCapacityState {
    /// Classify free space against a critical and a warning floor. `critical`
    /// must be the smaller floor; a mis-ordered pair still classifies safely by
    /// checking the critical floor first.
    #[must_use]
    pub fn classify(free_bytes: u64, critical_free_bytes: u64, warning_free_bytes: u64) -> Self {
        if free_bytes <= critical_free_bytes {
            Self::Critical
        } else if free_bytes <= warning_free_bytes {
            Self::Warning
        } else {
            Self::Healthy
        }
    }
}

/// A single, timestamped capacity observation for one volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacitySample {
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub state: DiskCapacityState,
    /// Age of this sample at evaluation time.
    pub age: Duration,
}

/// Current tracked byte totals for a volume, derived from the run-dir ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LedgerTotals {
    pub tracked_measured_bytes: u64,
    pub outstanding_reserved_bytes: u64,
}

/// A build request's disk footprint, as far as disk admission cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskAdmissionRequest {
    /// The last successful allocated-byte measurement for this project + base
    /// fingerprint (or a conservative inventory when there is no history). It is
    /// rounded up by 10% internally to form the seed projection.
    pub projected_seed_bytes: u64,
    /// True when the request reuses an already-ready run dir within its existing
    /// reservation, so no new bytes are reserved.
    pub reuses_ready_dir: bool,
}

/// Typed disk queue reason recorded in observe mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskQueueReason {
    /// Measured critical pressure, or a fresh sample that fails budget/headroom.
    DiskPressure,
    /// The sample is stale or unknown and the request would reserve new bytes.
    DiskCapacityUnknown,
}

impl DiskQueueReason {
    /// The bounded telemetry label for this reason.
    #[must_use]
    pub fn as_metric(self) -> &'static str {
        match self {
            Self::DiskPressure => djinn_telemetry::run_dir::QUEUE_REASON_DISK_PRESSURE,
            Self::DiskCapacityUnknown => {
                djinn_telemetry::run_dir::QUEUE_REASON_DISK_CAPACITY_UNKNOWN
            }
        }
    }
}

/// The observe-mode outcome. In phase 1 this NEVER changes a dispatch decision;
/// `would_defer` records what enforce mode would do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskObservation {
    /// `Some(reason)` when disk admission WOULD have queued this build.
    pub would_defer: Option<DiskQueueReason>,
    /// The bytes this request would reserve (0 for a zero-growth reuse).
    pub projected_reservation_bytes: u64,
}

/// Per-volume disk-admission configuration. Phase-1 defaults are inert
/// placeholders: nothing consumes them for a live decision yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskAdmissionConfig {
    pub cache_budget_bytes: u64,
    pub critical_free_bytes: u64,
    pub emergency_headroom_bytes: u64,
    pub per_lease_growth_bytes: u64,
    pub max_sample_age: Duration,
}

/// 10 GiB, the proposal's default emergency headroom.
pub const DEFAULT_EMERGENCY_HEADROOM_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// 90 seconds, the proposal's freshness bound for a capacity sample.
pub const DEFAULT_MAX_SAMPLE_AGE: Duration = Duration::from_secs(90);

impl Default for DiskAdmissionConfig {
    fn default() -> Self {
        Self {
            cache_budget_bytes: 200 * 1024 * 1024 * 1024,
            critical_free_bytes: 20 * 1024 * 1024 * 1024,
            emergency_headroom_bytes: DEFAULT_EMERGENCY_HEADROOM_BYTES,
            per_lease_growth_bytes: 4 * 1024 * 1024 * 1024,
            max_sample_age: DEFAULT_MAX_SAMPLE_AGE,
        }
    }
}

impl DiskAdmissionConfig {
    /// The effective emergency headroom for a volume: at least the configured
    /// value, and never less than 5% of filesystem capacity.
    #[must_use]
    pub fn effective_emergency_headroom(&self, total_bytes: u64) -> u64 {
        self.emergency_headroom_bytes.max(total_bytes / 20)
    }
}

/// Round a byte count up by 10% (saturating). This is the conservative seed
/// projection rounding from the proposal.
#[must_use]
pub fn round_up_by_10_percent(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / 10)
}

/// Evaluate what disk admission WOULD decide for a build request. Pure and
/// non-denying: the returned `would_defer` is telemetry only in phase 1.
///
/// A zero-growth reuse of an already-ready dir is always a grant (no new bytes),
/// even under critical pressure or an unknown sample, because it consumes no new
/// allocation. Otherwise:
///
/// - `Critical` → `DiskPressure`;
/// - stale or `Unknown` sample → `DiskCapacityUnknown` (an unmeasured new
///   allocation is unsafe to claim);
/// - a fresh `Healthy`/`Warning` sample that fails the budget or the
///   emergency-headroom check → `DiskPressure`;
/// - otherwise → grant.
#[must_use]
pub fn evaluate_disk_admission_observe(
    config: &DiskAdmissionConfig,
    sample: &CapacitySample,
    ledger: &LedgerTotals,
    request: &DiskAdmissionRequest,
) -> DiskObservation {
    if request.reuses_ready_dir {
        return DiskObservation {
            would_defer: None,
            projected_reservation_bytes: 0,
        };
    }

    let reservation = round_up_by_10_percent(request.projected_seed_bytes)
        .saturating_add(config.per_lease_growth_bytes);

    if sample.state == DiskCapacityState::Critical {
        return DiskObservation {
            would_defer: Some(DiskQueueReason::DiskPressure),
            projected_reservation_bytes: reservation,
        };
    }

    let stale = sample.age > config.max_sample_age;
    if stale || sample.state == DiskCapacityState::Unknown {
        return DiskObservation {
            would_defer: Some(DiskQueueReason::DiskCapacityUnknown),
            projected_reservation_bytes: reservation,
        };
    }

    // Fresh Healthy/Warning sample: apply budget and headroom.
    let projected_tracked = ledger
        .tracked_measured_bytes
        .saturating_add(ledger.outstanding_reserved_bytes)
        .saturating_add(reservation);
    let budget_ok = projected_tracked <= config.cache_budget_bytes;

    let free_after = sample.free_bytes.saturating_sub(reservation);
    let headroom_floor = config
        .critical_free_bytes
        .saturating_add(config.effective_emergency_headroom(sample.total_bytes));
    let headroom_ok = free_after > headroom_floor;

    let would_defer = if budget_ok && headroom_ok {
        None
    } else {
        Some(DiskQueueReason::DiskPressure)
    };

    DiskObservation {
        would_defer,
        projected_reservation_bytes: reservation,
    }
}

// ── Filesystem project-quota probe ──────────────────────────────────────────

/// Result of probing a volume for filesystem project-quota support. Enforce mode
/// is prohibited without working project quotas; observe mode remains available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuotaSupport {
    /// The mount backing the path advertises a project-quota option.
    Available,
    /// Project quotas are unavailable; the volume can only run in observe.
    Unavailable { reason: QuotaUnavailableReason },
}

/// Bounded reasons a project-quota probe reports unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaUnavailableReason {
    /// No mount line covered the path.
    NoMatchingMount,
    /// The covering mount carries no project-quota option.
    NoQuotaMountOption,
    /// The mount table could not be read.
    MountsUnreadable,
}

impl QuotaUnavailableReason {
    #[must_use]
    pub fn as_metric(self) -> &'static str {
        // Every unavailable reason maps to the bounded probe-unavailable label;
        // the specific reason is carried in logs, not the metric.
        match self {
            Self::NoMatchingMount | Self::NoQuotaMountOption | Self::MountsUnreadable => {
                djinn_telemetry::run_dir::QUOTA_FAILURE_PROBE_UNAVAILABLE
            }
        }
    }
}

/// Mount options that indicate project-quota accounting is enabled.
const PROJECT_QUOTA_OPTIONS: [&str; 3] = ["prjquota", "pquota", "prjjquota"];

/// Parse a Linux `/proc/mounts`-style table and decide whether the mount that
/// covers `path` advertises a project-quota option. Pure and testable: the
/// longest mount-point prefix of `path` wins, matching kernel semantics.
#[must_use]
pub fn parse_project_quota_support(mounts: &str, path: &Path) -> QuotaSupport {
    let path_str = path.to_string_lossy();
    let mut best_len = 0_usize;
    let mut best_options: Option<String> = None;

    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let _device = fields.next();
        let Some(mount_point) = fields.next() else {
            continue;
        };
        let _fstype = fields.next();
        let options = fields.next().unwrap_or("");
        if path_covered_by_mount(&path_str, mount_point) && mount_point.len() >= best_len {
            best_len = mount_point.len();
            best_options = Some(options.to_owned());
        }
    }

    match best_options {
        None => QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoMatchingMount,
        },
        Some(options) => {
            let has_quota = options
                .split(',')
                .any(|opt| PROJECT_QUOTA_OPTIONS.contains(&opt.trim()));
            if has_quota {
                QuotaSupport::Available
            } else {
                QuotaSupport::Unavailable {
                    reason: QuotaUnavailableReason::NoQuotaMountOption,
                }
            }
        }
    }
}

fn path_covered_by_mount(path: &str, mount_point: &str) -> bool {
    if mount_point == "/" {
        return true;
    }
    if path == mount_point {
        return true;
    }
    path.strip_prefix(mount_point)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Probe the live mount table for project-quota support of `path`.
///
/// Read-only and best-effort. When the mount table cannot be read the volume is
/// reported unavailable (observe), never optimistically available.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe_project_quota(path: &Path) -> QuotaSupport {
    match std::fs::read_to_string("/proc/mounts") {
        Ok(mounts) => parse_project_quota_support(&mounts, path),
        Err(_) => QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::MountsUnreadable,
        },
    }
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn probe_project_quota(_path: &Path) -> QuotaSupport {
    QuotaSupport::Unavailable {
        reason: QuotaUnavailableReason::MountsUnreadable,
    }
}

#[cfg(test)]
#[path = "disk_admission_tests.rs"]
mod tests;
