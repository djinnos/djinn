//! Deterministic filesystem tests for the startup volume-permission contract.
//!
//! Ownership cannot be changed without privilege, so the tests pin the required
//! gid to the process's own gid for the conforming cases and to a deliberately
//! foreign gid for the ownership-violation case. Mode violations are built with
//! real `chmod`s on a tempdir, so every assertion runs against the same code the
//! worker and warm Job execute at startup.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use djinn_agent_worker::volume_contract::{
    ContractMode, VolumeContract, VolumeContractError, VolumeRoot, current_gid, enforce_with,
    validate,
};
use tempfile::TempDir;

/// Conforming directory mode: setgid + group-write.
const DIR_CONFORMING: u32 = 0o2775;
/// Conforming file mode: group-write.
const FILE_CONFORMING: u32 = 0o664;

fn contract() -> VolumeContract {
    VolumeContract {
        required_gid: current_gid(),
        // Ownership is fixed by the harness, and the process obviously belongs
        // to its own gid; the membership arm has its own test below.
        require_process_membership: false,
        ..VolumeContract::default()
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A conforming root: `<root>/{sub/,sub/file,file}` all group-writable, dirs
/// setgid.
fn conforming_tree() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    let sub = root.join("sub");
    fs::create_dir(&sub).expect("mkdir sub");
    fs::write(root.join("file"), b"a").expect("write file");
    fs::write(sub.join("file"), b"b").expect("write nested file");
    chmod(&sub.join("file"), FILE_CONFORMING);
    chmod(&root.join("file"), FILE_CONFORMING);
    chmod(&sub, DIR_CONFORMING);
    chmod(root, DIR_CONFORMING);
    dir
}

fn root_of(dir: &TempDir) -> Vec<VolumeRoot> {
    vec![VolumeRoot::required("workspace", dir.path())]
}

#[test]
fn conforming_volume_passes_and_samples_the_subtree() {
    let dir = conforming_tree();
    let report = validate(&contract(), &root_of(&dir)).expect("conforming volume must pass");
    assert_eq!(report.roots_checked, 1);
    assert_eq!(report.roots_absent, 0);
    // Root + `sub` + two files.
    assert!(
        report.entries_sampled >= 4,
        "expected the bounded walk to reach the subtree, sampled {}",
        report.entries_sampled
    );
    assert!(!report.budget_exhausted);
}

#[test]
fn foreign_group_owner_fails_and_names_the_path() {
    let dir = conforming_tree();
    let foreign = current_gid().wrapping_add(4242);
    let contract = VolumeContract {
        required_gid: foreign,
        require_process_membership: false,
        ..VolumeContract::default()
    };
    let err = validate(&contract, &root_of(&dir)).expect_err("foreign gid must fail readiness");
    match &err {
        VolumeContractError::GroupOwner {
            path,
            observed_gid,
            required_gid,
            ..
        } => {
            assert_eq!(path, dir.path());
            assert_eq!(*observed_gid, current_gid());
            assert_eq!(*required_gid, foreign);
        }
        other => panic!("expected GroupOwner, got {other:?}"),
    }
    assert_eq!(err.kind(), "group_owner");
    let rendered = err.to_string();
    assert!(rendered.contains(&dir.path().display().to_string()));
    assert!(rendered.contains(&foreign.to_string()));
}

#[test]
fn missing_group_write_on_a_nested_file_fails() {
    let dir = conforming_tree();
    let victim = dir.path().join("sub").join("file");
    chmod(&victim, 0o644);
    let err = validate(&contract(), &root_of(&dir)).expect_err("644 file must fail readiness");
    match &err {
        VolumeContractError::GroupWrite {
            path,
            kind,
            observed_mode,
            ..
        } => {
            assert_eq!(path, &victim);
            assert_eq!(*kind, "file");
            assert_eq!(*observed_mode, 0o644);
        }
        other => panic!("expected GroupWrite, got {other:?}"),
    }
    assert_eq!(err.kind(), "group_write");
}

/// git loose objects/packfiles are `444` and cargo lays down registry sources
/// with the tarball's modes. Those are immutable by design and are replaced
/// through the (group-writable) directory, so they must not be read as a
/// permission regression.
#[test]
fn owner_read_only_files_are_not_a_group_write_violation() {
    let dir = conforming_tree();
    chmod(&dir.path().join("sub").join("file"), 0o444);
    validate(&contract(), &root_of(&dir)).expect("444 artifacts are immutable, not misowned");

    // …and `git init` copies its template hooks with the template's own mode,
    // so `.git/hooks/*.sample` is 755 in every fresh clone whatever the umask.
    chmod(&dir.path().join("sub").join("file"), 0o755);
    validate(&contract(), &root_of(&dir)).expect("executables are replaced, not written in place");

    // …but an owner-writable, non-executable file without group-write still
    // fails: that is the production 644 shape.
    chmod(&dir.path().join("sub").join("file"), 0o644);
    let err = validate(&contract(), &root_of(&dir)).expect_err("644 must still fail");
    assert_eq!(err.kind(), "group_write");
}

/// The umask is the other half of the contract: without it the worker creates
/// 755/644 into a conforming volume and breaks it from the inside.
#[test]
fn the_artifact_umask_makes_new_files_conforming() {
    use djinn_agent_worker::volume_contract::{ARTIFACT_UMASK, apply_artifact_umask};

    let previous = apply_artifact_umask();
    let dir = TempDir::new().expect("tempdir");
    chmod(dir.path(), DIR_CONFORMING);
    let nested = dir.path().join("created");
    fs::create_dir(&nested).expect("mkdir");
    fs::write(nested.join("artifact"), b"x").expect("write");

    // setgid on the parent propagates the group AND the setgid bit to the new
    // directory; the umask supplies the group-write bits.
    let report = validate(&contract(), &root_of(&dir))
        .expect("everything created under the artifact umask conforms");
    assert!(report.entries_sampled >= 3);
    assert_eq!(ARTIFACT_UMASK, 0o002);

    // SAFETY: restore the harness's umask; same contract as the setter.
    unsafe { libc::umask(previous) };
}

#[test]
fn missing_group_write_on_a_directory_fails() {
    let dir = conforming_tree();
    // 2755: setgid present, group-write gone — the production shape was 755.
    chmod(&dir.path().join("sub"), 0o2755);
    let err = validate(&contract(), &root_of(&dir)).expect_err("2755 dir must fail readiness");
    match &err {
        VolumeContractError::GroupWrite { kind, path, .. } => {
            assert_eq!(*kind, "directory");
            assert_eq!(path, &dir.path().join("sub"));
        }
        other => panic!("expected GroupWrite, got {other:?}"),
    }
}

#[test]
fn missing_setgid_on_a_directory_fails() {
    let dir = conforming_tree();
    chmod(&dir.path().join("sub"), 0o0775);
    let err = validate(&contract(), &root_of(&dir)).expect_err("non-setgid dir must fail");
    match &err {
        VolumeContractError::Setgid {
            path,
            observed_mode,
            ..
        } => {
            assert_eq!(path, &dir.path().join("sub"));
            assert_eq!(*observed_mode, 0o0775);
        }
        other => panic!("expected Setgid, got {other:?}"),
    }
    assert_eq!(err.kind(), "setgid");
}

/// The exact production near-miss: the volume ROOT was hand-fixed (which is
/// also what makes kubelet's `OnRootMismatch` skip its recursive pass) while
/// the 13,512-file subtree stayed 755/644. A root-only check would pass this.
#[test]
fn conforming_root_over_a_non_conforming_subtree_still_fails() {
    let dir = TempDir::new().expect("tempdir");
    let sub = dir.path().join("cargo-target");
    fs::create_dir(&sub).expect("mkdir");
    fs::write(sub.join("artifact"), b"x").expect("write");
    chmod(&sub.join("artifact"), 0o644);
    chmod(&sub, 0o755);
    chmod(dir.path(), DIR_CONFORMING);

    let err = validate(&contract(), &root_of(&dir)).expect_err("broken subtree must fail");
    assert_eq!(err.kind(), "group_write");
    assert_eq!(err.path(), Some(sub.as_path()));
}

#[test]
fn missing_required_root_fails_but_absent_optional_roots_are_skipped() {
    let dir = conforming_tree();
    let absent = dir.path().join("not-mounted");

    let err = validate(
        &contract(),
        &[VolumeRoot::required("cache", absent.clone())],
    )
    .expect_err("unmounted required root must fail");
    assert_eq!(err.kind(), "missing_root");
    assert_eq!(err.path(), Some(absent.as_path()));

    let report = validate(
        &contract(),
        &[
            VolumeRoot::required("workspace", dir.path()),
            VolumeRoot::optional("cargo-warm-base", absent),
        ],
    )
    .expect("absent optional root is not a violation");
    assert_eq!(report.roots_checked, 1);
    assert_eq!(report.roots_absent, 1);
}

#[test]
fn the_walk_is_bounded_by_depth_and_budget() {
    let dir = TempDir::new().expect("tempdir");
    // Conforming down to depth 3, broken at depth 5.
    let mut cursor = dir.path().to_path_buf();
    chmod(&cursor, DIR_CONFORMING);
    for level in 0..6 {
        cursor = cursor.join(format!("l{level}"));
        fs::create_dir(&cursor).expect("mkdir level");
        chmod(&cursor, if level >= 4 { 0o0755 } else { DIR_CONFORMING });
    }

    let bounded = VolumeContract {
        max_depth: 3,
        ..contract()
    };
    validate(&bounded, &root_of(&dir)).expect("violations below max_depth are out of scope");

    let deep = VolumeContract {
        max_depth: 8,
        ..contract()
    };
    validate(&deep, &root_of(&dir)).expect_err("a deeper walk sees the violation");

    // A budget of one stat can only cover the root itself.
    let starved = VolumeContract {
        max_depth: 8,
        stat_budget: 1,
        ..contract()
    };
    let report = validate(&starved, &root_of(&dir)).expect("budget stops the walk");
    assert!(report.budget_exhausted);
    assert_eq!(report.entries_sampled, 1);
}

#[test]
fn enforce_fails_closed_while_audit_and_off_do_not() {
    let dir = conforming_tree();
    chmod(&dir.path().join("sub"), 0o0775);
    let roots = root_of(&dir);
    let contract = contract();

    assert!(
        enforce_with("task-run", &contract, &roots, ContractMode::Enforce).is_err(),
        "enforce must fail readiness"
    );
    assert!(
        enforce_with("task-run", &contract, &roots, ContractMode::Audit).is_ok(),
        "audit is a loud break-glass, not a failure"
    );
    assert!(
        enforce_with("task-run", &contract, &roots, ContractMode::Off).is_ok(),
        "off short-circuits entirely"
    );
}

#[test]
fn conforming_volume_passes_through_enforce() {
    let dir = conforming_tree();
    assert!(
        enforce_with(
            "warm-graph",
            &contract(),
            &root_of(&dir),
            ContractMode::Enforce
        )
        .is_ok()
    );
}

#[test]
fn process_group_membership_is_asserted_before_any_stat() {
    let dir = conforming_tree();
    let unreachable_gid = u32::MAX - 7;
    let contract = VolumeContract {
        required_gid: unreachable_gid,
        require_process_membership: true,
        ..VolumeContract::default()
    };
    let err = validate(&contract, &root_of(&dir)).expect_err("non-member process must fail");
    assert_eq!(err.kind(), "process_group_membership");

    let member = VolumeContract {
        required_gid: current_gid(),
        require_process_membership: true,
        ..VolumeContract::default()
    };
    validate(&member, &root_of(&dir)).expect("own gid is always a member");
}

#[test]
fn the_check_never_mutates_the_volume() {
    let dir = conforming_tree();
    let sub = dir.path().join("sub");
    chmod(&sub, 0o0775);
    let before = fs::symlink_metadata(&sub)
        .expect("stat")
        .permissions()
        .mode();

    let _ = validate(&contract(), &root_of(&dir));
    let _ = enforce_with("task-run", &contract(), &root_of(&dir), ContractMode::Audit);

    let after = fs::symlink_metadata(&sub)
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(
        before, after,
        "startup validation must never repair the volume"
    );
}
