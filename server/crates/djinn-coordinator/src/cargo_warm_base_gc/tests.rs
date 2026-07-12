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

// ─── Pressure planning tests ─────────────────────────────────────────

struct Capacity(Result<CapacitySnapshot, String>);
impl FilesystemCapacity for Capacity {
    fn capacity(&self, _: &Path) -> Result<CapacitySnapshot, String> {
        self.0.clone()
    }
}

fn pressure_config(low: f64, high: f64) -> crate::context::CacheCleanupConfig {
    crate::context::CacheCleanupConfig {
        warm_base_low_free_ratio: low,
        warm_base_high_free_ratio: high,
        ..default_config()
    }
}

fn pressure_entry(id: &str, size: u64) -> WarmBaseEntry {
    WarmBaseEntry {
        project_id: id.into(),
        path: PathBuf::from(id),
        size_bytes: size,
    }
}

fn old_activity(days_ago: u64) -> String {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(days_ago * 24 * 60 * 60);
    format!("{}T00:00:00Z", OffsetDateTime::from(when).date())
}

fn snapshot_at(activity: Option<String>) -> ActivitySnapshot {
    ActivitySnapshot {
        known_project: true,
        deleted_project: false,
        has_active_task_run: false,
        latest_activity: activity,
    }
}

#[tokio::test]
async fn pressure_above_low_watermark_produces_no_candidates() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 200,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 100)],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(14)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert!(plan.candidates.is_empty());
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].1, PressureSkipReason::AboveLowWatermark);
}

#[tokio::test]
async fn pressure_at_low_watermark_is_no_op() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 150,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 100)],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(14)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert!(plan.candidates.is_empty());
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].1, PressureSkipReason::AboveLowWatermark);
}

#[tokio::test]
async fn pressure_below_low_selects_minimal_prefix() {
    let config = pressure_config(0.15, 0.25);
    // 10% free; target = 250 - 100 = 150 bytes.
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000003", 80),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 100),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 60),
        ],
        ignored: 0,
    };
    // Same activity for all, so ordering is by project ID.
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 2);
    assert_eq!(plan.projected_bytes, 160);
    assert_eq!(plan.target_bytes, 150);
    assert_eq!(
        plan.candidates[0].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000001"
    );
    assert_eq!(
        plan.candidates[1].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000002"
    );
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].1, PressureSkipReason::NotSelected);
}

#[tokio::test]
async fn pressure_orders_by_activity_then_project_id() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000003", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 50),
        ],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 3);
    assert_eq!(
        plan.candidates[0].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000001"
    );
    assert_eq!(
        plan.candidates[1].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000002"
    );
    assert_eq!(
        plan.candidates[2].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000003"
    );
}

#[tokio::test]
async fn pressure_excluded_candidates_are_not_selected() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000003", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000004", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000005", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000006", 50),
        ],
        ignored: 0,
    };

    struct GuardActivity;
    #[async_trait]
    impl ActivityGuard for GuardActivity {
        async fn activity(&self, project_id: &str) -> Result<ActivitySnapshot, String> {
            Ok(match project_id {
                "018f8b9a-0d70-7f0a-8000-000000000001" => ActivitySnapshot {
                    has_active_task_run: true,
                    latest_activity: Some(old_activity(12)),
                    ..snapshot()
                },
                "018f8b9a-0d70-7f0a-8000-000000000004" => return Err("activity error".into()),
                _ => snapshot_at(Some(old_activity(12))),
            })
        }
    }

    struct GuardWarm;
    #[async_trait]
    impl WarmJobGuard for GuardWarm {
        async fn has_in_flight_warm(&self, project_id: &str) -> Result<bool, String> {
            match project_id {
                "018f8b9a-0d70-7f0a-8000-000000000002" => Ok(true),
                "018f8b9a-0d70-7f0a-8000-000000000005" => Err("warm error".into()),
                _ => Ok(false),
            }
        }
    }

    struct BusyLock;
    impl BaseLockGuard for BusyLock {
        fn try_lock(&self, path: &Path) -> LockOutcome {
            if path.as_os_str() == "018f8b9a-0d70-7f0a-8000-000000000006" {
                LockOutcome::Busy
            } else {
                LockOutcome::Available
            }
        }
    }

    let warm = GuardWarm;
    let activity = GuardActivity;
    let locks = BusyLock;
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    let selected_ids: Vec<_> = plan
        .candidates
        .iter()
        .map(|c| c.entry.project_id.as_str())
        .collect();
    assert!(!selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000001"));
    assert!(!selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000002"));
    assert!(!selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000004"));
    assert!(!selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000005"));
    assert!(!selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000006"));
    assert!(selected_ids.contains(&"018f8b9a-0d70-7f0a-8000-000000000003"));
}

#[tokio::test]
async fn pressure_measurement_error_fails_closed() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Err("statvfs failed".into()));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 100)],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert!(plan.candidates.is_empty());
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].1, PressureSkipReason::MeasurementError);
}

#[tokio::test]
async fn pressure_insufficient_reclaimable_space_selects_all_safe() {
    let config = pressure_config(0.15, 0.25);
    // target = 250 - 100 = 150, but safe bases total only 100.
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 40),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 60),
        ],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 2);
    assert_eq!(plan.projected_bytes, 100);
    assert_eq!(plan.target_bytes, 150);
    assert!(plan.retained.is_empty());
}

#[tokio::test]
async fn pressure_exact_stop_at_high_watermark() {
    let config = pressure_config(0.15, 0.25);
    // target = 250 - 100 = 150 exactly.
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 150),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 50),
        ],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.projected_bytes, 150);
    assert_eq!(plan.target_bytes, 150);
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].1, PressureSkipReason::NotSelected);
}

#[tokio::test]
async fn pressure_young_base_is_excluded() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 50),
        ],
        ignored: 0,
    };

    struct YoungActivity;
    #[async_trait]
    impl ActivityGuard for YoungActivity {
        async fn activity(&self, project_id: &str) -> Result<ActivitySnapshot, String> {
            Ok(match project_id {
                "018f8b9a-0d70-7f0a-8000-000000000001" => snapshot_at(Some(old_activity(12))),
                _ => snapshot_at(Some(format!(
                    "{}T00:00:00Z",
                    OffsetDateTime::from(future(15)).date()
                ))),
            })
        }
    }

    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory,
        &YoungActivity,
        &warm,
        &locks,
        &capacity,
        &config,
        &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(
        plan.candidates[0].entry.project_id,
        "018f8b9a-0d70-7f0a-8000-000000000001"
    );
    assert_eq!(plan.retained.len(), 1);
    assert_eq!(plan.retained[0].0, "018f8b9a-0d70-7f0a-8000-000000000002");
    assert_eq!(plan.retained[0].1, PressureSkipReason::Young);
}

#[tokio::test]
async fn pressure_fractional_byte_boundary_reaches_high_watermark() {
    // Regression: total_bytes is so small that total_bytes * high_free_ratio
    // is not an integer. A ceiling calculation must be used so that the
    // required high-watermark byte count is met, not truncated below it.
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 10,
        available_bytes: 1,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 1)],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    // Need 3 free bytes to reach ceil(10 * 0.25) = 3. Available is 1, so target
    // is 2 bytes. With only a 1-byte base, the whole safe prefix is selected.
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.target_bytes, 2);
    assert_eq!(plan.projected_bytes, 1);
}

#[tokio::test]
async fn pressure_large_capacity_preserves_high_watermark_byte_ceiling() {
    // Regression: converting this capacity to f64 rounds it down by one. The
    // planner must retain that final byte when computing ceil(total * 0.25).
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 9_007_199_254_740_993,
        available_bytes: 0,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry(
            "018f8b9a-0d70-7f0a-8000-000000000001",
            2_251_799_813_685_248,
        )],
        ignored: 0,
    };
    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = Lock(LockOutcome::Available);
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.projected_bytes, 2_251_799_813_685_248);
    assert_eq!(plan.target_bytes, 2_251_799_813_685_249);
}

#[tokio::test]
async fn pressure_large_capacity_compares_low_watermark_exactly() {
    let config = pressure_config(0.5, 0.5);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 9_007_199_254_740_993,
        available_bytes: 4_503_599_627_370_496,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 1)],
        ignored: 0,
    };
    let plan = plan_pressure_eviction(
        inventory,
        &Activity(Ok(snapshot_at(Some(old_activity(12))))),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        &capacity,
        &config,
        &TestClock::new(future(15), std::time::Instant::now()),
    )
    .await;
    assert_eq!(plan.target_bytes, 1);
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(plan.projected_bytes, 1);
}

#[tokio::test]
async fn pressure_lock_busy_and_error_retained() {
    let config = pressure_config(0.15, 0.25);
    let capacity = Capacity(Ok(CapacitySnapshot {
        total_bytes: 1000,
        available_bytes: 100,
    }));
    let inventory = WarmBaseInventory {
        entries: vec![
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000001", 50),
            pressure_entry("018f8b9a-0d70-7f0a-8000-000000000002", 50),
        ],
        ignored: 0,
    };

    struct BusyLock;
    impl BaseLockGuard for BusyLock {
        fn try_lock(&self, path: &Path) -> LockOutcome {
            if path.as_os_str() == "018f8b9a-0d70-7f0a-8000-000000000001" {
                LockOutcome::Busy
            } else {
                LockOutcome::Error
            }
        }
    }

    let activity = Activity(Ok(snapshot_at(Some(old_activity(12)))));
    let warm = Warm(Ok(false));
    let locks = BusyLock;
    let clock = TestClock::new(future(15), std::time::Instant::now());

    let plan = plan_pressure_eviction(
        inventory, &activity, &warm, &locks, &capacity, &config, &clock,
    )
    .await;

    assert!(plan.candidates.is_empty());
    assert_eq!(plan.retained.len(), 2);
    assert_eq!(plan.retained[0].1, PressureSkipReason::LockBusy);
    assert_eq!(plan.retained[1].1, PressureSkipReason::LockError);
}

#[tokio::test]
async fn pressure_execute_dry_run_reports_planner_prefix_without_locking() {
    let temp = tempfile::tempdir().unwrap();
    let base = old_base(&temp, "018f8b9a-0d70-7f0a-8000-000000000001");
    let planned = WarmBaseEntry {
        project_id: "018f8b9a-0d70-7f0a-8000-000000000001".into(),
        path: base.clone(),
        size_bytes: 99,
    };
    let locks = RecordingBaseLock {
        attempts: std::sync::Mutex::new(Vec::new()),
        succeed: true,
    };
    let result = execute_pressure_eviction(
        PressureEvictionPlan {
            candidates: vec![WarmBaseCandidate {
                entry: planned.clone(),
                classification: BaseClassification::Registered,
                latest_activity: None,
                free_space_bytes: 0,
            }],
            retained: Vec::new(),
            projected_bytes: 99,
            target_bytes: 99,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &locks,
        &Capacity(Err("must not measure dry run".into())),
        &default_config(),
        &epoch_clock(),
        crate::context::CacheCleanupMode::DryRun,
        temp.path(),
    )
    .await;

    assert_eq!(result.dry_run, vec![planned]);
    assert_eq!(result.projected_bytes, 99);
    assert!(result.deleted.is_empty());
    assert!(base.exists());
    assert!(locks.attempts.lock().unwrap().is_empty());
}
