//! Reclamation of directories inside a warm project root that no current
//! layout produces.
//!
//! The production symptom these cover: a 27 GB `<project-id>/debug` tree, left
//! behind when the warm base moved to `mold-jobs-N` subdirectories, sitting
//! untouched for ten days beside a live `<project-id>/mold-jobs-4`. Every
//! existing phase walks a strict `<project-id>/mold-jobs-N` allowlist, so the
//! tree was never *retained* by a safety decision — it was unreachable.

use super::*;
use std::os::fd::AsRawFd;

const PROJECT: &str = "018f8b9a-0d70-7f0a-8000-000000000001";

/// Build `<root>/<PROJECT>/{mold-jobs-4, debug}` with the stale tree aged to
/// the epoch and the live variant left fresh.
fn stale_debug_beside_live_variant(root: &Path) -> (PathBuf, PathBuf) {
    let project = root.join(PROJECT);
    let live = project.join("mold-jobs-4");
    let stale = project.join("debug");
    std::fs::create_dir_all(live.join("deps")).expect("live variant");
    std::fs::write(live.join("deps").join("libdjinn.rlib"), b"live").expect("live artifact");
    std::fs::create_dir_all(stale.join("deps")).expect("stale tree");
    std::fs::write(stale.join("deps").join("libdjinn.rlib"), b"stale bytes").expect("stale");
    age_tree(&stale, SystemTime::UNIX_EPOCH);
    (live, stale)
}

/// Set every mtime in a tree, deepest first, so the root's own mtime is not
/// refreshed by a later child write.
fn age_tree(root: &Path, at: SystemTime) {
    let mut stack = vec![root.to_path_buf()];
    let mut all = Vec::new();
    while let Some(path) = stack.pop() {
        all.push(path.clone());
        if let Ok(children) = std::fs::read_dir(&path) {
            for child in children.flatten() {
                stack.push(child.path());
            }
        }
    }
    let time = filetime::FileTime::from_system_time(at);
    for path in all.into_iter().rev() {
        filetime::set_file_mtime(&path, time).expect("set mtime");
    }
}

fn config_with_idle_days(days: u64) -> crate::context::CacheCleanupConfig {
    crate::context::CacheCleanupConfig {
        warm_unrecognized_min_idle_days: days,
        ..default_config()
    }
}

async fn reclaim_at(
    root: &Path,
    activity: &Activity,
    warm: &Warm,
    locks: &dyn BaseLock,
    config: &crate::context::CacheCleanupConfig,
    now: SystemTime,
    mode: crate::context::CacheCleanupMode,
) -> UnrecognizedReclaimResult {
    let inventory = inventory_under(root).expect("inventory");
    reclaim_unrecognized_warm_entries(
        &inventory.unrecognized,
        activity,
        warm,
        locks,
        config,
        &TestClock::new(now, std::time::Instant::now()),
        mode,
        root,
    )
    .await
}

#[test]
fn inventory_reports_the_stale_tree_the_allowlist_cannot_classify() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    let inventory = inventory_under(temp.path()).expect("inventory");

    assert_eq!(inventory.entries.len(), 1, "one canonical variant");
    assert_eq!(inventory.entries[0].mold_jobs, 4);
    assert_eq!(
        inventory
            .unrecognized
            .iter()
            .map(|entry| (entry.project_id.as_str(), entry.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(PROJECT, "debug")],
        "the abandoned layout must appear by name, not only in `ignored`"
    );
    let entry = &inventory.unrecognized[0];
    assert_eq!(entry.path, stale);
    assert_eq!(
        entry.size_bytes, 11,
        "the reported size must be the measured tree, not a placeholder"
    );
    assert_eq!(entry.newest_mtime, Some(SystemTime::UNIX_EPOCH));
}

/// The load-bearing test: the stale tree is gone and the live variant is
/// untouched, through the real whole-project flock.
#[tokio::test]
async fn sweep_reclaims_the_stale_tree_and_keeps_the_live_variant() {
    let temp = tempfile::tempdir().expect("temp");
    let (live, stale) = stale_debug_beside_live_variant(temp.path());

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(
        !stale.exists(),
        "the stale tree must actually be gone from disk"
    );
    assert!(
        live.join("deps").join("libdjinn.rlib").exists(),
        "the live mold-jobs-4 variant and its artifacts must survive"
    );
    assert_eq!(
        result
            .deleted
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["debug"]
    );
    assert!(result.retained.is_empty(), "{:?}", result.retained);
    assert_eq!(
        result.reclaimed_bytes, 11,
        "reclaimed bytes must be the measured tree size"
    );

    // Re-running is a no-op rather than an error: nothing is left to find.
    let second = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;
    assert!(second.scanned.is_empty());
    assert_eq!(second.reclaimed_bytes, 0);
}

#[tokio::test]
async fn a_tree_younger_than_the_threshold_is_retained() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());
    age_tree(&stale, future(14));

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(stale.exists(), "a tree written a day ago must survive");
    assert!(result.deleted.is_empty());
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::Young);
}

/// A single fresh file deep inside an otherwise ancient tree keeps the whole
/// tree. The root directory's own mtime is deliberately left old, so this fails
/// if the age check ever narrows to a top-level `stat`.
#[tokio::test]
async fn one_fresh_file_deep_in_the_tree_keeps_the_whole_tree() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());
    let deep = stale.join("deps").join("libdjinn.rlib");
    filetime::set_file_mtime(&deep, filetime::FileTime::from_system_time(future(14)))
        .expect("touch");
    filetime::set_file_mtime(
        &stale,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
    )
    .expect("keep the root old");

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(stale.exists());
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::Young);
}

/// The whole-project warm lock, taken at the worker's own production identity
/// (`.warm-locks/<project-id>.lock`), blocks reclamation. Acquiring it here
/// directly rather than through the coordinator adapter is what proves the two
/// contend on the same inode.
#[tokio::test]
async fn a_held_worker_project_lock_blocks_reclamation() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    let lock_dir = temp.path().join(".warm-locks");
    std::fs::create_dir_all(&lock_dir).expect("lock dir");
    let lock_path = lock_dir.join(format!("{PROJECT}.lock"));
    let worker_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open worker lock");
    assert_eq!(
        unsafe { libc::flock(worker_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "worker-compatible lock must be acquired"
    );

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(stale.exists(), "a locked project must not be reclaimed");
    assert!(result.deleted.is_empty());
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::LockBusy);

    // Releasing the worker lock lets the very next pass reclaim it, proving the
    // retention was the lock and not some unrelated guard.
    drop(worker_lock);
    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;
    assert!(!stale.exists());
    assert_eq!(result.deleted.len(), 1);
}

#[tokio::test]
async fn every_project_guard_fails_closed() {
    for (reason, activity, warm) in [
        (
            UnrecognizedRetainReason::ActiveTaskRun,
            Activity(Ok(ActivitySnapshot {
                has_active_task_run: true,
                ..snapshot()
            })),
            Warm(Ok(false)),
        ),
        (
            UnrecognizedRetainReason::ActivityError,
            Activity(Err("db down".into())),
            Warm(Ok(false)),
        ),
        (
            UnrecognizedRetainReason::WarmJobInFlight,
            Activity(Ok(snapshot())),
            Warm(Ok(true)),
        ),
        (
            UnrecognizedRetainReason::WarmJobError,
            Activity(Ok(snapshot())),
            Warm(Err("apiserver down".into())),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let (_live, stale) = stale_debug_beside_live_variant(temp.path());

        let result = reclaim_at(
            temp.path(),
            &activity,
            &warm,
            &SharedWarmProjectLock,
            &config_with_idle_days(7),
            future(15),
            crate::context::CacheCleanupMode::Delete,
        )
        .await;

        assert!(stale.exists(), "{reason:?} must retain the tree");
        assert_eq!(result.retained[0].1, reason);
        assert!(result.deleted.is_empty());
    }
}

#[tokio::test]
async fn a_lock_error_retains_rather_than_deleting() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &FailingBaseLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(stale.exists());
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::LockError);
}

#[tokio::test]
async fn dry_run_reports_without_deleting_or_locking() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::DryRun,
    )
    .await;

    assert!(stale.exists(), "dry-run must never delete");
    assert!(result.deleted.is_empty());
    assert_eq!(result.dry_run.len(), 1);
    assert!(
        !temp.path().join(".warm-locks").exists(),
        "dry-run must not create lock state"
    );
}

#[tokio::test]
async fn reserved_dot_directories_are_never_reclaimed() {
    let temp = tempfile::tempdir().expect("temp");
    let project = temp.path().join(PROJECT);
    std::fs::create_dir_all(project.join("mold-jobs-4")).expect("variant");
    let reserved = project.join(".operator-parked");
    std::fs::create_dir_all(&reserved).expect("reserved");
    std::fs::write(reserved.join("keep"), b"keep").expect("file");
    age_tree(&reserved, SystemTime::UNIX_EPOCH);

    let result = reclaim_at(
        temp.path(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        future(15),
        crate::context::CacheCleanupMode::Delete,
    )
    .await;

    assert!(reserved.exists(), "dot-directories are out of reach");
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::Reserved);
}

/// A tree that changes between the inventory walk and the lock must not be
/// removed on the strength of the stale observation.
#[tokio::test]
async fn a_write_between_planning_and_the_lock_aborts_the_removal() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    // Plan against the ancient tree, then let a "writer" touch it before the
    // reclaimer takes the lock and rechecks.
    let inventory = inventory_under(temp.path()).expect("inventory");
    assert_eq!(inventory.unrecognized.len(), 1);
    std::fs::write(stale.join("deps").join("fresh.rlib"), b"fresh").expect("write");
    filetime::set_file_mtime(
        stale.join("deps").join("fresh.rlib"),
        filetime::FileTime::from_system_time(future(15)),
    )
    .expect("touch");

    let result = reclaim_unrecognized_warm_entries(
        &inventory.unrecognized,
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        &TestClock::new(future(15), std::time::Instant::now()),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;

    assert!(stale.exists(), "the post-lock recheck must abort");
    assert!(result.deleted.is_empty());
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::Young);
}

/// A symlink is not a directory, so it is counted in `ignored` and never
/// becomes a reclamation candidate.
#[test]
fn a_symlinked_project_child_is_not_a_reclamation_candidate() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, _stale) = stale_debug_beside_live_variant(temp.path());
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::os::unix::fs::symlink(&outside, temp.path().join(PROJECT).join("linked"))
        .expect("symlink");

    let inventory = inventory_under(temp.path()).expect("inventory");

    assert_eq!(
        inventory
            .unrecognized
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["debug"],
        "a symlink must never become a reclamation candidate"
    );
    assert!(outside.exists());
}

/// Nothing at the warm root itself is a candidate — that level holds reserved
/// machinery and other roots' namespaces, and stays purely in `ignored`.
#[test]
fn warm_root_children_outside_a_project_uuid_are_never_candidates() {
    let temp = tempfile::tempdir().expect("temp");
    stale_debug_beside_live_variant(temp.path());
    std::fs::create_dir_all(temp.path().join(".warm-locks")).expect("locks");
    std::fs::create_dir_all(temp.path().join("not-a-uuid")).expect("stray");
    std::fs::write(temp.path().join(".djinn-gc.lock"), b"").expect("lock file");

    let inventory = inventory_under(temp.path()).expect("inventory");

    assert_eq!(
        inventory
            .unrecognized
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["debug"]
    );
    assert_eq!(
        inventory.ignored, 4,
        "three root-level strays plus the project-level `debug`"
    );
}

/// An unmeasurable tree has an unknown age, which must never read as "old".
#[tokio::test]
async fn an_unmeasurable_tree_is_retained_not_reclaimed() {
    let temp = tempfile::tempdir().expect("temp");
    let (_live, stale) = stale_debug_beside_live_variant(temp.path());

    let entries = vec![UnrecognizedWarmEntry {
        project_id: PROJECT.into(),
        name: "debug".into(),
        path: stale.clone(),
        size_bytes: 0,
        newest_mtime: None,
    }];
    let result = reclaim_unrecognized_warm_entries(
        &entries,
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        &TestClock::new(future(15), std::time::Instant::now()),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;

    assert!(stale.exists());
    assert_eq!(
        result.retained[0].1,
        UnrecognizedRetainReason::MeasurementError
    );
}

/// Containment is re-derived from the filesystem under the lock, so an entry
/// pointing outside the warm root cannot be removed even if it reaches the
/// reclaimer with a plausible-looking record.
#[tokio::test]
async fn a_target_outside_the_warm_root_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    stale_debug_beside_live_variant(temp.path());
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let victim = elsewhere.path().join(PROJECT).join("debug");
    std::fs::create_dir_all(&victim).expect("victim");
    age_tree(&victim, SystemTime::UNIX_EPOCH);

    let entries = vec![UnrecognizedWarmEntry {
        project_id: PROJECT.into(),
        name: "debug".into(),
        path: victim.clone(),
        size_bytes: 0,
        newest_mtime: Some(SystemTime::UNIX_EPOCH),
    }];
    let result = reclaim_unrecognized_warm_entries(
        &entries,
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmProjectLock,
        &config_with_idle_days(7),
        &TestClock::new(future(15), std::time::Instant::now()),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;

    assert!(victim.exists(), "a target outside the root must survive");
    assert_eq!(result.retained[0].1, UnrecognizedRetainReason::Changed);
}
