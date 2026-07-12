//! Conservative inventory and guard planning for Cargo warm bases, plus idle
//! whole-base eviction.
//!
//! The inventory and guard-planning phase deliberately does not remove
//! directories.  The idle evictor consumes guard-plan candidates and performs
//! the destructive work only after re-checking every safety condition under a
//! held per-base lock.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use djinn_core::clock::Clock;
use time::OffsetDateTime;
use uuid::Uuid;

pub const CARGO_WARM_BASE_ROOT: &str = "/cache/cargo-target";
pub const WARM_BASE_GC_LOCK_FILE: &str = ".djinn-gc.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseClassification {
    Registered,
    Deleted,
    Orphaned,
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
    for entry in inventory.entries {
        let snapshot = match activity.activity(&entry.project_id).await {
            Ok(value) => value,
            Err(_) => {
                plan.retained
                    .push((entry.project_id, RetainReason::ActivityError));
                continue;
            }
        };
        if snapshot.has_active_task_run {
            plan.retained
                .push((entry.project_id, RetainReason::ActiveTaskRun));
            continue;
        }
        match warm_jobs.has_in_flight_warm(&entry.project_id).await {
            Ok(true) => {
                plan.retained
                    .push((entry.project_id, RetainReason::WarmJobInFlight));
                continue;
            }
            Err(_) => {
                plan.retained
                    .push((entry.project_id, RetainReason::WarmJobError));
                continue;
            }
            Ok(false) => {}
        }
        match locks.try_lock(&entry.path) {
            LockOutcome::Busy => {
                plan.retained
                    .push((entry.project_id, RetainReason::LockBusy));
                continue;
            }
            LockOutcome::Error => {
                plan.retained
                    .push((entry.project_id, RetainReason::LockError));
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
        plan.candidates.push(WarmBaseCandidate {
            entry,
            classification,
            latest_activity: snapshot.latest_activity,
            free_space_bytes: free,
        });
    }
    plan
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
    fn free_space_bytes(&self, path: &Path) -> Result<u64, String> {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| error.to_string())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: statvfs initializes `stat` on a successful return.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stat = unsafe { stat.assume_init() };
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
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
mod tests {
    use super::*;
    use djinn_core::clock::TestClock;

    struct Activity(Result<ActivitySnapshot, String>);
    #[async_trait]
    impl ActivityGuard for Activity {
        async fn activity(&self, _: &str) -> Result<ActivitySnapshot, String> {
            self.0.clone()
        }
    }
    struct Warm(Result<bool, String>);
    #[async_trait]
    impl WarmJobGuard for Warm {
        async fn has_in_flight_warm(&self, _: &str) -> Result<bool, String> {
            self.0.clone()
        }
    }
    struct Space(Result<u64, String>);
    impl FreeSpaceGuard for Space {
        fn free_space_bytes(&self, _: &Path) -> Result<u64, String> {
            self.0.clone()
        }
    }
    struct Lock(LockOutcome);
    impl BaseLockGuard for Lock {
        fn try_lock(&self, _: &Path) -> LockOutcome {
            self.0
        }
    }
    struct RecordingBaseLock {
        attempts: std::sync::Mutex<Vec<PathBuf>>,
        succeed: bool,
    }
    impl BaseLock for RecordingBaseLock {
        fn try_lock(&self, path: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
            self.attempts.lock().unwrap().push(path.to_path_buf());
            if self.succeed {
                Ok(Some(Box::new(NoopGuard)))
            } else {
                Ok(None)
            }
        }
    }
    struct NoopGuard;
    impl LockGuard for NoopGuard {}
    struct FailingBaseLock;
    impl BaseLock for FailingBaseLock {
        fn try_lock(&self, _: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
            Err("lock error".into())
        }
    }
    struct NoopBaseLock;
    impl BaseLock for NoopBaseLock {
        fn try_lock(&self, _: &Path) -> Result<Option<Box<dyn LockGuard>>, String> {
            Ok(Some(Box::new(NoopGuard)))
        }
    }
    fn entry() -> WarmBaseEntry {
        WarmBaseEntry {
            project_id: "018f8b9a-0d70-7f0a-8000-000000000001".into(),
            path: PathBuf::from("base"),
            size_bytes: 7,
        }
    }
    fn snapshot() -> ActivitySnapshot {
        ActivitySnapshot {
            known_project: true,
            deleted_project: false,
            has_active_task_run: false,
            latest_activity: None,
        }
    }
    fn default_config() -> crate::context::CacheCleanupConfig {
        crate::context::CacheCleanupConfig::default()
    }
    fn epoch_clock() -> TestClock {
        TestClock::new(SystemTime::UNIX_EPOCH, std::time::Instant::now())
    }
    fn old_base(temp: &tempfile::TempDir, id: &str) -> PathBuf {
        let base = temp.path().join(id);
        std::fs::create_dir(&base).expect("dir");
        filetime::set_file_mtime(
            &base,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        )
        .unwrap();
        base
    }
    fn make_entry(base: &Path) -> WarmBaseEntry {
        WarmBaseEntry {
            project_id: base.file_name().unwrap().to_str().unwrap().into(),
            path: base.to_path_buf(),
            size_bytes: directory_size(base),
        }
    }
    fn future(days: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
    }

    #[tokio::test]
    async fn classifications_registered_deleted_and_orphaned() {
        let lock = Lock(LockOutcome::Available);
        let space = Space(Ok(9));
        let warm = Warm(Ok(false));
        let registered = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &Activity(Ok(ActivitySnapshot {
                known_project: true,
                ..snapshot()
            })),
            &warm,
            &space,
            &lock,
        )
        .await;
        assert_eq!(
            registered.candidates[0].classification,
            BaseClassification::Registered
        );

        let deleted = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &Activity(Ok(ActivitySnapshot {
                known_project: false,
                deleted_project: true,
                ..snapshot()
            })),
            &warm,
            &space,
            &lock,
        )
        .await;
        assert_eq!(
            deleted.candidates[0].classification,
            BaseClassification::Deleted
        );

        let orphaned = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &Activity(Ok(ActivitySnapshot {
                known_project: false,
                ..snapshot()
            })),
            &warm,
            &space,
            &lock,
        )
        .await;
        assert_eq!(
            orphaned.candidates[0].classification,
            BaseClassification::Orphaned
        );
    }
    #[tokio::test]
    async fn guards_fail_closed_and_classify() {
        let activity = Activity(Ok(snapshot()));
        let space = Space(Ok(9));
        let lock = Lock(LockOutcome::Available);
        let initial_plan = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &activity,
            &Warm(Ok(false)),
            &space,
            &lock,
        )
        .await;
        assert_eq!(
            initial_plan.candidates[0].classification,
            BaseClassification::Registered
        );
        for reason in [
            RetainReason::ActiveTaskRun,
            RetainReason::WarmJobInFlight,
            RetainReason::WarmJobError,
            RetainReason::LockBusy,
            RetainReason::LockError,
        ] {
            let active = if reason == RetainReason::ActiveTaskRun {
                Activity(Ok(ActivitySnapshot {
                    has_active_task_run: true,
                    ..snapshot()
                }))
            } else {
                Activity(Ok(snapshot()))
            };
            let warm = if reason == RetainReason::WarmJobInFlight {
                Warm(Ok(true))
            } else if reason == RetainReason::WarmJobError {
                Warm(Err("no".into()))
            } else {
                Warm(Ok(false))
            };
            let lock = if reason == RetainReason::LockBusy {
                Lock(LockOutcome::Busy)
            } else if reason == RetainReason::LockError {
                Lock(LockOutcome::Error)
            } else {
                Lock(LockOutcome::Available)
            };
            let planned = plan(
                WarmBaseInventory {
                    entries: vec![entry()],
                    ignored: 0,
                },
                &active,
                &warm,
                &space,
                &lock,
            )
            .await;
            assert_eq!(planned.retained[0].1, reason);
        }
    }
    #[tokio::test]
    async fn activity_and_measurement_errors_retain() {
        let lock = Lock(LockOutcome::Available);
        let warm = Warm(Ok(false));
        let result = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &Activity(Err("db".into())),
            &warm,
            &Space(Ok(1)),
            &lock,
        )
        .await;
        assert_eq!(result.retained[0].1, RetainReason::ActivityError);
        let result = plan(
            WarmBaseInventory {
                entries: vec![entry()],
                ignored: 0,
            },
            &Activity(Ok(snapshot())),
            &warm,
            &Space(Err("stat".into())),
            &lock,
        )
        .await;
        assert_eq!(result.retained[0].1, RetainReason::FreeSpaceError);
    }
    #[test]
    fn strict_inventory_ignores_malformed_and_files() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        std::fs::create_dir(temp.path().join(id)).expect("dir");
        std::fs::create_dir(temp.path().join("018f8b9a0d707f0a8000000000000001")).expect("bad");
        std::fs::write(temp.path().join("file"), b"x").expect("file");
        let inventory = inventory_under(temp.path()).expect("inventory");
        assert_eq!(inventory.entries.len(), 1);
        assert_eq!(inventory.ignored, 2);
    }

    // ─── Idle eviction tests ───────────────────────────────────────────

    #[tokio::test]
    async fn idle_eviction_deletes_old_registered_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = temp.path().join(id);
        std::fs::create_dir(&base).expect("dir");
        std::fs::write(base.join("artifact"), b"x").expect("file");
        filetime::set_file_mtime(
            &base,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        )
        .unwrap();

        let clock = TestClock::new(future(15), std::time::Instant::now());
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(
            result.deleted.len(),
            1,
            "deleted={:?}, retained={:?}",
            result.deleted,
            result.retained
        );
        assert_eq!(result.retained.len(), 0);
        assert!(result.reclaimed_bytes > 0);
        assert!(!base.exists());
    }

    #[tokio::test]
    async fn idle_eviction_retains_young_base_by_mtime() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = temp.path().join(id);
        std::fs::create_dir(&base).expect("dir");

        let clock = TestClock::new(SystemTime::now(), std::time::Instant::now());
        let entry = WarmBaseEntry {
            project_id: id.into(),
            path: base.clone(),
            size_bytes: 1,
        };
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::Young);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn idle_eviction_db_activity_takes_precedence_over_mtime() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);

        let now = future(15);
        let clock = TestClock::new(now, std::time::Instant::now());
        let recent = (now - Duration::from_secs(24 * 60 * 60))
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent_iso = format!(
            "{}T00:00:00Z",
            OffsetDateTime::from(SystemTime::UNIX_EPOCH + Duration::from_secs(recent)).date()
        );
        let entry = WarmBaseEntry {
            project_id: id.into(),
            path: base.clone(),
            size_bytes: 1,
        };
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: Some(recent_iso),
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::Young);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn idle_eviction_deletes_deleted_project_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: false,
            deleted_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.deleted.len(), 1);
        assert!(!base.exists());
    }

    #[tokio::test]
    async fn idle_eviction_deletes_orphaned_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: false,
            deleted_project: false,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.deleted.len(), 1);
        assert!(!base.exists());
    }

    #[tokio::test]
    async fn active_task_run_retains_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: true,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = epoch_clock();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::ActiveTaskRun);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn in_flight_warm_job_retains_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(true));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = epoch_clock();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::WarmJobInFlight);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn lock_busy_retains_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = RecordingBaseLock {
            attempts: std::sync::Mutex::new(Vec::new()),
            succeed: false,
        };
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::LockBusy);
        assert_eq!(locks.attempts.lock().unwrap().len(), 1);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn post_lock_recheck_retains_base() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        struct FlipActivityGuard {
            first: std::sync::Mutex<Option<ActivitySnapshot>>,
            second: ActivitySnapshot,
        }
        #[async_trait]
        impl ActivityGuard for FlipActivityGuard {
            async fn activity(&self, _: &str) -> Result<ActivitySnapshot, String> {
                let mut first = self.first.lock().unwrap();
                if let Some(snapshot) = first.take() {
                    Ok(snapshot)
                } else {
                    Ok(self.second.clone())
                }
            }
        }
        let activity = FlipActivityGuard {
            first: std::sync::Mutex::new(Some(ActivitySnapshot {
                known_project: true,
                has_active_task_run: false,
                latest_activity: None,
                ..snapshot()
            })),
            second: ActivitySnapshot {
                known_project: true,
                has_active_task_run: true,
                latest_activity: None,
                ..snapshot()
            },
        };
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::ActiveTaskRun);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn dry_run_and_delete_select_same_candidates() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = WarmBaseEntry {
            project_id: id.into(),
            path: base.clone(),
            size_bytes: 42,
        };
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let dry = evict_idle_warm_bases(
            inventory.clone(),
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::DryRun,
            temp.path(),
        )
        .await;
        let delete = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;

        assert_eq!(dry.dry_run.len(), 1);
        assert_eq!(delete.deleted.len(), 1);
        assert_eq!(dry.projected_bytes, 42);
        assert_eq!(delete.reclaimed_bytes, 42);
        assert_eq!(dry.retained.len(), delete.retained.len());
        assert!(!base.exists());
    }

    #[tokio::test]
    async fn dry_run_and_delete_parity_with_flock_lock() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = WarmBaseEntry {
            project_id: id.into(),
            path: base.clone(),
            size_bytes: 42,
        };
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = FlockBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let dry = evict_idle_warm_bases(
            inventory.clone(),
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::DryRun,
            temp.path(),
        )
        .await;

        // Dry-run must not create the lock file; doing so would refresh the
        // directory mtime and change the fallback activity for a later delete
        // pass, breaking parity between the two modes.
        assert!(!base.join(WARM_BASE_GC_LOCK_FILE).exists());

        let delete = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;

        assert_eq!(dry.dry_run.len(), 1);
        assert_eq!(delete.deleted.len(), 1);
        assert_eq!(dry.dry_run[0].project_id, delete.deleted[0].project_id);
        assert_eq!(dry.projected_bytes, 42);
        assert_eq!(delete.reclaimed_bytes, 42);
        assert_eq!(dry.retained.len(), delete.retained.len());
        assert!(!base.exists());
    }

    #[tokio::test]
    async fn activity_error_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Err("db down".into()));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = epoch_clock();

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::ActivityError);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn lock_error_fails_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = FailingBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            temp.path(),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::LockError);
        assert!(base.exists());
    }

    #[tokio::test]
    async fn unsafe_path_is_not_deleted() {
        let temp = tempfile::tempdir().expect("temp");
        let id = "018f8b9a-0d70-7f0a-8000-000000000001";
        let base = old_base(&temp, id);
        let entry = make_entry(&base);
        let inventory = WarmBaseInventory {
            entries: vec![entry],
            ignored: 0,
        };
        let activity = Activity(Ok(ActivitySnapshot {
            known_project: true,
            has_active_task_run: false,
            latest_activity: None,
            ..snapshot()
        }));
        let warm = Warm(Ok(false));
        let locks = NoopBaseLock;
        let config = default_config();
        let clock = TestClock::new(future(15), std::time::Instant::now());

        // Pass a different root so the path is outside it.
        let result = evict_idle_warm_bases(
            inventory,
            &activity,
            &warm,
            &locks,
            &config,
            &clock,
            crate::context::CacheCleanupMode::Delete,
            Path::new("/some/other/root"),
        )
        .await;
        assert_eq!(result.retained.len(), 1);
        assert_eq!(result.retained[0].1, RetainReason::DeleteError);
        assert!(base.exists());
    }
}
