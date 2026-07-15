// djinn:allow-oversize — whole-base guardrails and report-only fingerprint orchestration share this safety boundary.
//! Conservative inventory and guard planning for Cargo warm bases, including
//! whole-base eviction and a report-only fingerprint-unit inventory.
//!
//! The inventory and guard-planning phase deliberately does not remove
//! directories.  The idle evictor consumes guard-plan candidates and performs
//! the destructive work only after re-checking every safety condition under a
//! held per-base lock.
//!
//! The fingerprint-unit inventory is additive and side-effect-free: it only
//! reports what it observes, and it never calls removal APIs or interprets a
//! newest fingerprint mtime as a last-use signal.

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use djinn_core::clock::Clock;
use time::OffsetDateTime;
use uuid::Uuid;

mod fingerprint_inventory;
mod three_rung_plan;

pub use fingerprint_inventory::{
    FINGERPRINT_DIR, FingerprintUnitEntry, FingerprintUnitInventory, inventory_fingerprint_units,
};
pub use three_rung_plan::{
    PressureBaseSnapshot, PressurePlanDisposition, PressurePlanRetainReason, PressurePlanUnit,
    PressureRung, ThreeRungPressurePlan, build_three_rung_pressure_plan,
};

/// Capture the DB-derived activity facts needed by the immutable three-rung
/// planner. Profile staleness is project activity only: this intentionally
/// never consults artifact or directory mtimes.
///
/// This adapter does not open locks, probe capacity, or mutate the filesystem.
pub async fn snapshot_three_rung_pressure_bases(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
) -> Vec<PressureBaseSnapshot> {
    let mut snapshots = Vec::with_capacity(inventory.entries.len());
    for entry in inventory.entries {
        let effective_latest_activity = activity
            .activity(&entry.project_id)
            .await
            .ok()
            .and_then(|snapshot| snapshot.latest_activity)
            .and_then(|value| parse_iso8601(&value).ok())
            .map(system_time_from_offset);
        snapshots.push(PressureBaseSnapshot {
            project_id: entry.project_id,
            canonical_base: std::fs::canonicalize(&entry.path).unwrap_or(entry.path),
            effective_latest_activity,
        });
    }
    snapshots
}

/// The lock shared with warm jobs.  Pressure deletion must use this rather
/// than the legacy in-base GC lock, so deletion and warm writes serialize even
/// when a base is being removed.
pub struct SharedWarmBaseLock;

impl BaseLock for SharedWarmBaseLock {
    fn try_lock(&self, path: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
        let project_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid project id")?;
        let parsed = Uuid::parse_str(project_id).map_err(|_| "invalid project id")?;
        if parsed.to_string() != project_id {
            return Err("invalid project id".into());
        }
        let root = path.parent().ok_or("base has no warm root")?;
        let lock_dir = root.join(".warm-locks");
        std::fs::create_dir_all(&lock_dir)
            .map_err(|error| format!("failed to create warm lock directory: {error}"))?;
        let lock_path = lock_dir.join(format!("{project_id}.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                format!("failed to open warm lock {}: {error}", lock_path.display())
            })?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(Box::new(FlockGuard { _file: file })))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error.to_string())
            }
        }
    }
}

/// Consume the immutable three-rung plan in delete mode. Every operation after
/// planning is performed under the shared warmer lock for that project.
#[allow(clippy::too_many_arguments)]
pub async fn execute_three_rung_pressure_plan(
    plan: &ThreeRungPressurePlan,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLock,
    capacity: &dyn FilesystemCapacity,
    config: &crate::context::CacheCleanupConfig,
    clock: &dyn Clock,
    root: &Path,
) -> ThreeRungPressureResult {
    use djinn_telemetry::cache_cleanup::{
        self as metrics, PressureMode, PressureOutcome, PressureTermination,
    };
    let mut result = ThreeRungPressureResult::default();
    result.planned = plan.units.clone();
    result.projected_allocated_bytes = plan
        .units
        .iter()
        .map(|unit| unit.projected_allocated_bytes)
        .sum();
    for unit in &plan.units {
        metrics::increment_pressure_unit(
            PressureMode::Delete,
            pressure_metric_rung(unit),
            PressureOutcome::Planned,
        );
        metrics::record_pressure_projected_allocated_bytes(
            PressureMode::Delete,
            pressure_metric_rung(unit),
            unit.projected_allocated_bytes,
        );
    }
    let mut blocked = HashSet::new();
    for (index, unit) in plan.units.iter().enumerate() {
        if blocked.contains(&unit.project_id)
            || !matches!(unit.disposition, PressurePlanDisposition::Eligible)
        {
            blocked.insert(unit.project_id.clone());
            result.retained.push(unit.clone());
            continue;
        }
        let guard = match locks.try_lock(&unit.canonical_base) {
            Ok(Some(guard)) => guard,
            Ok(None) | Err(_) => {
                blocked.insert(unit.project_id.clone());
                result.retained.push(unit.clone());
                continue;
            }
        };
        if recheck_three_rung_unit(unit, activity, warm_jobs, config, clock.now())
            .await
            .is_err()
        {
            blocked.insert(unit.project_id.clone());
            result.retained.push(unit.clone());
            drop(guard);
            continue;
        }
        // `NotFound` from a concurrent safe remover is idempotent; it does
        // not make the broader rung eligible for escalation in this pass.
        if !unit.canonical_target.exists() {
            blocked.insert(unit.project_id.clone());
            drop(guard);
            continue;
        }
        if !execution_target_is_still_canonical_directory(unit) {
            blocked.insert(unit.project_id.clone());
            result.retained.push(unit.clone());
            drop(guard);
            continue;
        }
        let bytes = match allocated_tree_bytes_for_execution(&unit.canonical_target) {
            Ok(bytes) => bytes,
            Err(()) => {
                blocked.insert(unit.project_id.clone());
                result.retained.push(unit.clone());
                drop(guard);
                continue;
            }
        };
        // It is intentionally immediately before every removal, including the first.
        let pre = match capacity.capacity(root) {
            Ok(value) => value,
            Err(_) => {
                result.remeasurement_failed = true;
                result.failed.push(unit.clone());
                metrics::increment_pressure_unit(
                    PressureMode::Delete,
                    pressure_metric_rung(unit),
                    PressureOutcome::Failed,
                );
                result.retained.extend(plan.units[index..].iter().cloned());
                metrics::increment_pressure_termination(
                    PressureMode::Delete,
                    PressureTermination::RemeasureFailed,
                );
                break;
            }
        };
        if !available_below_ratio(
            pre.available_bytes,
            pre.total_bytes,
            config.warm_base_high_free_ratio,
        ) {
            result.reached_high_watermark = true;
            result.retained.extend(plan.units[index..].iter().cloned());
            metrics::increment_pressure_termination(
                PressureMode::Delete,
                PressureTermination::ReachedHigh,
            );
            break;
        }
        result.post_lock_eligible.push(unit.clone());
        metrics::increment_pressure_unit(
            PressureMode::Delete,
            pressure_metric_rung(unit),
            PressureOutcome::PostLockEligible,
        );
        result.attempted.push(unit.clone());
        metrics::increment_pressure_unit(
            PressureMode::Delete,
            pressure_metric_rung(unit),
            PressureOutcome::Attempted,
        );
        match remove_three_rung_target(unit, root) {
            Ok(Removal::Removed) => {
                result.reclaimed_bytes = result.reclaimed_bytes.saturating_add(bytes);
                result.deleted.push(unit.clone());
                metrics::increment_pressure_unit(
                    PressureMode::Delete,
                    pressure_metric_rung(unit),
                    PressureOutcome::Deleted,
                );
                metrics::record_pressure_reclaimed_allocated_bytes(
                    PressureMode::Delete,
                    pressure_metric_rung(unit),
                    bytes,
                );
                match capacity.capacity(root) {
                    Ok(post)
                        if available_below_ratio(
                            post.available_bytes,
                            post.total_bytes,
                            config.warm_base_high_free_ratio,
                        ) => {}
                    Ok(_) => {
                        result.reached_high_watermark = true;
                        result
                            .retained
                            .extend(plan.units[index + 1..].iter().cloned());
                        metrics::increment_pressure_termination(
                            PressureMode::Delete,
                            PressureTermination::ReachedHigh,
                        );
                        break;
                    }
                    Err(_) => {
                        result.remeasurement_failed = true;
                        result.failed.push(unit.clone());
                        metrics::increment_pressure_unit(
                            PressureMode::Delete,
                            pressure_metric_rung(unit),
                            PressureOutcome::Failed,
                        );
                        result
                            .retained
                            .extend(plan.units[index + 1..].iter().cloned());
                        metrics::increment_pressure_termination(
                            PressureMode::Delete,
                            PressureTermination::RemeasureFailed,
                        );
                        break;
                    }
                }
            }
            // A concurrent remover may win after the guarded existence check.
            // That no-op is idempotent, but it still makes this base's planned
            // state stale, so never escalate to a broader rung this pass.
            Ok(Removal::Absent) => {
                blocked.insert(unit.project_id.clone());
            }
            Err(()) => {
                blocked.insert(unit.project_id.clone());
                result.retained.push(unit.clone());
                result.failed.push(unit.clone());
                metrics::increment_pressure_unit(
                    PressureMode::Delete,
                    pressure_metric_rung(unit),
                    PressureOutcome::Failed,
                );
            }
        }
        drop(guard);
    }
    for unit in &result.retained {
        metrics::increment_pressure_unit(
            PressureMode::Delete,
            pressure_metric_rung(unit),
            PressureOutcome::Retained,
        );
    }
    if !result.reached_high_watermark && !result.remeasurement_failed {
        metrics::increment_pressure_termination(
            PressureMode::Delete,
            PressureTermination::Completed,
        );
    }
    result
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ThreeRungPressureResult {
    pub planned: Vec<PressurePlanUnit>,
    pub post_lock_eligible: Vec<PressurePlanUnit>,
    pub attempted: Vec<PressurePlanUnit>,
    pub deleted: Vec<PressurePlanUnit>,
    pub retained: Vec<PressurePlanUnit>,
    pub failed: Vec<PressurePlanUnit>,
    pub projected_allocated_bytes: u64,
    pub reclaimed_bytes: u64,
    pub reached_high_watermark: bool,
    pub remeasurement_failed: bool,
}

async fn recheck_three_rung_unit(
    unit: &PressurePlanUnit,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    config: &crate::context::CacheCleanupConfig,
    now: SystemTime,
) -> Result<(), ()> {
    let base = std::fs::symlink_metadata(&unit.canonical_base).map_err(|_| ())?;
    if base.file_type().is_symlink()
        || !base.is_dir()
        || std::fs::canonicalize(&unit.canonical_base).map_err(|_| ())? != unit.canonical_base
    {
        return Err(());
    }
    match std::fs::symlink_metadata(&unit.canonical_target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(()),
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Err(()),
        Ok(_) => {}
    }
    if std::fs::canonicalize(&unit.canonical_target).map_err(|_| ())? != unit.canonical_target
        || !unit.canonical_target.starts_with(&unit.canonical_base)
    {
        return Err(());
    }
    let snapshot = activity.activity(&unit.project_id).await.map_err(|_| ())?;
    if snapshot.has_active_task_run
        || warm_jobs
            .has_in_flight_warm(&unit.project_id)
            .await
            .map_err(|_| ())?
    {
        return Err(());
    }
    let last = match snapshot.latest_activity.as_deref() {
        Some(value) => system_time_from_offset(parse_iso8601(value).map_err(|_| ())?),
        None => base.modified().map_err(|_| ())?,
    };
    if now < last.checked_add(config.warm_base_grace_period).ok_or(())? {
        return Err(());
    }
    if unit.rung == PressureRung::StaleProfile
        && (snapshot.latest_activity.is_none()
            || now.duration_since(last).unwrap_or_default()
                < Duration::from_secs(config.warm_profile_min_idle_hours.saturating_mul(3600)))
    {
        return Err(());
    }
    Ok(())
}

fn execution_target_is_still_canonical_directory(unit: &PressurePlanUnit) -> bool {
    matches!(
        std::fs::symlink_metadata(&unit.canonical_target),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink()
    ) && std::fs::canonicalize(&unit.canonical_target).is_ok_and(|target| {
        target == unit.canonical_target && target.starts_with(&unit.canonical_base)
    })
}

fn allocated_tree_bytes_for_execution(root: &Path) -> Result<u64, ()> {
    let mut total: u64 = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        for child in std::fs::read_dir(path).map_err(|_| ())? {
            let child = child.map_err(|_| ())?;
            let metadata = std::fs::symlink_metadata(child.path()).map_err(|_| ())?;
            if metadata.file_type().is_symlink() {
                return Err(());
            }
            if metadata.is_dir() {
                pending.push(child.path());
            } else if metadata.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    total = total.saturating_add(metadata.blocks().saturating_mul(512));
                }
                #[cfg(not(unix))]
                {
                    total = total.saturating_add(metadata.len());
                }
            } else {
                return Err(());
            }
        }
    }
    Ok(total)
}

enum Removal {
    Removed,
    Absent,
}

// Keep the destructive syscall behind this small seam so the executor's
// fail-closed behavior can be exercised for a real, mid-removal filesystem
// error. Production always uses `remove_dir_all`; the hook only exists in
// unit-test builds and is thread-local so parallel tests cannot affect one
// another.
#[cfg(test)]
thread_local! {
    static REMOVE_DIR_ALL_HOOK: std::cell::RefCell<Option<Box<dyn Fn(&Path) -> std::io::Result<()>>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_remove_dir_all_hook(hook: Option<Box<dyn Fn(&Path) -> std::io::Result<()>>>) {
    REMOVE_DIR_ALL_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn remove_dir_all_for_three_rung_target(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(result) =
        REMOVE_DIR_ALL_HOOK.with(|slot| slot.borrow().as_ref().map(|hook| hook(path)))
    {
        return result;
    }
    std::fs::remove_dir_all(path)
}

fn remove_three_rung_target(unit: &PressurePlanUnit, root: &Path) -> Result<Removal, ()> {
    let root = std::fs::canonicalize(root).map_err(|_| ())?;
    if !unit.canonical_base.starts_with(&root)
        || !unit.canonical_target.starts_with(&unit.canonical_base)
    {
        return Err(());
    }
    match remove_dir_all_for_three_rung_target(&unit.canonical_target) {
        Ok(()) => Ok(Removal::Removed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Removal::Absent),
        Err(_) => Err(()),
    }
}

/// Consume an immutable plan in dry-run mode. Returning cloned units preserves
/// the captured plan for diagnostics while guaranteeing no lock, guard,
/// capacity, or filesystem operation occurs during consumption.
pub fn consume_three_rung_pressure_plan_dry_run(
    plan: &ThreeRungPressurePlan,
) -> Vec<PressurePlanUnit> {
    use djinn_telemetry::cache_cleanup::{self as metrics, PressureMode, PressureOutcome};
    for unit in &plan.units {
        metrics::increment_pressure_unit(
            PressureMode::DryRun,
            pressure_metric_rung(unit),
            PressureOutcome::Planned,
        );
        metrics::record_pressure_projected_allocated_bytes(
            PressureMode::DryRun,
            pressure_metric_rung(unit),
            unit.projected_allocated_bytes,
        );
    }
    plan.units.clone()
}

fn pressure_metric_rung(unit: &PressurePlanUnit) -> djinn_telemetry::cache_cleanup::PressureRung {
    match unit.rung {
        PressureRung::Incremental => djinn_telemetry::cache_cleanup::PressureRung::Incremental,
        PressureRung::StaleProfile => djinn_telemetry::cache_cleanup::PressureRung::Profile,
        PressureRung::WholeBase => djinn_telemetry::cache_cleanup::PressureRung::Base,
    }
}

pub const CARGO_WARM_BASE_ROOT: &str = "/cache/cargo-target";
pub const WARM_BASE_GC_LOCK_FILE: &str = ".djinn-gc.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseClassification {
    Registered,
    Deleted,
    Orphaned,
}

/// Execute a deterministic pressure plan.
///
/// Dry-run does not acquire a lock or recheck candidates, because creating a
/// lock file mutates fallback mtime state. It reports precisely the planner's
/// ordered prefix. Delete mode holds an owned non-blocking lock across a full
/// post-lock safety recheck, actual-size measurement, and removal. Capacity is
/// remeasured after every successful deletion and terminates the sweep on a
/// measurement failure or as soon as the high watermark is reached.
#[allow(clippy::too_many_arguments)]
pub async fn execute_pressure_eviction(
    plan: PressureEvictionPlan,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLock,
    capacity: &dyn FilesystemCapacity,
    config: &crate::context::CacheCleanupConfig,
    clock: &dyn Clock,
    mode: crate::context::CacheCleanupMode,
    root: &Path,
) -> PressureEvictionResult {
    use crate::context::CacheCleanupMode;
    use djinn_telemetry::cache_cleanup as metrics;

    let mut result = PressureEvictionResult {
        projected_bytes: plan.projected_bytes,
        retained: plan.retained.clone(),
        ..Default::default()
    };
    // Planning failures are fail-closed too. Carry them into the result and
    // emit bounded telemetry even when planning produced no executable prefix.
    for (_, reason) in &plan.retained {
        emit_pressure_metric(*reason, mode);
    }
    if mode == CacheCleanupMode::DryRun {
        result.dry_run = plan
            .candidates
            .into_iter()
            .map(|candidate| candidate.entry)
            .collect();
        for _ in &result.dry_run {
            metrics::increment_cleanup_total(
                metrics::COMPONENT_CARGO_WARM_BASE,
                metrics::OUTCOME_DRY_RUN,
                mode.as_metric_label(),
            );
        }
        return result;
    }

    for candidate in plan.candidates {
        let entry = candidate.entry;
        let cached_last_activity =
            match latest_activity_time(candidate.latest_activity.as_deref(), &entry.path) {
                Ok(last) => last,
                Err(_) => {
                    retain_pressure(
                        &mut result,
                        &entry.project_id,
                        PressureSkipReason::ActivityError,
                        mode,
                    );
                    continue;
                }
            };
        let guard = match locks.try_lock(&entry.path) {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                retain_pressure(
                    &mut result,
                    &entry.project_id,
                    PressureSkipReason::LockBusy,
                    mode,
                );
                continue;
            }
            Err(_) => {
                retain_pressure(
                    &mut result,
                    &entry.project_id,
                    PressureSkipReason::LockError,
                    mode,
                );
                continue;
            }
        };
        let safe = match recheck_pressure_after_lock(
            &entry.project_id,
            activity,
            warm_jobs,
            clock.now(),
            config.warm_base_grace_period,
            cached_last_activity,
        )
        .await
        {
            Ok(value) => value,
            Err(reason) => {
                retain_pressure(&mut result, &entry.project_id, reason, mode);
                drop(guard);
                continue;
            }
        };
        if !safe {
            retain_pressure(
                &mut result,
                &entry.project_id,
                PressureSkipReason::Young,
                mode,
            );
            drop(guard);
            continue;
        }

        let actual_bytes = directory_size(&entry.path);
        if safe_remove_directory(&entry.path, root).is_err() {
            retain_pressure(
                &mut result,
                &entry.project_id,
                PressureSkipReason::DeleteError,
                mode,
            );
            drop(guard);
            continue;
        }
        result.reclaimed_bytes = result.reclaimed_bytes.saturating_add(actual_bytes);
        result.deleted.push(entry);
        metrics::increment_cleanup_total(
            metrics::COMPONENT_CARGO_WARM_BASE,
            metrics::OUTCOME_DELETED,
            mode.as_metric_label(),
        );
        if actual_bytes > 0 {
            metrics::record_reclaimed_bytes(
                metrics::COMPONENT_CARGO_WARM_BASE,
                mode.as_metric_label(),
                actual_bytes,
            );
        }
        drop(guard);

        let measured = match capacity.capacity(root) {
            Ok(measured) => measured,
            Err(_) => {
                result.remeasurement_failed = true;
                metrics::increment_cleanup_total(
                    metrics::COMPONENT_CARGO_WARM_BASE,
                    metrics::OUTCOME_ERROR,
                    mode.as_metric_label(),
                );
                break;
            }
        };
        if !available_below_ratio(
            measured.available_bytes,
            measured.total_bytes,
            config.warm_base_high_free_ratio,
        ) {
            result.reached_high_watermark = true;
            break;
        }
    }
    result
}

fn retain_pressure(
    result: &mut PressureEvictionResult,
    project_id: &str,
    reason: PressureSkipReason,
    mode: crate::context::CacheCleanupMode,
) {
    result.retained.push((project_id.to_owned(), reason));
    emit_pressure_metric(reason, mode);
}

fn emit_pressure_metric(reason: PressureSkipReason, mode: crate::context::CacheCleanupMode) {
    use djinn_telemetry::cache_cleanup as metrics;

    metrics::increment_cleanup_total(
        metrics::COMPONENT_CARGO_WARM_BASE,
        pressure_outcome(reason),
        mode.as_metric_label(),
    );
}

fn pressure_outcome(reason: PressureSkipReason) -> &'static str {
    use djinn_telemetry::cache_cleanup as metrics;

    match reason {
        PressureSkipReason::Young => metrics::OUTCOME_RETAINED_YOUNG,
        PressureSkipReason::ActiveTaskRun => metrics::OUTCOME_RETAINED_ACTIVE,
        PressureSkipReason::LockBusy => metrics::OUTCOME_RETAINED_LOCK_BUSY,
        PressureSkipReason::WarmJobInFlight | PressureSkipReason::NotSelected => {
            metrics::OUTCOME_RETAINED
        }
        PressureSkipReason::AboveLowWatermark
        | PressureSkipReason::ActivityError
        | PressureSkipReason::WarmJobError
        | PressureSkipReason::LockError
        | PressureSkipReason::MeasurementError
        | PressureSkipReason::DeleteError => metrics::OUTCOME_ERROR,
    }
}

async fn recheck_pressure_after_lock(
    project_id: &str,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    now: SystemTime,
    grace: Duration,
    cached_last_activity: SystemTime,
) -> Result<bool, PressureSkipReason> {
    let snapshot = activity
        .activity(project_id)
        .await
        .map_err(|_| PressureSkipReason::ActivityError)?;
    if snapshot.has_active_task_run {
        return Err(PressureSkipReason::ActiveTaskRun);
    }
    match warm_jobs.has_in_flight_warm(project_id).await {
        Ok(true) => return Err(PressureSkipReason::WarmJobInFlight),
        Err(_) => return Err(PressureSkipReason::WarmJobError),
        Ok(false) => {}
    }
    let last = match snapshot.latest_activity.as_deref() {
        Some(ts) => system_time_from_offset(
            parse_iso8601(ts).map_err(|_| PressureSkipReason::ActivityError)?,
        ),
        None => cached_last_activity,
    };
    let cutoff = last
        .checked_add(grace)
        .ok_or(PressureSkipReason::ActivityError)?;
    Ok(now >= cutoff)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmBaseEntry {
    pub project_id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmBaseInventory {
    pub entries: Vec<WarmBaseEntry>,
    pub ignored: usize,
}

/// Inventory immediate children only.  Symlinks and files are intentionally
/// ignored: a warm base must be a real directory named by a canonical UUID.
pub fn inventory_under(root: &Path) -> Result<WarmBaseInventory, String> {
    let mut entries = Vec::new();
    let mut ignored = 0;
    let children = match std::fs::read_dir(root) {
        Ok(children) => children,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WarmBaseInventory { entries, ignored });
        }
        Err(error) => return Err(error.to_string()),
    };
    for child in children {
        let Ok(child) = child else {
            ignored += 1;
            continue;
        };
        let Ok(file_type) = child.file_type() else {
            ignored += 1;
            continue;
        };
        if !file_type.is_dir() {
            ignored += 1;
            continue;
        }
        let Some(name) = child.file_name().to_str().map(str::to_owned) else {
            ignored += 1;
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(&name) else {
            ignored += 1;
            continue;
        };
        // Parse_str accepts compact UUIDs; base names are deliberately stricter.
        if uuid.to_string() != name {
            ignored += 1;
            continue;
        }
        let size_bytes = directory_size(&child.path());
        entries.push(WarmBaseEntry {
            project_id: name,
            path: child.path(),
            size_bytes,
        });
    }
    entries.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    Ok(WarmBaseInventory { entries, ignored })
}

fn directory_size(root: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(children) = std::fs::read_dir(path) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(child.path());
            } else if let Ok(metadata) = child.metadata() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivitySnapshot {
    pub known_project: bool,
    /// A durable deletion tombstone exists for this project id. This is
    /// intentionally distinct from an unknown UUID directory.
    pub deleted_project: bool,
    pub has_active_task_run: bool,
    pub latest_activity: Option<String>,
}

#[async_trait]
pub trait ActivityGuard: Send + Sync {
    async fn activity(&self, project_id: &str) -> Result<ActivitySnapshot, String>;
}

#[async_trait]
pub trait WarmJobGuard: Send + Sync {
    /// Return whether a non-terminal warm Job exists. Errors retain the base.
    async fn has_in_flight_warm(&self, project_id: &str) -> Result<bool, String>;
}

pub trait FreeSpaceGuard: Send + Sync {
    fn free_space_bytes(&self, path: &Path) -> Result<u64, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacitySnapshot {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

pub trait FilesystemCapacity: Send + Sync {
    fn capacity(&self, path: &Path) -> Result<CapacitySnapshot, String>;
}

pub trait BaseLockGuard: Send + Sync {
    fn try_lock(&self, path: &Path) -> LockOutcome;
}

/// Owned lock guard returned by [`BaseLock`]. The guard holds the lock until it
/// is dropped.
pub trait LockGuard: Send {}

/// Production-style lock acquisition that returns an owned guard. The guard
/// must be held across any destructive operation and the post-lock safety
/// recheck.
pub trait BaseLock: Send + Sync {
    fn try_lock(&self, path: &Path) -> Result<Option<Box<dyn LockGuard>>, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    Available,
    Busy,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainReason {
    ActivityError,
    ActiveTaskRun,
    WarmJobError,
    WarmJobInFlight,
    FreeSpaceError,
    LockBusy,
    LockError,
    Young,
    DeleteError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmBaseCandidate {
    pub entry: WarmBaseEntry,
    pub classification: BaseClassification,
    pub latest_activity: Option<String>,
    pub free_space_bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WarmBasePlan {
    pub candidates: Vec<WarmBaseCandidate>,
    pub retained: Vec<(String, RetainReason)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureSkipReason {
    AboveLowWatermark,
    ActivityError,
    ActiveTaskRun,
    WarmJobError,
    WarmJobInFlight,
    LockBusy,
    LockError,
    Young,
    MeasurementError,
    DeleteError,
    NotSelected,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PressureEvictionPlan {
    pub candidates: Vec<WarmBaseCandidate>,
    pub retained: Vec<(String, PressureSkipReason)>,
    pub projected_bytes: u64,
    pub target_bytes: u64,
}

/// Result of executing a previously computed pressure plan. `dry_run` is the
/// exact ordered planner prefix; delete mode measures each base immediately
/// before removal rather than reporting the planner's estimate as reclaimed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PressureEvictionResult {
    pub deleted: Vec<WarmBaseEntry>,
    pub dry_run: Vec<WarmBaseEntry>,
    pub retained: Vec<(String, PressureSkipReason)>,
    pub reclaimed_bytes: u64,
    pub projected_bytes: u64,
    pub reached_high_watermark: bool,
    pub remeasurement_failed: bool,
}

/// Emit the pressure sweep completion event without exposing per-project
/// identifiers. Retention reasons are enums, preserving bounded diagnostics
/// while callers retain the detailed result for control flow.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "legacy pressure eviction remains additive while health consumes the three-rung executor"
    )
)]
pub(crate) fn log_pressure_eviction_completion(
    pressure: &PressureEvictionResult,
    mode: crate::context::CacheCleanupMode,
) {
    use djinn_telemetry::cache_cleanup as metrics;

    let retained_outcomes: Vec<_> = pressure.retained.iter().map(|(_, reason)| reason).collect();
    tracing::info!(
        component = metrics::COMPONENT_CARGO_WARM_BASE,
        mode = mode.as_metric_label(),
        deleted = pressure.deleted.len(),
        dry_run = pressure.dry_run.len(),
        retained = pressure.retained.len(),
        retained_outcomes = ?retained_outcomes,
        reclaimed_bytes = pressure.reclaimed_bytes,
        projected_bytes = pressure.projected_bytes,
        reached_high_watermark = pressure.reached_high_watermark,
        remeasurement_failed = pressure.remeasurement_failed,
        "warm-base pressure GC completed"
    );
}

/// Report-only result of a fingerprint-unit inventory sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FingerprintSweepReport {
    /// Number of fingerprint units discovered across all passing bases.
    pub candidate_count: u64,
    /// Measured bytes inside all discovered fingerprint units.
    pub projected_bytes: u64,
    /// Always zero while fingerprint sweeping remains report-only.
    pub reclaimed_bytes: u64,
    /// Bases retained by guard failure (active task, warm job, lock, etc.).
    pub retained: Vec<(String, RetainReason)>,
    /// Bases whose inventory traversal failed after passing guards.
    pub error_bases: Vec<(String, String)>,
}

/// Report-only fingerprint-unit inventory sweep.
///
/// Runs the existing whole-base guard surfaces (activity, warm-job, lock) for
/// every base in `inventory`. For each base that passes, performs a
/// side-effect-free inventory of `.fingerprint/<unit>` directories. No
/// artifacts are deleted in any mode; delete mode emits an explicit
/// safety-disabled outcome.
///
/// Errors from DB, Kubernetes, lock, or fingerprint traversal fail closed and
/// are recorded in the report.
pub async fn report_only_fingerprint_sweep(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLockGuard,
    mode: crate::context::CacheCleanupMode,
) -> FingerprintSweepReport {
    use crate::context::CacheCleanupMode;
    use djinn_telemetry::cache_cleanup as metrics;

    let mut report = FingerprintSweepReport::default();
    let (candidates, retained) = evaluate_guards(inventory, activity, warm_jobs, locks).await;
    report.retained = retained.clone();

    for (project_id, reason) in &retained {
        let outcome = match reason {
            RetainReason::ActivityError
            | RetainReason::WarmJobError
            | RetainReason::LockError
            | RetainReason::Young
            | RetainReason::DeleteError
            | RetainReason::FreeSpaceError => metrics::OUTCOME_ERROR,
            RetainReason::ActiveTaskRun => metrics::OUTCOME_RETAINED_ACTIVE,
            RetainReason::WarmJobInFlight => metrics::OUTCOME_RETAINED,
            RetainReason::LockBusy => metrics::OUTCOME_RETAINED_LOCK_BUSY,
        };
        metrics::increment_cleanup_total(
            metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
            outcome,
            mode.as_metric_label(),
        );
        tracing::info!(
            project_id = %project_id,
            reason = ?reason,
            "fingerprint report-only sweep retained base"
        );
    }

    for eval in candidates {
        match inventory_fingerprint_units(&eval.entry.path) {
            Ok(inventory) => {
                let unit_count = inventory.units.len() as u64;
                let projected: u64 = inventory
                    .units
                    .iter()
                    .map(|unit| unit.projected_bytes)
                    .fold(0u64, u64::saturating_add);
                report.candidate_count = report.candidate_count.saturating_add(unit_count);
                report.projected_bytes = report.projected_bytes.saturating_add(projected);
                if unit_count > 0 {
                    metrics::increment_candidates(
                        metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
                        mode.as_metric_label(),
                        unit_count,
                    );
                }
            }
            Err(error) => {
                report
                    .error_bases
                    .push((eval.entry.project_id.clone(), error.clone()));
                metrics::increment_cleanup_total(
                    metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
                    metrics::OUTCOME_ERROR,
                    mode.as_metric_label(),
                );
                tracing::warn!(
                    project_id = %eval.entry.project_id,
                    error = %error,
                    "fingerprint inventory traversal failed; retaining base"
                );
            }
        }
    }

    let outcome = match mode {
        CacheCleanupMode::DryRun => metrics::OUTCOME_DRY_RUN,
        CacheCleanupMode::Delete => metrics::OUTCOME_SAFETY_DISABLED_REPORT_ONLY,
    };
    metrics::increment_cleanup_total(
        metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
        outcome,
        mode.as_metric_label(),
    );

    log_fingerprint_sweep_completion(&report, mode);

    report
}

/// Emit the fingerprint report-only sweep completion event with bounded fields.
/// Delete mode explicitly reports the safety-disabled outcome; no per-project
/// identifiers are exposed.
pub(crate) fn log_fingerprint_sweep_completion(
    report: &FingerprintSweepReport,
    mode: crate::context::CacheCleanupMode,
) {
    use djinn_telemetry::cache_cleanup as metrics;

    let outcome = match mode {
        crate::context::CacheCleanupMode::DryRun => metrics::OUTCOME_DRY_RUN,
        crate::context::CacheCleanupMode::Delete => metrics::OUTCOME_SAFETY_DISABLED_REPORT_ONLY,
    };
    tracing::info!(
        component = metrics::COMPONENT_CARGO_WARM_BASE_FINGERPRINT,
        mode = mode.as_metric_label(),
        outcome = outcome,
        candidate_count = report.candidate_count,
        projected_bytes = report.projected_bytes,
        reclaimed_bytes = report.reclaimed_bytes,
        retained = report.retained.len(),
        error_bases = report.error_bases.len(),
        "fingerprint report-only sweep completed"
    );
}

/// Evaluate all destructive-operation guards.  Any guard error becomes a
/// retention result; callers must never turn it into eligibility.
pub async fn plan(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    free_space: &dyn FreeSpaceGuard,
    locks: &dyn BaseLockGuard,
) -> WarmBasePlan {
    let mut plan = WarmBasePlan::default();
    let free = match free_space.free_space_bytes(Path::new(CARGO_WARM_BASE_ROOT)) {
        Ok(value) => value,
        Err(_) => {
            for entry in inventory.entries {
                plan.retained
                    .push((entry.project_id, RetainReason::FreeSpaceError));
            }
            return plan;
        }
    };
    let (evaluations, retained) = evaluate_guards(inventory, activity, warm_jobs, locks).await;
    plan.retained = retained;
    for eval in evaluations {
        plan.candidates.push(WarmBaseCandidate {
            entry: eval.entry,
            classification: eval.classification,
            latest_activity: eval.latest_activity,
            free_space_bytes: free,
        });
    }
    plan
}

struct GuardEvaluation {
    entry: WarmBaseEntry,
    classification: BaseClassification,
    latest_activity: Option<String>,
}

async fn evaluate_guards(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLockGuard,
) -> (Vec<GuardEvaluation>, Vec<(String, RetainReason)>) {
    let mut candidates = Vec::new();
    let mut retained = Vec::new();
    for entry in inventory.entries {
        let snapshot = match activity.activity(&entry.project_id).await {
            Ok(value) => value,
            Err(_) => {
                retained.push((entry.project_id, RetainReason::ActivityError));
                continue;
            }
        };
        if snapshot.has_active_task_run {
            retained.push((entry.project_id, RetainReason::ActiveTaskRun));
            continue;
        }
        match warm_jobs.has_in_flight_warm(&entry.project_id).await {
            Ok(true) => {
                retained.push((entry.project_id, RetainReason::WarmJobInFlight));
                continue;
            }
            Err(_) => {
                retained.push((entry.project_id, RetainReason::WarmJobError));
                continue;
            }
            Ok(false) => {}
        }
        match locks.try_lock(&entry.path) {
            LockOutcome::Busy => {
                retained.push((entry.project_id, RetainReason::LockBusy));
                continue;
            }
            LockOutcome::Error => {
                retained.push((entry.project_id, RetainReason::LockError));
                continue;
            }
            LockOutcome::Available => {}
        }
        let classification = if snapshot.deleted_project {
            BaseClassification::Deleted
        } else if snapshot.known_project {
            BaseClassification::Registered
        } else {
            BaseClassification::Orphaned
        };
        candidates.push(GuardEvaluation {
            entry,
            classification,
            latest_activity: snapshot.latest_activity,
        });
    }
    (candidates, retained)
}

/// Pure, deterministic disk-pressure planning for warm bases.
///
/// The measurement starts pressure planning only when the free percentage is
/// strictly below the configured low watermark. Measurement errors fail closed
/// and return an empty candidate list. Candidates that pass the shared safety,
/// activity, warm-job, and lock guards are then filtered by the grace period,
/// ordered by oldest derived activity and then canonical project ID, and the
/// minimal prefix needed to reach the high watermark is selected.
///
/// No directories are deleted and no per-project caps are applied.
pub async fn plan_pressure_eviction(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLockGuard,
    capacity: &dyn FilesystemCapacity,
    config: &crate::context::CacheCleanupConfig,
    clock: &dyn Clock,
) -> PressureEvictionPlan {
    let mut plan = PressureEvictionPlan::default();
    let capacity_snapshot = match capacity.capacity(Path::new(CARGO_WARM_BASE_ROOT)) {
        Ok(value) => value,
        Err(_) => {
            for entry in inventory.entries {
                plan.retained
                    .push((entry.project_id, PressureSkipReason::MeasurementError));
            }
            return plan;
        }
    };
    plan.target_bytes = target_reclaim_bytes(&capacity_snapshot, config.warm_base_high_free_ratio);

    if !available_below_ratio(
        capacity_snapshot.available_bytes,
        capacity_snapshot.total_bytes,
        config.warm_base_low_free_ratio,
    ) {
        for entry in inventory.entries {
            plan.retained
                .push((entry.project_id, PressureSkipReason::AboveLowWatermark));
        }
        return plan;
    }

    let (evaluations, retained) = evaluate_guards(inventory, activity, warm_jobs, locks).await;
    for (project_id, reason) in retained {
        plan.retained
            .push((project_id, pressure_skip_reason_from_retain(reason)));
    }

    let grace = config.warm_base_grace_period;
    let now = clock.now();
    let mut safe: Vec<(GuardEvaluation, SystemTime)> = Vec::new();
    for eval in evaluations {
        let last = match latest_activity_time(eval.latest_activity.as_deref(), &eval.entry.path) {
            Ok(value) => value,
            Err(_) => {
                plan.retained.push((
                    eval.entry.project_id.clone(),
                    PressureSkipReason::ActivityError,
                ));
                continue;
            }
        };
        let cutoff = match last.checked_add(grace) {
            Some(value) => value,
            None => {
                plan.retained.push((
                    eval.entry.project_id.clone(),
                    PressureSkipReason::ActivityError,
                ));
                continue;
            }
        };
        if now < cutoff {
            plan.retained
                .push((eval.entry.project_id.clone(), PressureSkipReason::Young));
            continue;
        }
        safe.push((eval, last));
    }

    safe.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.entry.project_id.cmp(&right.0.entry.project_id))
    });

    let mut cumulative: u64 = 0;
    for (eval, _last) in safe {
        let size = eval.entry.size_bytes;
        let selected = cumulative < plan.target_bytes;
        cumulative = cumulative.saturating_add(size);
        if selected {
            plan.candidates.push(WarmBaseCandidate {
                entry: eval.entry,
                classification: eval.classification,
                latest_activity: eval.latest_activity,
                free_space_bytes: 0,
            });
            plan.projected_bytes = plan.projected_bytes.saturating_add(size);
        } else {
            plan.retained
                .push((eval.entry.project_id, PressureSkipReason::NotSelected));
        }
    }
    plan
}

fn target_reclaim_bytes(capacity: &CapacitySnapshot, high_free_ratio: f64) -> u64 {
    // We need *at least* `total_bytes * high_free_ratio` bytes to be free, so
    // round up to a whole byte. Do this with the ratio's IEEE-754 components,
    // rather than converting total_bytes to f64: u64 capacities above 2^53
    // would otherwise be rounded before the ceiling is applied.
    let high_bytes = ceiling_ratio_bytes(capacity.total_bytes, high_free_ratio);
    high_bytes.saturating_sub(capacity.available_bytes)
}

/// Compare `available / total < ratio` exactly, without converting bytes to f64.
/// Invalid ratios and zero capacity fail closed (pressure is not started).
fn available_below_ratio(available: u64, total: u64, ratio: f64) -> bool {
    if total == 0 || ratio <= 0.0 || !ratio.is_finite() || ratio >= 1.0 {
        return false;
    }
    let (significand, binary_exponent) = finite_positive_ratio_parts(ratio);
    let numerator = (total as u128) * (significand as u128);
    let shift = (-binary_exponent) as u32;
    if shift >= 128 {
        return available == 0 && numerator != 0;
    }
    let available = available as u128;
    if available > (u128::MAX >> shift) {
        return false;
    }
    (available << shift) < numerator
}

fn finite_positive_ratio_parts(ratio: f64) -> (u64, i32) {
    let bits = ratio.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    if exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1_u64 << 52), exponent - 1023 - 52)
    }
}

/// Return `ceil(total * ratio)` without lossy conversion of `total` to f64.
///
/// Watermark configuration is constrained to finite ratios in `[0, 1)`, but
/// this helper defensively bounds invalid values to keep planning fail-safe and
/// bounded when called with an unchecked test fixture.
fn ceiling_ratio_bytes(total: u64, ratio: f64) -> u64 {
    if total == 0 || ratio <= 0.0 {
        return 0;
    }
    if !ratio.is_finite() || ratio >= 1.0 {
        return total;
    }

    let (significand, binary_exponent) = finite_positive_ratio_parts(ratio);

    // A valid positive ratio below one has a negative binary exponent. For a
    // denominator wider than u128, even the largest u64 capacity times the
    // significand is below one byte, so its ceiling is exactly one byte.
    let shift = (-binary_exponent) as u32;
    if shift >= 128 {
        return 1;
    }
    let numerator = (total as u128).saturating_mul(significand as u128);
    let denominator = 1_u128 << shift;
    let whole_bytes = numerator / denominator;
    let has_fraction = !numerator.is_multiple_of(denominator);
    (whole_bytes + u128::from(has_fraction)).min(total as u128) as u64
}

fn pressure_skip_reason_from_retain(reason: RetainReason) -> PressureSkipReason {
    match reason {
        RetainReason::ActivityError => PressureSkipReason::ActivityError,
        RetainReason::ActiveTaskRun => PressureSkipReason::ActiveTaskRun,
        RetainReason::WarmJobError => PressureSkipReason::WarmJobError,
        RetainReason::WarmJobInFlight => PressureSkipReason::WarmJobInFlight,
        RetainReason::FreeSpaceError => PressureSkipReason::MeasurementError,
        RetainReason::LockBusy => PressureSkipReason::LockBusy,
        RetainReason::LockError => PressureSkipReason::LockError,
        RetainReason::Young => PressureSkipReason::Young,
        RetainReason::DeleteError => PressureSkipReason::MeasurementError,
    }
}

pub struct DbActivityGuard {
    db: djinn_db::Database,
}
impl DbActivityGuard {
    pub fn new(db: djinn_db::Database) -> Self {
        Self { db }
    }
}
#[async_trait]
impl ActivityGuard for DbActivityGuard {
    async fn activity(&self, project_id: &str) -> Result<ActivitySnapshot, String> {
        let repo = djinn_db::WarmBaseActivityRepository::new(self.db.clone());
        let record = repo
            .get(project_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(match record {
            Some(record) => ActivitySnapshot {
                known_project: !record.deleted_project,
                deleted_project: record.deleted_project,
                has_active_task_run: record.has_active_task_run,
                latest_activity: record.latest_activity,
            },
            None => ActivitySnapshot {
                known_project: false,
                deleted_project: false,
                has_active_task_run: false,
                latest_activity: None,
            },
        })
    }
}

/// A deliberately unavailable production default until the composition root
/// supplies Kubernetes credentials.  It fails closed rather than silently
/// treating an unknown Job listing as absent.
pub struct UnavailableWarmJobGuard;
#[async_trait]
impl WarmJobGuard for UnavailableWarmJobGuard {
    async fn has_in_flight_warm(&self, _: &str) -> Result<bool, String> {
        Err("Kubernetes warm-job guard unavailable".into())
    }
}

/// Production guard that delegates to the same [`WarmJobLister`] used by
/// [`K8sGraphWarmer`], ensuring the GC sees the same non-terminal warm Job
/// semantics as the warmer. Kubernetes errors are propagated so the GC
/// fails closed and retains the base.
pub struct WarmJobListerGuard {
    lister: Arc<dyn djinn_k8s::graph_warmer::WarmJobLister>,
    namespace: String,
}

impl WarmJobListerGuard {
    pub fn new(lister: Arc<dyn djinn_k8s::graph_warmer::WarmJobLister>, namespace: String) -> Self {
        Self { lister, namespace }
    }
}

#[async_trait]
impl WarmJobGuard for WarmJobListerGuard {
    async fn has_in_flight_warm(&self, project_id: &str) -> Result<bool, String> {
        self.lister
            .has_in_flight_warm(&self.namespace, project_id)
            .await
            .map_err(|error| format!("warm-job lister failed: {error}"))
    }
}

pub struct StatvfsFreeSpaceGuard;
impl FreeSpaceGuard for StatvfsFreeSpaceGuard {
    // statvfs field widths vary by platform (u32 on macOS, u64 on Linux).
    #[allow(clippy::unnecessary_cast)]
    fn free_space_bytes(&self, path: &Path) -> Result<u64, String> {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| error.to_string())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: statvfs initializes `stat` on a successful return.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stat = unsafe { stat.assume_init() };
        Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
}

pub struct StatvfsFilesystemCapacity;
impl FilesystemCapacity for StatvfsFilesystemCapacity {
    // statvfs field widths vary by platform (u32 on macOS, u64 on Linux).
    #[allow(clippy::unnecessary_cast)]
    fn capacity(&self, path: &Path) -> Result<CapacitySnapshot, String> {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| error.to_string())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: statvfs initializes `stat` on a successful return.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stat = unsafe { stat.assume_init() };
        let total_bytes = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
        let available_bytes = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
        Ok(CapacitySnapshot {
            total_bytes,
            available_bytes,
        })
    }
}

pub struct NoopLockGuard;
impl BaseLockGuard for NoopLockGuard {
    fn try_lock(&self, _: &Path) -> LockOutcome {
        LockOutcome::Available
    }
}

/// Per-base filesystem lock using a non-blocking exclusive `flock` on
/// `<base>/.djinn-gc.lock`. The lock file is created if it does not exist.
/// The lock is released when the returned guard is dropped.
pub struct FlockBaseLock;

struct FlockGuard {
    _file: File,
}

impl LockGuard for FlockGuard {}

impl BaseLock for FlockBaseLock {
    fn try_lock(&self, path: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
        let lock_path = path.join(WARM_BASE_GC_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                format!("failed to open lock file {}: {error}", lock_path.display())
            })?;
        let fd = file.as_raw_fd();
        // SAFETY: fd is valid for the lifetime of `file`, and flock is async-signal-safe.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(Box::new(FlockGuard { _file: file })))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error.to_string())
            }
        }
    }
}

/// Adapter that lets a [`BaseLock`] implementation satisfy the planning-time
/// [`BaseLockGuard`] trait. The lock is acquired and immediately released,
/// which is safe because planning only tests availability; the actual deletion
/// phase reacquires and holds the lock.
pub struct BaseLockPlanningAdapter<L: BaseLock> {
    inner: L,
}

impl<L: BaseLock> BaseLockPlanningAdapter<L> {
    pub fn new(inner: L) -> Self {
        Self { inner }
    }
}

impl<L: BaseLock> BaseLockGuard for BaseLockPlanningAdapter<L> {
    fn try_lock(&self, path: &Path) -> LockOutcome {
        match self.inner.try_lock(path) {
            Ok(Some(_guard)) => LockOutcome::Available,
            Ok(None) => LockOutcome::Busy,
            Err(_) => LockOutcome::Error,
        }
    }
}

/// Result of an idle whole-base eviction pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IdleEvictionResult {
    pub deleted: Vec<WarmBaseEntry>,
    pub dry_run: Vec<WarmBaseEntry>,
    pub retained: Vec<(String, RetainReason)>,
    pub reclaimed_bytes: u64,
    pub projected_bytes: u64,
}

/// Evict idle warm bases.  Both `DryRun` and `Delete` modes select candidates
/// with the same DB-first activity derivation, retention, and grace-period
/// checks.  In `DryRun` mode the candidate directories are left intact and
/// `projected_bytes` is reported; in `Delete` mode they are removed after a
/// non-blocking per-base lock is acquired and safety is rechecked.
///
/// Errors from activity, warm-job, lock, or filesystem checks retain the base
/// (fail closed).  Directory deletion is refused if the path cannot be proven
/// to lie under the configured root.
#[allow(clippy::too_many_arguments)]
pub async fn evict_idle_warm_bases(
    inventory: WarmBaseInventory,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    locks: &dyn BaseLock,
    config: &crate::context::CacheCleanupConfig,
    clock: &dyn Clock,
    mode: crate::context::CacheCleanupMode,
    root: &Path,
) -> IdleEvictionResult {
    use crate::context::CacheCleanupMode;
    use djinn_telemetry::cache_cleanup as metrics;

    let mut result = IdleEvictionResult::default();
    let retention = Duration::from_secs(config.warm_base_idle_retention_days * 24 * 60 * 60);
    let grace = config.warm_base_grace_period;

    for entry in inventory.entries {
        let snapshot = match activity.activity(&entry.project_id).await {
            Ok(value) => value,
            Err(_) => {
                result
                    .retained
                    .push((entry.project_id.clone(), RetainReason::ActivityError));
                emit_idle_metric(
                    &entry.project_id,
                    RetainReason::ActivityError,
                    mode,
                    &mut result,
                );
                continue;
            }
        };

        if snapshot.has_active_task_run {
            result
                .retained
                .push((entry.project_id.clone(), RetainReason::ActiveTaskRun));
            emit_idle_metric(
                &entry.project_id,
                RetainReason::ActiveTaskRun,
                mode,
                &mut result,
            );
            continue;
        }

        match warm_jobs.has_in_flight_warm(&entry.project_id).await {
            Ok(true) => {
                result
                    .retained
                    .push((entry.project_id.clone(), RetainReason::WarmJobInFlight));
                emit_idle_metric(
                    &entry.project_id,
                    RetainReason::WarmJobInFlight,
                    mode,
                    &mut result,
                );
                continue;
            }
            Err(_) => {
                result
                    .retained
                    .push((entry.project_id.clone(), RetainReason::WarmJobError));
                emit_idle_metric(
                    &entry.project_id,
                    RetainReason::WarmJobError,
                    mode,
                    &mut result,
                );
                continue;
            }
            Ok(false) => {}
        }

        let (is_idle, cached_last_activity) = match is_idle_base(
            snapshot.latest_activity.as_deref(),
            &entry.path,
            clock.now(),
            retention,
            grace,
        ) {
            Ok(value) => value,
            Err(_) => {
                result
                    .retained
                    .push((entry.project_id.clone(), RetainReason::ActivityError));
                emit_idle_metric(
                    &entry.project_id,
                    RetainReason::ActivityError,
                    mode,
                    &mut result,
                );
                continue;
            }
        };
        if !is_idle {
            result
                .retained
                .push((entry.project_id.clone(), RetainReason::Young));
            emit_idle_metric(&entry.project_id, RetainReason::Young, mode, &mut result);
            continue;
        }

        // Acquire the per-base lock only in delete mode.  Dry-run must not
        // create the lock file because doing so refreshes the directory mtime
        // and would change the fallback activity used by a later delete pass.
        // The same post-lock safety recheck is performed in both modes so the
        // candidate decisions remain identical.
        let guard = if mode == CacheCleanupMode::Delete {
            match locks.try_lock(&entry.path) {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => {
                    result
                        .retained
                        .push((entry.project_id.clone(), RetainReason::LockBusy));
                    emit_idle_metric(&entry.project_id, RetainReason::LockBusy, mode, &mut result);
                    continue;
                }
                Err(_) => {
                    result
                        .retained
                        .push((entry.project_id.clone(), RetainReason::LockError));
                    emit_idle_metric(
                        &entry.project_id,
                        RetainReason::LockError,
                        mode,
                        &mut result,
                    );
                    continue;
                }
            }
        } else {
            None
        };

        let post_lock_idle = match recheck_idle_after_lock(
            &entry.project_id,
            &entry.path,
            activity,
            warm_jobs,
            clock.now(),
            retention,
            grace,
            cached_last_activity,
        )
        .await
        {
            Ok(value) => value,
            Err(reason) => {
                result.retained.push((entry.project_id.clone(), reason));
                emit_idle_metric(&entry.project_id, reason, mode, &mut result);
                continue;
            }
        };
        if !post_lock_idle {
            result
                .retained
                .push((entry.project_id.clone(), RetainReason::Young));
            emit_idle_metric(&entry.project_id, RetainReason::Young, mode, &mut result);
            continue;
        }

        match mode {
            CacheCleanupMode::DryRun => {
                result.dry_run.push(entry.clone());
                result.projected_bytes = result.projected_bytes.saturating_add(entry.size_bytes);
                tracing::info!(
                    project_id = %entry.project_id,
                    size_bytes = entry.size_bytes,
                    mode = "dry_run",
                    "warm-base idle GC would delete idle base"
                );
                metrics::increment_cleanup_total(
                    metrics::COMPONENT_CARGO_WARM_BASE,
                    metrics::OUTCOME_DRY_RUN,
                    mode.as_metric_label(),
                );
            }
            CacheCleanupMode::Delete => match safe_remove_directory(&entry.path, root) {
                Ok(()) => {
                    result.deleted.push(entry.clone());
                    result.reclaimed_bytes =
                        result.reclaimed_bytes.saturating_add(entry.size_bytes);
                    tracing::info!(
                        project_id = %entry.project_id,
                        size_bytes = entry.size_bytes,
                        mode = "delete",
                        "warm-base idle GC deleted idle base"
                    );
                    metrics::increment_cleanup_total(
                        metrics::COMPONENT_CARGO_WARM_BASE,
                        metrics::OUTCOME_DELETED,
                        mode.as_metric_label(),
                    );
                    let _guard = guard;
                }
                Err(_) => {
                    result
                        .retained
                        .push((entry.project_id.clone(), RetainReason::DeleteError));
                    emit_idle_metric(
                        &entry.project_id,
                        RetainReason::DeleteError,
                        mode,
                        &mut result,
                    );
                    let _guard = guard;
                }
            },
        }
    }

    result
}

fn emit_idle_metric(
    project_id: &str,
    reason: RetainReason,
    mode: crate::context::CacheCleanupMode,
    _result: &mut IdleEvictionResult,
) {
    use djinn_telemetry::cache_cleanup as metrics;

    let outcome = match reason {
        RetainReason::Young => metrics::OUTCOME_RETAINED_YOUNG,
        RetainReason::ActiveTaskRun => metrics::OUTCOME_RETAINED_ACTIVE,
        RetainReason::WarmJobInFlight | RetainReason::WarmJobError => metrics::OUTCOME_RETAINED,
        RetainReason::LockBusy => metrics::OUTCOME_RETAINED_LOCK_BUSY,
        RetainReason::ActivityError
        | RetainReason::LockError
        | RetainReason::DeleteError
        | RetainReason::FreeSpaceError => metrics::OUTCOME_ERROR,
    };

    let _ = project_id;

    metrics::increment_cleanup_total(
        metrics::COMPONENT_CARGO_WARM_BASE,
        outcome,
        mode.as_metric_label(),
    );
}

#[allow(clippy::too_many_arguments)]
async fn recheck_idle_after_lock(
    project_id: &str,
    _path: &Path,
    activity: &dyn ActivityGuard,
    warm_jobs: &dyn WarmJobGuard,
    now: SystemTime,
    retention: Duration,
    grace: Duration,
    cached_last_activity: SystemTime,
) -> Result<bool, RetainReason> {
    let snapshot = activity
        .activity(project_id)
        .await
        .map_err(|_| RetainReason::ActivityError)?;
    if snapshot.has_active_task_run {
        return Err(RetainReason::ActiveTaskRun);
    }
    match warm_jobs.has_in_flight_warm(project_id).await {
        Ok(true) => return Err(RetainReason::WarmJobInFlight),
        Err(_) => return Err(RetainReason::WarmJobError),
        Ok(false) => {}
    }
    // DB activity is authoritative; if it is absent, use the cached mtime
    // captured before we acquired the lock (creating the lock file would have
    // refreshed the directory mtime).
    let last = if let Some(ts) = snapshot.latest_activity.as_deref() {
        system_time_from_offset(parse_iso8601(ts).map_err(|_| RetainReason::ActivityError)?)
    } else {
        cached_last_activity
    };
    let cutoff = last
        .checked_add(retention + grace)
        .ok_or(RetainReason::ActivityError)?;
    Ok(now >= cutoff)
}

/// Determine whether a base is idle.  DB activity takes precedence; the
/// directory mtime is used only when DB activity is genuinely absent (not when
/// the DB query failed).  A base is idle when its latest activity is older
/// than `retention + grace`.
fn is_idle_base(
    latest_activity: Option<&str>,
    path: &Path,
    now: SystemTime,
    retention: Duration,
    grace: Duration,
) -> Result<(bool, SystemTime), String> {
    let last = latest_activity_time(latest_activity, path)?;
    let cutoff = last
        .checked_add(retention + grace)
        .ok_or("retention cutoff overflow")?;
    Ok((now >= cutoff, last))
}

fn latest_activity_time(latest_activity: Option<&str>, path: &Path) -> Result<SystemTime, String> {
    if let Some(ts) = latest_activity {
        Ok(system_time_from_offset(parse_iso8601(ts)?))
    } else {
        let mtime = std::fs::metadata(path)
            .map_err(|e| format!("failed to read mtime for {}: {e}", path.display()))?
            .modified()
            .map_err(|e| format!("no mtime for {}: {e}", path.display()))?;
        Ok(mtime)
    }
}

fn parse_iso8601(value: &str) -> Result<OffsetDateTime, String> {
    let value = value.trim();
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|e| format!("failed to parse activity timestamp {value:?}: {e}"))
}

fn system_time_from_offset(dt: OffsetDateTime) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(dt.unix_timestamp() as u64)
}

/// Remove a directory only if it can be proven to reside under `root`.  This
/// prevents symlink traversal and escape attempts from deleting data outside the
/// warm-base pool.
fn safe_remove_directory(path: &Path, root: &Path) -> Result<(), String> {
    let root_canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path_canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("failed to canonicalize {}: {e}", path.display()))?;
    if !path_canonical.starts_with(&root_canonical) {
        return Err(format!(
            "refusing to delete {} because it is outside {}",
            path_canonical.display(),
            root_canonical.display()
        ));
    }
    if path_canonical == root_canonical {
        return Err(format!(
            "refusing to delete the warm-base root {}",
            root_canonical.display()
        ));
    }
    std::fs::remove_dir_all(&path_canonical)
        .map_err(|e| format!("failed to remove {}: {e}", path_canonical.display()))
}

#[cfg(test)]
mod tests;
