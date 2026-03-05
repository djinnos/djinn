// macOS Seatbelt (sandbox-exec) backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt

use std::path::Path;

use anyhow::Result;

use super::Sandbox;

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
    fn apply(&self, worktree_path: &Path, cmd: &mut tokio::process::Command) -> Result<()> {
        let worktree = worktree_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worktree path contains non-UTF-8 characters"))?;

        let policy = format!(
            "(version 1)\
             (allow default)\
             (allow file-read*)\
             (deny file-write*)\
             (allow file-write* (subpath \"{worktree}\"))\
             (allow file-write* (subpath \"/tmp\"))\
             (allow file-write* (literal \"/dev/null\"))\
             (allow file-write* (literal \"/dev/zero\"))\
             (allow file-write* (literal \"/dev/urandom\"))"
        );

        // Snapshot the existing command configuration before we overwrite it.
        let program = cmd.as_std().get_program().to_owned();
        let args: Vec<std::ffi::OsString> =
            cmd.as_std().get_args().map(|a| a.to_owned()).collect();
        let current_dir = cmd.as_std().get_current_dir().map(|p| p.to_owned());
        let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();

        // Replace the command: sandbox-exec -p {policy} {original_program} {original_args}
        *cmd = tokio::process::Command::new("sandbox-exec");
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
    fn apply(&self, _worktree_path: &Path, _cmd: &mut tokio::process::Command) -> Result<()> {
        Ok(())
    }
}
