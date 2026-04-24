use std::path::{Path, PathBuf};

use tempfile::TempDir;
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum EphemeralWorkspaceError {
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

/// How the workspace's on-disk root is owned.
///
/// `Owned` drops the underlying `TempDir` when the workspace is dropped —
/// the in-process / host-side path. `Attached` just remembers a borrowed
/// path; someone else (e.g. the Docker runtime that bind-mounted
/// `/workspace` into the container) is responsible for cleanup.
#[derive(Debug)]
enum WorkspaceRoot {
    Owned(TempDir),
    Attached(PathBuf),
}

impl WorkspaceRoot {
    fn path(&self) -> &Path {
        match self {
            WorkspaceRoot::Owned(dir) => dir.path(),
            WorkspaceRoot::Attached(path) => path.as_path(),
        }
    }
}

/// A tempdir-backed (or externally-bound) ephemeral clone of a project mirror.
///
/// Scope = one task-run. For the default `Owned` variant, contents (including
/// the git object db, since clones are `--local --shared`) are discarded when
/// the `TempDir` is dropped.  The `Attached` variant — constructed via
/// [`Workspace::attach_existing`] — wraps a directory the caller manages
/// (e.g. a bind-mounted `/workspace` inside a container); drop is a no-op.
///
/// Mutations that must survive the task-run are pushed to the origin remote
/// via `commit` → push-by-the-supervisor.
#[derive(Debug)]
pub struct Workspace {
    root: WorkspaceRoot,
    branch: String,
}

impl Workspace {
    pub(crate) fn new(dir: TempDir, branch: String) -> Self {
        Self {
            root: WorkspaceRoot::Owned(dir),
            branch,
        }
    }

    /// Attach to an existing on-disk workspace the caller already owns.
    ///
    /// Used by `djinn-agent-worker` when the host-side runtime has already
    /// cloned the mirror into a bind mount (`/workspace` inside the
    /// container) — the in-container supervisor reuses the same path instead
    /// of re-cloning.  The returned [`Workspace`] never drops the directory
    /// itself; lifetime is bound to the caller's mount lifecycle.
    ///
    /// Fails if `path` does not exist or is not a directory — the runtime is
    /// expected to materialise the clone before calling this.
    pub fn attach_existing(
        path: impl Into<PathBuf>,
        branch: impl Into<String>,
    ) -> Result<Self, EphemeralWorkspaceError> {
        let path = path.into();
        let meta = std::fs::metadata(&path).map_err(EphemeralWorkspaceError::Io)?;
        if !meta.is_dir() {
            return Err(EphemeralWorkspaceError::Git(format!(
                "attach_existing: {} is not a directory",
                path.display()
            )));
        }
        Ok(Self {
            root: WorkspaceRoot::Attached(path),
            branch: branch.into(),
        })
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn path_buf(&self) -> PathBuf {
        self.root.path().to_path_buf()
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
    ) -> Result<bool, EphemeralWorkspaceError> {
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
    ) -> Result<String, EphemeralWorkspaceError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.root.path()).args(args);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let output = cmd.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(EphemeralWorkspaceError::Git(format!(
                "git {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_existing_wraps_existing_dir_without_temp_ownership() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().to_path_buf();
        let ws = Workspace::attach_existing(&path, "main").expect("attach");
        assert_eq!(ws.path(), path);
        assert_eq!(ws.branch(), "main");
        // Dropping the workspace must NOT remove the caller-owned directory.
        drop(ws);
        assert!(path.exists(), "attach_existing must not delete the dir");
    }

    #[test]
    fn attach_existing_rejects_missing_path() {
        let tmp = TempDir::new().expect("tempdir");
        let missing = tmp.path().join("nope");
        let err = Workspace::attach_existing(&missing, "main").unwrap_err();
        assert!(matches!(err, EphemeralWorkspaceError::Io(_)));
    }

    #[test]
    fn attach_existing_rejects_non_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, b"not a dir").expect("write");
        let err = Workspace::attach_existing(&file, "main").unwrap_err();
        match err {
            EphemeralWorkspaceError::Git(msg) => assert!(msg.contains("not a directory")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
