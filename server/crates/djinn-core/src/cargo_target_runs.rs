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
    /// Number of non-directory entries directly under the runs root. Unlike
    /// allocated bytes, this is not inode-deduplicated: every such top-level
    /// entry is independently non-removable and therefore protected.
    pub top_level_non_directory_count: usize,
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
        if !metadata.is_dir() {
            inventory.top_level_non_directory_count =
                inventory.top_level_non_directory_count.saturating_add(1);
        }
        if metadata.file_type().is_symlink() {
            inventory.protected.push(InventoryIssue {
                top_level_name: Some(raw_name),
                kind: InventoryIssueKind::TopLevelSymlink,
            });
        } else if metadata.is_dir() {
            inventory.top_level_directory_count += 1;
            let valid_contained_name = entry.file_name().to_str().is_some_and(|name| {
                run_dir_within(root, name).as_deref() == Some(entry.path().as_path())
            });
            if !valid_contained_name {
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
        let mut protected = protected_entry_count(&inventory);
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

/// Count protected top-level entries exactly. Top-level symlinks are represented
/// both in `protected` and in the non-directory entry count, so subtract their
/// issue records before adding all non-directory entries to avoid double-counting.
#[cfg(unix)]
fn protected_entry_count(inventory: &CargoTargetRunsInventory) -> usize {
    let top_level_symlinks = inventory
        .protected
        .iter()
        .filter(|issue| issue.kind == InventoryIssueKind::TopLevelSymlink)
        .count();
    inventory
        .protected
        .len()
        .saturating_sub(top_level_symlinks)
        .saturating_add(inventory.top_level_non_directory_count)
}

#[cfg(test)]
#[path = "cargo_target_runs_tests.rs"]
mod tests;
