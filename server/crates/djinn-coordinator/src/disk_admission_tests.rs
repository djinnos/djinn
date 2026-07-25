//! Unit tests for the observe-only disk-admission evaluator and quota probe.

use std::path::Path;
use std::time::Duration;

use super::*;

const GIB: u64 = 1024 * 1024 * 1024;

fn config() -> DiskAdmissionConfig {
    DiskAdmissionConfig {
        cache_budget_bytes: 100 * GIB,
        critical_free_bytes: 20 * GIB,
        emergency_headroom_bytes: 10 * GIB,
        per_lease_growth_bytes: 4 * GIB,
        max_sample_age: Duration::from_secs(90),
    }
}

fn fresh(state: DiskCapacityState, free_bytes: u64) -> CapacitySample {
    CapacitySample {
        free_bytes,
        total_bytes: 500 * GIB,
        state,
        age: Duration::from_secs(5),
    }
}

fn new_request(seed_bytes: u64) -> DiskAdmissionRequest {
    DiskAdmissionRequest {
        projected_seed_bytes: seed_bytes,
        reuses_ready_dir: false,
    }
}

#[test]
fn round_up_by_10_percent_is_conservative() {
    assert_eq!(round_up_by_10_percent(0), 0);
    assert_eq!(round_up_by_10_percent(100), 110);
    assert_eq!(round_up_by_10_percent(u64::MAX), u64::MAX);
}

#[test]
fn classify_orders_critical_then_warning_then_healthy() {
    assert_eq!(
        DiskCapacityState::classify(5, 10, 20),
        DiskCapacityState::Critical
    );
    assert_eq!(
        DiskCapacityState::classify(15, 10, 20),
        DiskCapacityState::Warning
    );
    assert_eq!(
        DiskCapacityState::classify(50, 10, 20),
        DiskCapacityState::Healthy
    );
}

#[test]
fn healthy_fresh_within_budget_and_headroom_would_grant() {
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Healthy, 200 * GIB),
        &LedgerTotals::default(),
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, None);
    // 2 GiB + 10% + 4 GiB per-lease growth.
    assert_eq!(
        obs.projected_reservation_bytes,
        round_up_by_10_percent(2 * GIB) + 4 * GIB
    );
}

#[test]
fn critical_new_reservation_would_defer_disk_pressure() {
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Critical, 5 * GIB),
        &LedgerTotals::default(),
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, Some(DiskQueueReason::DiskPressure));
}

#[test]
fn unknown_sample_new_reservation_would_defer_capacity_unknown() {
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Unknown, 200 * GIB),
        &LedgerTotals::default(),
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, Some(DiskQueueReason::DiskCapacityUnknown));
}

#[test]
fn stale_sample_new_reservation_would_defer_capacity_unknown() {
    let mut sample = fresh(DiskCapacityState::Healthy, 200 * GIB);
    sample.age = Duration::from_secs(120);
    let obs = evaluate_disk_admission_observe(
        &config(),
        &sample,
        &LedgerTotals::default(),
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, Some(DiskQueueReason::DiskCapacityUnknown));
}

#[test]
fn reuse_of_ready_dir_always_grants_even_under_critical() {
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Critical, GIB),
        &LedgerTotals::default(),
        &DiskAdmissionRequest {
            projected_seed_bytes: 50 * GIB,
            reuses_ready_dir: true,
        },
    );
    assert_eq!(obs.would_defer, None);
    assert_eq!(obs.projected_reservation_bytes, 0);
}

#[test]
fn budget_exceeded_would_defer_disk_pressure() {
    let ledger = LedgerTotals {
        tracked_measured_bytes: 98 * GIB,
        outstanding_reserved_bytes: 0,
    };
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Healthy, 400 * GIB),
        &ledger,
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, Some(DiskQueueReason::DiskPressure));
}

#[test]
fn headroom_breach_would_defer_disk_pressure() {
    // Free is above critical, but reserving new bytes would drop free below
    // critical + emergency headroom.
    let obs = evaluate_disk_admission_observe(
        &config(),
        &fresh(DiskCapacityState::Warning, 33 * GIB),
        &LedgerTotals::default(),
        &new_request(2 * GIB),
    );
    assert_eq!(obs.would_defer, Some(DiskQueueReason::DiskPressure));
}

#[test]
fn effective_emergency_headroom_never_below_five_percent() {
    let cfg = config();
    // 5% of 1000 GiB = 50 GiB, which exceeds the configured 10 GiB.
    assert_eq!(cfg.effective_emergency_headroom(1000 * GIB), 50 * GIB);
    // For a small volume the configured floor wins.
    assert_eq!(cfg.effective_emergency_headroom(20 * GIB), 10 * GIB);
}

#[test]
fn queue_reason_metric_labels_are_stable() {
    assert_eq!(DiskQueueReason::DiskPressure.as_metric(), "disk_pressure");
    assert_eq!(
        DiskQueueReason::DiskCapacityUnknown.as_metric(),
        "disk_capacity_unknown"
    );
}

// ── Quota probe parsing ─────────────────────────────────────────────────────

#[test]
fn parse_quota_available_when_mount_advertises_prjquota() {
    let mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0
/dev/sdb1 /cache xfs rw,relatime,prjquota 0 0
";
    assert_eq!(
        parse_project_quota_support(mounts, Path::new("/cache/cargo-target-runs")),
        QuotaSupport::Available
    );
}

#[test]
fn parse_quota_unavailable_without_option() {
    let mounts = "\
/dev/sda1 / ext4 rw,relatime 0 0
/dev/sdb1 /cache xfs rw,relatime 0 0
";
    assert_eq!(
        parse_project_quota_support(mounts, Path::new("/cache/runs")),
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoQuotaMountOption,
        }
    );
}

#[test]
fn parse_quota_prefers_longest_matching_mount() {
    // Root has no quota; the nested /cache mount does. The longest prefix wins.
    let mounts = "\
/dev/sda1 / ext4 rw,relatime,prjquota 0 0
/dev/sdb1 /cache xfs rw,relatime 0 0
";
    assert_eq!(
        parse_project_quota_support(mounts, Path::new("/cache/runs/x")),
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoQuotaMountOption,
        }
    );
}

#[test]
fn parse_quota_falls_back_to_root_mount() {
    let mounts = "/dev/sda1 / ext4 rw,relatime,pquota 0 0\n";
    assert_eq!(
        parse_project_quota_support(mounts, Path::new("/cache/runs")),
        QuotaSupport::Available
    );
}

#[test]
fn parse_quota_no_matching_mount() {
    assert_eq!(
        parse_project_quota_support("", Path::new("/cache/runs")),
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoMatchingMount,
        }
    );
}
