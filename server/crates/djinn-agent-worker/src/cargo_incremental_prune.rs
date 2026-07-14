//! Safe removal of Cargo's disposable warm-base incremental state.
//!
//! This module deliberately derives its deletion root from
//! [`crate::cargo_target_seed::warm_base_dir`].  It is not a general purpose
//! directory remover: callers cannot supply an arbitrary production path.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::cargo_target_seed::{WARM_BASE_ROOT, warm_base_dir};

const CARGO_TARGET_DIR: &str = "CARGO_TARGET_DIR";
const LOCK_DIRECTORY: &str = ".warm-locks";

/// The bounded reason used by orchestration and telemetry for a failed prune.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PruneErrorKind {
    Scan,
    Permission,
    Remove,
    TargetMismatch,
    InvalidProjectId,
    Symlink,
    LockOpen,
    LockProbe,
    LockAcquire,
}

impl PruneErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Permission => "permission",
            Self::Remove => "remove",
            Self::TargetMismatch => "target_mismatch",
            Self::InvalidProjectId => "invalid_project_id",
            Self::Symlink => "symlink",
            Self::LockOpen => "lock_open",
            Self::LockProbe => "lock_probe",
            Self::LockAcquire => "lock_acquire",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn tree() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let base = root.join("project");
        fs::create_dir_all(base.join("debug/incremental/nested")).unwrap();
        (temp, root, base)
    }

    #[test]
    fn rejects_non_component_project_ids() {
        for id in ["", ".", "..", "a/b", "/absolute"] {
            assert_eq!(
                validate_project_id(id).unwrap_err().kind,
                PruneErrorKind::InvalidProjectId
            );
        }
        validate_project_id("project-1").unwrap();
    }

    #[test]
    fn absent_base_and_partial_trees_are_idempotent() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        fs::create_dir_all(&root).unwrap();
        let base = root.join("project");
        assert_eq!(
            prune_derived_base(&base, &root).unwrap().outcome,
            PruneOutcome::AlreadyAbsent
        );
        fs::create_dir_all(base.join("debug")).unwrap();
        assert_eq!(
            prune_derived_base(&base, &root).unwrap().outcome,
            PruneOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn counts_logical_regular_file_lengths_and_changes_no_sibling() {
        let (_temp, root, base) = tree();
        fs::write(base.join("debug/incremental/a"), vec![0; 7]).unwrap();
        fs::write(base.join("debug/incremental/nested/b"), vec![0; 11]).unwrap();
        fs::write(base.join("debug/keep"), b"unchanged").unwrap();
        let result = prune_derived_base(&base, &root).unwrap();
        assert_eq!(
            result,
            PruneResult {
                outcome: PruneOutcome::Pruned,
                logical_bytes: 18
            }
        );
        assert!(!base.join("debug/incremental").exists());
        assert_eq!(fs::read(base.join("debug/keep")).unwrap(), b"unchanged");
    }

    #[test]
    fn counts_each_hardlinked_regular_file_entry() {
        let (_temp, root, base) = tree();
        let first = base.join("debug/incremental/first");
        fs::write(&first, vec![0; 13]).unwrap();
        fs::hard_link(&first, base.join("debug/incremental/second")).unwrap();

        assert_eq!(
            prune_derived_base(&base, &root).unwrap(),
            PruneResult {
                outcome: PruneOutcome::Pruned,
                logical_bytes: 26,
            }
        );
    }

    #[test]
    fn scan_does_not_follow_nested_links() {
        let (_temp, root, base) = tree();
        let outside = root.join("outside");
        fs::write(&outside, b"outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, base.join("debug/incremental/link")).unwrap();
        let result = prune_derived_base(&base, &root).unwrap();
        assert_eq!(result.logical_bytes, 0);
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_at_delete_components() {
        for component in ["project", "debug", "incremental"] {
            let (_temp, root, base) = tree();
            let outside = root.join("outside");
            fs::create_dir_all(&outside).unwrap();
            let path = match component {
                "project" => base.clone(),
                "debug" => base.join("debug"),
                _ => base.join("debug/incremental"),
            };
            fs::remove_dir_all(&path).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            assert_eq!(
                prune_derived_base(&base, &root).unwrap_err().kind,
                PruneErrorKind::Symlink
            );
        }
    }

    #[test]
    fn target_must_be_exact_derived_path() {
        let error =
            prune_warm_incremental_for_target("project", Path::new("/somewhere-else")).unwrap_err();
        assert_eq!(error.kind, PruneErrorKind::TargetMismatch);
    }

    #[test]
    fn rejects_derived_base_lexically_outside_cache_root_without_mutation() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let base = temp.path().join("outside/project");
        let incremental = base.join("debug/incremental");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&incremental).unwrap();
        fs::write(incremental.join("keep"), b"outside").unwrap();

        let error = prune_derived_base_with_operations(
            &base,
            &root,
            |_| panic!("outside base must not be scanned"),
            |_| panic!("outside base must not be removed"),
        )
        .unwrap_err();
        assert_eq!(error.kind, PruneErrorKind::Scan);
        assert_eq!(fs::read(incremental.join("keep")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_canonical_base_escape_through_intermediate_symlink_without_mutation() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("cache");
        let outside = temp.path().join("outside");
        let base = root.join("linked/project");
        let incremental = base.join("debug/incremental");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        fs::create_dir_all(&incremental).unwrap();
        fs::write(incremental.join("keep"), b"outside").unwrap();

        let error = prune_derived_base_with_operations(
            &base,
            &root,
            |_| panic!("escaped base must not be scanned"),
            |_| panic!("escaped base must not be removed"),
        )
        .unwrap_err();
        assert_eq!(error.kind, PruneErrorKind::Symlink);
        assert_eq!(fs::read(incremental.join("keep")).unwrap(), b"outside");
    }

    #[test]
    fn scanner_counts_sparse_logical_bytes() {
        let (_temp, root, base) = tree();
        let mut file = File::create(base.join("debug/incremental/sparse")).unwrap();
        file.set_len(1024 * 1024).unwrap();
        file.write_all(b"x").unwrap();
        assert_eq!(
            prune_derived_base(&base, &root).unwrap().logical_bytes,
            1024 * 1024
        );
    }

    #[test]
    fn injected_scan_errors_are_classified() {
        let (_temp, root, base) = tree();
        for (error, expected) in [
            (
                io::Error::other("broken directory entry"),
                PruneErrorKind::Scan,
            ),
            (
                io::Error::from(io::ErrorKind::PermissionDenied),
                PruneErrorKind::Permission,
            ),
        ] {
            assert_eq!(
                prune_derived_base_with_operations(&base, &root, |_| Err(error), |_| Ok(()))
                    .unwrap_err()
                    .kind,
                expected
            );
        }
    }

    #[test]
    fn injected_remove_error_is_classified() {
        let (_temp, root, base) = tree();
        assert_eq!(
            prune_derived_base_with_operations(
                &base,
                &root,
                |_| Ok(0),
                |_| Err(io::Error::other("remove failed")),
            )
            .unwrap_err()
            .kind,
            PruneErrorKind::Remove
        );
    }

    #[test]
    fn injected_remove_not_found_is_already_absent_with_zero_bytes() {
        let (_temp, root, base) = tree();
        let result = prune_derived_base_with_operations(
            &base,
            &root,
            |_| Ok(37),
            |_| Err(io::Error::from(io::ErrorKind::NotFound)),
        )
        .unwrap();
        assert_eq!(result, absent());
    }

    #[test]
    fn injected_lock_errors_are_classified() {
        let lock_open = WarmBaseLock::acquire_with_operations(
            "project",
            |_| Err(io::Error::other("cannot create lock directory")),
            |_| Ok(()),
            |_| -> io::Result<File> { unreachable!("lock file should not be opened") },
            |_| Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(lock_open.kind, PruneErrorKind::LockOpen);

        let lock_probe = WarmBaseLock::acquire_with_operations(
            "project",
            |_| Ok(()),
            |_| Err(io::Error::other("locking unsupported")),
            |_| -> io::Result<File> { unreachable!("lock file should not be opened") },
            |_| Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(lock_probe.kind, PruneErrorKind::LockProbe);

        let temp = TempDir::new().unwrap();
        let lock_file = File::create(temp.path().join("lock")).unwrap();
        let lock_acquire = WarmBaseLock::acquire_with_operations(
            "project",
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(lock_file),
            |_| Err(io::Error::other("lock acquisition failed")),
        )
        .err()
        .unwrap();
        assert_eq!(lock_acquire.kind, PruneErrorKind::LockAcquire);
    }

    /// This uses separate processes because release on warm-pod death is a
    /// shared-filesystem contract, not something a same-process mutex proves.
    #[test]
    fn shared_filesystem_lock_contract() {
        let fixture = TempDir::new().expect("shared fixture");
        let project = format!("lock-contract-{}", std::process::id());
        let base = fixture.path().join("cache/project");
        let incremental = base.join("debug/incremental");
        fs::create_dir_all(&incremental).expect("incremental tree");
        fs::write(incremental.join("left-after-interruption"), b"partial").expect("partial file");

        // A normal holder exit must hand off to a separately spawned waiter.
        let mut release_holder = contract_child("release-holder", &project, fixture.path());
        wait_for(&fixture.path().join("release-holder-acquired"));
        let mut release_waiter = contract_child("release-waiter", &project, fixture.path());
        thread::sleep(Duration::from_millis(150));
        assert!(
            !fixture.path().join("release-waiter-acquired").exists(),
            "a second warm process must wait while the first owns the guard"
        );
        fs::write(fixture.path().join("release-holder-exit"), b"exit").expect("release marker");
        assert!(release_holder.wait().expect("reap normal holder").success());
        assert!(
            release_waiter
                .wait()
                .expect("reap release waiter")
                .success()
        );
        assert!(fixture.path().join("release-waiter-acquired").exists());

        // Recreate a partial tree, then prove kernel process-death release.
        fs::remove_file(fixture.path().join("cleanup-complete"))
            .expect("clear first cleanup marker");
        fs::remove_file(fixture.path().join("compile-after-cleanup"))
            .expect("clear first compile marker");
        fs::create_dir_all(&incremental).expect("partial incremental tree");
        fs::write(incremental.join("left-after-death"), b"partial").expect("partial file");
        let mut holder = contract_child("holder", &project, fixture.path());
        wait_for(&fixture.path().join("holder-acquired"));
        let mut killed_waiter = contract_child("killed-waiter", &project, fixture.path());
        thread::sleep(Duration::from_millis(150));
        assert!(!fixture.path().join("killed-waiter-acquired").exists());
        killed_waiter.kill().expect("kill waiting process");
        killed_waiter.wait().expect("reap waiting process");
        assert!(
            !fixture.path().join("killed-waiter-terminal").exists(),
            "a killed waiting attempt must not fabricate a terminal result"
        );
        let mut waiter = contract_child("waiter", &project, fixture.path());
        holder.kill().expect("kill lock holder");
        holder.wait().expect("reap lock holder");
        assert!(waiter.wait().expect("reap waiter").success());
        assert!(fixture.path().join("waiter-acquired").exists());
        assert!(fixture.path().join("cleanup-complete").exists());
        assert!(fixture.path().join("compile-after-cleanup").exists());
        assert!(!incremental.exists(), "retry must remove the partial tree");
    }

    #[test]
    fn shared_filesystem_lock_contract_child() {
        let Ok(role) = std::env::var("DJINN_LOCK_CONTRACT_ROLE") else {
            return;
        };
        let project = std::env::var("DJINN_LOCK_CONTRACT_PROJECT").expect("project");
        let fixture = PathBuf::from(std::env::var("DJINN_LOCK_CONTRACT_FIXTURE").expect("fixture"));
        // This is the same returned-error recorder boundary the warm flow uses.
        // A SIGKILL while flock is blocked cannot reach this terminal callback.
        let terminal = fixture.join(format!("{role}-terminal"));
        let guard = WarmBaseLock::acquire_and_record_failure(&project, |error| {
            fs::write(&terminal, error.kind.as_str()).expect("terminal observation")
        })
        .expect("shared filesystem lock");
        fs::write(fixture.join(format!("{role}-acquired")), b"acquired").expect("acquired marker");
        if role == "holder" {
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        if role == "release-holder" {
            wait_for(&fixture.join("release-holder-exit"));
            drop(guard);
            return;
        }
        let base = fixture.join("cache/project");
        prune_fixture_incremental(&base, &fixture.join("cache")).expect("retry cleanup");
        fs::write(fixture.join("cleanup-complete"), b"clean").expect("cleanup marker");
        assert!(
            !base.join("debug/incremental").exists(),
            "compile only after cleanup"
        );
        fs::write(fixture.join("compile-after-cleanup"), b"compiled").expect("compile marker");
        drop(guard);
    }

    fn contract_child(role: &str, project: &str, fixture: &Path) -> std::process::Child {
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "cargo_incremental_prune::tests::shared_filesystem_lock_contract_child",
                "--exact",
                "--nocapture",
            ])
            .env("DJINN_LOCK_CONTRACT_ROLE", role)
            .env("DJINN_LOCK_CONTRACT_PROJECT", project)
            .env("DJINN_LOCK_CONTRACT_FIXTURE", fixture)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lock-contract process")
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A fail-closed pruning or locking error.  The free-form detail is for logs,
/// never for a metric label.
#[derive(Debug, Error)]
#[error("cargo incremental prune {kind}: {detail}", kind = .kind.as_str())]
pub struct PruneError {
    pub kind: PruneErrorKind,
    detail: String,
}

impl PruneError {
    fn new(kind: PruneErrorKind, error: impl std::fmt::Display) -> Self {
        Self {
            kind,
            detail: error.to_string(),
        }
    }
}

/// Terminal result of a successful prune attempt.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PruneResult {
    /// `pruned` when an existing incremental directory was removed.
    pub outcome: PruneOutcome,
    /// Sum of regular-file logical lengths scanned before removal.
    pub logical_bytes: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PruneOutcome {
    Pruned,
    AlreadyAbsent,
}

impl PruneOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pruned => "pruned",
            Self::AlreadyAbsent => "already_absent",
        }
    }
}

/// An exclusive PVC lock. Dropping it closes the file descriptor, releasing
/// the advisory lock; process death has the same kernel-guaranteed effect.
pub struct WarmBaseLock {
    _file: File,
}

impl WarmBaseLock {
    /// Probe the repository-local filesystem and wait for the per-project lock.
    /// A lock open/probe/acquisition error intentionally fails closed.
    pub fn acquire(project_id: &str) -> Result<Self, PruneError> {
        Self::acquire_with_operations(
            project_id,
            |lock_dir| fs::create_dir_all(lock_dir),
            probe_advisory_lock,
            |path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(path)
            },
            flock_exclusive,
        )
    }

    /// Acquire through the warm flow's terminal recorder boundary. The callback
    /// runs only after a returned lock failure, never while an attempt blocks.
    pub(crate) fn acquire_and_record_failure<Record>(
        project_id: &str,
        record_failure: Record,
    ) -> Result<Self, PruneError>
    where
        Record: FnOnce(&PruneError),
    {
        let result = Self::acquire(project_id);
        if let Err(error) = &result {
            record_failure(error);
        }
        result
    }

    /// Test-only operation-level seam used by `warm_cargo_target_base` tests.
    /// It retains the production lock derivation and only substitutes the
    /// requested probe or acquisition operation.
    #[cfg(test)]
    pub(crate) fn acquire_with_operation_failure_and_record<Record>(
        project_id: &str,
        failure: WarmLockOperationFailure,
        record_failure: Record,
    ) -> Result<Self, PruneError>
    where
        Record: FnOnce(&PruneError),
    {
        let result = Self::acquire_with_operations(
            project_id,
            |lock_dir| fs::create_dir_all(lock_dir),
            |lock_dir| match failure {
                WarmLockOperationFailure::Probe => {
                    Err(io::Error::other("injected unsupported lock probe"))
                }
                WarmLockOperationFailure::Acquire => probe_advisory_lock(lock_dir),
            },
            |path| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(path)
            },
            |file| match failure {
                WarmLockOperationFailure::Probe => flock_exclusive(file),
                WarmLockOperationFailure::Acquire => {
                    Err(io::Error::other("injected lock acquisition failure"))
                }
            },
        );
        if let Err(error) = &result {
            record_failure(error);
        }
        result
    }

    /// Injectable filesystem operations keep fail-closed classifications
    /// deterministic without allowing callers to choose a lock path.
    fn acquire_with_operations<Create, Probe, Open, Acquire>(
        project_id: &str,
        create_lock_dir: Create,
        probe: Probe,
        open_lock: Open,
        acquire_lock: Acquire,
    ) -> Result<Self, PruneError>
    where
        Create: FnOnce(&Path) -> io::Result<()>,
        Probe: FnOnce(&Path) -> io::Result<()>,
        Open: FnOnce(&Path) -> io::Result<File>,
        Acquire: FnOnce(&File) -> io::Result<()>,
    {
        validate_project_id(project_id)?;
        let lock_dir = Path::new(WARM_BASE_ROOT).join(LOCK_DIRECTORY);
        create_lock_dir(&lock_dir)
            .map_err(|error| PruneError::new(PruneErrorKind::LockOpen, error))?;
        probe(&lock_dir).map_err(|error| PruneError::new(PruneErrorKind::LockProbe, error))?;

        let path = lock_dir.join(format!("{project_id}.lock"));
        let file =
            open_lock(&path).map_err(|error| PruneError::new(PruneErrorKind::LockOpen, error))?;
        acquire_lock(&file).map_err(|error| PruneError::new(PruneErrorKind::LockAcquire, error))?;
        Ok(Self { _file: file })
    }
}

/// The operation failures injected by the warm-flow ordering test.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum WarmLockOperationFailure {
    Probe,
    Acquire,
}

/// Validate the configured warm target and remove only `debug/incremental`.
///
/// The target must exactly equal `warm_base_dir(project_id)` rather than merely
/// canonically resolving to it. This catches accidental task-run target dirs
/// before any filesystem mutation.
pub fn prune_warm_incremental(project_id: &str) -> Result<PruneResult, PruneError> {
    let configured = std::env::var_os(CARGO_TARGET_DIR).ok_or_else(|| {
        PruneError::new(PruneErrorKind::TargetMismatch, "CARGO_TARGET_DIR is unset")
    })?;
    prune_warm_incremental_for_target(project_id, Path::new(&configured))
}

/// Same production derivation with an explicit environment value seam for
/// deterministic callers/tests. `base` is still always derived internally.
pub(crate) fn prune_warm_incremental_for_target(
    project_id: &str,
    configured_target: &Path,
) -> Result<PruneResult, PruneError> {
    validate_project_id(project_id)?;
    let base = warm_base_dir(project_id);
    if configured_target != base {
        return Err(PruneError::new(
            PruneErrorKind::TargetMismatch,
            format!(
                "configured {} does not exactly match {}",
                configured_target.display(),
                base.display()
            ),
        ));
    }
    prune_derived_base(&base, Path::new(WARM_BASE_ROOT))
}

fn validate_project_id(project_id: &str) -> Result<(), PruneError> {
    let mut components = Path::new(project_id).components();
    let valid = matches!(components.next(), Some(Component::Normal(part)) if part == OsStr::new(project_id))
        && components.next().is_none()
        && !project_id.is_empty();
    if valid {
        Ok(())
    } else {
        Err(PruneError::new(
            PruneErrorKind::InvalidProjectId,
            "project id must be one normal path component",
        ))
    }
}

fn prune_derived_base(base: &Path, cache_root: &Path) -> Result<PruneResult, PruneError> {
    prune_derived_base_with_operations(base, cache_root, scan_regular_file_bytes, |path| {
        fs::remove_dir_all(path)
    })
}

/// Exercise the same checked pruning operation against an isolated fixture.
///
/// This is deliberately test-only: production callers must derive both paths
/// from the fixed warm-base convention through [`prune_warm_incremental`].
#[cfg(test)]
pub(crate) fn prune_fixture_incremental(
    base: &Path,
    cache_root: &Path,
) -> Result<PruneResult, PruneError> {
    prune_derived_base(base, cache_root)
}

/// The operations are injected only after the deletion root has been derived
/// and every component has been validated. This makes error handling testable
/// without creating a caller-controlled deletion target.
fn prune_derived_base_with_operations<Scan, Remove>(
    base: &Path,
    cache_root: &Path,
    scan: Scan,
    remove: Remove,
) -> Result<PruneResult, PruneError>
where
    Scan: FnOnce(&Path) -> io::Result<u64>,
    Remove: FnOnce(&Path) -> io::Result<()>,
{
    let canonical_root = checked_directory(cache_root, PruneErrorKind::Scan)?;
    // This check is redundant for the production derivation, but makes the
    // containment invariant explicit before inspecting descendants.
    if !base.starts_with(cache_root) {
        return Err(PruneError::new(
            PruneErrorKind::Scan,
            "derived base escapes cache root",
        ));
    }

    let base_state = component_state(base, &canonical_root)?;
    if base_state == ComponentState::Absent {
        return Ok(absent());
    }
    let debug = base.join("debug");
    if component_state(&debug, &canonical_root)? == ComponentState::Absent {
        return Ok(absent());
    }
    let incremental = debug.join("incremental");
    if component_state(&incremental, &canonical_root)? == ComponentState::Absent {
        return Ok(absent());
    }

    let bytes = scan(&incremental).map_err(classify_scan_error)?;
    match remove_incremental_dir_with(&incremental, remove)? {
        RemovalOutcome::Removed => Ok(PruneResult {
            outcome: PruneOutcome::Pruned,
            logical_bytes: bytes,
        }),
        RemovalOutcome::AlreadyAbsent => Ok(absent()),
    }
}

#[derive(Eq, PartialEq)]
enum ComponentState {
    Present,
    Absent,
}

/// Reject links/non-directories, then prove each existing component remains
/// below the canonical cache root. For a missing component the nearest existing
/// parent is canonicalized and checked instead.
fn component_state(path: &Path, canonical_root: &Path) -> Result<ComponentState, PruneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(PruneError::new(
                    PruneErrorKind::Symlink,
                    format!("{} is a symlink", path.display()),
                ));
            }
            if !metadata.is_dir() {
                return Err(PruneError::new(
                    PruneErrorKind::Scan,
                    format!("{} is not a directory", path.display()),
                ));
            }
            let canonical = fs::canonicalize(path).map_err(classify_scan_error)?;
            require_contained(&canonical, canonical_root)?;
            Ok(ComponentState::Present)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = nearest_existing_parent(path)?;
            require_contained(&parent, canonical_root)?;
            Ok(ComponentState::Absent)
        }
        Err(error) => Err(classify_scan_error(error)),
    }
}

fn checked_directory(path: &Path, kind: PruneErrorKind) -> Result<PathBuf, PruneError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| PruneError::new(kind, error))?;
    if metadata.file_type().is_symlink() {
        return Err(PruneError::new(
            PruneErrorKind::Symlink,
            format!("{} is a symlink", path.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(PruneError::new(
            kind,
            format!("{} is not a directory", path.display()),
        ));
    }
    fs::canonicalize(path).map_err(classify_scan_error)
}

fn nearest_existing_parent(path: &Path) -> Result<PathBuf, PruneError> {
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(PruneError::new(
                        PruneErrorKind::Symlink,
                        format!("{} is a symlink", candidate.display()),
                    ));
                }
                if !metadata.is_dir() {
                    return Err(PruneError::new(
                        PruneErrorKind::Scan,
                        format!("{} is not a directory", candidate.display()),
                    ));
                }
                return fs::canonicalize(candidate).map_err(classify_scan_error);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => parent = candidate.parent(),
            Err(error) => return Err(classify_scan_error(error)),
        }
    }
    Err(PruneError::new(PruneErrorKind::Scan, "no existing parent"))
}

fn require_contained(candidate: &Path, canonical_root: &Path) -> Result<(), PruneError> {
    if candidate.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(PruneError::new(
            PruneErrorKind::Symlink,
            format!(
                "{} escapes {}",
                candidate.display(),
                canonical_root.display()
            ),
        ))
    }
}

fn scan_regular_file_bytes(root: &Path) -> io::Result<u64> {
    let mut pending = vec![root.to_path_buf()];
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            // `file_type` is lstat-like: do not call metadata on symlinks.
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.is_file() {
                    bytes = bytes
                        .checked_add(metadata.len())
                        .ok_or_else(|| io::Error::other("logical byte count overflow"))?;
                }
            }
        }
    }
    Ok(bytes)
}

enum RemovalOutcome {
    Removed,
    AlreadyAbsent,
}

// Production supplies exactly one `remove_dir_all` call after scanning.
fn remove_incremental_dir_with<Remove>(
    path: &Path,
    remove: Remove,
) -> Result<RemovalOutcome, PruneError>
where
    Remove: FnOnce(&Path) -> io::Result<()>,
{
    match remove(path) {
        Ok(()) => Ok(RemovalOutcome::Removed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RemovalOutcome::AlreadyAbsent),
        Err(error) => Err(PruneError::new(PruneErrorKind::Remove, error)),
    }
}

fn absent() -> PruneResult {
    PruneResult {
        outcome: PruneOutcome::AlreadyAbsent,
        logical_bytes: 0,
    }
}

fn classify_scan_error(error: io::Error) -> PruneError {
    let kind = if error.kind() == io::ErrorKind::PermissionDenied {
        PruneErrorKind::Permission
    } else {
        PruneErrorKind::Scan
    };
    PruneError::new(kind, error)
}

fn probe_advisory_lock(lock_dir: &Path) -> io::Result<()> {
    let path = lock_dir.join(format!(".capability-{}.probe", std::process::id()));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)?;
    flock_exclusive_nonblocking(&file)?;
    // Explicit unlock verifies both lock operations on this filesystem. Closing
    // the descriptor is also a release fallback if unlock itself fails.
    flock_unlock(&file)
}

fn flock_exclusive(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_EX)
}
fn flock_exclusive_nonblocking(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_EX | libc::LOCK_NB)
}
fn flock_unlock(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_UN)
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of this call;
        // flock neither takes ownership nor requires aliasing guarantees.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
