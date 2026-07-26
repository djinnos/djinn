//! Publishing a git config file for a process in ANOTHER container to read.
//!
//! # Why this is not just `git config --file`
//!
//! `djinn_agent_worker::configure_private_dep_access` stores the project's
//! GitHub installation token as a `url.<...>.insteadOf` rewrite. Its
//! `git config --global` write lands in the worker container's
//! `$HOME/.gitconfig` — and once the cgroup launcher is armed, the cargo / go /
//! pnpm processes that exist to consume it do not run in that container. They are
//! born in the launcher's mount namespace, where `$HOME` is a different volume.
//! Measured on the production node: the worker's `/home/djinn/.gitconfig` is
//! present and owned `1000:1000`; the launcher's `/home/djinn` is empty. So every
//! brokered private-dependency fetch went out unauthenticated, and silently
//! (GitHub answers a private repo with 404, which reads as "not found").
//!
//! The fix hands the rewrite across on a one-way channel — see
//! `djinn_k8s::private_dep_config` — and that changes two things about the write:
//!
//! * **the reader is a different uid than the writer**, by construction: the
//!   worker is uid 1000 and a brokered child is `CHILD_UID` (1001). `git config
//!   --file` creates with `0666` masked by the process umask, and this repo has
//!   already shipped a umask-022 regression that broke the warm path — so the
//!   child's ability to read this must not depend on an ambient umask. The mode
//!   is set explicitly, exactly as `djinn_cgroup_launcher::git_trust` does for
//!   the trust anchor.
//! * **the directory may not exist yet**, because it is a fresh `emptyDir`.
//!
//! It stays a real `git config` invocation rather than a hand-rolled INI writer:
//! the value carries a token that may contain characters git escapes, and the
//! file has to be one git will read back.

use std::path::Path;

use crate::GitError;

/// Mode the published file is left at.
///
/// World-readable on purpose: the reader is a different uid inside the same pod,
/// and the pod boundary is the security perimeter. Not group- or other-WRITABLE,
/// because the launcher-side mount is `readOnly` and this must not be the thing
/// that would have made it writable if that flag were ever dropped.
pub const PUBLISHED_MODE: u32 = 0o644;

/// Write `key = value` into the git config file at `path`, readable by another
/// uid in the pod.
///
/// Creates the parent directory if needed. Never logs `key`: callers pass a
/// token-bearing key.
pub async fn publish(path: &Path, key: String, value: String) -> Result<(), GitError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| io_as_git_error(format!("create_dir_all {}", parent.display()), &e))?;
    }
    crate::run_git_command_in(
        Path::new("/"),
        vec![
            "config".into(),
            "--file".into(),
            path.display().to_string(),
            key,
            value,
        ],
    )
    .await?;
    tokio::fs::set_permissions(
        path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(PUBLISHED_MODE),
    )
    .await
    .map_err(|e| io_as_git_error(format!("chmod {PUBLISHED_MODE:o} {}", path.display()), &e))
}

/// Shape an `io::Error` as the crate's own error type so callers keep one match.
fn io_as_git_error(command: String, error: &std::io::Error) -> GitError {
    GitError::CommandFailed {
        code: -1,
        command,
        cwd: "/".to_owned(),
        stdout: String::new(),
        stderr: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> std::path::PathBuf {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "djinn-exported-config-{label}-{}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    /// Real git writes it, real git reads it back, and the mode does not depend
    /// on the ambient umask — which is the whole reason this is not a one-liner.
    #[tokio::test]
    async fn a_published_config_is_readable_by_another_uid_under_a_hostile_umask() {
        let root = scratch("umask");
        let path = root.join("child-git").join("gitconfig");
        // SAFETY: `umask` cannot fail. Restored immediately; the value only
        // affects file creation inside this call.
        let previous = unsafe { libc::umask(0o077) };
        let published = publish(
            &path,
            "url.https://x-access-token:t0ken@github.com/acme/.insteadOf".into(),
            "https://github.com/acme/".into(),
        )
        .await;
        unsafe { libc::umask(previous) };
        published.expect("publish must create the directory and the file");

        assert_eq!(
            std::fs::symlink_metadata(&path)
                .expect("stat published config")
                .permissions()
                .mode()
                & 0o777,
            PUBLISHED_MODE,
            "a umask-077 worker must still leave a file the brokered child (a different uid) \
             can read; this repo has already shipped a umask regression of exactly this shape"
        );

        // Read it back with real git, from the file alone — no scope inheritance.
        let read = crate::run_git_command_in(
            Path::new("/"),
            vec![
                "config".into(),
                "--file".into(),
                path.display().to_string(),
                "--get".into(),
                "url.https://x-access-token:t0ken@github.com/acme/.insteadOf".into(),
            ],
        )
        .await
        .expect("git must read back what git wrote");
        assert_eq!(read.stdout.trim(), "https://github.com/acme/");
        let _ = std::fs::remove_dir_all(&root);
    }
}
