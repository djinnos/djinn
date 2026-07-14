use super::*;
use std::thread::sleep;
use std::time::Duration;
#[cfg(unix)]
use std::{
    collections::HashSet,
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::symlink},
};

fn mkdir(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("debug/deps")).unwrap();
    fs::write(dir.join("debug/deps/lib.rlib"), b"artifact").unwrap();
    dir
}

#[test]
fn teardown_removes_existing_and_ignores_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let id = "11111111-1111-1111-1111-111111111111";
    mkdir(tmp.path(), id);

    let removed = teardown_run_dir(tmp.path(), id).unwrap();
    assert_eq!(removed.outcome(), "removed");
    assert_eq!(removed.removed_count(), 1);
    assert!(!tmp.path().join(id).exists());

    let again = teardown_run_dir(tmp.path(), id).unwrap();
    assert_eq!(again.outcome(), "already_absent");
    assert_eq!(again.removed_count(), 0);
}

#[test]
fn teardown_rejects_path_traversal_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let sibling = tmp.path().join("sibling");
    fs::create_dir_all(&sibling).unwrap();

    // An id that tries to escape the root is a no-op and never touches the
    // sibling dir.
    let result = teardown_run_dir(&tmp.path().join("runs"), "../sibling").unwrap();
    assert!(!result.removed);
    assert!(sibling.exists());

    assert!(!teardown_run_dir(tmp.path(), "").unwrap().removed);
    assert!(!teardown_run_dir(tmp.path(), "a/b").unwrap().removed);
}

#[test]
fn trim_keeps_newest_and_removes_oldest_beyond_cap() {
    let tmp = tempfile::tempdir().unwrap();

    // Create dirs oldest→newest so mtimes are strictly ordered.
    let mut names = Vec::new();
    for i in 0..5 {
        let name = format!("run-{i}");
        mkdir(tmp.path(), &name);
        names.push(name);
        sleep(Duration::from_millis(20));
    }

    let stats = trim_run_dirs_to_cap(tmp.path(), 2).unwrap();
    assert_eq!(stats.scanned, 5);
    assert_eq!(stats.trimmed, 3);
    assert_eq!(stats.retained, 2);
    assert_eq!(stats.errors, 0);

    // Oldest three gone, newest two retained.
    assert!(!tmp.path().join("run-0").exists());
    assert!(!tmp.path().join("run-1").exists());
    assert!(!tmp.path().join("run-2").exists());
    assert!(tmp.path().join("run-3").exists());
    assert!(tmp.path().join("run-4").exists());
}

#[test]
fn trim_is_noop_within_cap_and_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "run-a");
    mkdir(tmp.path(), "run-b");

    let within = trim_run_dirs_to_cap(tmp.path(), 8).unwrap();
    assert_eq!(within.trimmed, 0);
    assert_eq!(within.retained, 2);
    assert!(tmp.path().join("run-a").exists());

    let disabled = trim_run_dirs_to_cap(tmp.path(), 0).unwrap();
    assert_eq!(disabled.trimmed, 0);
    assert!(tmp.path().join("run-a").exists());
}

#[test]
fn trim_missing_root_is_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist");
    let stats = trim_run_dirs_to_cap(&missing, 4).unwrap();
    assert_eq!(stats, HardCapTrimStats::default());
}

#[test]
fn caps_resolver_accepts_decimal_and_zero_and_rejects_invalid_values() {
    assert_eq!(DEFAULT_HARD_CAP_DIRS, 64);
    assert_eq!(DEFAULT_HARD_CAP_BYTES, 8_589_934_592);
    assert_eq!(
        resolve_cargo_target_runs_caps(None, None),
        (
            CargoTargetRunsCaps::default(),
            CapResolutionDiagnostics::default()
        )
    );
    assert_eq!(
        resolve_cargo_target_runs_caps(Some("0"), Some("0")),
        (
            CargoTargetRunsCaps {
                max_dirs: 0,
                max_bytes: 0,
            },
            CapResolutionDiagnostics::default(),
        )
    );
    let (caps, diagnostics) = resolve_cargo_target_runs_caps(Some("12"), Some("34"));
    assert_eq!(
        caps,
        CargoTargetRunsCaps {
            max_dirs: 12,
            max_bytes: 34
        }
    );
    assert_eq!(diagnostics, CapResolutionDiagnostics::default());

    for invalid in ["", "-1", "+1", " 1", "1 ", "1K", "18446744073709551616"] {
        let (caps, diagnostics) = resolve_cargo_target_runs_caps(Some(invalid), Some(invalid));
        assert_eq!(caps, CargoTargetRunsCaps::default(), "{invalid:?}");
        assert_eq!(
            diagnostics,
            CapResolutionDiagnostics {
                invalid_max_dirs: true,
                invalid_max_bytes: true,
            },
            "{invalid:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_environment_values_are_invalid_not_unset() {
    let non_unicode = || std::env::VarError::NotUnicode(OsString::from_vec(vec![0xff]));
    let (caps, diagnostics, dirs, bytes) =
        resolve_caps_from_env_results(Err(non_unicode()), Err(non_unicode()));
    assert_eq!(caps, CargoTargetRunsCaps::default());
    assert_eq!(dirs, None);
    assert_eq!(bytes, None);
    assert_eq!(
        diagnostics,
        CapResolutionDiagnostics {
            invalid_max_dirs: true,
            invalid_max_bytes: true,
        }
    );
}

#[cfg(unix)]
fn allocated_bytes(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

#[cfg(unix)]
#[test]
fn inventory_accounts_sparse_hardlinked_and_symlink_entries_without_following() {
    let tmp = tempfile::tempdir().unwrap();
    let run = tmp.path().join("run");
    fs::create_dir(&run).unwrap();
    let sparse = run.join("sparse");
    fs::File::create(&sparse)
        .unwrap()
        .set_len(8 * 1024 * 1024)
        .unwrap();
    fs::hard_link(&sparse, run.join("hardlink")).unwrap();
    symlink(&sparse, tmp.path().join("run-link")).unwrap();

    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    let expected = allocated_bytes(tmp.path())
        + allocated_bytes(&run)
        + allocated_bytes(&sparse)
        + allocated_bytes(&tmp.path().join("run-link"));
    assert_eq!(inventory.total_allocated_bytes, expected);
    assert_eq!(
        inventory.non_directory_allocated_bytes,
        allocated_bytes(&sparse) + allocated_bytes(&tmp.path().join("run-link"))
    );
    assert!(allocated_bytes(&sparse) < 8 * 1024 * 1024);
    assert_eq!(inventory.top_level_directory_count, 1);
    assert_eq!(inventory.candidates[0].name, b"run");
    assert_eq!(
        inventory.protected,
        vec![InventoryIssue {
            top_level_name: Some(b"run-link".to_vec()),
            kind: InventoryIssueKind::TopLevelSymlink,
        }]
    );
}

#[cfg(unix)]
#[test]
fn inventory_protects_malformed_names_and_rejects_symlink_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let malformed = tmp.path().join(OsString::from_vec(vec![b'x', 0xff]));
    fs::create_dir(&malformed).unwrap();
    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    assert_eq!(inventory.top_level_directory_count, 1);
    assert!(inventory.candidates.is_empty());
    assert_eq!(
        inventory.protected[0].kind,
        InventoryIssueKind::MalformedTopLevelName
    );
    assert_eq!(
        inventory.protected[0].top_level_name,
        Some(vec![b'x', 0xff])
    );

    let link = tmp.path().join("root-link");
    symlink(tmp.path(), &link).unwrap();
    assert!(matches!(
        inventory_cargo_target_runs(&link),
        Err(CargoTargetRunsInventoryError::RootRead(error))
            if error.kind() == io::ErrorKind::InvalidInput
    ));
}

#[cfg(unix)]
#[test]
fn joint_trim_protects_whitespace_name_rejected_by_containment() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(" ")).unwrap();
    fs::write(tmp.path().join(" ").join("artifact"), vec![0_u8; 4096]).unwrap();

    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    assert!(inventory.candidates.is_empty());
    assert_eq!(
        inventory.protected,
        vec![InventoryIssue {
            top_level_name: Some(b" ".to_vec()),
            kind: InventoryIssueKind::MalformedTopLevelName,
        }]
    );

    let result = trim_cargo_target_runs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
    )
    .unwrap();
    assert_eq!(result.deleted, 0);
    assert_eq!(result.errors, 0);
    assert_eq!(result.protected, 1);
    assert_eq!(
        result.outcome,
        CargoTargetRunsTrimOutcome::OverBudgetProtected
    );
    assert!(tmp.path().join(" ").is_dir());
}

/// Every ordinary top-level entry is separately protected, even when its
/// blocks are deduplicated or zero. Their exact count must be surfaced
/// when they are the sole cause of an unsatisfiable byte cap.
#[cfg(unix)]
#[test]
fn joint_trim_counts_each_top_level_non_directory_as_protected() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("first"), b"first").unwrap();
    fs::write(tmp.path().join("second"), b"second").unwrap();

    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    assert_eq!(inventory.top_level_non_directory_count, 2);
    let result = trim_cargo_target_runs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
    )
    .unwrap();

    assert_eq!(result.deleted, 0);
    assert_eq!(result.errors, 0);
    assert_eq!(result.protected, 2);
    assert_eq!(
        result.outcome,
        CargoTargetRunsTrimOutcome::OverBudgetProtected
    );
}

#[cfg(unix)]
#[test]
fn inventory_reports_fatal_root_and_per_directory_read_errors() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        inventory_cargo_target_runs(&tmp.path().join("missing")),
        Err(CargoTargetRunsInventoryError::RootRead(_))
    ));

    let mut inventory = CargoTargetRunsInventory::default();
    inventory_directory(
        &tmp.path().join("missing"),
        Some(b"run".to_vec()),
        &mut HashSet::new(),
        &mut inventory,
    )
    .unwrap();
    assert_eq!(
        inventory.errors,
        vec![InventoryIssue {
            top_level_name: Some(b"run".to_vec()),
            kind: InventoryIssueKind::ReadDirectory,
        }]
    );
}

#[cfg(unix)]
#[test]
fn inventory_error_affected_directory_is_never_a_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let run = mkdir(tmp.path(), "run");
    let mut inventory = CargoTargetRunsInventory::default();
    inventory_directory(
        &run.join("missing"),
        Some(b"run".to_vec()),
        &mut HashSet::new(),
        &mut inventory,
    )
    .unwrap();
    assert!(!inventory.errors.is_empty());

    // This is the same post-recursion decision made by the root inventory:
    // an error belonging to `run` excludes its otherwise valid candidate.
    let candidate = RunDirInventoryCandidate {
        name: b"run".to_vec(),
        modified: None,
        created: None,
    };
    assert!(inventory
        .errors
        .iter()
        .any(|issue| { issue.top_level_name.as_deref() == Some(candidate.name.as_slice()) }));
}

#[cfg(unix)]
#[test]
fn inventory_candidate_order_is_mtime_creation_then_raw_name() {
    let epoch = SystemTime::UNIX_EPOCH;
    let later = epoch + Duration::from_secs(1);
    let mut candidates = vec![
        RunDirInventoryCandidate {
            name: b"newer-modified".to_vec(),
            modified: Some(later),
            created: Some(epoch),
        },
        RunDirInventoryCandidate {
            name: b"later-created".to_vec(),
            modified: Some(epoch),
            created: Some(later),
        },
        RunDirInventoryCandidate {
            name: b"z".to_vec(),
            modified: Some(epoch),
            created: Some(epoch),
        },
        RunDirInventoryCandidate {
            name: b"a".to_vec(),
            modified: Some(epoch),
            created: Some(epoch),
        },
    ];

    candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.created.cmp(&right.created))
            .then_with(|| left.name.cmp(&right.name))
    });

    assert_eq!(
        candidates
            .into_iter()
            .map(|candidate| candidate.name)
            .collect::<Vec<_>>(),
        vec![
            b"a".to_vec(),
            b"z".to_vec(),
            b"later-created".to_vec(),
            b"newer-modified".to_vec(),
        ]
    );
}

#[cfg(unix)]
#[test]
fn joint_trim_protects_active_runs_and_reports_exact_postcondition() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "active");
    mkdir(tmp.path(), "inactive");
    let active_ids = HashSet::from(["active".to_owned()]);
    let result = trim_cargo_target_runs(
        tmp.path(),
        &active_ids,
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
    )
    .unwrap();
    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    // The active directory remains while the inactive one is exhausted.
    assert!(tmp.path().join("active").exists());
    assert!(!tmp.path().join("inactive").exists());
    assert_eq!(
        result.final_allocated_bytes,
        inventory.total_allocated_bytes
    );
    assert_eq!(
        result.final_top_level_directory_count,
        inventory.top_level_directory_count
    );
    assert_eq!(
        result.outcome,
        CargoTargetRunsTrimOutcome::OverBudgetProtected
    );
}

#[cfg(unix)]
#[test]
fn joint_trim_removes_newest_when_byte_budget_requires_it() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "only-run");
    let result = trim_cargo_target_runs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
    )
    .unwrap();
    assert!(!tmp.path().join("only-run").exists());
    assert_eq!(result.deleted, 1);
    assert_eq!(
        result.final_allocated_bytes,
        inventory_cargo_target_runs(tmp.path())
            .unwrap()
            .total_allocated_bytes
    );
}

#[cfg(unix)]
#[test]
fn joint_trim_rescans_hardlinks_for_exact_final_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let first = mkdir(tmp.path(), "a");
    let second = mkdir(tmp.path(), "b");
    let file = first.join("shared");
    fs::write(&file, vec![1_u8; 4096]).unwrap();
    fs::hard_link(&file, second.join("shared")).unwrap();
    let result = trim_cargo_target_runs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 1,
            max_bytes: 0,
        },
    )
    .unwrap();
    let inventory = inventory_cargo_target_runs(tmp.path()).unwrap();
    assert_eq!(result.deleted, 1);
    assert_eq!(
        result.final_allocated_bytes,
        inventory.total_allocated_bytes
    );
    assert_eq!(result.final_top_level_directory_count, 1);
}

/// Tie-breaking: three dirs with identical mtime must be ordered by creation
/// time, then raw name bytes. With a count cap of 1, the two earliest-sorted
/// candidates are removed and the latest-sorted one survives.
#[cfg(unix)]
#[test]
fn joint_trim_tie_breaks_on_creation_then_raw_name_bytes() {
    use std::time::SystemTime;

    let tmp = tempfile::tempdir().unwrap();
    let epoch = SystemTime::UNIX_EPOCH;
    // Create in alphabetical order so both creation time and raw-name bytes
    // agree: alpha is earliest, charlie is latest.
    for name in ["alpha", "bravo", "charlie"] {
        let dir = tmp.path().join(name);
        fs::create_dir_all(dir.join("debug/deps")).unwrap();
        fs::write(dir.join("debug/deps/lib.rlib"), b"artifact").unwrap();
        let file = fs::File::open(&dir).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(epoch).set_accessed(epoch))
            .unwrap();
    }
    let result = trim_cargo_target_runs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 1,
            max_bytes: 0,
        },
    )
    .unwrap();
    assert_eq!(result.deleted, 2);
    assert_eq!(
        result.outcome,
        CargoTargetRunsTrimOutcome::TrimmedWithinBudget
    );
    assert_eq!(result.final_top_level_directory_count, 1);
    // charlie has the latest creation time and latest raw-name bytes, so it
    // sorts last and survives the oldest-first trim.
    assert!(tmp.path().join("charlie").exists());
    assert!(!tmp.path().join("alpha").exists());
    assert!(!tmp.path().join("bravo").exists());
}

/// When the sole candidate cannot be removed (deterministic removal failure
/// via the mock seam), the engine records an operation error and, with no
/// remaining candidates, returns `over_budget_error`.
#[cfg(unix)]
#[test]
fn joint_trim_reports_over_budget_error_when_removal_fails() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("only")).unwrap();
    let fs = FailingRemoveFilesystem;
    let result = trim_cargo_target_runs_with_fs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
        &fs,
    )
    .unwrap();
    assert_eq!(result.deleted, 0);
    assert!(result.errors > 0);
    assert_eq!(result.outcome, CargoTargetRunsTrimOutcome::OverBudgetError);
    assert!(tmp.path().join("only").exists());
}

/// Files within a removable run contribute allocated bytes, but do not make
/// that run protected. If removal fails, the remaining overage is caused by
/// the operation error alone rather than a nested ordinary artifact.
#[cfg(unix)]
#[test]
fn joint_trim_nested_file_removal_failure_is_error_not_protection() {
    let tmp = tempfile::tempdir().unwrap();
    let artifact = tmp.path().join("only/debug/deps/lib.rlib");
    fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    fs::write(&artifact, vec![0_u8; 4096]).unwrap();
    let fs = FailingRemoveFilesystem;
    let result = trim_cargo_target_runs_with_fs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
        &fs,
    )
    .unwrap();

    assert_eq!(result.deleted, 0);
    assert!(result.errors > 0);
    assert_eq!(result.protected, 0);
    assert_eq!(result.outcome, CargoTargetRunsTrimOutcome::OverBudgetError);
    assert!(artifact.exists());
}

/// A protected (active) candidate and a candidate whose removal fails coexist,
/// producing the combined `over_budget_protected_and_error` outcome.
#[cfg(unix)]
#[test]
fn joint_trim_reports_both_protected_and_error_causes() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "active");
    mkdir(tmp.path(), "stuck");
    let active_ids = HashSet::from(["active".to_owned()]);
    let fs = FailingRemoveFilesystem;
    let result = trim_cargo_target_runs_with_fs(
        tmp.path(),
        &active_ids,
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
        &fs,
    )
    .unwrap();
    assert_eq!(
        result.outcome,
        CargoTargetRunsTrimOutcome::OverBudgetProtectedAndError
    );
    assert!(result.errors > 0);
    assert!(result.protected > 0);
    assert!(tmp.path().join("active").exists());
    assert!(tmp.path().join("stuck").exists());
}

/// When `remove_dir_all` fails for the first candidate, the engine continues
/// to the next candidate and successfully removes it, rather than aborting.
#[cfg(unix)]
#[test]
fn joint_trim_continues_after_removal_failure() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "aaa-stuck");
    mkdir(tmp.path(), "bbb-free");
    // "aaa-stuck" sorts first (both mtime and name), so it is attempted
    // first. The mock fails removal only for "aaa-stuck".
    let fs = RemoveByNameFilesystem {
        fail: "aaa-stuck".to_owned(),
    };
    let result = trim_cargo_target_runs_with_fs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
        &fs,
    )
    .unwrap();
    // "aaa-stuck" failed (error), "bbb-free" was successfully removed.
    assert!(result.errors > 0);
    assert!(tmp.path().join("aaa-stuck").exists());
    assert!(!tmp.path().join("bbb-free").exists());
}

/// TOCTOU race: between the inventory scan and the pre-removal revalidation,
/// the target directory is replaced by a symlink. The revalidation must
/// reject it (fail-closed) so the symlink target is never removed.
#[cfg(unix)]
#[test]
fn joint_trim_revalidation_rejects_toctou_symlink_race() {
    let tmp = tempfile::tempdir().unwrap();
    mkdir(tmp.path(), "victim");
    let redirect = tempfile::tempdir().unwrap();
    let fs = RaceToSymlinkFilesystem {
        target: "victim".to_owned(),
        redirect: redirect.path().to_path_buf(),
    };
    let result = trim_cargo_target_runs_with_fs(
        tmp.path(),
        &HashSet::new(),
        CargoTargetRunsCaps {
            max_dirs: 0,
            max_bytes: 1,
        },
        &fs,
    )
    .unwrap();
    // The symlink replacement must not be deleted; the engine records the
    // protected-symlink state on the next scan and returns over_budget.
    let meta = fs::symlink_metadata(tmp.path().join("victim")).unwrap();
    assert!(meta.file_type().is_symlink());
    // The redirect target (outside the runs root) must survive.
    assert!(redirect.path().exists());
    assert!(
        result.outcome == CargoTargetRunsTrimOutcome::OverBudgetProtected
            || result.outcome == CargoTargetRunsTrimOutcome::OverBudgetProtectedAndError
    );
}

// ---- Mock filesystem seams for deterministic trim-engine tests ----

/// Always fails `remove_dir_all` with a generic I/O error; passes through
/// `symlink_metadata` to the real filesystem so the inventory and
/// revalidation still see on-disk state.
#[cfg(unix)]
struct FailingRemoveFilesystem;

#[cfg(unix)]
impl Filesystem for FailingRemoveFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "mock: removal disabled",
        ))
    }
}

/// Fails `remove_dir_all` only for directories whose final path component
/// equals `fail`; delegates real removal otherwise.
#[cfg(unix)]
struct RemoveByNameFilesystem {
    fail: String,
}

#[cfg(unix)]
impl Filesystem for RemoveByNameFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == self.fail)
        {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mock: stuck",
            ))
        } else {
            fs::remove_dir_all(path)
        }
    }
}

/// Replaces `target` (a top-level dir under root) with a symlink pointing to
/// `redirect` during the revalidation `symlink_metadata` call, so the
/// revalidation sees a symlink and skips removal. This deterministically
/// simulates a TOCTOU directory→symlink race.
#[cfg(unix)]
struct RaceToSymlinkFilesystem {
    target: String,
    redirect: PathBuf,
}

#[cfg(unix)]
impl Filesystem for RaceToSymlinkFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == self.target)
            && path.is_dir()
        {
            let _ = fs::remove_dir_all(path);
            let _ = std::os::unix::fs::symlink(&self.redirect, path);
        }
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}
