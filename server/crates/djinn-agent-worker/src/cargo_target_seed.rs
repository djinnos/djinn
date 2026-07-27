//! Cargo target-dir seeding for per-task-run worker Pods.
//!
//! The worker seeds a private run target dir from a warm per-project base target
//! dir before Cargo starts. The base is treated as read-only: this module only
//! reads metadata/bytes from it and hardlinks immutable artifacts into the run
//! dir. Mutable Cargo metadata is copied, and incremental state is skipped so it
//! is never shared by inode with the warm base.
//!
//! # Only immutable payloads may be hardlinked
//!
//! The rule that governs [`classify_cargo_target_path`] is: **anything the run
//! may rewrite must be copied, not linked.** A hardlink shares the inode with the
//! warm base, so a rewrite in the private run dir lands THROUGH the link into the
//! shared base that every other task-run pod seeds from. Cargo's fingerprints
//! still consider the damaged units fresh, so nothing regenerates them and the
//! base stops re-converging.
//!
//! Four classes are known to be rewritten in place and are carved out for that
//! reason: cargo's per-profile lock files, `.rustc_info.json`, build-script
//! `OUT_DIR` payloads, and the build-script stamp files. Adding a fifth means
//! adding it here — the default arm is `Hardlink`, so an omission is silent.
//!
//! # Identities involved
//!
//! Four distinct uids touch this tree, and conflating any two of them produces a
//! wrong conclusion about who can write what:
//!
//! | role                          | uid     | source                              |
//! |-------------------------------|---------|-------------------------------------|
//! | on-disk warm base owner       | `10001` | legacy; predates the warm pod move  |
//! | warm Job pod                   | `1000`  | `warm_job.rs` (`WORKER_UID`)        |
//! | task-run worker (this seeder) | `1000`  | `WORKER_UID`                        |
//! | cargo and its build scripts   | `1001`  | `CHILD_UID`, gid `ARTIFACT_GID` 1000|
//!
//! The base's *bytes* were written by a warm pod that today runs as 1000, but the
//! *inodes* on disk are still owned by the legacy 10001 from before that move.
//! Cargo never runs as the seeder: the launcher `setresuid`s to `CHILD_UID` 1001,
//! and the two share only gid 1000. So a seeded entry is writable by cargo when
//! its GROUP bits allow it, never because the seeder created it.
//!
//! # Why a single entry must never discard the base
//!
//! With `fs.protected_hardlinks=1` the kernel's `safe_hardlink_source()` refuses
//! `linkat()` with **EPERM** for any source the calling process does not own
//! unless it is a *group-writable regular file*. Measured against a 6.x kernel
//! with a base owned by `10001:1000` and a process running `1000:1000`:
//!
//! | source            | `linkat` | `copy` |
//! |-------------------|----------|--------|
//! | regular `0664`    | ok       | ok     |
//! | regular `0775`    | ok       | ok     |
//! | regular `0644`    | EPERM    | ok     |
//! | regular `0755`    | EPERM    | ok     |
//! | regular `0444`    | EPERM    | ok     |
//! | regular `0600`    | EPERM    | EACCES |
//! | symlink `0777`    | EPERM    | ok     |
//!
//! A warm base built under `umask 002` is overwhelmingly `0664`/`0775`, but any
//! file a build script *copies* keeps its SOURCE mode regardless of umask — and
//! the Cargo registry ships `0644`/`0640`/`0600` sources. So a 38k-file base
//! reliably contains a handful of entries the kernel will not let the worker
//! hardlink, while the same entries copy fine.
//!
//! Consequently this module treats an unlinkable entry as a *degradation*, not
//! as a reason to abandon a 36G base: it falls back to a bounded byte copy, and
//! only reports a cold fallback when the base yielded nothing at all.

use std::fmt;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rayon::prelude::*;

const DEFAULT_PARALLELISM: usize = 12;
const MAX_PARALLELISM: usize = 64;
const PARALLELISM_ENV: &str = "DJINN_CARGO_TARGET_SEED_THREADS";

/// Bytes this seed may spend byte-copying artifacts the kernel refused to
/// hardlink, before it stops substituting and starts leaving them unseeded.
///
/// A copy is always a *correct* substitute for a hardlink (see
/// [`CloneAction::Hardlink`]) but it costs both wall clock and private disk in
/// `/cache/cargo-target-runs`, which has caused node DiskPressure before. The
/// budget is sized to absorb the anomalous minority of a warm base (build-script
/// `OUT_DIR` payloads and copied registry sources) without ever approaching a
/// full byte-for-byte duplication of a multi-tens-of-GiB base.
const DEFAULT_LINK_FALLBACK_COPY_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const LINK_FALLBACK_COPY_BUDGET_ENV: &str = "DJINN_CARGO_TARGET_SEED_LINK_FALLBACK_COPY_BYTES";

/// Filesystem convention for the warm, per-project Cargo target base.
///
/// Resolved from the cache-root manifest rather than written out, so this root
/// cannot exist without a manifest entry describing it.
pub const WARM_BASE_ROOT: &str = djinn_core::paths::CacheRootId::CargoTarget.job_pod_path();

/// Existing pod environment selector for the Cargo/mold parallelism variant.
pub const CARGO_BUILD_JOBS_ENV: &str = "CARGO_BUILD_JOBS";

/// Filesystem convention for private, per-task-run Cargo target directories.
///
/// Resolved from the cache-root manifest, like [`WARM_BASE_ROOT`].
pub const RUN_TARGET_ROOT: &str = djinn_core::paths::CacheRootId::CargoTargetRuns.job_pod_path();

/// Implementation used to hardlink one base artifact into the run dir.
///
/// Production always uses the real `linkat`. The test variant reproduces the
/// kernel's `safe_hardlink_source()` refusal deterministically, because CI
/// runners cannot create a base owned by a *different* uid (that needs root)
/// and a test running as the base owner never observes the refusal at all.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum LinkBackend {
    /// `std::fs::hard_link`, i.e. `linkat(..., 0)`.
    #[default]
    Native,
    /// Test-only: refuse exactly what `fs.protected_hardlinks=1` refuses for a
    /// source owned by another uid — anything that is not a group-writable
    /// regular file — and let everything else link natively.
    #[cfg(test)]
    RefuseSourcesForeignOwnersCannotLink,
}

/// Options controlling target-dir seed behavior.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CargoTargetSeedOptions {
    /// Bounded rayon worker count used for scanning and clone operations.
    pub parallelism: usize,
    /// Byte budget for copying artifacts that could not be hardlinked.
    pub link_fallback_copy_budget_bytes: u64,
    /// Hardlink implementation; see [`LinkBackend`].
    pub link_backend: LinkBackend,
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
        let link_fallback_copy_budget_bytes = std::env::var(LINK_FALLBACK_COPY_BUDGET_ENV)
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(DEFAULT_LINK_FALLBACK_COPY_BUDGET_BYTES);
        Self {
            link_fallback_copy_budget_bytes,
            ..Self::new(parallelism)
        }
    }

    /// Build options with a bounded worker count.
    pub fn new(parallelism: usize) -> Self {
        Self {
            parallelism: parallelism.clamp(1, MAX_PARALLELISM),
            link_fallback_copy_budget_bytes: DEFAULT_LINK_FALLBACK_COPY_BUDGET_BYTES,
            link_backend: LinkBackend::Native,
        }
    }

    /// Override the copy budget used when a hardlink is refused. Production
    /// configures this through [`LINK_FALLBACK_COPY_BUDGET_ENV`].
    #[cfg(test)]
    #[must_use]
    pub fn with_link_fallback_copy_budget_bytes(mut self, bytes: u64) -> Self {
        self.link_fallback_copy_budget_bytes = bytes;
        self
    }

    /// Override the hardlink implementation; see [`LinkBackend`].
    #[cfg(test)]
    #[must_use]
    pub fn with_link_backend(mut self, backend: LinkBackend) -> Self {
        self.link_backend = backend;
        self
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
    ///
    /// A byte copy is always a semantically valid substitute: hardlinking is
    /// purely an optimisation for immutable payloads, and a private copy is if
    /// anything safer because the run can never write through to the base.
    Hardlink,
    /// Byte-copy mutable metadata so the task run can safely rewrite it.
    Copy,
    /// Exclude state that must not be shared or does not need to be seeded.
    Skip,
}

/// Filesystem operation attempted for one seed entry.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SeedOp {
    CreateDir,
    Hardlink,
    Copy,
}

impl SeedOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateDir => "create_dir",
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
        }
    }
}

/// One entry-level seed failure, carrying the operation, the exact path and the
/// ownership/mode facts that explain a kernel refusal.
///
/// The production incident this type exists for reported only
/// `Operation not permitted (os error 1)` with no path and no operation, which
/// cost hours and a live production Pod to localise.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SeedEntryError {
    pub op: SeedOp,
    pub relative_path: PathBuf,
    pub message: String,
    /// Ownership/mode of the source plus this process's identity, when readable.
    pub identity: Option<String>,
    /// Why the kernel plausibly refused, when the errno says so.
    pub hint: Option<&'static str>,
    /// Failure of the copy substituted for a refused hardlink, when attempted.
    pub fallback_message: Option<String>,
}

impl fmt::Display for SeedEntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.op.as_str(),
            self.relative_path.display(),
            self.message
        )?;
        if let Some(identity) = &self.identity {
            write!(f, " [{identity}]")?;
        }
        if let Some(hint) = self.hint {
            write!(f, " ({hint})")?;
        }
        if let Some(fallback) = &self.fallback_message {
            write!(f, "; copy fallback also failed: {fallback}")?;
        }
        Ok(())
    }
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
    /// Files the kernel refused to hardlink that were byte-copied instead.
    pub degraded_link_file_count: u64,
    /// Files that could be neither linked nor copied, and are simply absent
    /// from the run dir. Cargo rebuilds the corresponding units.
    pub unseeded_file_count: u64,
    /// Seedable (non-skipped) regular files found in the base. `0` with no
    /// fallback reason means the base was genuinely empty.
    pub base_seedable_file_count: u64,
    /// `true` when the link-fallback copy budget stopped further substitution.
    pub link_fallback_budget_exhausted: bool,
    /// First entry-level failure, rendered with operation, path and identity.
    pub first_entry_error: Option<String>,
    /// `Some` means the worker intentionally left a ready private run dir for a
    /// cold Cargo build instead of failing dispatch.
    pub fallback_reason: Option<CargoTargetSeedFallback>,
}

impl CargoTargetSeedResult {
    pub fn cold_started(&self) -> bool {
        self.fallback_reason.is_some()
    }

    /// `true` when the base was used but not in full. A partial seed is a hit:
    /// Cargo treats a missing artifact as a dirty unit and rebuilds just that
    /// unit, so partial seeding is always correct and proportionally faster.
    pub fn degraded(&self) -> bool {
        !self.cold_started() && (self.degraded_link_file_count > 0 || self.unseeded_file_count > 0)
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

/// Derive a positive mold-job variant from the pod's existing Cargo setting.
/// Missing, zero, and malformed values deliberately fail closed to variant one.
pub fn cargo_build_jobs_variant() -> usize {
    std::env::var(CARGO_BUILD_JOBS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|&jobs| jobs > 0)
        .unwrap_or(1)
}

/// Derive the canonical warm base for an explicit mold-job variant.
///
/// Unlike [`warm_base_dir`], this never selects the legacy unkeyed project
/// directory, so callers cannot substitute a sibling cache variant.
pub fn warm_base_dir_for_jobs(project_id: impl AsRef<str>, jobs: usize) -> PathBuf {
    warm_base_dir_for_jobs_at_root(Path::new(WARM_BASE_ROOT), project_id, jobs)
}

/// Derive a variant warm base below an explicit root. This is useful for
/// isolated filesystem tests while retaining the same canonical layout.
pub fn warm_base_dir_for_jobs_at_root(
    warm_root: impl AsRef<Path>,
    project_id: impl AsRef<str>,
    jobs: usize,
) -> PathBuf {
    warm_root
        .as_ref()
        .join(project_id.as_ref())
        .join(format!("mold-jobs-{}", jobs.max(1)))
}

/// Derive the canonical warm base for this pod's existing Cargo job variant.
pub fn warm_base_dir_for_current_jobs(project_id: impl AsRef<str>) -> PathBuf {
    warm_base_dir_for_jobs(project_id, cargo_build_jobs_variant())
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
///
/// Per-entry failures never abort the seed. See the module docs: an entry the
/// kernel refuses to hardlink is byte-copied within a budget, and an entry that
/// can be neither linked nor copied is left absent and counted. A cold fallback
/// is reported only when the base yielded nothing at all.
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
                CargoTargetSeedFallback::BaseUnusable(format!(
                    "stat {}: {err}",
                    base_dir.display()
                )),
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
        // The only scan error that reaches here is an unreadable base ROOT;
        // every deeper failure now costs at most its own subtree.
        Err(err) => {
            return Ok(fallback_result(
                start,
                CargoTargetSeedFallback::ScanFailed(format!(
                    "read_dir {}: {err}",
                    base_dir.display()
                )),
            ));
        }
    };

    let base_seedable_file_count = entries
        .iter()
        .filter(|entry| !entry.is_dir && entry.action != CloneAction::Skip)
        .count() as u64;

    let metrics = Arc::new(SeedCounters::default());
    let first_error = Arc::new(Mutex::new(None::<String>));

    pool.install(|| {
        entries.par_iter().for_each(|entry| {
            // Deliberately no short-circuit: one unseedable entry out of 38k
            // must never discard a multi-tens-of-GiB warm base.
            if let Err(err) = apply_entry(base_dir, run_dir, entry, &metrics, options)
                && let Ok(mut guard) = first_error.lock()
                && guard.is_none()
            {
                *guard = Some(err.to_string());
            }
        });
    });

    let first_entry_error = first_error.lock().ok().and_then(|guard| guard.clone());
    let mut result = metrics.result(start.elapsed(), base_seedable_file_count, None);
    result.first_entry_error = first_entry_error.clone();

    // The base is abandoned only when it yielded nothing usable at all. Any
    // successfully seeded artifact is worth strictly more than a cold build.
    if base_seedable_file_count > 0 && result.seeded_file_count() == 0 {
        let reason = CargoTargetSeedFallback::CloneFailed(
            first_entry_error.unwrap_or_else(|| "no base entry could be seeded".to_owned()),
        );
        emit_cargo_target_seed_fallback_metric(&reason);
        result.fallback_reason = Some(reason);
    }

    emit_cargo_target_seed_entry_metrics(&result);

    Ok(result)
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

    // Build-script state under `build/<pkg>-<hash>/` is rewritten IN PLACE when
    // cargo re-runs a build script, and it is the third instance of the same
    // shared-inode hazard as `.rustc_info.json` and the cargo locks above.
    //
    // A `-sys` crate's build script populates `OUT_DIR` with `fs::copy`, and
    // `std::fs::copy` on Unix opens the destination `O_CREAT|O_TRUNC` and only
    // THEN `fchmod`s it to the source's mode, propagating the error
    // (`open_to_and_set_permissions` in `std::sys::fs::unix`). Applied to a
    // hardlinked destination, that single sequence produces both halves of the
    // incident:
    //
    // 1. **The truncation writes THROUGH into the shared warm base.** The link
    //    shares the inode, so `O_TRUNC` empties the base's copy — for every other
    //    task-run pod that seeds from it, not just this run. Cargo's fingerprints
    //    still consider those units fresh, so nothing ever regenerates them and
    //    the base silently stops re-converging. Verified on the production node:
    //    9 zero-byte files in the shared base, every one under `debug/build/*/out/`
    //    and every one `nlink=2`, including
    //    `build/libssh2-sys-*/out/include/libssh2.h`.
    // 2. **The `fchmod` then fails EPERM and the build script panics.** Inode
    //    metadata ops are owner-gated, and a hardlinked entry keeps the base's
    //    owner. `libssh2-sys/build.rs:54` does `fs::copy(...).unwrap()`, so the
    //    compile dies. Note EPERM, not EACCES: an OWNERSHIP failure, not a mode
    //    one, which is why `ensure_owner_writable` cannot rescue it.
    //
    // Copying is what stops (1): the truncation lands on a private inode and the
    // shared base is untouchable by construction. It does NOT by itself stop (2)
    // — the seeder is uid 1000 and cargo runs as uid 1001, so `fchmod` on a
    // seeded destination is still EPERM for the build script. Aligning those uids
    // is separately planned work, and it is only safe AFTER this change: with a
    // single identity the `fchmod` succeeds, and on a hardlinked entry that turns
    // a loud panic into a silent overwrite of the shared base. Ownership is not
    // the fix for the corruption; not sharing rewritable state by inode is.
    //
    // The `build-script-build` executables stay hardlinked: they are the heavy
    // part of `build/` and cargo replaces them by rename, never in place.
    if is_build_script_output(relative_path) || is_build_script_stamp(relative_path) {
        return CloneAction::Copy;
    }

    CloneAction::Hardlink
}

/// True for build-script `OUT_DIR` payloads: `[<triple>/]<profile>/build/<pkg>-<hash>/out/**`.
///
/// Matched positionally (`build/*/out`) rather than by "has a component named
/// `out`" so an unrelated path that merely contains `out` is not swept in.
fn is_build_script_output(path: &Path) -> bool {
    normal_components(path)
        .windows(3)
        .any(|window| window[0] == "build" && window[2] == "out")
}

/// True for the per-package stamp files cargo rewrites in place after re-running
/// a build script (`build/<pkg>-<hash>/{output,stderr,root-output,invoked.timestamp}`).
///
/// These are tiny, so copying them is free, and hardlinking them writes through
/// to the shared warm base exactly like `.rustc_info.json` does.
fn is_build_script_stamp(path: &Path) -> bool {
    const STAMP_NAMES: &[&str] = &["output", "stderr", "root-output", "invoked.timestamp"];

    let parts = normal_components(path);
    let Some((name, parents)) = parts.split_last() else {
        return false;
    };
    // `build/<pkg>-<hash>/<stamp>`: the stamp sits exactly one level below the
    // package dir, which itself sits directly under `build/`.
    let under_build_package = matches!(parents.split_last(), Some((_, rest)) if rest.last().is_some_and(|component| *component == "build"));

    under_build_package && STAMP_NAMES.iter().any(|stamp| *name == *stamp)
}

fn normal_components(path: &Path) -> Vec<&std::ffi::OsStr> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect()
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
        degraded_link_file_count: 0,
        unseeded_file_count: 0,
        base_seedable_file_count: 0,
        link_fallback_budget_exhausted: false,
        first_entry_error: None,
        fallback_reason: Some(reason),
    }
}

fn emit_cargo_target_seed_fallback_metric(reason: &CargoTargetSeedFallback) {
    djinn_telemetry::cargo_target_seed::increment_seed_fallback(
        cargo_target_seed_fallback_metric_reason(reason),
    );
}

fn emit_cargo_target_seed_entry_metrics(result: &CargoTargetSeedResult) {
    djinn_telemetry::cargo_target_seed::add_seed_entries(
        djinn_telemetry::cargo_target_seed::DISPOSITION_DEGRADED_COPY,
        result.degraded_link_file_count,
    );
    djinn_telemetry::cargo_target_seed::add_seed_entries(
        djinn_telemetry::cargo_target_seed::DISPOSITION_UNSEEDED,
        result.unseeded_file_count,
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

        let is_root_level = pending_dirs.len() == 1 && pending_dirs[0].as_os_str().is_empty();
        pending_dirs = Vec::new();
        for scan in scans {
            match scan {
                Ok(mut scan) => {
                    all_entries.append(&mut scan.entries);
                    pending_dirs.append(&mut scan.next_dirs);
                }
                // A subdirectory that cannot be read costs only its own subtree.
                // Only an unreadable base ROOT means there is nothing to seed.
                Err(err) if is_root_level => return Err(err),
                Err(_) => {}
            }
        }
    }

    Ok(all_entries)
}

fn scan_dir(base_dir: &Path, relative_dir: &Path) -> io::Result<DirScan> {
    let mut scan = DirScan::default();
    let absolute_dir = base_dir.join(relative_dir);

    for entry in fs::read_dir(&absolute_dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let relative_path = relative_dir.join(entry.file_name());
        let action = classify_cargo_target_path(&relative_path);

        // `file_type()` does NOT follow symlinks, unlike `metadata()`. A symlink
        // must never be classified from its target: `linkat` would pin the
        // SYMLINK inode, which is not `S_ISREG`, and `fs.protected_hardlinks=1`
        // refuses that for any source this process does not own. Descending
        // through one can also cycle.
        let Ok(file_type) = entry.file_type() else {
            scan.entries.push(skipped_entry(relative_path));
            continue;
        };

        if file_type.is_symlink() {
            // Materialise the target's bytes privately when it resolves to a
            // regular file; ignore dangling links and symlinked directories.
            let resolves_to_file = fs::metadata(entry.path())
                .ok()
                .filter(std::fs::Metadata::is_file);
            match (action, resolves_to_file) {
                (CloneAction::Skip, _) | (_, None) => {
                    scan.entries.push(skipped_entry(relative_path));
                }
                (_, Some(target_meta)) => scan.entries.push(SeedEntry {
                    relative_path,
                    action: CloneAction::Copy,
                    bytes: target_meta.len(),
                    is_dir: false,
                }),
            }
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            scan.entries.push(skipped_entry(relative_path));
            continue;
        };

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
            scan.entries.push(skipped_entry(relative_path));
        }
    }

    Ok(scan)
}

fn skipped_entry(relative_path: PathBuf) -> SeedEntry {
    SeedEntry {
        relative_path,
        action: CloneAction::Skip,
        bytes: 0,
        is_dir: false,
    }
}

fn apply_entry(
    base_dir: &Path,
    run_dir: &Path,
    entry: &SeedEntry,
    metrics: &SeedCounters,
    options: &CargoTargetSeedOptions,
) -> Result<(), SeedEntryError> {
    match entry.action {
        CloneAction::Skip => {
            metrics.skipped_file_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        CloneAction::Hardlink | CloneAction::Copy if entry.is_dir => {
            create_dir(run_dir.join(&entry.relative_path).as_path(), entry)
        }
        CloneAction::Hardlink => {
            let source = base_dir.join(&entry.relative_path);
            let destination = run_dir.join(&entry.relative_path);
            create_parent_dir(&destination, entry)?;

            let link_err = match link_file(options.link_backend, &source, &destination) {
                Ok(()) => {
                    metrics.linked_file_count.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .linked_bytes
                        .fetch_add(entry.bytes, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) => err,
            };

            // A hardlink is only an optimisation for immutable payloads, so a
            // private byte copy is always a correct substitute — and strictly
            // safer, since the run can never write through to the warm base.
            // Spend a bounded budget on it rather than lose the artifact.
            if !metrics.try_reserve_fallback_copy(entry.bytes, options) {
                metrics.unseeded_file_count.fetch_add(1, Ordering::Relaxed);
                return Err(
                    seed_entry_error(SeedOp::Hardlink, entry, &source, &link_err)
                        .with_fallback_message(format!(
                            "link-fallback copy budget of {} bytes exhausted",
                            options.link_fallback_copy_budget_bytes
                        )),
                );
            }

            match fs::copy(&source, &destination) {
                Ok(copied) => {
                    // `fs::copy` reproduces the SOURCE's mode on the destination,
                    // and the entries that land here are precisely the ones whose
                    // mode is unusual — including `0444`. A read-only artifact in
                    // the base is fine (nothing writes it there), but in the
                    // PRIVATE run dir Cargo may need to rewrite that very path
                    // when it rebuilds the unit, and a `0444` file rejects even
                    // its owner. The run dir is disposable, so restore owner
                    // write unconditionally.
                    ensure_owner_writable(&destination);
                    // A link would have carried the base's mtime; this copy is
                    // standing in for a link, so it must too.
                    preserve_source_times(&source, &destination);
                    metrics
                        .degraded_link_file_count
                        .fetch_add(1, Ordering::Relaxed);
                    metrics.copied_bytes.fetch_add(copied, Ordering::Relaxed);
                    Ok(())
                }
                Err(copy_err) => {
                    metrics.release_fallback_copy(entry.bytes);
                    metrics.unseeded_file_count.fetch_add(1, Ordering::Relaxed);
                    Err(
                        seed_entry_error(SeedOp::Hardlink, entry, &source, &link_err)
                            .with_fallback_message(copy_err.to_string()),
                    )
                }
            }
        }
        CloneAction::Copy => {
            let source = base_dir.join(&entry.relative_path);
            let destination = run_dir.join(&entry.relative_path);
            create_parent_dir(&destination, entry)?;
            match fs::copy(&source, &destination) {
                Ok(copied) => {
                    // Everything classified `Copy` is copied precisely because
                    // the run must be able to REWRITE it. `fs::copy` reproduces
                    // the source mode, so a read-only base entry (a symlinked
                    // vendored payload, say) would arrive unwritable.
                    ensure_owner_writable(&destination);
                    // Cargo compares raw mtimes for staleness, so a copied entry
                    // must present the same age as the links around it.
                    preserve_source_times(&source, &destination);
                    metrics.copied_file_count.fetch_add(1, Ordering::Relaxed);
                    metrics.copied_bytes.fetch_add(copied, Ordering::Relaxed);
                    Ok(())
                }
                Err(err) => {
                    metrics.unseeded_file_count.fetch_add(1, Ordering::Relaxed);
                    Err(seed_entry_error(SeedOp::Copy, entry, &source, &err))
                }
            }
        }
    }
}

fn link_file(backend: LinkBackend, source: &Path, destination: &Path) -> io::Result<()> {
    match backend {
        LinkBackend::Native => fs::hard_link(source, destination),
        #[cfg(test)]
        LinkBackend::RefuseSourcesForeignOwnersCannotLink => {
            if tests::source_is_linkable_by_foreign_owner(source) {
                fs::hard_link(source, destination)
            } else {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            }
        }
    }
}

/// Copy the source's access/modification times onto a seeded destination.
///
/// A hardlink shares the inode, so it carries the base's mtime for free. A byte
/// copy does not: `fs::copy` reproduces mode but *not* timestamps, so a copied
/// entry lands with a SEED-time mtime while its hardlinked neighbours keep their
/// WARM-time mtime. Cargo's `StaleDependency` check is a raw mtime comparison
/// against the unit's dependencies, so mixing the two epochs inside one target
/// dir makes cargo see freshly-seeded metadata as newer than the artifacts it
/// describes and rebuild units that were perfectly good — broadly, and dependent
/// on which side of the classification each path happened to fall. Restoring the
/// source times keeps a copy indistinguishable from a link to the freshness
/// checker, which is the entire point of seeding.
///
/// Must run AFTER [`ensure_owner_writable`]: `chmod` bumps ctime and the open
/// below needs the write bit, so setting times has to be the last touch.
///
/// The seeder owns every destination it creates, and an owner may always set
/// explicit times (`utimensat` with explicit values is owner-gated, not
/// root-gated), so this is permitted without any privilege.
///
/// Best-effort, like [`ensure_owner_writable`]: a stale timestamp costs a
/// needless rebuild, which is not a reason to drop an otherwise correctly seeded
/// artifact.
fn preserve_source_times(source: &Path, destination: &Path) {
    let Ok(source_meta) = fs::metadata(source) else {
        return;
    };
    let Ok(modified) = source_meta.modified() else {
        return;
    };

    let mut times = fs::FileTimes::new().set_modified(modified);
    if let Ok(accessed) = source_meta.accessed() {
        times = times.set_accessed(accessed);
    }

    // `File::set_times` requires a handle opened for writing on some targets;
    // open for write WITHOUT truncating, so this never touches the bytes.
    let Ok(file) = fs::OpenOptions::new().write(true).open(destination) else {
        return;
    };
    let _ = file.set_times(times);
}

/// Restore owner write on a seeded artifact in the private run dir.
///
/// Best-effort: failing to widen the mode is not a reason to drop an artifact
/// that is otherwise correctly seeded.
#[cfg(unix)]
fn ensure_owner_writable(destination: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = fs::metadata(destination) else {
        return;
    };
    let mode = meta.permissions().mode();
    if mode & 0o200 != 0 {
        return;
    }
    let _ = fs::set_permissions(destination, fs::Permissions::from_mode(mode | 0o200));
}

#[cfg(not(unix))]
fn ensure_owner_writable(_destination: &Path) {}

fn create_dir(path: &Path, entry: &SeedEntry) -> Result<(), SeedEntryError> {
    fs::create_dir_all(path).map_err(|err| seed_entry_error(SeedOp::CreateDir, entry, path, &err))
}

fn create_parent_dir(destination: &Path, entry: &SeedEntry) -> Result<(), SeedEntryError> {
    match destination.parent() {
        Some(parent) => create_dir(parent, entry),
        None => Ok(()),
    }
}

fn seed_entry_error(
    op: SeedOp,
    entry: &SeedEntry,
    source: &Path,
    err: &io::Error,
) -> SeedEntryError {
    SeedEntryError {
        op,
        relative_path: entry.relative_path.clone(),
        message: err.to_string(),
        identity: source_identity(source),
        hint: refusal_hint(op, err),
        fallback_message: None,
    }
}

impl SeedEntryError {
    fn with_fallback_message(mut self, message: String) -> Self {
        self.fallback_message = Some(message);
        self
    }
}

/// Render the ownership/mode facts that decide whether the kernel will allow a
/// hardlink, plus this process's identity. This is the single line that turns
/// "Operation not permitted (os error 1)" into a diagnosis.
#[cfg(unix)]
fn source_identity(source: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(source).ok()?;
    Some(format!(
        "source mode=0{:o} uid={} gid={} nlink={}; process euid={} egid={}",
        meta.mode(),
        meta.uid(),
        meta.gid(),
        meta.nlink(),
        // SAFETY: `geteuid`/`getegid` are always-successful, thread-safe reads
        // of the calling process's credentials.
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
    ))
}

#[cfg(not(unix))]
fn source_identity(_source: &Path) -> Option<String> {
    None
}

fn refusal_hint(op: SeedOp, err: &io::Error) -> Option<&'static str> {
    if op != SeedOp::Hardlink || err.raw_os_error() != Some(libc::EPERM) {
        return None;
    }
    Some(
        "fs.protected_hardlinks=1 refuses linkat for a source this process does not own \
         unless it is a group-writable regular file (kernel safe_hardlink_source)",
    )
}

#[derive(Debug, Default)]
struct SeedCounters {
    linked_file_count: AtomicU64,
    copied_file_count: AtomicU64,
    skipped_file_count: AtomicU64,
    linked_bytes: AtomicU64,
    copied_bytes: AtomicU64,
    degraded_link_file_count: AtomicU64,
    unseeded_file_count: AtomicU64,
    fallback_copy_reserved_bytes: AtomicU64,
    fallback_budget_exhausted: AtomicBool,
}

impl SeedCounters {
    /// Reserve budget for one link-fallback copy.
    ///
    /// Concurrent reservations may overshoot the budget by at most one entry
    /// per worker thread, which is bounded by [`MAX_PARALLELISM`] and therefore
    /// irrelevant next to the budget itself.
    fn try_reserve_fallback_copy(&self, bytes: u64, options: &CargoTargetSeedOptions) -> bool {
        let previous = self
            .fallback_copy_reserved_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        if previous.saturating_add(bytes) <= options.link_fallback_copy_budget_bytes {
            return true;
        }
        self.release_fallback_copy(bytes);
        self.fallback_budget_exhausted
            .store(true, Ordering::Relaxed);
        false
    }

    fn release_fallback_copy(&self, bytes: u64) {
        self.fallback_copy_reserved_bytes
            .fetch_sub(bytes, Ordering::Relaxed);
    }

    fn result(
        &self,
        elapsed: Duration,
        base_seedable_file_count: u64,
        fallback_reason: Option<CargoTargetSeedFallback>,
    ) -> CargoTargetSeedResult {
        CargoTargetSeedResult {
            elapsed,
            linked_file_count: self.linked_file_count.load(Ordering::Relaxed),
            copied_file_count: self.copied_file_count.load(Ordering::Relaxed),
            skipped_file_count: self.skipped_file_count.load(Ordering::Relaxed),
            linked_bytes: self.linked_bytes.load(Ordering::Relaxed),
            copied_bytes: self.copied_bytes.load(Ordering::Relaxed),
            degraded_link_file_count: self.degraded_link_file_count.load(Ordering::Relaxed),
            unseeded_file_count: self.unseeded_file_count.load(Ordering::Relaxed),
            base_seedable_file_count,
            link_fallback_budget_exhausted: self.fallback_budget_exhausted.load(Ordering::Relaxed),
            first_entry_error: None,
            fallback_reason,
        }
    }
}

impl CargoTargetSeedResult {
    /// Files actually present in the run dir because of this seed.
    pub fn seeded_file_count(&self) -> u64 {
        self.linked_file_count + self.copied_file_count + self.degraded_link_file_count
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
mod tests;

#[cfg(test)]
mod foreign_owner_tests;
