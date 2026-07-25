//! Startup readiness for an unwritable `$HOME` (task 9jrg).
//!
//! The production failure: `qut0` moved the task-run Pod to uid/gid 1000 while
//! the image kept `/home/djinn` at `10001:10001 0775`. The pod therefore matched
//! the "other" class (`r-x`) on its own `$HOME` and could not create a single
//! entry there. The durable output stash resolves
//! `$HOME/.cache/djinn/output_stash` when `XDG_CACHE_HOME` is unset, so every
//! worker and planner session died on `create durable blobs: Permission denied`
//! — hours after start, inside the reply loop, with nothing pointing at
//! ownership.
//!
//! **Every test here runs against a directory owned by a uid that is NOT the
//! running process's.** A test that owns the directory it probes proves nothing:
//! the owner class carries `rwx` in `0775`, so the check passes for the wrong
//! reason. That is the exact gap that let this, `ej9c` and `4hfr` ship.
//!
//! Two arms, chosen by privilege, and one of them ALWAYS runs:
//!
//! 1. **Unprivileged** — probes a real directory the filesystem already has
//!    owned by another uid with no group/other write (`/` and friends). Runs on
//!    any CI runner. It can prove the failure but not the fix, because an
//!    unprivileged process cannot fabricate "owned by 10001, group 1000".
//! 2. **Privileged** — builds both shapes for real (`10001:10001 0775`, the
//!    outage, and `10001:1000 2775`, the image fix), then `fork`s a child that
//!    drops to the pod's uid/gid 1000 and runs the check and a genuine
//!    `create_dir_all` of the stash path from that identity. Needs root to
//!    `chown`, so it runs in a container but not on a GitHub runner.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use djinn_agent_worker::volume_contract::{
    ContractMode, VolumeContract, VolumeContractError, check_home_writable, current_uid,
    enforce_with, home_from_env,
};
use tempfile::TempDir;

/// Owner of `/home/djinn` in the image — the identity the server-side path runs
/// as, which is why the fix must not change it.
const IMAGE_HOME_UID: u32 = 10001;
/// `djinn_cgroup_launcher::child::ARTIFACT_GID`.
const ARTIFACT_GID: u32 = 1000;
/// uid/gid the task-run Pod runs as since `qut0`.
const POD_UID: u32 = 1000;
/// The shape that caused the outage.
const BROKEN_HOME_MODE: u32 = 0o775;
/// The shape the image now bakes: group-write for the artifact GID, setgid so
/// entries created here keep the shared group.
const FIXED_HOME_MODE: u32 = 0o2775;

/// The stash path the reply loop failed to create, relative to `$HOME`.
const STASH_RELATIVE: &str = ".cache/djinn/output_stash/blobs";

/// Contract that isolates the `$HOME` arm: no roots to walk, no membership
/// assertion, so the only thing left to fail is `$HOME`.
fn home_only_contract() -> VolumeContract {
    VolumeContract {
        require_process_membership: false,
        require_writable_home: true,
        ..VolumeContract::default()
    }
}

/// A directory that already exists, is owned by a uid other than ours, and
/// grants neither group nor other write — the production `$HOME` shape, borrowed
/// rather than fabricated so the unprivileged arm can run it.
fn foreign_owned_unwritable_dir() -> Option<PathBuf> {
    ["/", "/usr", "/etc", "/opt"]
        .iter()
        .map(Path::new)
        .find(|path| {
            let Ok(meta) = fs::metadata(path) else {
                return false;
            };
            meta.uid() != current_uid() && meta.mode() & 0o022 == 0
        })
        .map(Path::to_path_buf)
}

#[test]
// Test diagnostic: which arm ran decides what was proven, and a run that does
// not say so is indistinguishable from a silent skip.
#[allow(clippy::print_stderr)]
fn a_home_owned_by_another_uid_with_no_group_write_fails_readiness() {
    match foreign_owned_unwritable_dir() {
        Some(home) => {
            eprintln!(
                "9jrg home contract: unprivileged arm against {} (uid {})",
                home.display(),
                current_uid()
            );
            unprivileged_arm(&home)
        }
        None => {
            eprintln!("9jrg home contract: privileged arm (fabricating both real shapes)");
            assert_eq!(
                current_uid(),
                0,
                "no foreign-owned unwritable directory found and we are not root: \
                 this test would prove nothing, so it must not pass"
            );
            privileged_arm();
        }
    }
}

/// The check must reject a `$HOME` owned by someone else, and the rejection must
/// match what the filesystem actually does to the stash write.
fn unprivileged_arm(home: &Path) {
    let observed = fs::metadata(home).expect("stat foreign home");
    assert_ne!(
        observed.uid(),
        current_uid(),
        "arm precondition: the probed directory must be owned by another uid"
    );

    let error = check_home_writable(Some(home)).expect_err("unwritable $HOME must be rejected");
    match &error {
        VolumeContractError::HomeNotWritable {
            path,
            observed_uid,
            process_uid,
            ..
        } => {
            assert_eq!(path, home);
            assert_eq!(*observed_uid, observed.uid());
            assert_eq!(*process_uid, current_uid());
            assert_ne!(
                observed_uid, process_uid,
                "the error must record that the owner is a different identity"
            );
        }
        other => panic!("expected HomeNotWritable, got {other:?}"),
    }
    assert_eq!(error.kind(), "home_not_writable");
    assert_eq!(error.path(), Some(home));

    // The check's verdict must match the kernel's: the very write that killed
    // every session has to fail here for the same reason.
    let stash = home.join(STASH_RELATIVE);
    let denied = fs::create_dir_all(&stash)
        .expect_err("creating the durable stash under a foreign-owned home must fail");
    assert_eq!(
        denied.kind(),
        io::ErrorKind::PermissionDenied,
        "expected the production EACCES at {}, got {denied}",
        stash.display()
    );

    // ... and readiness must fail closed on it, before any volume is walked.
    let outcome = in_forked_child(|| {
        // SAFETY: single-threaded forked child; nothing else reads the env.
        unsafe { std::env::set_var("HOME", home) };
        match enforce_with(
            "task-run",
            &home_only_contract(),
            &[],
            ContractMode::Enforce,
        ) {
            Err(VolumeContractError::HomeNotWritable { .. }) => 0,
            Err(_) => 2,
            Ok(()) => 3,
        }
    });
    assert_eq!(
        outcome, 0,
        "enforce_with must fail closed with HomeNotWritable (exit {outcome})"
    );
}

/// With `chown` available, reproduce both real shapes and judge them from the
/// pod's own identity.
fn privileged_arm() {
    let dir = TempDir::new().expect("tempdir");

    // The outage shape: owned by the image's uid, group-owned by it too, 0775.
    let broken = dir.path().join("broken-home");
    fs::create_dir(&broken).expect("mkdir broken home");
    chown(&broken, IMAGE_HOME_UID, IMAGE_HOME_UID);
    chmod(&broken, BROKEN_HOME_MODE);

    // The fix: same owner (the server-side path still needs it), group-owned by
    // the artifact GID with setgid.
    let fixed = dir.path().join("fixed-home");
    fs::create_dir(&fixed).expect("mkdir fixed home");
    chown(&fixed, IMAGE_HOME_UID, ARTIFACT_GID);
    chmod(&fixed, FIXED_HOME_MODE);

    // The tempdir itself must be traversable by the dropped child.
    chmod(dir.path(), 0o755);

    let broken_outcome = in_forked_child_as(POD_UID, ARTIFACT_GID, || {
        let rejected = matches!(
            check_home_writable(Some(&broken)),
            Err(VolumeContractError::HomeNotWritable { .. })
        );
        let denied = fs::create_dir_all(broken.join(STASH_RELATIVE))
            .is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
        match (rejected, denied) {
            (true, true) => 0,
            (false, _) => 2,
            (_, false) => 3,
        }
    });
    assert_eq!(
        broken_outcome, 0,
        "uid {POD_UID} must be refused by {IMAGE_HOME_UID}:{IMAGE_HOME_UID} \
         {BROKEN_HOME_MODE:o} and must fail the real stash write (exit {broken_outcome})"
    );

    let fixed_outcome = in_forked_child_as(POD_UID, ARTIFACT_GID, || {
        let accepted = check_home_writable(Some(&fixed)).is_ok();
        let created = fs::create_dir_all(fixed.join(STASH_RELATIVE)).is_ok();
        match (accepted, created) {
            (true, true) => 0,
            (false, _) => 2,
            (_, false) => 3,
        }
    });
    assert_eq!(
        fixed_outcome, 0,
        "uid {POD_UID} in gid {ARTIFACT_GID} must be accepted by \
         {IMAGE_HOME_UID}:{ARTIFACT_GID} {FIXED_HOME_MODE:o} and must complete the \
         real stash write (exit {fixed_outcome})"
    );
}

#[test]
fn an_unset_home_is_a_violation_because_every_relative_path_lands_in_slash() {
    let error = check_home_writable(None).expect_err("unset $HOME must be rejected");
    assert!(matches!(error, VolumeContractError::HomeUnset));
    assert_eq!(error.kind(), "home_unset");
    assert_eq!(error.path(), None);
}

#[test]
fn a_home_that_is_not_a_directory_is_a_violation() {
    let dir = TempDir::new().expect("tempdir");
    let file = dir.path().join("not-a-dir");
    fs::write(&file, b"x").expect("write");
    let error = check_home_writable(Some(&file)).expect_err("a file is not a home");
    assert!(matches!(
        error,
        VolumeContractError::HomeNotADirectory { .. }
    ));
    assert_eq!(error.kind(), "home_not_a_directory");
}

#[test]
fn a_missing_home_is_reported_as_a_stat_failure_naming_the_path() {
    let dir = TempDir::new().expect("tempdir");
    let missing = dir.path().join("no-such-home");
    let error = check_home_writable(Some(&missing)).expect_err("absent $HOME must be rejected");
    assert_eq!(error.kind(), "stat_failed");
    assert_eq!(error.path(), Some(missing.as_path()));
}

#[test]
fn a_writable_home_passes_and_the_check_creates_nothing() {
    let dir = TempDir::new().expect("tempdir");
    check_home_writable(Some(dir.path())).expect("an owned home must pass");
    assert_eq!(
        fs::read_dir(dir.path()).expect("read_dir").count(),
        0,
        "the readiness probe must not write into $HOME"
    );
}

#[test]
fn opting_out_skips_the_home_arm_entirely() {
    let contract = VolumeContract {
        require_process_membership: false,
        require_writable_home: false,
        ..VolumeContract::default()
    };
    let home = foreign_owned_unwritable_dir();
    let outcome = in_forked_child(|| {
        match &home {
            // SAFETY: single-threaded forked child.
            Some(path) => unsafe { std::env::set_var("HOME", path) },
            // SAFETY: same.
            None => unsafe { std::env::remove_var("HOME") },
        }
        match enforce_with("task-run", &contract, &[], ContractMode::Enforce) {
            Ok(()) => 0,
            Err(_) => 2,
        }
    });
    assert_eq!(
        outcome, 0,
        "with require_writable_home=false the home arm must not run (exit {outcome})"
    );
}

#[test]
fn the_environment_resolver_rejects_an_empty_home() {
    let outcome = in_forked_child(|| {
        // SAFETY: single-threaded forked child.
        unsafe { std::env::set_var("HOME", "") };
        if home_from_env().is_some() {
            return 2;
        }
        // SAFETY: same.
        unsafe { std::env::set_var("HOME", "/tmp") };
        if home_from_env().as_deref() != Some(Path::new("/tmp")) {
            return 3;
        }
        0
    });
    assert_eq!(outcome, 0, "home_from_env mis-resolved (exit {outcome})");
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// `chown` must run BEFORE `chmod`: changing ownership clears setgid.
fn chown(path: &Path, uid: u32, gid: u32) {
    let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("path");
    // SAFETY: `lchown` reads the NUL-terminated path and returns a status code;
    // it never follows the final symlink and mutates no Rust-owned memory.
    let rc = unsafe { libc::lchown(raw.as_ptr(), uid, gid) };
    assert_eq!(
        rc,
        0,
        "lchown {} to {uid}:{gid}: {}",
        path.display(),
        io::Error::last_os_error()
    );
}

/// Run `body` in a forked child, returning its exit status. Used to keep `HOME`
/// mutations and uid drops out of the test process.
fn in_forked_child(body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the test process is single-threaded here; the child only touches
    // its own env and filesystem and leaves via `_exit`, so no parent atexit
    // handler or allocator lock is inherited into an inconsistent state.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
    if pid == 0 {
        let code = body();
        // SAFETY: terminate the child without unwinding through parent state.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    // SAFETY: reaping the child just forked above.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    libc::WEXITSTATUS(status)
}

/// [`in_forked_child`] with the child dropped to `uid`/`gid` first — the only
/// way to judge a foreign-owned directory as the identity that must write it.
fn in_forked_child_as(uid: u32, gid: u32, body: impl FnOnce() -> i32) -> i32 {
    in_forked_child(|| {
        // SAFETY: privilege drop in a single-threaded child; each call is checked
        // and the order (groups, gid, then uid) is required because dropping the
        // uid first would forfeit the privilege the other two need.
        let groups = [gid as libc::gid_t];
        let dropped = unsafe {
            libc::setgroups(1, groups.as_ptr()) == 0
                && libc::setgid(gid) == 0
                && libc::setuid(uid) == 0
        };
        if !dropped {
            return 4;
        }
        body()
    })
}
