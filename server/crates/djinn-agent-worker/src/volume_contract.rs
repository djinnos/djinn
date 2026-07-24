//! Startup validation of the **mounted-volume permission contract**.
//!
//! `qut0` moved the task-run Pod from uid 10001 to uid/gid [`WORKER_UID`]/
//! [`ARTIFACT_GID`] (1000/1000) with `fsGroup: 1000` +
//! `fsGroupChangePolicy: OnRootMismatch`. That render only *declares* the
//! contract; nothing ever inspected the filesystem that actually got mounted.
//! When the live `djinn-cache` PVC held 13,512 files owned by 10001 with dirs
//! 755 / files 644 (no group-write anywhere), a uid-1000 pod could not create,
//! unlink, or overwrite anything in its own Cargo warm base — and because the
//! warm Job's cargo phase is best-effort (lock-unavailable, step failure and
//! timeout only WARN) while the Job reports success from the graph phase, the
//! result would have been *green warm Jobs over a permanently frozen cache*.
//!
//! `goxi`'s design names the missing piece: "Startup validates ownership/mode
//! on the workspace, git metadata, and configured cache roots and fails
//! readiness rather than falling back to UID 1000 if the volume driver cannot
//! preserve this contract." This module is that check.
//!
//! ## The contract
//!
//! For every mounted root the pod writes through:
//!   * group ownership is [`ARTIFACT_GID`] — so the worker (gid 1000) and the
//!     launcher-spawned child (primary group 1000) share every artifact;
//!   * directories carry **group-write** *and* **setgid**, so files created by
//!     either identity inherit the artifact group instead of the creator's;
//!   * files that their owner can write, and that are not executable, carry
//!     **group-write** so the other identity can overwrite them in place. The
//!     two exclusions cover the shapes a correct tree legitimately produces —
//!     `444` git objects / cargo registry sources, and the `755`
//!     `.git/hooks/*.sample` every clone copies from git's templates — neither
//!     of which can hide the `644` production shape this exists to catch;
//!   * the process runs with umask `0002` ([`apply_artifact_umask`]) so
//!     everything it creates keeps satisfying the above;
//!   * the process itself is a member of [`ARTIFACT_GID`] (primary or
//!     supplementary) — a conforming volume is useless to a pod that is not in
//!     the group.
//!
//! ## What it deliberately does NOT do
//!
//! * **No repair.** Not even a bounded one. A recursive `chown` over a 300G
//!   cache is a multi-minute stall on pod start; avoiding exactly that is why
//!   the render pins `fsGroupChangePolicy: OnRootMismatch`. Remediation is a
//!   one-shot operator action (see `docs/VOLUME_OWNERSHIP_CONTRACT_RUNBOOK.md`)
//!   plus the chart's O(1) `fix-volume-perms` init container.
//! * **No fallback to the legacy uid.** A violation fails readiness with a
//!   typed error naming the path and observed-versus-required ownership/mode.
//! * **No unbounded scan.** The walk is capped by depth, per-directory sample
//!   size, and a global stat budget ([`VolumeContract`] defaults: 3 / 32 /
//!   512), so a conforming volume costs a few hundred `lstat`s. Checking only
//!   the volume *root* would be worthless here: the production near-miss had a
//!   hand-fixed root over a broken subtree — which is also precisely the state
//!   that makes kubelet's `OnRootMismatch` heuristic skip its recursive pass.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{info, warn};

pub use djinn_cgroup_launcher::child::ARTIFACT_GID;

use crate::cargo_target_seed::{RUN_TARGET_ROOT, WARM_BASE_ROOT, warm_base_dir_for_current_jobs};

/// setgid bit — new files under the directory inherit its group.
const MODE_SETGID: u32 = 0o2000;
/// Group-write bit.
const MODE_GROUP_WRITE: u32 = 0o020;
/// Owner-write bit — a file without it is immutable by design (git objects,
/// cargo registry sources) rather than mis-permissioned.
const MODE_OWNER_WRITE: u32 = 0o200;
/// Any execute bit — git copies its template hooks with the template's own
/// mode, so `.git/hooks/*.sample` is 755 in every fresh clone whatever the
/// umask.
const MODE_ANY_EXEC: u32 = 0o111;
/// Permission bits we report on (mode & this) — keeps log lines readable.
const MODE_MASK: u32 = 0o7777;

/// Mount path of the shared cache PVC inside every djinn Pod.
pub const CACHE_MOUNT_ROOT: &str = "/cache";
/// Mount path of the git-mirror PVC (the pod's git metadata).
pub const MIRROR_MOUNT_ROOT: &str = "/mirror";
/// Contractual workspace mount path.
pub const WORKSPACE_MOUNT_ROOT: &str = "/workspace";

/// Env var selecting the enforcement mode (`enforce` | `audit`).
pub const MODE_ENV: &str = "DJINN_VOLUME_CONTRACT_MODE";
/// Always injected by the kubelet into a Pod; absent everywhere else.
pub const IN_CLUSTER_ENV: &str = "KUBERNETES_SERVICE_HOST";

/// How a contract violation is handled at startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractMode {
    /// Fail readiness. The in-cluster default.
    Enforce,
    /// Log the same typed error at ERROR level and continue. Break-glass for a
    /// rollout window only — it is loud, operator-selected, and never a
    /// fallback to the legacy uid.
    Audit,
    /// Not running against pod-mounted volumes; nothing to enforce.
    Off,
}

impl ContractMode {
    /// Resolve from the environment.
    ///
    /// Outside Kubernetes there is no mounted-volume contract to enforce (the
    /// worker binary is also spawned directly by integration tests against
    /// tempdirs), so the default is [`ContractMode::Off`]. In-cluster the
    /// default is [`ContractMode::Enforce`]; [`MODE_ENV`] overrides in both
    /// directions.
    pub fn from_env() -> Self {
        let explicit = std::env::var(MODE_ENV).ok();
        match explicit.as_deref().map(str::trim) {
            Some("enforce") => Self::Enforce,
            Some("audit") => Self::Audit,
            Some("off") => Self::Off,
            Some(other) if !other.is_empty() => {
                warn!(
                    value = other,
                    env = MODE_ENV,
                    "unrecognized volume-contract mode; failing closed to enforce-when-in-cluster"
                );
                Self::default_for_environment()
            }
            _ => Self::default_for_environment(),
        }
    }

    fn default_for_environment() -> Self {
        if std::env::var_os(IN_CLUSTER_ENV).is_some() {
            Self::Enforce
        } else {
            Self::Off
        }
    }

    /// Log label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Audit => "audit",
            Self::Off => "off",
        }
    }
}

/// Whether an absent root is a violation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootPresence {
    /// The root must exist — its absence means the volume was not mounted.
    Required,
    /// Absence is fine (a fresh cache PVC has no `cargo-target` yet); if it
    /// exists it must conform.
    OptionalWhenAbsent,
}

/// One mounted root to validate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VolumeRoot {
    /// Human label used in errors and logs (`workspace`, `cache`, …).
    pub label: String,
    /// The real, mounted path.
    pub path: PathBuf,
    pub presence: RootPresence,
}

impl VolumeRoot {
    pub fn required(label: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            label: label.into(),
            path: path.into(),
            presence: RootPresence::Required,
        }
    }

    pub fn optional(label: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            label: label.into(),
            path: path.into(),
            presence: RootPresence::OptionalWhenAbsent,
        }
    }
}

/// The contract plus the bounds of the sampling walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeContract {
    /// Group every mounted artifact must belong to.
    pub required_gid: u32,
    /// Directory levels below each root that are sampled (0 = root only).
    pub max_depth: usize,
    /// Entries sampled per directory.
    pub entries_per_dir: usize,
    /// Hard cap on `lstat`s across all roots.
    pub stat_budget: usize,
    /// Also assert the process is a member of `required_gid`.
    pub require_process_membership: bool,
}

impl Default for VolumeContract {
    fn default() -> Self {
        Self {
            required_gid: ARTIFACT_GID,
            max_depth: 3,
            entries_per_dir: 32,
            stat_budget: 512,
            require_process_membership: true,
        }
    }
}

/// Outcome of a passing validation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VolumeContractReport {
    pub roots_checked: usize,
    pub roots_absent: usize,
    pub entries_sampled: usize,
    pub budget_exhausted: bool,
}

/// Typed startup failure. Every variant names the offending path and the
/// observed-versus-required ownership/mode.
#[derive(Debug, Error)]
pub enum VolumeContractError {
    #[error("volume contract [{label}]: required root {path} is not mounted")]
    MissingRoot { label: String, path: PathBuf },

    #[error("volume contract [{label}]: {path} is not a directory")]
    NotADirectory { label: String, path: PathBuf },

    #[error("volume contract [{label}]: cannot stat {path}: {source}")]
    Stat {
        label: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "volume contract [{label}]: {path} is group-owned by gid {observed_gid} \
         but the artifact contract requires gid {required_gid} \
         (observed mode {observed_mode:o}, uid {observed_uid})"
    )]
    GroupOwner {
        label: String,
        path: PathBuf,
        observed_gid: u32,
        observed_uid: u32,
        observed_mode: u32,
        required_gid: u32,
    },

    #[error(
        "volume contract [{label}]: {path} ({kind}) has mode {observed_mode:o} \
         with no group-write bit; the artifact contract requires g+w so gid \
         {required_gid} can create, overwrite and unlink here"
    )]
    GroupWrite {
        label: String,
        path: PathBuf,
        kind: &'static str,
        observed_mode: u32,
        required_gid: u32,
    },

    #[error(
        "volume contract [{label}]: directory {path} has mode {observed_mode:o} \
         with no setgid bit; the artifact contract requires setgid so files \
         created here inherit gid {required_gid}"
    )]
    Setgid {
        label: String,
        path: PathBuf,
        observed_mode: u32,
        required_gid: u32,
    },

    #[error(
        "volume contract: process runs as gid {process_gid} with supplementary \
         groups {supplementary:?}; artifact gid {required_gid} is not among \
         them, so conforming volumes are still unwritable"
    )]
    ProcessGroupMembership {
        process_gid: u32,
        supplementary: Vec<u32>,
        required_gid: u32,
    },
}

impl VolumeContractError {
    /// Stable label for logs/metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MissingRoot { .. } => "missing_root",
            Self::NotADirectory { .. } => "not_a_directory",
            Self::Stat { .. } => "stat_failed",
            Self::GroupOwner { .. } => "group_owner",
            Self::GroupWrite { .. } => "group_write",
            Self::Setgid { .. } => "setgid",
            Self::ProcessGroupMembership { .. } => "process_group_membership",
        }
    }

    /// Offending path, when the violation has one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::MissingRoot { path, .. }
            | Self::NotADirectory { path, .. }
            | Self::Stat { path, .. }
            | Self::GroupOwner { path, .. }
            | Self::GroupWrite { path, .. }
            | Self::Setgid { path, .. } => Some(path.as_path()),
            Self::ProcessGroupMembership { .. } => None,
        }
    }
}

/// Validate `roots` against `contract`. Pure filesystem reads; never mutates.
pub fn validate(
    contract: &VolumeContract,
    roots: &[VolumeRoot],
) -> Result<VolumeContractReport, VolumeContractError> {
    let mut report = VolumeContractReport::default();

    if contract.require_process_membership {
        check_process_membership(contract.required_gid)?;
    }

    for root in roots {
        let meta = match fs::symlink_metadata(&root.path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => match root.presence {
                RootPresence::Required => {
                    return Err(VolumeContractError::MissingRoot {
                        label: root.label.clone(),
                        path: root.path.clone(),
                    });
                }
                RootPresence::OptionalWhenAbsent => {
                    report.roots_absent += 1;
                    continue;
                }
            },
            Err(source) => {
                return Err(VolumeContractError::Stat {
                    label: root.label.clone(),
                    path: root.path.clone(),
                    source,
                });
            }
        };

        if !meta.is_dir() {
            return Err(VolumeContractError::NotADirectory {
                label: root.label.clone(),
                path: root.path.clone(),
            });
        }

        report.roots_checked += 1;
        report.entries_sampled += 1;
        check_metadata(contract, &root.label, &root.path, &meta, true)?;
        walk_sample(contract, root, &mut report)?;
    }

    Ok(report)
}

/// Bounded breadth-first sample below `root`. Symlinks are never followed and
/// never checked — the contract is about the volume's own inodes.
fn walk_sample(
    contract: &VolumeContract,
    root: &VolumeRoot,
    report: &mut VolumeContractReport,
) -> Result<(), VolumeContractError> {
    if contract.max_depth == 0 {
        return Ok(());
    }

    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.path.clone(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if report.entries_sampled >= contract.stat_budget {
            report.budget_exhausted = true;
            return Ok(());
        }

        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory we cannot list is itself a contract failure only if
            // its own mode/ownership said otherwise, which `check_metadata`
            // already asserted; anything else here (races with a concurrent
            // prune, for instance) must not wedge startup.
            Err(_) => continue,
        };

        for entry in entries.take(contract.entries_per_dir) {
            let Ok(entry) = entry else { continue };
            if report.entries_sampled >= contract.stat_budget {
                report.budget_exhausted = true;
                return Ok(());
            }

            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                // Vanished between listing and stat — not a contract signal.
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }

            report.entries_sampled += 1;
            let is_dir = meta.is_dir();
            check_metadata(contract, &root.label, &path, &meta, is_dir)?;

            if is_dir && depth + 1 < contract.max_depth {
                queue.push_back((path, depth + 1));
            }
        }
    }

    Ok(())
}

fn check_metadata(
    contract: &VolumeContract,
    label: &str,
    path: &Path,
    meta: &fs::Metadata,
    is_dir: bool,
) -> Result<(), VolumeContractError> {
    let mode = meta.mode() & MODE_MASK;
    let gid = meta.gid();

    if gid != contract.required_gid {
        return Err(VolumeContractError::GroupOwner {
            label: label.to_string(),
            path: path.to_path_buf(),
            observed_gid: gid,
            observed_uid: meta.uid(),
            observed_mode: mode,
            required_gid: contract.required_gid,
        });
    }

    // Directories are checked unconditionally: write permission on the
    // DIRECTORY is what actually lets the other identity create, replace and
    // unlink entries, and its absence is what froze the warm base.
    //
    // Files are checked when they are owner-writable and not executable.
    // The two exclusions are not laxity, they are the two shapes a correct tree
    // legitimately produces:
    //   * owner-read-only (`444`) — git loose objects and packfiles, cargo
    //     registry sources carrying the crate tarball's modes. Immutable by
    //     design and replaced through the directory, never written in place.
    //   * executable (`755`) — `git init`/`clone` copies the template hooks
    //     with the template's own mode, so every fresh clone contains
    //     `.git/hooks/*.sample` at 755 regardless of umask.
    // Neither shape can mask the failure this exists to catch: the production
    // volume was dirs `755` (caught on the directory rule) over files `644` —
    // owner-writable, non-executable, and still caught here.
    let owner_writable = mode & MODE_OWNER_WRITE != 0;
    let executable = mode & MODE_ANY_EXEC != 0;
    if (is_dir || (owner_writable && !executable)) && mode & MODE_GROUP_WRITE == 0 {
        return Err(VolumeContractError::GroupWrite {
            label: label.to_string(),
            path: path.to_path_buf(),
            kind: if is_dir { "directory" } else { "file" },
            observed_mode: mode,
            required_gid: contract.required_gid,
        });
    }

    if is_dir && mode & MODE_SETGID == 0 {
        return Err(VolumeContractError::Setgid {
            label: label.to_string(),
            path: path.to_path_buf(),
            observed_mode: mode,
            required_gid: contract.required_gid,
        });
    }

    Ok(())
}

/// Assert the process can actually use a conforming volume.
fn check_process_membership(required_gid: u32) -> Result<(), VolumeContractError> {
    let process_gid = current_gid();
    let supplementary = supplementary_groups();
    if process_gid == required_gid || supplementary.contains(&required_gid) {
        return Ok(());
    }
    Err(VolumeContractError::ProcessGroupMembership {
        process_gid,
        supplementary,
        required_gid,
    })
}

/// Effective gid of this process.
pub fn current_gid() -> u32 {
    // SAFETY: `getegid` takes no arguments, cannot fail, and touches no memory.
    unsafe { libc::getegid() }
}

/// Supplementary group list of this process.
pub fn supplementary_groups() -> Vec<u32> {
    // SAFETY: the first call only queries the count (null buffer is the
    // documented probe form); the second fills a buffer sized by that count and
    // the returned length bounds the truncation below.
    unsafe {
        let count = libc::getgroups(0, std::ptr::null_mut());
        if count <= 0 {
            return Vec::new();
        }
        let mut buf: Vec<libc::gid_t> = vec![0; count as usize];
        let filled = libc::getgroups(count, buf.as_mut_ptr());
        if filled <= 0 {
            return Vec::new();
        }
        buf.truncate(filled as usize);
        buf
    }
}

/// Process umask that keeps the contract true for everything this process
/// creates: `0o002` clears only "other"-write, so new directories land `775`
/// and new files `664` instead of `755`/`644`.
pub const ARTIFACT_UMASK: libc::mode_t = 0o002;

/// Apply [`ARTIFACT_UMASK`] to this process and return the previous mask.
///
/// This is the *other half* of the contract, and it was missing: the
/// launcher-spawned child already sets `umask 0002`
/// (`djinn_cgroup_launcher::child::prepare_child`), but the worker process
/// itself inherited the container's default `022`. Everything the worker
/// created on the shared volumes — the Cargo warm base above all — therefore
/// landed group-READABLE but not group-WRITABLE, so the child (uid 1001, group
/// 1000) could not overwrite it even on a perfectly conforming volume. Setgid
/// on the directories fixes the *group*; only the umask fixes the *mode*.
///
/// Must run before any filesystem work, and before [`enforce_at_startup`] so a
/// first-run pod that creates its own cache subtree creates it conforming.
pub fn apply_artifact_umask() -> libc::mode_t {
    // SAFETY: `umask` takes a mode, cannot fail, and touches no memory. It is
    // process-global, which is exactly the intent, and is applied once at
    // startup before any worker thread does filesystem work.
    unsafe { libc::umask(ARTIFACT_UMASK) }
}

/// Mounted roots a **task-run** Pod must be able to write.
pub fn task_run_roots(workspace: &Path) -> Vec<VolumeRoot> {
    let mut roots = vec![VolumeRoot::required("workspace", workspace)];
    roots.push(VolumeRoot::optional("git-metadata", workspace.join(".git")));
    roots.extend(git_mirror_roots());
    roots.extend(cache_roots());
    roots
}

/// Mounted roots the **warm Job** must be able to write. Same shared `/cache`
/// as the task-runs, plus this pod's own Cargo warm base variant — the exact
/// directory the near-miss froze.
pub fn warm_roots(project_root: &Path, project_id: &str) -> Vec<VolumeRoot> {
    let mut roots = vec![VolumeRoot::required("workspace", project_root)];
    roots.push(VolumeRoot::optional(
        "git-metadata",
        project_root.join(".git"),
    ));
    roots.extend(git_mirror_roots());
    roots.extend(cache_roots());
    roots.push(VolumeRoot::optional(
        "cargo-warm-base",
        warm_base_dir_for_current_jobs(project_id),
    ));
    roots
}

fn git_mirror_roots() -> Vec<VolumeRoot> {
    let mirror = std::env::var("DJINN_MIRROR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(MIRROR_MOUNT_ROOT));
    vec![VolumeRoot::optional("git-mirror", mirror)]
}

/// Every configured cache root, including the Cargo warm-base parent and the
/// per-run target parent. All optional-when-absent: a fresh PVC has none of
/// them yet, and the mount root itself is the required anchor.
fn cache_roots() -> Vec<VolumeRoot> {
    let mut roots = vec![
        VolumeRoot::optional("cache-mount", CACHE_MOUNT_ROOT),
        VolumeRoot::optional("cargo-warm-base-root", WARM_BASE_ROOT),
        VolumeRoot::optional("cargo-target-run-root", RUN_TARGET_ROOT),
    ];
    for (label, env) in [
        ("cargo-home", "CARGO_HOME"),
        ("sccache-dir", "SCCACHE_DIR"),
        ("cargo-target-dir", "CARGO_TARGET_DIR"),
    ] {
        if let Ok(value) = std::env::var(env) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                roots.push(VolumeRoot::optional(label, PathBuf::from(trimmed)));
            }
        }
    }
    roots
}

/// Run the check at startup for `context` (`task-run` / `warm-graph`), honouring
/// [`ContractMode`]. Returns `Err` only when the mode is
/// [`ContractMode::Enforce`] — the caller propagates it and the pod fails.
pub fn enforce_at_startup(context: &str, roots: &[VolumeRoot]) -> Result<(), VolumeContractError> {
    enforce_with(
        context,
        &VolumeContract::default(),
        roots,
        ContractMode::from_env(),
    )
}

/// [`enforce_at_startup`] with the contract and mode supplied explicitly.
// Approved boundary for `Instant::now`: the elapsed value is observational only
// — it lands in one startup log line so operators can confirm the check costs
// milliseconds ([`EXPECTED_MAX_DURATION`]). No control flow reads it.
#[allow(clippy::disallowed_methods)]
pub fn enforce_with(
    context: &str,
    contract: &VolumeContract,
    roots: &[VolumeRoot],
    mode: ContractMode,
) -> Result<(), VolumeContractError> {
    if mode == ContractMode::Off {
        return Ok(());
    }

    let started = Instant::now();
    let outcome = validate(contract, roots);
    let elapsed = started.elapsed();

    match outcome {
        Ok(report) => {
            info!(
                context,
                mode = mode.as_str(),
                required_gid = contract.required_gid,
                roots_checked = report.roots_checked,
                roots_absent = report.roots_absent,
                entries_sampled = report.entries_sampled,
                budget_exhausted = report.budget_exhausted,
                elapsed_ms = elapsed.as_millis() as u64,
                "volume permission contract satisfied"
            );
            Ok(())
        }
        Err(error) => {
            let path = error
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            tracing::error!(
                context,
                mode = mode.as_str(),
                kind = error.kind(),
                path = %path,
                required_gid = contract.required_gid,
                elapsed_ms = elapsed.as_millis() as u64,
                error = %error,
                "volume permission contract VIOLATED"
            );
            match mode {
                ContractMode::Enforce => Err(error),
                // Loud, operator-selected, and explicitly not a fallback to the
                // legacy uid: the pod proceeds with a known-broken cache.
                ContractMode::Audit | ContractMode::Off => Ok(()),
            }
        }
    }
}

/// Wall-clock budget the check is expected to stay inside on a conforming
/// volume. Documentation-only — nothing enforces it — but it pins the intent
/// that this is a few hundred `lstat`s, not a scan.
pub const EXPECTED_MAX_DURATION: Duration = Duration::from_millis(250);
