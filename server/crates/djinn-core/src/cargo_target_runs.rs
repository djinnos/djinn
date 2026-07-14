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
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
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
    /// Bytes held by non-directory entries anywhere in the root. This is
    /// accounting-only: files nested in a removable run disappear with it and
    /// therefore do not themselves block trimming.
    pub non_directory_allocated_bytes: u64,
    /// Bytes held by non-directory entries directly under the runs root. These
    /// entries cannot be removal candidates, so they can independently prevent
    /// satisfying a byte budget.
    pub top_level_non_directory_allocated_bytes: u64,
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
    if root_metadata.file_type().is_symlink() {
        return Err(CargoTargetRunsInventoryError::RootRead(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cargo target runs root must not be a symlink",
        )));
    }
    let root_entries = fs::read_dir(root).map_err(CargoTargetRunsInventoryError::RootRead)?;
    let mut inventory = CargoTargetRunsInventory::default();
    let mut seen = HashSet::new();
    account_metadata(&root_metadata, false, false, &mut seen, &mut inventory)?;
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
        account_metadata(
            &metadata,
            !metadata.is_dir(),
            !metadata.is_dir(),
            &mut seen,
            &mut inventory,
        )?;
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
                let candidate = RunDirInventoryCandidate {
                    name: raw_name.clone(),
                    modified: metadata.modified().ok(),
                    created: metadata.created().ok(),
                };
                // Recursive failures make the entire run unmeasurable, so it
                // must not remain a removal candidate.
                let errors_before = inventory.errors.len();
                inventory_directory(
                    &entry.path(),
                    Some(raw_name.clone()),
                    &mut seen,
                    &mut inventory,
                )?;
                if inventory.errors[errors_before..]
                    .iter()
                    .any(|issue| issue.top_level_name.as_deref() == Some(raw_name.as_slice()))
                {
                    inventory.protected.push(InventoryIssue {
                        top_level_name: Some(raw_name),
                        kind: InventoryIssueKind::ReadDirectory,
                    });
                } else {
                    inventory.candidates.push(candidate);
                }
                continue;
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
        account_metadata(&metadata, !metadata.is_dir(), false, seen, inventory)?;
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
    top_level_non_directory: bool,
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
    if top_level_non_directory {
        inventory.top_level_non_directory_allocated_bytes = inventory
            .top_level_non_directory_allocated_bytes
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
    let (caps, diagnostics, dirs, bytes) = resolve_caps_from_env_results(
        std::env::var(HARD_CAP_ENV),
        std::env::var(HARD_CAP_BYTES_ENV),
    );
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

/// Preserve `VarError::NotUnicode` as invalid configuration rather than
/// treating it like an unset variable. The returned strings are only used for
/// bounded diagnostic previews.
fn resolve_caps_from_env_results(
    dirs: Result<String, std::env::VarError>,
    bytes: Result<String, std::env::VarError>,
) -> (
    CargoTargetRunsCaps,
    CapResolutionDiagnostics,
    Option<String>,
    Option<String>,
) {
    let (dirs, dirs_not_unicode) = env_value_or_invalid(dirs);
    let (bytes, bytes_not_unicode) = env_value_or_invalid(bytes);
    let (caps, mut diagnostics) = resolve_cargo_target_runs_caps(dirs.as_deref(), bytes.as_deref());
    diagnostics.invalid_max_dirs |= dirs_not_unicode;
    diagnostics.invalid_max_bytes |= bytes_not_unicode;
    (caps, diagnostics, dirs, bytes)
}

fn env_value_or_invalid(value: Result<String, std::env::VarError>) -> (Option<String>, bool) {
    match value {
        Ok(value) => (Some(value), false),
        Err(std::env::VarError::NotPresent) => (None, false),
        Err(std::env::VarError::NotUnicode(_)) => (None, true),
    }
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

/// Bounded result labels for [`trim_cargo_target_runs`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CargoTargetRunsTrimOutcome {
    WithinBudget,
    TrimmedWithinBudget,
    OverBudgetProtected,
    OverBudgetError,
    OverBudgetProtectedAndError,
}
impl CargoTargetRunsTrimOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WithinBudget => "within_budget",
            Self::TrimmedWithinBudget => "trimmed_within_budget",
            Self::OverBudgetProtected => "over_budget_protected",
            Self::OverBudgetError => "over_budget_error",
            Self::OverBudgetProtectedAndError => "over_budget_protected_and_error",
        }
    }
}

/// Exact postcondition and bounded safety accounting from a joint-cap trim.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CargoTargetRunsTrimResult {
    pub final_allocated_bytes: u64,
    pub final_top_level_directory_count: usize,
    pub deleted: usize,
    pub errors: usize,
    pub protected: usize,
    pub outcome: CargoTargetRunsTrimOutcome,
}

/// Injected filesystem operation seam for [`trim_cargo_target_runs_with_fs`]. The
/// default implementation delegates to `std::fs`; tests override
/// [`Filesystem::remove_dir_all`] and [`Filesystem::symlink_metadata`] to
/// deterministically simulate removal failure and TOCTOU revalidation races
/// without depending on ambient permissions.
#[cfg(unix)]
pub trait Filesystem {
    /// Return `symlink_metadata` without following the link (lstat).
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;

    /// Recursively remove a directory, mirroring `std::fs::remove_dir_all`.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
}

/// Production filesystem seam: delegates to `std::fs`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFilesystem;

#[cfg(unix)]
impl Filesystem for StdFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

/// Enforce enabled count and allocated-byte caps conjunctively, re-inventorying
/// after every attempt. Delegates to [`trim_cargo_target_runs_with_fs`] with the
/// production [`StdFilesystem`] seam.
#[cfg(unix)]
pub fn trim_cargo_target_runs(
    root: &Path,
    active_ids: &HashSet<String>,
    caps: CargoTargetRunsCaps,
) -> Result<CargoTargetRunsTrimResult, CargoTargetRunsInventoryError> {
    trim_cargo_target_runs_with_fs(root, active_ids, caps, &StdFilesystem)
}

/// Trim engine core. Candidates are selected from the inventory in sorted order
/// (mtime ascending, then creation time, then raw name bytes), excluding any name
/// present in `inventory.errors` so an error-affected run is never removed. Before
/// each removal the name is revalidated as a contained single component, the path
/// is confirmed to still be a non-symlink directory, and the ID is confirmed still
/// inactive. The whole root is re-inventoried after every deletion attempt so
/// hardlink release and races produce exact totals. The loop continues until both
/// enabled caps hold or candidates exhaust, including deleting an oversized newest
/// inactive directory when required.
#[cfg(unix)]
pub fn trim_cargo_target_runs_with_fs(
    root: &Path,
    active_ids: &HashSet<String>,
    caps: CargoTargetRunsCaps,
    fs: &dyn Filesystem,
) -> Result<CargoTargetRunsTrimResult, CargoTargetRunsInventoryError> {
    let (mut deleted, mut operation_errors) = (0_usize, 0_usize);
    let mut attempted = HashSet::<Vec<u8>>::new();
    loop {
        let inventory = inventory_cargo_target_runs(root)?;
        let errors = operation_errors.saturating_add(inventory.errors.len());
        let mut protected = inventory.protected.len()
            + usize::from(inventory.top_level_non_directory_allocated_bytes > 0);
        let within = (caps.max_dirs == 0 || inventory.top_level_directory_count <= caps.max_dirs)
            && (caps.max_bytes == 0 || inventory.total_allocated_bytes <= caps.max_bytes);
        if within {
            return Ok(CargoTargetRunsTrimResult {
                final_allocated_bytes: inventory.total_allocated_bytes,
                final_top_level_directory_count: inventory.top_level_directory_count,
                deleted,
                errors,
                protected,
                outcome: if deleted == 0 {
                    CargoTargetRunsTrimOutcome::WithinBudget
                } else {
                    CargoTargetRunsTrimOutcome::TrimmedWithinBudget
                },
            });
        }
        let candidate = inventory.candidates.iter().find(|candidate| {
            std::str::from_utf8(&candidate.name)
                .ok()
                .is_some_and(|id| !active_ids.contains(id))
                && !inventory
                    .errors
                    .iter()
                    .any(|issue| issue.top_level_name.as_deref() == Some(candidate.name.as_slice()))
                && !attempted.contains(&candidate.name)
        });
        let Some(candidate) = candidate else {
            protected = protected.saturating_add(
                inventory
                    .candidates
                    .iter()
                    .filter(|candidate| {
                        std::str::from_utf8(&candidate.name)
                            .ok()
                            .is_some_and(|id| active_ids.contains(id))
                    })
                    .count(),
            );
            return Ok(CargoTargetRunsTrimResult {
                final_allocated_bytes: inventory.total_allocated_bytes,
                final_top_level_directory_count: inventory.top_level_directory_count,
                deleted,
                errors,
                protected,
                outcome: over_budget_outcome(protected > 0, errors > 0),
            });
        };
        let name = candidate.name.clone();
        attempted.insert(name.clone());
        let path = root.join(std::ffi::OsString::from_vec(name.clone()));
        let id = std::str::from_utf8(&name).expect("inventory candidates are UTF-8");
        // Revalidate immediately before removal: the name must be a contained
        // single component, the ID must still be inactive, and the path must
        // still be a non-symlink directory. A race between the scan and removal
        // (TOCTOU) is fail-closed: a now-symlink or now-active entry is skipped.
        let removable = run_dir_within(root, id).as_deref() == Some(path.as_path())
            && !active_ids.contains(id)
            && fs
                .symlink_metadata(&path)
                .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                .unwrap_or_else(|_| {
                    operation_errors = operation_errors.saturating_add(1);
                    false
                });
        if !removable {
            // Revalidation failure is fail-closed; the next full scan records
            // the current protection/error state.
            continue;
        }
        match fs.remove_dir_all(&path) {
            Ok(()) => deleted += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => deleted += 1,
            Err(_) => operation_errors = operation_errors.saturating_add(1),
        }
    }
}
#[cfg(not(unix))]
pub fn trim_cargo_target_runs(
    _root: &Path,
    _active_ids: &std::collections::HashSet<String>,
    _caps: CargoTargetRunsCaps,
) -> Result<CargoTargetRunsTrimResult, CargoTargetRunsInventoryError> {
    Err(CargoTargetRunsInventoryError::UnsupportedPlatform)
}
fn over_budget_outcome(protected: bool, errors: bool) -> CargoTargetRunsTrimOutcome {
    match (protected, errors) {
        (true, true) => CargoTargetRunsTrimOutcome::OverBudgetProtectedAndError,
        (true, false) => CargoTargetRunsTrimOutcome::OverBudgetProtected,
        (false, _) => CargoTargetRunsTrimOutcome::OverBudgetError,
    }
}

#[cfg(test)]
mod tests {
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
        assert!(
            inventory.errors.iter().any(|issue| {
                issue.top_level_name.as_deref() == Some(candidate.name.as_slice())
            })
        );
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
}
