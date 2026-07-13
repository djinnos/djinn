use super::super::{LockOutcome, RetainReason, report_only_fingerprint_sweep};
use super::{Activity, ActivitySnapshot, Lock, Warm, WarmBaseEntry, WarmBaseInventory, snapshot};
use crate::context::CacheCleanupMode;
use std::path::PathBuf;

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
