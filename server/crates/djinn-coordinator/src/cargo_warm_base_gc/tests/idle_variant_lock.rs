use super::*;
use std::os::fd::AsRawFd;

#[test]
fn inventory_returns_each_canonical_mold_variant_without_substitution() {
    let temp = tempfile::tempdir().expect("temp");
    let id = "018f8b9a-0d70-7f0a-8000-000000000001";
    let project = temp.path().join(id);
    for jobs in [1_u32, 4, 7] {
        let variant = project.join(format!("mold-jobs-{jobs}"));
        std::fs::create_dir_all(&variant).expect("canonical variant");
        std::fs::write(variant.join("artifact"), b"artifact").expect("artifact");
    }
    std::fs::write(project.join("legacy-artifact"), b"legacy").expect("legacy file");
    std::fs::create_dir(project.join("mold-jobs-0")).expect("zero variant");
    std::fs::create_dir(project.join("mold-jobs-04")).expect("noncanonical variant");
    std::fs::create_dir(project.join("mold-jobs-x")).expect("malformed variant");

    let inventory = inventory_under(temp.path()).expect("inventory");
    assert_eq!(inventory.entries.len(), 3);
    assert_eq!(
        inventory
            .entries
            .iter()
            .map(|entry| (
                entry.project_id.as_str(),
                entry.mold_jobs,
                entry.path.file_name().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            (id, 1, std::ffi::OsStr::new("mold-jobs-1")),
            (id, 4, std::ffi::OsStr::new("mold-jobs-4")),
            (id, 7, std::ffi::OsStr::new("mold-jobs-7")),
        ]
    );
    assert_eq!(inventory.ignored, 4);
}

#[tokio::test]
async fn idle_eviction_deletes_only_the_selected_mold_variant() {
    let temp = tempfile::tempdir().expect("temp");
    let id = "018f8b9a-0d70-7f0a-8000-000000000001";
    let project = temp.path().join(id);
    let one = project.join("mold-jobs-1");
    let four = project.join("mold-jobs-4");
    std::fs::create_dir_all(&one).expect("one");
    std::fs::create_dir_all(&four).expect("four");
    std::fs::write(one.join("artifact"), b"one").unwrap();
    std::fs::write(four.join("artifact"), b"four").unwrap();
    filetime::set_file_mtime(
        &one,
        filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
    )
    .unwrap();
    let inventory = inventory_under(temp.path()).unwrap();
    let selected = inventory
        .entries
        .into_iter()
        .find(|entry| entry.mold_jobs == 1)
        .unwrap();
    let locks = RecordingBaseLock {
        attempts: std::sync::Mutex::new(Vec::new()),
        succeed: true,
    };
    let result = evict_idle_warm_bases(
        WarmBaseInventory {
            entries: vec![selected],
            ignored: 0,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &locks,
        &default_config(),
        &TestClock::new(future(15), std::time::Instant::now()),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;
    assert_eq!(result.deleted[0].mold_jobs, 1);
    assert_eq!(locks.attempts.lock().unwrap().as_slice(), &[one.clone()]);
    assert!(!one.exists());
    assert!(
        four.exists(),
        "evicting mold-jobs-1 must not substitute mold-jobs-4"
    );
}

#[tokio::test]
async fn idle_eviction_honors_worker_variant_lock_without_blocking_unlocked_sibling() {
    let temp = tempfile::tempdir().expect("temp");
    let id = "018f8b9a-0d70-7f0a-8000-000000000001";
    let project = temp.path().join(id);
    let one = project.join("mold-jobs-1");
    let four = project.join("mold-jobs-4");
    std::fs::create_dir_all(&one).expect("mold-jobs-1");
    std::fs::create_dir_all(&four).expect("mold-jobs-4");
    std::fs::write(one.join("artifact"), b"one").unwrap();
    std::fs::write(four.join("artifact"), b"four").unwrap();
    for variant in [&one, &four] {
        filetime::set_file_mtime(
            variant,
            filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH),
        )
        .unwrap();
    }

    // Acquire the lock by its worker production identity rather than through
    // the coordinator adapter. This independently proves idle GC contends on
    // `.warm-locks/<project-id>/mold-jobs-N.lock`.
    let worker_lock_dir = temp.path().join(".warm-locks").join(id);
    std::fs::create_dir_all(&worker_lock_dir).unwrap();
    let worker_lock_path = worker_lock_dir.join("mold-jobs-1.lock");
    let worker_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&worker_lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(worker_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "worker-compatible lock must be acquired"
    );

    let result = evict_idle_warm_bases(
        inventory_under(temp.path()).unwrap(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &SharedWarmBaseLock,
        &default_config(),
        &TestClock::new(future(15), std::time::Instant::now()),
        crate::context::CacheCleanupMode::Delete,
        temp.path(),
    )
    .await;

    assert_eq!(result.deleted.len(), 1);
    assert_eq!(result.deleted[0].mold_jobs, 4);
    assert_eq!(result.retained, vec![(id.into(), RetainReason::LockBusy)]);
    assert!(one.exists(), "held mold-jobs-1 variant must be retained");
    assert!(
        !four.exists(),
        "locked mold-jobs-1 must not block or substitute for mold-jobs-4"
    );
    assert_eq!(
        worker_lock_path,
        temp.path()
            .join(".warm-locks")
            .join(id)
            .join("mold-jobs-1.lock")
    );
}
