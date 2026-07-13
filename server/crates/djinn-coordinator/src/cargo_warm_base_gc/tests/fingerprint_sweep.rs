use super::super::{LockOutcome, RetainReason, report_only_fingerprint_sweep};
use super::{Activity, ActivitySnapshot, Lock, Warm, WarmBaseEntry, WarmBaseInventory, snapshot};
use crate::context::CacheCleanupMode;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, PartialEq, Eq)]
enum FixtureEntry {
    Directory(PathBuf),
    File(PathBuf, Vec<u8>),
}

fn write_fixture_file(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture file has a parent")).expect("fixture parent");
    fs::write(path, contents).expect("fixture file");
}

fn fixture_snapshot(base: &Path) -> Vec<FixtureEntry> {
    fn collect(base: &Path, path: &Path, entries: &mut Vec<FixtureEntry>) {
        let relative = path.strip_prefix(base).expect("path stays under fixture");
        if path.is_dir() {
            entries.push(FixtureEntry::Directory(relative.to_path_buf()));
            for child in fs::read_dir(path).expect("read fixture directory") {
                collect(base, &child.expect("fixture entry").path(), entries);
            }
        } else {
            entries.push(FixtureEntry::File(
                relative.to_path_buf(),
                fs::read(path).expect("read fixture file"),
            ));
        }
    }

    let mut entries = Vec::new();
    collect(base, base, &mut entries);
    entries.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    entries
}

fn fixture_entry(base: &Path) -> WarmBaseEntry {
    WarmBaseEntry {
        project_id: base.file_name().unwrap().to_str().unwrap().into(),
        path: base.to_path_buf(),
        size_bytes: 0,
    }
}

fn comprehensive_fixture(base: &Path) -> u64 {
    let fingerprint_files = [
        (
            "debug/.fingerprint/old-dependency-aaaa/unit.json",
            b"old-json".as_slice(),
        ),
        (
            "debug/.fingerprint/fresh-dependency-bbbb/unit.json",
            b"fresh-json".as_slice(),
        ),
        (
            "release/.fingerprint/build-script-build-cccc/build.json",
            b"release".as_slice(),
        ),
        (
            "test/.fingerprint/proc-macro-dddd/proc.json",
            b"test".as_slice(),
        ),
        ("doc/.fingerprint/docs-eeee/doc.json", b"doc".as_slice()),
        (
            "x86_64-unknown-linux-gnu/debug/.fingerprint/nested-dependency-ffff/unit.json",
            b"nested".as_slice(),
        ),
    ];
    for (path, contents) in fingerprint_files {
        write_fixture_file(&base.join(path), contents);
    }
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let fresh = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    filetime::set_file_mtime(
        base.join("debug/.fingerprint/old-dependency-aaaa/unit.json"),
        filetime::FileTime::from_system_time(old),
    )
    .expect("old mtime");
    filetime::set_file_mtime(
        base.join("debug/.fingerprint/fresh-dependency-bbbb/unit.json"),
        filetime::FileTime::from_system_time(fresh),
    )
    .expect("fresh mtime");

    // Plausible deletion-adjacent Cargo shapes must all survive report-only
    // sweeps, including ambiguous names and otherwise-empty directories.
    for (path, contents) in [
        ("Cargo.lock", b"cargo metadata".as_slice()),
        (".rustc_info.json", b"rustc metadata".as_slice()),
        ("unknown-top-level", b"unknown".as_slice()),
        (
            "debug/deps/libold_dependency-aaaa.rlib",
            b"dependency artifact".as_slice(),
        ),
        (
            "debug/deps/libproc_macro-dddd.so",
            b"proc macro artifact".as_slice(),
        ),
        (
            "release/build/build-script-build-cccc/output",
            b"build output".as_slice(),
        ),
        (
            "test/deps/proc_macro-dddd.dll",
            b"proc macro alternate".as_slice(),
        ),
        (
            "debug/incremental/old-dependency-aaaa-ambiguous-a/state",
            b"incremental a".as_slice(),
        ),
        (
            "debug/incremental/old-dependency-aaaa-ambiguous-b/state",
            b"incremental b".as_slice(),
        ),
        (
            "x86_64-unknown-linux-gnu/debug/deps/libnested-ffff.rmeta",
            b"nested dep".as_slice(),
        ),
    ] {
        write_fixture_file(&base.join(path), contents);
    }
    for path in [
        "debug/otherwise-empty",
        "release/otherwise-empty",
        "test/otherwise-empty",
        "doc/otherwise-empty",
        "x86_64-unknown-linux-gnu/debug/otherwise-empty",
    ] {
        fs::create_dir_all(base.join(path)).expect("empty fixture directory");
    }

    fingerprint_files
        .iter()
        .map(|(_, contents)| contents.len() as u64)
        .sum()
}

#[tokio::test]
async fn dry_run_and_delete_report_identical_candidates_and_preserve_artifacts() {
    let temp = tempfile::tempdir().expect("temp");
    let id = "018f8b9a-0d70-7f0a-8000-000000000001";
    let base = temp.path().join(id);
    std::fs::create_dir(&base).expect("dir");

    let unit_path = base.join("debug/.fingerprint/libcrate-abc123");
    std::fs::create_dir_all(&unit_path).unwrap();
    std::fs::write(unit_path.join("lib-libcrate.json"), b"{}").unwrap();

    let entry = WarmBaseEntry {
        project_id: id.into(),
        path: base.clone(),
        size_bytes: 0,
    };
    let inventory = WarmBaseInventory {
        entries: vec![entry],
        ignored: 0,
    };

    let dry_run = report_only_fingerprint_sweep(
        inventory.clone(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::DryRun,
    )
    .await;

    let delete = report_only_fingerprint_sweep(
        inventory,
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::Delete,
    )
    .await;

    assert_eq!(dry_run.candidate_count, delete.candidate_count);
    assert_eq!(dry_run.projected_bytes, delete.projected_bytes);
    assert_eq!(dry_run.candidate_count, 1);
    assert_eq!(dry_run.projected_bytes, 2);

    // Artifacts preserved in both modes.
    assert!(unit_path.join("lib-libcrate.json").exists());

    assert!(dry_run.error_bases.is_empty());
    assert!(delete.error_bases.is_empty());
}

#[tokio::test]
async fn active_task_run_retains_base() {
    let entry = WarmBaseEntry {
        project_id: "018f8b9a-0d70-7f0a-8000-000000000001".into(),
        path: PathBuf::from("base"),
        size_bytes: 0,
    };
    let inventory = WarmBaseInventory {
        entries: vec![entry],
        ignored: 0,
    };
    let activity = Activity(Ok(ActivitySnapshot {
        has_active_task_run: true,
        ..snapshot()
    }));

    let report = report_only_fingerprint_sweep(
        inventory,
        &activity,
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::DryRun,
    )
    .await;

    assert_eq!(report.candidate_count, 0);
    assert_eq!(report.projected_bytes, 0);
    assert_eq!(report.retained.len(), 1);
    assert_eq!(report.retained[0].1, RetainReason::ActiveTaskRun);
}

#[tokio::test]
async fn guard_error_retains_base_and_reports_error() {
    let entry = WarmBaseEntry {
        project_id: "018f8b9a-0d70-7f0a-8000-000000000001".into(),
        path: PathBuf::from("base"),
        size_bytes: 0,
    };
    let inventory = WarmBaseInventory {
        entries: vec![entry],
        ignored: 0,
    };

    let report = report_only_fingerprint_sweep(
        inventory,
        &Activity(Err("db down".into())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::DryRun,
    )
    .await;

    assert_eq!(report.candidate_count, 0);
    assert_eq!(report.projected_bytes, 0);
    assert_eq!(report.retained.len(), 1);
    assert_eq!(report.retained[0].1, RetainReason::ActivityError);
}

#[tokio::test]
async fn comprehensive_fixture_has_report_only_mode_parity_and_byte_preservation() {
    let temp = tempfile::tempdir().expect("temp");
    let base = temp.path().join("018f8b9a-0d70-7f0a-8000-000000000001");
    fs::create_dir(&base).expect("base");
    let expected_projected_bytes = comprehensive_fixture(&base);
    let before = fixture_snapshot(&base);
    let inventory = WarmBaseInventory {
        entries: vec![fixture_entry(&base)],
        ignored: 0,
    };

    let dry_run = report_only_fingerprint_sweep(
        inventory.clone(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::DryRun,
    )
    .await;
    assert_eq!(fixture_snapshot(&base), before);

    let delete = report_only_fingerprint_sweep(
        inventory,
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::Delete,
    )
    .await;

    assert_eq!(dry_run.candidate_count, 6);
    assert_eq!(dry_run.projected_bytes, expected_projected_bytes);
    assert_eq!(dry_run.reclaimed_bytes, 0);
    assert_eq!(delete.candidate_count, dry_run.candidate_count);
    assert_eq!(delete.projected_bytes, dry_run.projected_bytes);
    assert_eq!(delete.reclaimed_bytes, 0);
    assert!(dry_run.retained.is_empty() && dry_run.error_bases.is_empty());
    assert!(delete.retained.is_empty() && delete.error_bases.is_empty());
    // Delete mode is safety-disabled report-only: this byte/path snapshot
    // catches artifact removal and forbidden empty-directory cleanup.
    assert_eq!(fixture_snapshot(&base), before);
}

#[tokio::test]
async fn lock_db_kubernetes_and_traversal_failures_preserve_fixture_and_close_candidates() {
    let temp = tempfile::tempdir().expect("temp");
    let base = temp.path().join("018f8b9a-0d70-7f0a-8000-000000000001");
    fs::create_dir(&base).expect("base");
    comprehensive_fixture(&base);
    let before = fixture_snapshot(&base);
    let inventory = WarmBaseInventory {
        entries: vec![fixture_entry(&base)],
        ignored: 0,
    };

    let lock_busy = report_only_fingerprint_sweep(
        inventory.clone(),
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Busy),
        CacheCleanupMode::Delete,
    )
    .await;
    assert_eq!(lock_busy.candidate_count, 0);
    assert_eq!(lock_busy.retained[0].1, RetainReason::LockBusy);

    let db_failure = report_only_fingerprint_sweep(
        inventory.clone(),
        &Activity(Err("injected database failure".into())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::Delete,
    )
    .await;
    assert_eq!(db_failure.candidate_count, 0);
    assert_eq!(db_failure.retained[0].1, RetainReason::ActivityError);

    let kubernetes_failure = report_only_fingerprint_sweep(
        inventory,
        &Activity(Ok(snapshot())),
        &Warm(Err("injected Kubernetes failure".into())),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::Delete,
    )
    .await;
    assert_eq!(kubernetes_failure.candidate_count, 0);
    assert_eq!(kubernetes_failure.retained[0].1, RetainReason::WarmJobError);

    let traversal_base = temp.path().join("018f8b9a-0d70-7f0a-8000-000000000002");
    fs::create_dir(&traversal_base).expect("traversal base");
    fs::create_dir_all(traversal_base.join("debug/.fingerprint/empty-unit"))
        .expect("empty fingerprint unit");
    let traversal_before = fixture_snapshot(&traversal_base);
    let traversal = report_only_fingerprint_sweep(
        WarmBaseInventory {
            entries: vec![fixture_entry(&traversal_base)],
            ignored: 0,
        },
        &Activity(Ok(snapshot())),
        &Warm(Ok(false)),
        &Lock(LockOutcome::Available),
        CacheCleanupMode::Delete,
    )
    .await;
    assert_eq!(traversal.candidate_count, 0);
    assert_eq!(traversal.projected_bytes, 0);
    assert_eq!(traversal.error_bases.len(), 1);
    assert_eq!(fixture_snapshot(&base), before);
    assert_eq!(fixture_snapshot(&traversal_base), traversal_before);
}
