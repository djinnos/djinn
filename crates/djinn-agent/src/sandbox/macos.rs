// macOS Seatbelt (sandbox-exec) backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt

use std::path::Path;

use anyhow::Result;

use super::Sandbox;
#[cfg(target_os = "macos")]
use super::git_metadata_dir;

/// Seatbelt (sandbox-exec) based filesystem sandbox for macOS.
///
/// Generates a per-invocation SBPL policy string, then rewrites the command to
/// run under `sandbox-exec -p {policy} {original_program} {original_args}`.
/// Policy grants read everywhere and restricts writes to the task worktree and
/// `/tmp`.
#[cfg(target_os = "macos")]
pub struct SeatbeltSandbox;

#[cfg(target_os = "macos")]
impl Sandbox for SeatbeltSandbox {
    fn apply(&self, worktree_path: &Path, cmd: &mut std::process::Command) -> Result<()> {
        let worktree = worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree path contains non-UTF-8 characters"))?;

        // Git worktree metadata dir (e.g. .git/worktrees/{id}/) needs write
        // access for merge/rebase lock files.
        let git_meta_rule = git_metadata_dir(worktree_path)
            .and_then(|p| p.to_str().map(|s| s.to_owned()))
            .map(|m| format!("(allow file-write* (subpath \"{m}\"))"))
            .unwrap_or_default();

        // Cargo shared build cache: {CARGO_HOME}/build/ (default ~/.cargo/build/).
        let cargo_build_rule = std::env::var("CARGO_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.cargo")))
            .ok()
            .map(|base| format!("{base}/build"))
            .filter(|p| std::path::Path::new(p).is_dir())
            .map(|p| format!("(allow file-write* (subpath \"{p}\"))"))
            .unwrap_or_default();

        let policy = format!(
            "(version 1)\
             (allow default)\
             (allow file-read*)\
             (deny file-write*)\
             (allow file-write* (subpath \"{worktree}\"))\
             {git_meta_rule}\
             {cargo_build_rule}\
             (allow file-write* (subpath \"/tmp\"))\
             (allow file-write* (literal \"/dev/null\"))\
             (allow file-write* (literal \"/dev/zero\"))\
             (allow file-write* (literal \"/dev/urandom\"))"
        );

        // Snapshot the existing command configuration before we overwrite it.
        let program = cmd.get_program().to_owned();
        let args: Vec<std::ffi::OsString> = cmd.get_args().map(|a| a.to_owned()).collect();
        let current_dir = cmd.get_current_dir().map(|p| p.to_owned());
        let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();

        // Replace the command: sandbox-exec -p {policy} {original_program} {original_args}
        *cmd = std::process::Command::new("sandbox-exec");
        cmd.arg("-p").arg(policy).arg(program).args(args);

        if let Some(dir) = current_dir {
            cmd.current_dir(dir);
        }
        for (key, val) in envs {
            match val {
                Some(v) => {
                    cmd.env(key, v);
                }
                None => {
                    cmd.env_remove(key);
                }
            }
        }

        Ok(())
    }
}

// Compilation stub for non-macOS targets — allows `pub mod macos` in mod.rs
// without a platform gate at the module level.
#[cfg(not(target_os = "macos"))]
pub struct SeatbeltSandbox;

#[cfg(not(target_os = "macos"))]
impl Sandbox for SeatbeltSandbox {
    fn apply(&self, _worktree_path: &Path, _cmd: &mut std::process::Command) -> Result<()> {
        Ok(())
    }
}
