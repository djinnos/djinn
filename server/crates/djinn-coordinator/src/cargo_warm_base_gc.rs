//! Conservative inventory and guard planning for Cargo warm bases.
//!
//! This module deliberately does not remove directories.  The idle and pressure
//! evictors consume its candidates in later passes; keeping inventory and guard
//! evaluation separate makes every unsafe/unknown condition retain the base.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

pub const CARGO_WARM_BASE_ROOT: &str = "/cache/cargo-target";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    Available,
    Busy,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainReason {
    ActivityError,
    ActiveTaskRun,
    WarmJobError,
    WarmJobInFlight,
    FreeSpaceError,
    LockBusy,
    LockError,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
