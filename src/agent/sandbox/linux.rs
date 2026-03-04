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

use super::Sandbox;

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-everywhere, write only to the
/// task worktree and /tmp + /var/tmp.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, worktree_path: &Path, cmd: &mut tokio::process::Command) -> Result<()> {
        let worktree = worktree_path.to_path_buf();
        // Safety: pre_exec runs in the forked child process. The closure only
        // performs Landlock syscalls and open(2) calls, which are safe after fork.
        unsafe {
            cmd.pre_exec(move || {
                apply_policy(&worktree)
                    .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
            });
        }
        Ok(())
    }
}

/// Build and apply the Landlock policy in the current process.
///
/// Called inside `pre_exec` (forked child) so it takes effect before exec.
fn apply_policy(worktree: &Path) -> anyhow::Result<()> {
    // Use V3 (Linux 5.19+). The probe in mod.rs verified the kernel supports
    // Landlock; V3 covers all practical kernels in 2026.
    let abi = ABI::V3;
    let full_access = AccessFs::from_all(abi);

    // Read-only subset: allow read and execute, deny all write operations.
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    Ruleset::default()
        .handle_access(full_access)?
        .create()?
        // Read + execute access everywhere on the filesystem.
        .add_rule(PathBeneath::new(PathFd::new("/")?, read_exec))?
        // Full (read + write) access to the task worktree.
        .add_rule(PathBeneath::new(PathFd::new(worktree)?, full_access))?
        // Full access to shared temp directories.
        .add_rule(PathBeneath::new(PathFd::new("/tmp")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/var/tmp")?, full_access))?
        .restrict_self()?;

    Ok(())
}
