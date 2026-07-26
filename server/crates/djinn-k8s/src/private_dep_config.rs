//! The one-way worker→child channel carrying private-dependency git auth.
//!
//! # The blocker (goxi, ninth in the chain)
//!
//! `djinn_agent_worker::configure_private_dep_access` mints the project's GitHub
//! installation token into a `url.<...>.insteadOf` rewrite and stores it with
//! `git config --global`, i.e. in the **worker container's** `$HOME/.gitconfig`.
//! cargo (`git-fetch-with-cli`), `go` (`GOPRIVATE` + direct git), and pnpm/npm
//! git dependencies then read it back with djinn nowhere in the loop. That is
//! the entire mechanism by which a build resolves the project org's private
//! transitive dependencies.
//!
//! A brokered command does not run in the worker container. `$HOME` resolves to
//! the same *string* in both containers and to a different *filesystem* in each:
//! the worker's is the image layer, the launcher's is the `launcher-home`
//! emptyDir `readOnlyRootFilesystem: true` forced it to mount. Measured on the
//! production node, in a pod carrying both containers on the same image:
//!
//! ```text
//! # worker: git config --global url.https://x-access-token:…@github.com/… …
//! -rw-r--r-- 1 1000 1000 98 /home/djinn/.gitconfig
//!
//! # launcher, same pod, same $HOME string
//! $ ls -la /home/djinn
//! total 8            <- empty: a different volume entirely
//! ```
//!
//! So arming the launcher makes every brokered private-dependency fetch go out
//! **unauthenticated**, and silently: GitHub answers a private repo with a 404,
//! which cargo and go report as "not found", not as "not authorized". This repo
//! already carries a `warm promisor git auth failure` note for the same class.
//! `#2617` reasoned that "`$HOME/.gitconfig` keeps being read exactly as
//! before", which is true within one container and false across the boundary —
//! and that boundary is exactly what arming the launcher introduces.
//!
//! # Why the fix is not "share `$HOME`"
//!
//! Mounting one emptyDir at `/home/djinn` in both containers is the obvious
//! move and it is a privilege escalation. `$HOME/.gitconfig` is **global-scope
//! git configuration for whoever reads it**, and the worker reads it too — every
//! `git` the worker itself runs (mirror fetch, task-branch push) loads it. A
//! shared writable `$HOME` therefore lets the brokered child — repository-
//! controlled code running as `CHILD_UID`, deliberately holding nothing — write
//! `core.sshCommand` into a file the worker executes as uid 1000, which *can*
//! read `/var/run/djinn/credentials.bin`. The broker exists to prevent exactly
//! that hop. The same objection defeats "have the worker write it somewhere
//! both containers can write".
//!
//! # The channel
//!
//! An `emptyDir` mounted **read-write in the worker** and **`readOnly: true` in
//! the launcher**. The direction is enforced by the kubelet, not by convention:
//! the child cannot write the file at all, and the worker never reads anything
//! the child produced. `#2617`'s choice of `GIT_CONFIG_SYSTEM` over
//! `GIT_CONFIG_GLOBAL` is preserved untouched — `$HOME/.gitconfig` remains the
//! private-dep token's home *in the worker*, and the launcher's own trust anchor
//! `[include]`s this file so the child receives the rewrite in protected
//! system scope without any `GIT_CONFIG_GLOBAL` redirection.
//!
//! The path is passed to the worker as [`CHILD_GIT_CONFIG_PATH_ENV`] and to the
//! launcher as the same key, rather than compiled into either binary. That is
//! the `DJINN_INVOCATION_JOURNAL_DIR` pattern from blocker 4, and it is also
//! what lets the reachability guard *see* this handoff: a runtime-written path
//! that is named in the render is a path the derived invariant covers.
//!
//! # Exposing the token to the launcher container: yes, deliberately
//!
//! The launcher does not mount the credentials Secret or the projected
//! ServiceAccount token, and this does not change that. What it does do is put a
//! short-lived (~1h) GitHub **App installation token**, scoped to the project's
//! own installation, on a filesystem the launcher container and its child can
//! read. That is accepted, for three reasons:
//!
//! 1. **The child is the intended consumer.** cargo/go/pnpm are the processes
//!    that must present this credential; a design in which they cannot see it is
//!    a design in which private dependencies do not resolve.
//! 2. **It is not a widening against the unbrokered baseline.** Today the
//!    agent's shell runs in the worker container and can read the same token out
//!    of `$HOME/.gitconfig` directly. Brokering must not *reduce* what a command
//!    can do, and this restores parity — it does not exceed it.
//! 3. **It is categorically different from the credential mounts.** Those carry
//!    the org's provider credentials and an apiserver-audience token, neither of
//!    which any build tool has a use for. This carries repo-scoped git auth with
//!    a one-hour life.
//!
//! What is NOT accepted, and is what the read-only direction buys: the child
//! being able to *modify* the configuration the token travels in.

use k8s_openapi::api::core::v1::{EmptyDirVolumeSource, EnvVar, Volume, VolumeMount};

/// Volume name for the one-way private-dependency git config channel.
pub const VOLUME_CHILD_GIT_CONFIG: &str = "child-git-config";

/// Directory the channel is mounted at in both containers.
///
/// Nested inside the read-only `spec` Secret mount at `/var/run/djinn`, exactly
/// as `launcher-ipc` and `invocation-journal` already are. A nested `emptyDir`
/// is writable while its read-only parent is not — measured for blocker 4 — and
/// the reachability guard resolves coverage by LONGEST mount path, so this is
/// classified by its own volume rather than by the Secret above it.
pub const CHILD_GIT_CONFIG_DIR: &str = "/var/run/djinn/child-git";

/// The file the worker writes and the launcher's trust anchor includes.
pub const CHILD_GIT_CONFIG_FILE: &str = "/var/run/djinn/child-git/gitconfig";

/// Env var naming [`CHILD_GIT_CONFIG_FILE`] to the worker and the launcher.
///
/// `DJINN_`-prefixed, so `is_allowed_environment_key` forwards it to the child
/// too. That is harmless — the child already reads the file's *contents*
/// through the anchor — and it keeps the path a single rendered fact instead of
/// a constant compiled separately into three binaries.
pub const CHILD_GIT_CONFIG_PATH_ENV: &str = "DJINN_CHILD_GIT_CONFIG_PATH";

/// The channel volume.
///
/// Disk-backed rather than `Memory`: the launcher container's memory limit is
/// the build's ceiling, and this file is real (if tiny) data.
pub fn child_git_config_volume() -> Volume {
    Volume {
        name: VOLUME_CHILD_GIT_CONFIG.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    }
}

/// The worker's mount: read-write, because the worker is the writer.
pub fn worker_child_git_mount() -> VolumeMount {
    VolumeMount {
        name: VOLUME_CHILD_GIT_CONFIG.to_string(),
        mount_path: CHILD_GIT_CONFIG_DIR.to_string(),
        ..VolumeMount::default()
    }
}

/// The launcher's mount: `readOnly`, because the child is only ever a reader.
///
/// This flag is the whole security property of the channel. Without it the file
/// is child-writable and the launcher's system-scope `[include]` becomes an
/// arbitrary-git-configuration primitive handed to repository-controlled code —
/// the precise hazard `#2617` closed for the environment form.
pub fn launcher_child_git_mount() -> VolumeMount {
    VolumeMount {
        name: VOLUME_CHILD_GIT_CONFIG.to_string(),
        mount_path: CHILD_GIT_CONFIG_DIR.to_string(),
        read_only: Some(true),
        ..VolumeMount::default()
    }
}

/// `DJINN_CHILD_GIT_CONFIG_PATH`, for either container's env list.
pub fn child_git_config_env() -> EnvVar {
    EnvVar {
        name: CHILD_GIT_CONFIG_PATH_ENV.to_string(),
        value: Some(CHILD_GIT_CONFIG_FILE.to_string()),
        ..EnvVar::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The direction IS the security property, so assert it directly rather
    /// than trusting the doc comment above it.
    #[test]
    fn the_channel_is_writable_only_on_the_worker_side() {
        assert_eq!(
            worker_child_git_mount().read_only,
            None,
            "the worker is the writer; a read-only mount would make \
             configure_private_dep_access fail EROFS"
        );
        assert_eq!(
            launcher_child_git_mount().read_only,
            Some(true),
            "the child must not be able to rewrite git configuration that the launcher's \
             trust anchor includes at protected system scope"
        );
        assert_eq!(
            worker_child_git_mount().name,
            launcher_child_git_mount().name,
            "one volume, or it is not a channel at all"
        );
        assert_eq!(
            worker_child_git_mount().mount_path,
            launcher_child_git_mount().mount_path,
            "both sides must agree on the path the env var names"
        );
    }

    #[test]
    fn the_rendered_path_is_the_file_inside_the_rendered_directory() {
        assert_eq!(
            child_git_config_env().value.as_deref(),
            Some(CHILD_GIT_CONFIG_FILE)
        );
        assert!(
            crate::launcher_child_fs::is_under(CHILD_GIT_CONFIG_FILE, CHILD_GIT_CONFIG_DIR),
            "the named file must live inside the mounted directory, or the worker writes \
             onto the read-only Secret above it"
        );
        assert_ne!(
            CHILD_GIT_CONFIG_FILE, CHILD_GIT_CONFIG_DIR,
            "the mount root is a directory; the env var names a file in it"
        );
    }
}
