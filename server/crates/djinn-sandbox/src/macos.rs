// macOS Seatbelt (sandbox-exec) backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt

use std::path::Path;

use anyhow::Result;

use crate::{Sandbox, SandboxScope, djinn_cache_dir, git_metadata_dir};

/// Seatbelt (sandbox-exec) based filesystem sandbox for macOS.
///
/// Generates a per-invocation SBPL policy string, then rewrites the command to
/// run under `sandbox-exec -p {policy} {original_program} {original_args}`.
/// Policy grants read everywhere and restricts writes to the task worktree,
/// its git metadata directory, `/var/tmp`, a dedicated djinn agent scratch
/// dir (`$XDG_CACHE_HOME/djinn` or `$HOME/.cache/djinn`), and the usual
/// `/dev/{null,zero,urandom}` nodes. `/tmp` is intentionally excluded for
/// parity with the Linux backend, where it is tmpfs-backed and prone to
/// RAM leaks from runaway build artifacts.
pub struct SeatbeltSandbox;

impl Sandbox for SeatbeltSandbox {
    fn apply(&self, scope: SandboxScope<'_>, cmd: &mut std::process::Command) -> Result<()> {
        scope.validate()?;
        let worktree_path = match scope {
            SandboxScope::Worktree(path) => Some(path),
            SandboxScope::ReadSource { .. } => None,
        };

        // Git worktree metadata dir (e.g. .git/worktrees/{id}/) needs write
        // access for merge/rebase lock files.
        let git_meta_rule = worktree_path
            .and_then(git_metadata_dir)
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

        // Djinn agent scratch dir: $XDG_CACHE_HOME/djinn or $HOME/.cache/djinn.
        // Lazy-create so the subpath is a real directory when sandbox-exec
        // evaluates the policy. On create failure (e.g. read-only home), log
        // and skip the rule rather than aborting the whole sandbox setup.
        let cache_rule = match djinn_cache_dir() {
            Some(dir) => match std::fs::create_dir_all(&dir) {
                Ok(()) => dir
                    .to_str()
                    .map(|p| format!("(allow file-write* (subpath \"{p}\"))"))
                    .unwrap_or_else(|| {
                        tracing::warn!(
                            path = %dir.display(),
                            "sandbox: djinn cache dir contains non-UTF-8; skipping Seatbelt rule"
                        );
                        String::new()
                    }),
                Err(e) => {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "sandbox: failed to create djinn cache dir; skipping Seatbelt rule"
                    );
                    String::new()
                }
            },
            None => String::new(),
        };

        let worktree_rule = worktree_path
            .and_then(Path::to_str)
            .map(|p| format!("(allow file-write* (subpath \"{p}\"))"))
            .unwrap_or_default();
        let policy = format!(
            "(version 1)\
             (allow default)\
             (allow file-read*)\
             (deny file-write*)\
             {worktree_rule}\
             {git_meta_rule}\
             {cargo_build_rule}\
             {cache_rule}\
             (allow file-write* (subpath \"/var/tmp\"))\
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(scope: SandboxScope<'_>, path: &Path) -> std::process::Output {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf x > \"$1\"", "--"]).arg(path);
        SeatbeltSandbox
            .apply(scope, &mut cmd)
            .expect("scope should configure Seatbelt");
        cmd.output().expect("sandboxed shell should spawn")
    }

    /// Execute Seatbelt for both authorities. A read source gets no writable
    /// subpath rule, including for `.git`, whereas the owning task worktree
    /// continues to receive its explicit writable allowance.
    #[test]
    fn read_source_policy_denies_content_and_git_writes_but_allows_worktree() {
        if std::process::Command::new("sandbox-exec")
            .arg("-h")
            .output()
            .is_err()
        {
            return;
        }
        let source = tempfile::tempdir_in(std::env::current_dir().expect("test directory"))
            .expect("read source");
        let source_git = source.path().join(".git");
        std::fs::create_dir(&source_git).expect("source git directory");
        let worktree = tempfile::tempdir_in("/var/tmp").expect("worktree");

        let source_content = source.path().join("source-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_content,
            )
            .status
            .success(),
            "Seatbelt must deny writes to read-source content"
        );
        assert!(!source_content.exists());

        let source_metadata = source_git.join("metadata-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_metadata,
            )
            .status
            .success(),
            "Seatbelt must deny writes to read-source Git metadata"
        );
        assert!(!source_metadata.exists());

        let worktree_content = worktree.path().join("worktree-write");
        assert!(
            write_file(SandboxScope::Worktree(worktree.path()), &worktree_content)
                .status
                .success(),
            "Seatbelt must retain task-worktree write access"
        );
        assert!(worktree_content.exists());
    }
}
