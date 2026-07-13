//! Cargo target-dir seeding for per-task-run worker Pods.
//!
//! The worker seeds a private run target dir from a warm per-project base target
//! dir before Cargo starts. The base is treated as read-only: this module only
//! reads metadata/bytes from it and hardlinks immutable artifacts into the run
//! dir. Mutable Cargo metadata is copied, and incremental state is skipped so it
//! is never shared by inode with the warm base.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;

const DEFAULT_PARALLELISM: usize = 12;
const MAX_PARALLELISM: usize = 64;
const PARALLELISM_ENV: &str = "DJINN_CARGO_TARGET_SEED_THREADS";

/// Filesystem convention for the warm, per-project Cargo target base.
pub const WARM_BASE_ROOT: &str = "/cache/cargo-target";

/// Filesystem convention for private, per-task-run Cargo target directories.
pub const RUN_TARGET_ROOT: &str = "/cache/cargo-target-runs";

/// Options controlling target-dir seed behavior.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CargoTargetSeedOptions {
    /// Bounded rayon worker count used for scanning and clone operations.
    pub parallelism: usize,
}

/// Outcome returned by worker teardown so logs can report observable cleanup
/// success/failure/count information for the private run dir.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CargoTargetTeardownResult {
    /// `true` when a run directory existed and was removed.
    pub removed: bool,
}

impl CargoTargetTeardownResult {
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

impl CargoTargetSeedOptions {
    /// Build options from environment, falling back to the production default.
    pub fn from_env() -> Self {
        let parallelism = std::env::var(PARALLELISM_ENV)
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(DEFAULT_PARALLELISM);
        Self::new(parallelism)
    }

    /// Build options with a bounded worker count.
    pub fn new(parallelism: usize) -> Self {
        Self {
            parallelism: parallelism.clamp(1, MAX_PARALLELISM),
        }
    }
}

impl Default for CargoTargetSeedOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Per-file clone decision for a Cargo target path.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CloneAction {
    /// Hardlink immutable/heavy artifacts from the warm base into the run dir.
    Hardlink,
    /// Byte-copy mutable metadata so the task run can safely rewrite it.
    Copy,
    /// Exclude state that must not be shared or does not need to be seeded.
    Skip,
}

/// Metrics and status returned to callers for structured logs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CargoTargetSeedResult {
    pub elapsed: Duration,
    pub linked_file_count: u64,
    pub copied_file_count: u64,
    pub skipped_file_count: u64,
    pub linked_bytes: u64,
    pub copied_bytes: u64,
    /// `Some` means the worker intentionally left a ready private run dir for a
    /// cold Cargo build instead of failing dispatch.
    pub fallback_reason: Option<CargoTargetSeedFallback>,
}

impl CargoTargetSeedResult {
    pub fn cold_started(&self) -> bool {
        self.fallback_reason.is_some()
    }
}

/// Reason target seeding degraded to a cold private target dir.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum CargoTargetSeedFallback {
    BaseMissing,
    BaseNotDirectory,
    BaseUnusable(String),
    ScanFailed(String),
    CloneFailed(String),
}

impl fmt::Display for CargoTargetSeedFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BaseMissing => f.write_str("base target dir is missing"),
            Self::BaseNotDirectory => f.write_str("base target path is not a directory"),
            Self::BaseUnusable(err) => write!(f, "base target dir is unusable: {err}"),
            Self::ScanFailed(err) => write!(f, "base target scan failed: {err}"),
            Self::CloneFailed(err) => write!(f, "base target clone failed: {err}"),
        }
    }
}

/// Derive the conventional warm base dir for a project id.
pub fn warm_base_dir(project_id: impl AsRef<str>) -> PathBuf {
    Path::new(WARM_BASE_ROOT).join(project_id.as_ref())
}

/// Derive the conventional private target dir for a task-run id.
pub fn run_target_dir(task_run_id: impl AsRef<str>) -> PathBuf {
    Path::new(RUN_TARGET_ROOT).join(task_run_id.as_ref())
}

/// Seed `run_dir` from `base_dir` using the default options.
pub fn seed_cargo_target_dir(
    base_dir: impl AsRef<Path>,
    run_dir: impl AsRef<Path>,
) -> io::Result<CargoTargetSeedResult> {
    seed_cargo_target_dir_with_options(base_dir, run_dir, &CargoTargetSeedOptions::default())
}

/// Seed `run_dir` from `base_dir` using bounded parallel Rust filesystem work.
///
/// Missing, invalid, or safely-detected clone failures return `Ok` with a
/// fallback reason so dispatch can proceed with a cold private target dir. The
/// only hard error is inability to prepare the destination directory itself.
pub fn seed_cargo_target_dir_with_options(
    base_dir: impl AsRef<Path>,
    run_dir: impl AsRef<Path>,
    options: &CargoTargetSeedOptions,
) -> io::Result<CargoTargetSeedResult> {
    let start = djinn_core::clock::Clock::now_instant(&djinn_core::clock::SystemClock::new());
    let base_dir = base_dir.as_ref();
    let run_dir = run_dir.as_ref();

    fs::create_dir_all(run_dir)?;

    let base_meta = match fs::metadata(base_dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(fallback_result(start, CargoTargetSeedFallback::BaseMissing));
        }
        Err(err) => {
            return Ok(fallback_result(
                start,
                CargoTargetSeedFallback::BaseUnusable(err.to_string()),
            ));
        }
    };

    if !base_meta.is_dir() {
        return Ok(fallback_result(
            start,
            CargoTargetSeedFallback::BaseNotDirectory,
        ));
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.parallelism.clamp(1, MAX_PARALLELISM))
        .thread_name(|idx| format!("cargo-target-seed-{idx}"))
        .build()
        .map_err(|err| io::Error::other(format!("build cargo target seed pool: {err}")))?;

    let entries = match pool.install(|| scan_entries(base_dir)) {
        Ok(entries) => entries,
        Err(err) => {
            return Ok(fallback_result(
                start,
                CargoTargetSeedFallback::ScanFailed(err.to_string()),
            ));
        }
    };

    let metrics = Arc::new(SeedCounters::default());
    let first_error = Arc::new(Mutex::new(None::<String>));

    pool.install(|| {
        entries.par_iter().for_each(|entry| {
            if first_error.lock().is_ok_and(|guard| guard.is_some()) {
                return;
            }

            if let Err(err) = apply_entry(base_dir, run_dir, entry, &metrics)
                && let Ok(mut guard) = first_error.lock()
                && guard.is_none()
            {
                *guard = Some(err.to_string());
            }
        });
    });

    if let Some(err) = first_error.lock().ok().and_then(|guard| guard.clone()) {
        let mut result = metrics.result(start.elapsed(), None);
        let reason = CargoTargetSeedFallback::CloneFailed(err);
        emit_cargo_target_seed_fallback_metric(&reason);
        result.fallback_reason = Some(reason);
        return Ok(result);
    }

    Ok(metrics.result(start.elapsed(), None))
}

/// Remove a private per-task-run Cargo target dir on worker teardown.
///
/// Teardown is intentionally best-effort for the normal already-cleaned-up
/// case: a missing run dir is considered success so terminal-report paths can
/// call this helper unconditionally.
pub fn teardown_run_dir(run_dir: impl AsRef<Path>) -> io::Result<CargoTargetTeardownResult> {
    match fs::remove_dir_all(run_dir.as_ref()) {
        Ok(()) => Ok(CargoTargetTeardownResult { removed: true }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            Ok(CargoTargetTeardownResult { removed: false })
        }
        Err(err) => Err(err),
    }
}

/// Classify a Cargo target relative path.
///
/// The helper is intentionally pure for unit coverage and later callers. It is
/// valid for both file and directory paths; directories classified as `Skip`
/// should not be descended into.
pub fn classify_cargo_target_path(relative_path: &Path) -> CloneAction {
    if has_component(relative_path, "incremental") {
        return CloneAction::Skip;
    }

    // Cargo's per-profile lock files (`debug/.cargo-lock`,
    // `debug/.cargo-build-lock`, `debug/.cargo-artifact-lock`, and the `release/`
    // equivalents — one set per profile dir at any depth). File locks (flock)
    // attach to the INODE, not the path, so a hardlinked lock file makes every
    // seeded run's `cargo` serialize against the warm base's `cargo` — and
    // against every sibling run seeded from the same base file — across pods via
    // the shared PVC. That is a platform-wide global build mutex: one incident
    // showed 8 cargo processes across 7 task-run pods + the warm Job all queued
    // (wchan=locks_lock_inode_wait) on a single inode with links=8. Cargo
    // recreates these locks on demand, so seeding them is pointless and
    // hardlinking them is the bug. Match by name at any depth: `.cargo` prefix +
    // `lock` substring covers today's three variants and any future
    // `.cargo-*-lock` cargo adds, while excluding in-place-rewritten `.cargo`
    // metadata that is not a lock. Never seed a cargo lock.
    if file_name_is_cargo_lock(relative_path) {
        return CloneAction::Skip;
    }

    if has_component(relative_path, ".fingerprint")
        || relative_path.extension().is_some_and(|ext| ext == "d")
    {
        return CloneAction::Copy;
    }

    // `.rustc_info.json` (rustc-version cache at the target root) is rewritten
    // IN PLACE by cargo's `paths::write` (`File::create` truncates the existing
    // inode; it is not an atomic tmp+rename) whenever a new rustc invocation is
    // cached. Verified empirically: a hardlinked copy shares the inode, so a
    // regenerate writes through and corrupts the shared warm base. Copy it so
    // each run mutates a private inode while still inheriting the cached value.
    if file_name_is(relative_path, ".rustc_info.json") {
        return CloneAction::Copy;
    }

    CloneAction::Hardlink
}

fn fallback_result(start: Instant, reason: CargoTargetSeedFallback) -> CargoTargetSeedResult {
    emit_cargo_target_seed_fallback_metric(&reason);

    CargoTargetSeedResult {
        elapsed: start.elapsed(),
        linked_file_count: 0,
        copied_file_count: 0,
        skipped_file_count: 0,
        linked_bytes: 0,
        copied_bytes: 0,
        fallback_reason: Some(reason),
    }
}

fn emit_cargo_target_seed_fallback_metric(reason: &CargoTargetSeedFallback) {
    djinn_telemetry::cargo_target_seed::increment_seed_fallback(
        cargo_target_seed_fallback_metric_reason(reason),
    );
}

fn cargo_target_seed_fallback_metric_reason(reason: &CargoTargetSeedFallback) -> &'static str {
    match reason {
        CargoTargetSeedFallback::BaseMissing => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING
        }
        CargoTargetSeedFallback::BaseNotDirectory => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY
        }
        CargoTargetSeedFallback::BaseUnusable(_) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE
        }
        CargoTargetSeedFallback::ScanFailed(_) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_SCAN_FAILED
        }
        CargoTargetSeedFallback::CloneFailed(_) => {
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_CLONE_FAILED
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SeedEntry {
    relative_path: PathBuf,
    action: CloneAction,
    bytes: u64,
    is_dir: bool,
}

#[derive(Debug, Default)]
struct DirScan {
    entries: Vec<SeedEntry>,
    next_dirs: Vec<PathBuf>,
}

fn scan_entries(base_dir: &Path) -> io::Result<Vec<SeedEntry>> {
    let mut all_entries = Vec::new();
    let mut pending_dirs = vec![PathBuf::new()];

    while !pending_dirs.is_empty() {
        let scans: Vec<io::Result<DirScan>> = pending_dirs
            .par_iter()
            .map(|relative_dir| scan_dir(base_dir, relative_dir))
            .collect();

        pending_dirs = Vec::new();
        for scan in scans {
            let mut scan = scan?;
            all_entries.append(&mut scan.entries);
            pending_dirs.append(&mut scan.next_dirs);
        }
    }

    Ok(all_entries)
}

fn scan_dir(base_dir: &Path, relative_dir: &Path) -> io::Result<DirScan> {
    let mut scan = DirScan::default();
    let absolute_dir = base_dir.join(relative_dir);

    for entry in fs::read_dir(&absolute_dir)? {
        let entry = entry?;
        let relative_path = relative_dir.join(entry.file_name());
        let metadata = entry.metadata()?;
        let action = classify_cargo_target_path(&relative_path);

        if metadata.is_dir() {
            if action == CloneAction::Skip {
                scan.entries.push(SeedEntry {
                    relative_path,
                    action,
                    bytes: 0,
                    is_dir: true,
                });
            } else {
                scan.entries.push(SeedEntry {
                    relative_path: relative_path.clone(),
                    action,
                    bytes: 0,
                    is_dir: true,
                });
                scan.next_dirs.push(relative_path);
            }
        } else if metadata.is_file() {
            scan.entries.push(SeedEntry {
                relative_path,
                action,
                bytes: metadata.len(),
                is_dir: false,
            });
        } else {
            scan.entries.push(SeedEntry {
                relative_path,
                action: CloneAction::Skip,
                bytes: 0,
                is_dir: false,
            });
        }
    }

    Ok(scan)
}

fn apply_entry(
    base_dir: &Path,
    run_dir: &Path,
    entry: &SeedEntry,
    metrics: &SeedCounters,
) -> io::Result<()> {
    match entry.action {
        CloneAction::Skip => {
            metrics.skipped_file_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        CloneAction::Hardlink | CloneAction::Copy if entry.is_dir => {
            fs::create_dir_all(run_dir.join(&entry.relative_path))
        }
        CloneAction::Hardlink => {
            let source = base_dir.join(&entry.relative_path);
            let destination = run_dir.join(&entry.relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::hard_link(source, destination)?;
            metrics.linked_file_count.fetch_add(1, Ordering::Relaxed);
            metrics
                .linked_bytes
                .fetch_add(entry.bytes, Ordering::Relaxed);
            Ok(())
        }
        CloneAction::Copy => {
            let source = base_dir.join(&entry.relative_path);
            let destination = run_dir.join(&entry.relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let copied = fs::copy(source, destination)?;
            metrics.copied_file_count.fetch_add(1, Ordering::Relaxed);
            metrics.copied_bytes.fetch_add(copied, Ordering::Relaxed);
            Ok(())
        }
    }
}

#[derive(Debug, Default)]
struct SeedCounters {
    linked_file_count: AtomicU64,
    copied_file_count: AtomicU64,
    skipped_file_count: AtomicU64,
    linked_bytes: AtomicU64,
    copied_bytes: AtomicU64,
}

impl SeedCounters {
    fn result(
        &self,
        elapsed: Duration,
        fallback_reason: Option<CargoTargetSeedFallback>,
    ) -> CargoTargetSeedResult {
        CargoTargetSeedResult {
            elapsed,
            linked_file_count: self.linked_file_count.load(Ordering::Relaxed),
            copied_file_count: self.copied_file_count.load(Ordering::Relaxed),
            skipped_file_count: self.skipped_file_count.load(Ordering::Relaxed),
            linked_bytes: self.linked_bytes.load(Ordering::Relaxed),
            copied_bytes: self.copied_bytes.load(Ordering::Relaxed),
            fallback_reason,
        }
    }
}

fn has_component(path: &Path, needle: &str) -> bool {
    path.components().any(|component| match component {
        Component::Normal(part) => part == needle,
        _ => false,
    })
}

fn file_name_is(path: &Path, needle: &str) -> bool {
    path.file_name().is_some_and(|name| name == needle)
}

/// Match any of cargo's per-profile lock files by file name at any depth.
///
/// Covers `.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock` (empirically
/// the full set emitted by the pinned toolchain) and any future `.cargo-*-lock`
/// variant, while deliberately NOT matching in-place-rewritten `.cargo` metadata
/// that is not a lock. Kept name-based so it fires regardless of profile dir or
/// target-triple nesting.
fn file_name_is_cargo_lock(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".cargo") && name.contains("lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::sync::MutexGuard;

    const CARGO_TARGET_SEED_TOTAL: &str = "djinn_cargo_target_seed_total";

    #[test]
    fn classifies_heavy_artifacts_for_hardlink() {
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/deps/libserde-abc.rlib")),
            CloneAction::Hardlink
        );
        assert_eq!(
            classify_cargo_target_path(Path::new("release/build/foo/out/generated.rs")),
            CloneAction::Hardlink
        );
    }

    #[test]
    fn classifies_fingerprint_and_dep_info_for_copy() {
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/.fingerprint/foo/lib-foo.json")),
            CloneAction::Copy
        );
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/deps/foo.d")),
            CloneAction::Copy
        );
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/build/foo/output.d")),
            CloneAction::Copy
        );
    }

    #[test]
    fn classifies_build_directory_lock_for_skip() {
        // Cargo's per-profile lock files must never be hardlinked: flock attaches
        // to the inode, so a shared lock serializes `cargo` across every run and
        // the warm base via the shared PVC. All three variants emitted by the
        // pinned toolchain (`.cargo-lock`, `.cargo-build-lock`,
        // `.cargo-artifact-lock`) must be skipped in both `debug/` and `release/`.
        for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            assert_eq!(
                classify_cargo_target_path(Path::new(&format!("debug/{name}"))),
                CloneAction::Skip,
                "debug/{name} must be skipped"
            );
            assert_eq!(
                classify_cargo_target_path(Path::new(&format!("release/{name}"))),
                CloneAction::Skip,
                "release/{name} must be skipped"
            );
            // Root-level and target-triple-nested variants are skipped by name.
            assert_eq!(
                classify_cargo_target_path(Path::new(name)),
                CloneAction::Skip,
                "root-level {name} must be skipped"
            );
            assert_eq!(
                classify_cargo_target_path(Path::new(&format!(
                    "x86_64-unknown-linux-gnu/debug/{name}"
                ))),
                CloneAction::Skip,
                "target-triple-nested {name} must be skipped"
            );
        }
    }

    #[test]
    fn cargo_lock_matcher_excludes_non_lock_cargo_metadata() {
        // The matcher is `.cargo` prefix + `lock` substring, so `.cargo` files
        // that are NOT locks (e.g. a hypothetical in-place-rewritten metadata
        // file) must fall through to their normal classification rather than
        // being wrongly skipped.
        assert_ne!(
            classify_cargo_target_path(Path::new("debug/.cargo-metadata.json")),
            CloneAction::Skip
        );
        assert_ne!(
            classify_cargo_target_path(Path::new(".rustc_info.json")),
            CloneAction::Skip
        );
    }

    #[test]
    fn classifies_rustc_info_cache_for_copy() {
        // `.rustc_info.json` is rewritten in place by cargo, so a hardlink would
        // write through to the shared warm base. Copy gives each run a private
        // inode while inheriting the cached value.
        assert_eq!(
            classify_cargo_target_path(Path::new(".rustc_info.json")),
            CloneAction::Copy
        );
    }

    #[test]
    fn classifies_incremental_for_skip_even_under_fingerprint() {
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/incremental/foo/s-cache.bin")),
            CloneAction::Skip
        );
        assert_eq!(
            classify_cargo_target_path(Path::new("debug/.fingerprint/incremental/foo.d")),
            CloneAction::Skip
        );
    }

    #[test]
    fn missing_base_returns_cold_start_and_prepares_run_dir() {
        let _guard = metric_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("missing-base");
        let run = tmp.path().join("run-target");

        let result =
            seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
                .expect("missing base should not fail dispatch");

        assert_eq!(
            result.fallback_reason,
            Some(CargoTargetSeedFallback::BaseMissing)
        );
        assert!(result.cold_started());
        assert!(run.is_dir());
        assert_eq!(result.linked_file_count, 0);
        assert_eq!(result.copied_file_count, 0);
        assert_eq!(result.skipped_file_count, 0);
    }

    #[test]
    fn non_directory_base_returns_cold_start() {
        let _guard = metric_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("base-file");
        let run = tmp.path().join("run-target");
        fs::write(&base, b"not a directory").expect("write base file");

        let result =
            seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
                .expect("non-directory base should not fail dispatch");

        assert_eq!(
            result.fallback_reason,
            Some(CargoTargetSeedFallback::BaseNotDirectory)
        );
        assert!(run.is_dir());
    }

    #[test]
    fn seeds_target_tree_with_required_clone_semantics() {
        let _guard = metric_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("warm-base");
        let run = tmp.path().join("run-target");

        let heavy = Path::new("debug/deps/libfoo.rlib");
        let fingerprint = Path::new("debug/.fingerprint/foo-abc/invoked.timestamp");
        let dep_info = Path::new("debug/deps/foo.d");
        let incremental = Path::new("debug/incremental/foo-abc/s-cache.bin");
        // Every cargo lock variant, in both profile dirs, must be skipped.
        let lock_files = [
            Path::new("debug/.cargo-lock"),
            Path::new("debug/.cargo-build-lock"),
            Path::new("debug/.cargo-artifact-lock"),
            Path::new("release/.cargo-lock"),
            Path::new("release/.cargo-build-lock"),
            Path::new("release/.cargo-artifact-lock"),
        ];
        let rustc_info = Path::new(".rustc_info.json");

        write_base_file(&base, heavy, b"large immutable artifact");
        write_base_file(&base, fingerprint, b"fingerprint metadata");
        write_base_file(&base, dep_info, b"dep-info metadata");
        write_base_file(&base, incremental, b"incremental state");
        for lock in lock_files {
            write_base_file(&base, lock, b"");
        }
        write_base_file(&base, rustc_info, b"rustc info cache");

        let result =
            seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(4))
                .expect("seed target dir");

        assert_eq!(result.fallback_reason, None);
        assert_eq!(result.linked_file_count, 1);
        // fingerprint + dep-info + .rustc_info.json are byte-copied.
        assert_eq!(result.copied_file_count, 3);
        assert!(
            result.skipped_file_count > lock_files.len() as u64,
            "incremental state and every cargo lock variant should be skipped"
        );

        assert_eq!(
            fs::read(run.join(heavy)).expect("read linked artifact"),
            b"large immutable artifact"
        );
        assert_eq!(
            fs::read(run.join(fingerprint)).expect("read copied fingerprint"),
            b"fingerprint metadata"
        );
        assert_eq!(
            fs::read(run.join(dep_info)).expect("read copied dep-info"),
            b"dep-info metadata"
        );
        assert!(
            !run.join(incremental).exists(),
            "incremental state must not be seeded into the private run dir"
        );
        assert!(
            !run.join("debug/incremental").exists(),
            "incremental directories must be skipped before descent"
        );
        for lock in lock_files {
            assert!(
                !run.join(lock).exists(),
                "cargo lock file {} must not be seeded into the private run dir",
                lock.display()
            );
        }
        assert_eq!(
            fs::read(run.join(rustc_info)).expect("read copied rustc info"),
            b"rustc info cache"
        );

        #[cfg(unix)]
        {
            assert_same_inode(&base.join(heavy), &run.join(heavy));
            assert_different_inode(&base.join(fingerprint), &run.join(fingerprint));
            assert_different_inode(&base.join(dep_info), &run.join(dep_info));
            // No seeded file may share an inode with ANY base file whose name
            // matches the cargo lock pattern: flock is inode-scoped, so a shared
            // inode would make this run's `cargo` serialize against the base and
            // every sibling. Each lock is skipped entirely, so it must simply be
            // absent from the run dir.
            let base_lock_inodes: std::collections::HashSet<(u64, u64)> = lock_files
                .iter()
                .map(|lock| {
                    let meta = fs::metadata(base.join(lock)).expect("base lock metadata");
                    (meta.dev(), meta.ino())
                })
                .collect();
            for entry in walk_files(&run) {
                let meta = fs::metadata(&entry).expect("run file metadata");
                assert!(
                    !base_lock_inodes.contains(&(meta.dev(), meta.ino())),
                    "seeded file {} shares an inode with a base cargo lock file",
                    entry.display()
                );
            }
            // `.rustc_info.json` is copied, so the run owns a private inode and a
            // cargo rewrite cannot corrupt the shared warm base.
            assert_different_inode(&base.join(rustc_info), &run.join(rustc_info));
        }
    }

    #[test]
    fn emits_fallback_metric_with_bounded_reason_label() {
        let _guard = metric_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("missing-base");
        let run = tmp.path().join("run-target");
        let reason = djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING;
        let before = fallback_metric_value(reason);

        let result =
            seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
                .expect("missing base should not fail dispatch");

        assert_eq!(
            result.fallback_reason,
            Some(CargoTargetSeedFallback::BaseMissing)
        );
        let after = fallback_metric_value(reason);
        assert_eq!(
            after,
            before + 1.0,
            "fallback metric should increment exactly once for BaseMissing"
        );
    }

    #[test]
    fn successful_seed_does_not_emit_fallback_metric() {
        let _guard = metric_test_guard();
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("warm-base");
        let run = tmp.path().join("run-target");
        write_base_file(
            &base,
            Path::new("debug/deps/libsuccess.rlib"),
            b"seeded artifact",
        );
        let before = total_fallback_metric_value();

        let result =
            seed_cargo_target_dir_with_options(&base, &run, &CargoTargetSeedOptions::new(2))
                .expect("seed target dir");

        assert_eq!(result.fallback_reason, None);
        assert_eq!(total_fallback_metric_value(), before);
    }

    #[test]
    fn teardown_run_dir_removes_private_dir_and_ignores_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run = tmp.path().join("run-target");
        fs::create_dir_all(run.join("debug/deps")).expect("create run tree");
        fs::write(run.join("debug/deps/libfoo.rlib"), b"artifact").expect("write run file");

        let removed = teardown_run_dir(&run).expect("remove run dir");
        assert_eq!(removed.outcome(), "removed");
        assert_eq!(removed.removed_count(), 1);
        assert!(!run.exists());

        let missing = teardown_run_dir(&run).expect("missing run dir should be non-fatal");
        assert_eq!(missing.outcome(), "already_absent");
        assert_eq!(missing.removed_count(), 0);
    }

    /// Recursively collect every regular file under `root` (test-only helper for
    /// the inode-level lock-sharing assertion).
    fn walk_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).expect("entry metadata");
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    files.push(path);
                }
            }
        }
        files
    }

    fn write_base_file(base: &Path, relative: &Path, contents: &[u8]) {
        let path = base.join(relative);
        fs::create_dir_all(path.parent().expect("relative path parent")).expect("create parent");
        fs::write(path, contents).expect("write base file");
    }

    fn metric_test_guard() -> MutexGuard<'static, ()> {
        crate::tests::seed_telemetry_guard()
    }

    fn total_fallback_metric_value() -> f64 {
        [
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_MISSING,
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY,
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE,
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_SCAN_FAILED,
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_CLONE_FAILED,
            djinn_telemetry::cargo_target_seed::FALLBACK_REASON_UNKNOWN,
        ]
        .into_iter()
        .map(fallback_metric_value)
        .sum()
    }

    fn fallback_metric_value(reason: &str) -> f64 {
        djinn_telemetry::render()
            .expect("render telemetry")
            .lines()
            .find_map(|line| {
                let (sample, value) = line.rsplit_once(' ')?;
                if sample.starts_with(CARGO_TARGET_SEED_TOTAL)
                    && sample.contains("outcome=\"fallback\"")
                    && sample.contains(&format!("fallback_reason=\"{reason}\""))
                {
                    value.parse::<f64>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0.0)
    }

    #[cfg(unix)]
    fn assert_same_inode(left: &Path, right: &Path) {
        let left = fs::metadata(left).expect("left metadata");
        let right = fs::metadata(right).expect("right metadata");
        assert_eq!((left.dev(), left.ino()), (right.dev(), right.ino()));
    }

    #[cfg(unix)]
    fn assert_different_inode(left: &Path, right: &Path) {
        let left = fs::metadata(left).expect("left metadata");
        let right = fs::metadata(right).expect("right metadata");
        assert_ne!((left.dev(), left.ino()), (right.dev(), right.ino()));
    }
}
