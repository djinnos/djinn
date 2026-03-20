// Linux Landlock sandbox backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate

#![cfg(target_os = "linux")]

use std::io;
use std::path::Path;

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

use super::{Sandbox, git_dir, git_metadata_dir};

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-everywhere, write only to the
/// task worktree, its git metadata directory, and /tmp + /var/tmp.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, worktree_path: &Path, cmd: &mut std::process::Command) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let worktree = worktree_path.to_path_buf();
        let git_meta = git_metadata_dir(worktree_path);
        // Safety: pre_exec runs in the forked child process. The closure only
        // performs Landlock syscalls and open(2) calls, which are safe after fork.
        unsafe {
            cmd.pre_exec(move || {
                apply_policy(&worktree, git_meta.as_deref())
                    .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
            });
        }
        Ok(())
    }
}

/// Build and apply the Landlock policy in the current process.
///
/// Called inside `pre_exec` (forked child) so it takes effect before exec.
fn apply_policy(worktree: &Path, git_meta: Option<&Path>) -> anyhow::Result<()> {
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
        // Full access to shared temp directories and /dev/null et al.
        .add_rule(PathBeneath::new(PathFd::new("/tmp")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/var/tmp")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/null")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/zero")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/urandom")?, full_access))?;

    // Cargo shared build cache directory.
    if let Some(ref dir) = cargo_build_dir.filter(|d| d.is_dir()) {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, full_access))?;
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
