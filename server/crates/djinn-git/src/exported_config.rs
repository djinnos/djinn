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
//!
//! # The errors carry the credential, so they are scrubbed
//!
//! `GitError::CommandFailed` renders `command: args.join(" ")`, and for this
//! family of writes one of those args is
//! `url.https://x-access-token:<TOKEN>@github.com/<owner>/.insteadOf`. Every
//! caller logs the error. So an ordinary `git config` failure — a bad owner
//! string, a full disk — would print a live installation token into the worker's
//! log stream, which is shipped and retained. `git` can echo the key back on
//! stderr too (`error: invalid key: …`), so scrubbing the argv alone is not
//! enough. [`scrub`] covers both, and the token never travels through this module
//! unredacted in an error.
//!
//! Both token-bearing writes therefore live here, [`publish`] for the
//! launcher-visible channel and [`publish_global`] for the worker's own
//! `$HOME/.gitconfig`, so the redaction cannot be applied to one and forgotten on
//! the other.

use std::path::Path;

use crate::GitError;

/// Mode the published file is left at.
///
/// World-readable on purpose: the reader is a different uid inside the same pod,
/// and the pod boundary is the security perimeter. Not group- or other-WRITABLE,
/// because the launcher-side mount is `readOnly` and this must not be the thing
/// that would have made it writable if that flag were ever dropped.
pub const PUBLISHED_MODE: u32 = 0o644;

/// Marker whose value in a rewrite URL is the credential.
const TOKEN_USER: &str = "x-access-token:";

/// Placeholder substituted for anything that could be the credential.
const REDACTED: &str = "<redacted>";

/// Remove `key` — and any `x-access-token:…@` credential, however it was
/// reformatted on the way out — from every rendered field of `error`.
///
/// Returns a `CommandFailed` whose `command` names the operation rather than the
/// argv, because the argv is the leak. `stdout`/`stderr` are kept (they are what
/// makes a failure diagnosable) with both forms of the credential scrubbed.
fn scrub(error: GitError, operation: &str, key: &str) -> GitError {
    let GitError::CommandFailed {
        code,
        cwd,
        stdout,
        stderr,
        ..
    } = error
    else {
        // Nothing else in this crate's error set embeds the argv.
        return error;
    };
    GitError::CommandFailed {
        code,
        command: operation.to_owned(),
        cwd,
        stdout: scrub_text(&stdout, key),
        stderr: scrub_text(&stderr, key),
    }
}

/// [`scrub`] for one string: the whole key, then any surviving
/// `x-access-token:<...>@` span.
fn scrub_text(text: &str, key: &str) -> String {
    let mut out = if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, REDACTED)
    };
    while let Some(start) = out.find(TOKEN_USER) {
        let secret = start + TOKEN_USER.len();
        // Up to the `@` that ends the userinfo, or to the end of the line if git
        // truncated it. Either way the span cannot be left in.
        let end = out[secret..]
            .find(['@', '\n'])
            .map_or(out.len(), |offset| secret + offset);
        out.replace_range(start..end, REDACTED);
    }
    out
}

/// Write `key = value` into the git config file at `path`, readable by another
/// uid in the pod.
///
/// Creates the parent directory if needed. The returned error is scrubbed:
/// callers pass a token-bearing `key` and then log what comes back.
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
            key.clone(),
            value,
        ],
    )
    .await
    .map_err(|e| {
        scrub(
            e,
            &format!("config --file {} <key> <value>", path.display()),
            &key,
        )
    })?;
    tokio::fs::set_permissions(
        path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(PUBLISHED_MODE),
    )
    .await
    .map_err(|e| io_as_git_error(format!("chmod {PUBLISHED_MODE:o} {}", path.display()), &e))
}

/// `git config --global key value`, with the same redaction as [`publish`].
///
/// This is the pre-existing worker-side write; it lives here so the credential
/// scrubbing is applied to both token-bearing writes from one place rather than
/// remembered at each call site.
pub async fn publish_global(key: String, value: String) -> Result<(), GitError> {
    crate::run_git_command_in(
        Path::new("/"),
        vec!["config".into(), "--global".into(), key.clone(), value],
    )
    .await
    .map(|_| ())
    .map_err(|e| scrub(e, "config --global <key> <value>", &key))
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

    const CANARY: &str = "ghs-EXPORTED-CONFIG-TOKEN-CANARY";

    /// A real `git config` failure must not print the installation token.
    ///
    /// `GitError::CommandFailed` renders `command: args.join(" ")` and every
    /// caller logs the error, so without scrubbing an ordinary failure ships a
    /// live credential to the log stream. Driven with a REAL failing git
    /// invocation, not a hand-built error, and the control is the same failure
    /// through the unscrubbed path — which must leak, or this proves nothing.
    #[tokio::test]
    async fn a_failing_publish_does_not_render_the_token() {
        let key = format!("url.https://{TOKEN_USER}{CANARY}@github.com/acme/.insteadOf");
        // A path under a plain file: `git config --file` cannot open it, and the
        // parent `create_dir_all` fails first only if it is a directory — so use
        // an unwritable location git itself must reject.
        let root = scratch("leak");
        std::fs::create_dir_all(&root).expect("create root");
        let blocker = root.join("blocker");
        std::fs::write(&blocker, "not a directory").expect("write blocker");
        let target = blocker.join("nested").join("gitconfig");

        let error = publish(&target, key.clone(), "https://github.com/acme/".into())
            .await
            .expect_err("publishing under a regular file must fail");
        let rendered = error.to_string();
        assert!(
            !rendered.contains(CANARY),
            "the installation token reached a rendered error: {rendered}"
        );

        // CONTROL: the same failure without scrubbing. If this does not leak, the
        // assertion above is vacuous — the error simply never carried the key.
        let unscrubbed = crate::run_git_command_in(
            Path::new("/"),
            vec![
                "config".into(),
                "--file".into(),
                target.display().to_string(),
                key,
                "https://github.com/acme/".into(),
            ],
        )
        .await
        .expect_err("the same git invocation must fail");
        assert!(
            unscrubbed.to_string().contains(CANARY),
            "CONTROL FAILED TO FAIL: the unscrubbed error did not carry the token, so the \
             assertion above proves nothing. Got: {unscrubbed}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The scrubber must also survive git reformatting the key on stderr, where
    /// the whole-key replacement cannot match.
    #[test]
    fn a_reformatted_credential_is_still_removed() {
        let key = format!("url.https://{TOKEN_USER}{CANARY}@github.com/acme/.insteadOf");
        for text in [
            format!("error: invalid key: {key}"),
            // Quoted, re-cased, or truncated: the userinfo span still goes.
            format!("fatal: 'https://{TOKEN_USER}{CANARY}@github.com/acme/' rejected"),
            format!("https://{TOKEN_USER}{CANARY}\nnext line"),
        ] {
            let scrubbed = scrub_text(&text, &key);
            assert!(
                !scrubbed.contains(CANARY),
                "credential survived scrubbing: {scrubbed}"
            );
        }
        // And an unrelated message is left alone.
        assert_eq!(
            scrub_text("fatal: could not lock config file", &key),
            "fatal: could not lock config file"
        );
    }
}
