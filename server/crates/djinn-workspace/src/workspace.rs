use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("git: {0}")]
    Git(String),
}

/// A committer / author identity used for automated commits inside a workspace.
///
/// Decoupled from any specific bot identity so `djinn-workspace` has no
/// dependency on `djinn-provider`. Callers (typically the supervisor) supply
/// the GitHub App bot identity resolved at runtime.
#[derive(Debug, Clone, Copy)]
pub struct GitIdentity<'a> {
    pub name: &'a str,
    pub email: &'a str,
}

/// A tempdir-backed ephemeral clone of a project mirror.
///
/// Scope = one task-run. Contents (including the git object db, since clones
/// are `--local --shared`) are discarded when the `TempDir` is dropped.
/// Mutations that must survive the task-run are pushed to the origin remote
/// via `commit` → push-by-the-supervisor.
#[derive(Debug)]
pub struct Workspace {
    dir: TempDir,
    branch: String,
}

impl Workspace {
    pub(crate) fn new(dir: TempDir, branch: String) -> Self {
        Self { dir, branch }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Stage every change and commit with `message` under `identity`.
    ///
    /// Returns `Ok(true)` if a commit was created, `Ok(false)` if the tree
    /// was clean (nothing to commit). Both outcomes are success — callers
    /// that require a commit should check the return value.
    pub async fn commit(
        &self,
        message: &str,
        identity: GitIdentity<'_>,
    ) -> Result<bool, WorkspaceError> {
        self.run_git(&["add", "-A"], &[]).await?;
        let staged = self
            .run_git(&["diff", "--cached", "--name-only"], &[])
            .await?;
        if staged.trim().is_empty() {
            return Ok(false);
        }
        self.run_git(
            &["commit", "-m", message],
            &[
                ("GIT_AUTHOR_NAME", identity.name),
                ("GIT_AUTHOR_EMAIL", identity.email),
                ("GIT_COMMITTER_NAME", identity.name),
                ("GIT_COMMITTER_EMAIL", identity.email),
            ],
        )
        .await?;
        Ok(true)
    }

    /// Explicit teardown. Equivalent to `drop(self)` — the `TempDir` cleans
    /// itself up on drop. Callers may prefer the explicit form to document
    /// lifecycle points in supervisor code.
    pub fn teardown(self) {}

    async fn run_git(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<String, WorkspaceError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.dir.path()).args(args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorkspaceError::Git(format!(
                "git {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
