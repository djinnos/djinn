// Linux Landlock sandbox backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate

#![cfg(target_os = "linux")]

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

use super::{Sandbox, djinn_cache_dir, git_dir, git_metadata_dir};

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-everywhere, write only to the
/// task worktree, its git metadata directory, `/var/tmp`, a dedicated djinn
/// agent scratch dir (`$XDG_CACHE_HOME/djinn` or `$HOME/.cache/djinn`), and
/// the usual `/dev/{null,zero,urandom}` nodes. `/tmp` is intentionally not
/// writable: on typical Linux it's tmpfs, and allowing writes there caused a
/// 3.8 GB cargo-artifact leak into RAM-backed storage.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, worktree_path: &Path, cmd: &mut std::process::Command) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let worktree = worktree_path.to_path_buf();
        let git_meta = git_metadata_dir(worktree_path);

        // Resolve + create the djinn cache dir in the PARENT process, before
        // fork. `create_dir_all` and `tracing::warn!` are not async-signal-safe,
        // so they must not run inside `pre_exec` — doing so risks deadlocking
        // a forked child if another thread in the tokio-based parent held a
        // malloc/tracing mutex at fork time. Only the Landlock ruleset
        // construction runs post-fork in pre_exec.
        let cache_dir_for_rule = prepare_cache_dir();

        // Safety: pre_exec runs in the forked child process. The closure only
        // performs Landlock syscalls and open(2) calls, both of which are
        // async-signal-safe per POSIX.
        unsafe {
            cmd.pre_exec(move || {
                apply_policy(
                    &worktree,
                    git_meta.as_deref(),
                    cache_dir_for_rule.as_deref(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
            });
        }
        Ok(())
    }
}

/// Resolve the djinn agent scratch directory and ensure it exists.
///
/// Runs in the parent process only. Returns `Some(path)` if the directory
/// exists (either already present or successfully created), `None` otherwise.
/// On creation failure, logs a warning and returns `None` so the sandbox
/// setup can continue without the cache-dir allowance.
fn prepare_cache_dir() -> Option<PathBuf> {
    let dir = djinn_cache_dir()?;
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "sandbox: failed to create djinn cache dir; skipping Landlock rule"
            );
            None
        }
    }
}

/// Build and apply the Landlock policy in the current process.
///
/// Called inside `pre_exec` (forked child) so it takes effect before exec.
/// Only async-signal-safe operations are performed here: Landlock syscalls
/// and `open(2)` via `PathFd::new`. Path resolution, directory creation,
/// logging, and any allocator-heavy work must happen in the parent before
/// fork — see `LandlockSandbox::apply` and `prepare_cache_dir`.
fn apply_policy(
    worktree: &Path,
    git_meta: Option<&Path>,
    cache_dir: Option<&Path>,
) -> anyhow::Result<()> {
    // Use V3 (Linux 5.19+). The probe in mod.rs verified the kernel supports
    // Landlock; V3 covers all practical kernels in 2026.
    let abi = ABI::V3;
    let full_access = AccessFs::from_all(abi);

    // Read-only subset: allow read and execute, deny all write operations.
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    // Cargo shared build cache: {CARGO_HOME}/build/ (default ~/.cargo/build/).
    // Agents need write access so `cargo test`/`cargo clippy` can use the shared
    // build-dir configured in .cargo/config.toml.
    let cargo_build_dir = std::env::var("CARGO_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.cargo")))
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("build"));

    let mut ruleset = Ruleset::default()
        .handle_access(full_access)?
        .create()?
        // Read + execute access everywhere on the filesystem.
        .add_rule(PathBeneath::new(PathFd::new("/")?, read_exec))?
        // Full (read + write) access to the task worktree.
        .add_rule(PathBeneath::new(PathFd::new(worktree)?, full_access))?
        // Full access to /var/tmp (disk-backed) and /dev/null et al.
        // /tmp is intentionally excluded: on Linux it's typically tmpfs and
        // writes there can silently consume RAM.
        .add_rule(PathBeneath::new(PathFd::new("/var/tmp")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/null")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/zero")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/urandom")?, full_access))?;

    // Cargo shared build cache directory.
    if let Some(ref dir) = cargo_build_dir.filter(|d| d.is_dir()) {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, full_access))?;
    }

    // Djinn agent scratch dir. The directory was already resolved and created
    // in the parent (see `prepare_cache_dir`) so here we only need to open it
    // and add the rule. If the open fails for any reason, silently skip: we
    // cannot log safely from pre_exec, and the sandbox is still functional
    // without the scratch allowance.
    if let Some(dir) = cache_dir
        && let Ok(fd) = PathFd::new(dir)
    {
        ruleset = ruleset.add_rule(PathBeneath::new(fd, full_access))?;
    }

    // Full .git/ dir needs write access for merge operations: object writes
    // (.git/objects/), ref updates (.git/refs/, .git/packed-refs), and
    // per-worktree state (.git/worktrees/{id}/ORIG_HEAD.lock etc.).
    if let Some(dot_git) = git_dir(worktree) {
        if dot_git.is_dir() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(&dot_git)?, full_access))?;
        }
    } else if let Some(meta) = git_meta {
        // Fallback: at least allow the worktree metadata dir.
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(meta)?, full_access))?;
    }

    ruleset.restrict_self()?;

    Ok(())
}
