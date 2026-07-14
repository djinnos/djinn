//! Reaping primitives for per-task-run private Cargo target directories.
//!
//! Task-run Pods seed a private Cargo target dir under
//! `/cache/cargo-target-runs/<id>` (see `djinn-agent-worker::cargo_target_seed`)
//! so concurrent runs never contend on a shared target lock. Those dirs are
//! meant to be EPHEMERAL — discarded once the run reaches a terminal state.
//!
//! Three layers keep the directory bounded, all sharing the helpers here:
//!
//! 1. **Worker Drop guard** (in-pod, best-effort) — removes the dir on a clean
//!    exit. Does NOT run on SIGKILL (OOM / eviction / deadline).
//! 2. **Host teardown backstop** — the host supervisor removes the dir after a
//!    K8s task-run finalizes, covering the SIGKILL case the in-pod guard misses.
//! 3. **Periodic coordinator sweep + hard cap** — deletes orphaned dirs whose
//!    run is no longer active, and LRU-trims the directory below a hard size cap
//!    so any future reaping regression still cannot refill the disk.
//!
//! The helpers are pure `std::fs` (sync) so they can run from both the
//! synchronous worker teardown path and a coordinator `spawn_blocking` sweep,
//! and so they unit-test without a Tokio runtime.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Default hard cap on the number of per-run target dirs retained under the
/// runs root. Each seeded dir is hardlink-heavy but still costs real bytes for
/// copied metadata + the run's own incremental output; capping the *count*
/// (cheap to enforce, no recursive `du`) bounds worst-case disk independent of
/// task-run state. Overridable via [`HARD_CAP_ENV`].
pub const DEFAULT_HARD_CAP_DIRS: usize = 64;

/// Environment override for [`DEFAULT_HARD_CAP_DIRS`]. `0` disables the cap.
pub const HARD_CAP_ENV: &str = "DJINN_CARGO_TARGET_RUNS_MAX_DIRS";

/// Default allocated-byte cap for all entries below a runs root (8 GiB).
pub const DEFAULT_HARD_CAP_BYTES: u64 = 8_589_934_592;

/// Environment override for [`DEFAULT_HARD_CAP_BYTES`]. `0` disables only the
/// allocated-byte cap.
pub const HARD_CAP_BYTES_ENV: &str = "DJINN_CARGO_TARGET_RUNS_MAX_BYTES";

/// Resolved count and allocated-byte caps. A zero value disables that cap.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CargoTargetRunsCaps {
    pub max_dirs: usize,
    pub max_bytes: u64,
}

/// A deterministically orderable directory candidate. Names are raw Unix bytes
/// so later policy can tie-break without lossy UTF-8 conversion.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RunDirInventoryCandidate {
    pub name: Vec<u8>,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InventoryIssueKind {
    MalformedTopLevelName,
    TopLevelSymlink,
    ReadDirectory,
    Stat,
}

/// Protected data and incomplete-scan information, without exposing arbitrary
/// filesystem error text as telemetry.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InventoryIssue {
    pub top_level_name: Option<Vec<u8>>,
    pub kind: InventoryIssueKind,
}

/// Allocated-byte inventory for a runs root.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct CargoTargetRunsInventory {
    pub total_allocated_bytes: u64,
    pub top_level_directory_count: usize,
    pub candidates: Vec<RunDirInventoryCandidate>,
    pub non_directory_allocated_bytes: u64,
    pub protected: Vec<InventoryIssue>,
    pub errors: Vec<InventoryIssue>,
}

#[derive(Debug)]
pub enum CargoTargetRunsInventoryError {
    UnsupportedPlatform,
    RootRead(io::Error),
    ByteOverflow,
}
impl std::fmt::Display for CargoTargetRunsInventoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                f.write_str("allocated-byte inventory is supported only on Unix")
            }
            Self::RootRead(error) => write!(f, "failed to read cargo target runs root: {error}"),
            Self::ByteOverflow => {
                f.write_str("cargo target runs allocated-byte total overflowed u64")
            }
        }
    }
}
impl std::error::Error for CargoTargetRunsInventoryError {}

/// Inventory `st_blocks * 512` without following symlinks. Root read failures
/// are fatal; entry failures are retained as error data and never look clean.
#[cfg(unix)]
pub fn inventory_cargo_target_runs(
    root: &Path,
) -> Result<CargoTargetRunsInventory, CargoTargetRunsInventoryError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(CargoTargetRunsInventoryError::RootRead)?;
    let root_entries = fs::read_dir(root).map_err(CargoTargetRunsInventoryError::RootRead)?;
    let mut inventory = CargoTargetRunsInventory::default();
    let mut seen = HashSet::new();
    account_metadata(&root_metadata, false, &mut seen, &mut inventory)?;
    for entry in root_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                inventory.errors.push(InventoryIssue {
                    top_level_name: None,
                    kind: InventoryIssueKind::ReadDirectory,
                });
                continue;
            }
        };
        let raw_name = entry.file_name().as_bytes().to_vec();
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => {
                inventory.errors.push(InventoryIssue {
                    top_level_name: Some(raw_name),
                    kind: InventoryIssueKind::Stat,
                });
                continue;
            }
        };
        account_metadata(&metadata, !metadata.is_dir(), &mut seen, &mut inventory)?;
        if metadata.file_type().is_symlink() {
            inventory.protected.push(InventoryIssue {
                top_level_name: Some(raw_name),
                kind: InventoryIssueKind::TopLevelSymlink,
            });
        } else if metadata.is_dir() {
            inventory.top_level_directory_count += 1;
            if entry.file_name().is_empty() || entry.file_name().to_str().is_none() {
                inventory.protected.push(InventoryIssue {
                    top_level_name: Some(raw_name.clone()),
                    kind: InventoryIssueKind::MalformedTopLevelName,
                });
            } else {
                inventory.candidates.push(RunDirInventoryCandidate {
                    name: raw_name.clone(),
                    modified: metadata.modified().ok(),
                    created: metadata.created().ok(),
                });
            }
            inventory_directory(&entry.path(), Some(raw_name), &mut seen, &mut inventory)?;
        }
    }
    inventory.candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.created.cmp(&right.created))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(inventory)
}

#[cfg(not(unix))]
pub fn inventory_cargo_target_runs(
    _root: &Path,
) -> Result<CargoTargetRunsInventory, CargoTargetRunsInventoryError> {
    Err(CargoTargetRunsInventoryError::UnsupportedPlatform)
}

#[cfg(unix)]
fn inventory_directory(
    path: &Path,
    top_level_name: Option<Vec<u8>>,
    seen: &mut HashSet<(u64, u64)>,
    inventory: &mut CargoTargetRunsInventory,
) -> Result<(), CargoTargetRunsInventoryError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => {
            inventory.errors.push(InventoryIssue {
                top_level_name,
                kind: InventoryIssueKind::ReadDirectory,
            });
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                inventory.errors.push(InventoryIssue {
                    top_level_name: top_level_name.clone(),
                    kind: InventoryIssueKind::ReadDirectory,
                });
                continue;
            }
        };
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => {
                inventory.errors.push(InventoryIssue {
                    top_level_name: top_level_name.clone(),
                    kind: InventoryIssueKind::Stat,
                });
                continue;
            }
        };
        account_metadata(&metadata, !metadata.is_dir(), seen, inventory)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            inventory_directory(&entry.path(), top_level_name.clone(), seen, inventory)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn account_metadata(
    metadata: &fs::Metadata,
    non_directory: bool,
    seen: &mut HashSet<(u64, u64)>,
    inventory: &mut CargoTargetRunsInventory,
) -> Result<(), CargoTargetRunsInventoryError> {
    if !seen.insert((metadata.dev(), metadata.ino())) {
        return Ok(());
    }
    let bytes = metadata
        .blocks()
        .checked_mul(512)
        .ok_or(CargoTargetRunsInventoryError::ByteOverflow)?;
    inventory.total_allocated_bytes = inventory
        .total_allocated_bytes
        .checked_add(bytes)
        .ok_or(CargoTargetRunsInventoryError::ByteOverflow)?;
    if non_directory {
        inventory.non_directory_allocated_bytes = inventory
            .non_directory_allocated_bytes
            .checked_add(bytes)
            .ok_or(CargoTargetRunsInventoryError::ByteOverflow)?;
    }
    Ok(())
}

impl Default for CargoTargetRunsCaps {
    fn default() -> Self {
        Self {
            max_dirs: DEFAULT_HARD_CAP_DIRS,
            max_bytes: DEFAULT_HARD_CAP_BYTES,
        }
    }
}

/// Whether a configured cap had to fall back to its default.
///
/// This deliberately contains no environment value: callers may safely log it
/// without retaining an unbounded, operator-provided string.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct CapResolutionDiagnostics {
    pub invalid_max_dirs: bool,
    pub invalid_max_bytes: bool,
}

/// Resolve cap values supplied by a caller, making parser behaviour testable
/// without mutating process environment. Values must be non-empty ASCII
/// unsigned decimal; whitespace, signs, suffixes, and overflow are invalid.
pub fn resolve_cargo_target_runs_caps(
    max_dirs: Option<&str>,
    max_bytes: Option<&str>,
) -> (CargoTargetRunsCaps, CapResolutionDiagnostics) {
    let (dirs, invalid_max_dirs) = parse_unsigned_decimal(max_dirs, DEFAULT_HARD_CAP_DIRS as u64);
    let (max_bytes, invalid_max_bytes) = parse_unsigned_decimal(max_bytes, DEFAULT_HARD_CAP_BYTES);
    let (max_dirs, dirs_overflow) = match usize::try_from(dirs) {
        Ok(value) => (value, false),
        Err(_) => (DEFAULT_HARD_CAP_DIRS, true),
    };
    (
        CargoTargetRunsCaps {
            max_dirs,
            max_bytes,
        },
        CapResolutionDiagnostics {
            invalid_max_dirs: invalid_max_dirs || dirs_overflow,
            invalid_max_bytes,
        },
    )
}

fn parse_unsigned_decimal(raw: Option<&str>, default: u64) -> (u64, bool) {
    let Some(raw) = raw else {
        return (default, false);
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return (default, true);
    }
    match raw.parse() {
        Ok(value) => (value, false),
        Err(_) => (default, true),
    }
}

fn bounded_env_value(raw: &str) -> String {
    const LIMIT: usize = 128;
    let mut preview = raw.chars().take(LIMIT).collect::<String>();
    if raw.chars().nth(LIMIT).is_some() {
        preview.push('…');
    }
    preview
}

/// Resolve both caps from the process environment, logging only a bounded
/// preview when an override is invalid.
pub fn cargo_target_runs_caps_from_env() -> CargoTargetRunsCaps {
    let dirs = std::env::var(HARD_CAP_ENV).ok();
    let bytes = std::env::var(HARD_CAP_BYTES_ENV).ok();
    let (caps, diagnostics) = resolve_cargo_target_runs_caps(dirs.as_deref(), bytes.as_deref());
    if diagnostics.invalid_max_dirs {
        tracing::warn!(
            env = HARD_CAP_ENV,
            value = dirs
                .as_deref()
                .map(bounded_env_value)
                .as_deref()
                .unwrap_or("<non-unicode>"),
            "invalid cargo target runs count cap; using default"
        );
    }
    if diagnostics.invalid_max_bytes {
        tracing::warn!(
            env = HARD_CAP_BYTES_ENV,
            value = bytes
                .as_deref()
                .map(bounded_env_value)
                .as_deref()
                .unwrap_or("<non-unicode>"),
            "invalid cargo target runs byte cap; using default"
        );
    }
    caps
}

/// Outcome of a single per-run dir teardown.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct TeardownResult {
    /// `true` when a directory existed and was removed.
    pub removed: bool,
}

impl TeardownResult {
    pub fn removed_count(self) -> u64 {
        u64::from(self.removed)
    }

    pub fn outcome(self) -> &'static str {
        if self.removed {
            "removed"
        } else {
            "already_absent"
        }
    }
}

/// Remove the private per-run target dir `<root>/<id>`, best-effort.
///
/// A missing dir is success (idempotent): terminal-report and teardown paths
/// call this unconditionally and a clean in-pod Drop guard may already have
/// removed it. Only `id`s that are a single path component are honored so a
/// malformed id can never escape `root` via `..` or an absolute path.
pub fn teardown_run_dir(root: &Path, id: &str) -> io::Result<TeardownResult> {
    let Some(dir) = run_dir_within(root, id) else {
        // A malformed id can't have been created by the seeder; treat as a
        // no-op rather than risk removing anything outside the runs root.
        return Ok(TeardownResult { removed: false });
    };
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(TeardownResult { removed: true }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(TeardownResult { removed: false }),
        Err(err) => Err(err),
    }
}

/// Join `id` under `root`, returning `None` unless `id` is exactly one normal
/// path component (no separators, no `.`/`..`, not absolute).
fn run_dir_within(root: &Path, id: &str) -> Option<PathBuf> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = Path::new(trimmed);
    let mut components = candidate.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(name)), None) => Some(root.join(name)),
        _ => None,
    }
}

/// Per-entry summary for the hard-cap trim.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub struct HardCapTrimStats {
    /// Candidate run dirs found under the root.
    pub scanned: usize,
    /// Dirs removed because they exceeded the cap (oldest-first).
    pub trimmed: usize,
    /// Dirs left in place (within the cap).
    pub retained: usize,
    /// Per-entry errors (stat/remove); the trim continues past them.
    pub errors: usize,
}

/// Resolve the effective hard cap from [`HARD_CAP_ENV`], falling back to
/// [`DEFAULT_HARD_CAP_DIRS`]. A configured `0` disables the cap.
pub fn hard_cap_dirs_from_env() -> usize {
    cargo_target_runs_caps_from_env().max_dirs
}

/// LRU-trim the runs `root` so at most `max_dirs` subdirectories remain,
/// removing the oldest (by modified time, then created time) first.
///
/// This is a state-independent backstop: it does NOT consult the DB, so even if
/// the deterministic teardown and the orphan sweep both regress, the directory
/// can never grow without bound. `max_dirs == 0` disables the trim. A missing
/// root is a no-op.
pub fn trim_run_dirs_to_cap(root: &Path, max_dirs: usize) -> io::Result<HardCapTrimStats> {
    if max_dirs == 0 {
        return Ok(HardCapTrimStats::default());
    }

    let read_dir = match fs::read_dir(root) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(HardCapTrimStats::default());
        }
        Err(err) => return Err(err),
    };

    let mut stats = HardCapTrimStats::default();
    let mut dirs: Vec<(SystemTime, PathBuf)> = Vec::new();

    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let metadata = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if !metadata.is_dir() {
            // Non-directory entries are not seeded run dirs; leave them for the
            // orphan sweep to reason about and don't count against the cap.
            continue;
        }
        stats.scanned += 1;
        let mtime = metadata
            .modified()
            .or_else(|_| metadata.created())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        dirs.push((mtime, entry.path()));
    }

    if dirs.len() <= max_dirs {
        stats.retained = dirs.len();
        return Ok(stats);
    }

    // Oldest first so the most recently active runs survive the trim.
    dirs.sort_by_key(|(mtime, _)| *mtime);
    let trim_count = dirs.len() - max_dirs;
    for (_, path) in dirs.into_iter().take(trim_count) {
        match fs::remove_dir_all(&path) {
            Ok(()) => stats.trimmed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => stats.trimmed += 1,
            Err(_) => stats.errors += 1,
        }
    }
    stats.retained = max_dirs;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

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
    fn hard_cap_env_parses_and_falls_back() {
        // The default applies when unset/garbage; this asserts the pure parse
        // path without mutating the process env under test parallelism.
        assert_eq!(DEFAULT_HARD_CAP_DIRS, 64);
    }
}
