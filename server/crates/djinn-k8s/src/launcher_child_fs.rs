//! The filesystem surface a launcher-spawned child needs, and why the launcher
//! container has to carry it.
//!
//! # The blocker (goxi, fifth in the chain)
//!
//! A brokered command does not run in the worker container. `clone3` /
//! `CLONE_INTO_CGROUP` puts it in the **launcher container's** mount namespace,
//! so the only paths it can reach are the ones the *launcher* mounts. Before
//! this module the launcher mounted exactly two volumes — `launcher-ipc` — and nothing else. Measured inside a rendered launcher
//! container on the production node:
//!
//! ```text
//! $ ls /workspace /cache /mirror
//! ls: cannot access '/workspace': No such file or directory
//! ls: cannot access '/mirror': No such file or directory
//! /cache:   <- the IMAGE's /cache, not the PVC
//! ```
//!
//! Every brokered command carries `cwd` under the workspace, so `spawn.rs`'s
//! post-fork `chdir` failed `ENOENT` and the child `_exit`ed before `execve`.
//! `env.rs` already documents that the child is expected to see
//! `CARGO_TARGET_DIR` under `/cache` and `TMPDIR` under the workspace — the pod
//! *names* those paths to the worker and the broker *forwards* them — so this
//! was a render omission, not a design choice.
//!
//! # The invariant
//!
//! > Every filesystem path the rendered Pod names to the worker through an
//! > environment variable the broker forwards to a child must be reachable and
//! > writable in the launcher container's mount namespace; every path the child
//! > is forbidden to see must NOT be.
//!
//! Both halves matter. The first is this blocker. The second is the isolation
//! property `djinn_cgroup_launcher::child::ChildMounts::validate` enforces at
//! runtime: the task spec, the per-task-run credentials bundle and the
//! projected service-account token are worker-private, and mirroring *those*
//! onto the launcher would hand a child the credentials the broker exists to
//! keep away from it.
//!
//! The guard in `tests/launcher_child_filesystem_reachability.rs` derives both
//! directions from the rendered manifest rather than naming `/workspace`: the
//! required set comes from the rendered worker env filtered through
//! `is_allowed_environment_key`, and "worker-private" means "the worker's own
//! covering mount is Secret- or projected-backed" — which is why the launcher
//! IPC volume at `/var/run/djinn/launcher`, nested *inside* the `spec` Secret
//! mount, is correctly classified as shared rather than private. It proves
//! itself non-vacuous by stripping the mount and reproducing the production
//! `chdir` `ENOENT` by name against a materialized mount tree.
//!
//! # `readOnlyRootFilesystem: true` and the image-layer scratch surfaces
//!
//! Mirroring the worker's volumes is necessary but not sufficient. The launcher
//! runs with `readOnlyRootFilesystem: true`, which the worker container does
//! not, so **every writable surface that lives in the image layer disappears**.
//! Measured in the rendered launcher container on the production node, with the
//! data mounts already added:
//!
//! ```text
//! # uid 0, inside cgroup-launcher
//! /tmp:         touch: Read-only file system
//! /home/djinn:  touch: Read-only file system
//!
//! # setpriv --reuid=1001 --regid=1000 --clear-groups  (the real child creds)
//! git config --global user.name probe
//!   error: could not lock config file /home/djinn/.gitconfig: Read-only file system
//! ```
//!
//! The same two paths are writable in the worker container (measured on the
//! same pod), so arming the launcher without this would be a *regression*
//! against the unbrokered path rather than a new constraint.
//!
//! * **`/tmp`** — a build is a tree of third-party tools and the ones that
//!   hardcode `/tmp` do not consult `TMPDIR` at all.
//! * **`/var/tmp`** — the eighth blocker, and the reason the render can no
//!   longer be derived from the manifest alone. See below.
//! * **`$HOME`** — `git config --global`, `rustup`, `npm`, `fnm` and `gopls`
//!   all write there; `djinn_agent_worker::volume_contract` fails worker
//!   readiness when `$HOME` is not writable *for exactly this reason*, and its
//!   docs already assert "the launcher-spawned child (uid 1001, group 1000)
//!   write[s] through the group". The image satisfies that with `2775`
//!   ownership; `readOnlyRootFilesystem` defeats it regardless of ownership.
//!
//! Both are supplied as ordinary (disk-backed) `emptyDir`s: a build's scratch
//! and a toolchain's home state are real data, not a socket, so they should not
//! consume the pod's memory limit. Masking the image's own copies costs
//! nothing — measured, the image's `/home/djinn` holds one empty `.local/state`
//! and `/tmp` holds only build-time detritus.
//!
//! # Ownership: no chown is needed, and that was measured, not assumed
//!
//! This repo has twice been bitten by assuming a mount is enough (the
//! `protected_hardlinks` EPERM on foreign-owned sources, and the `/mirror`
//! chown see-saw where no single owner satisfied both uid 10001 and uid 1000),
//! so the child's ability to write was measured on the real PVCs rather than
//! derived from the manifest. On the production node:
//!
//! | path         | owner:group  | mode         | uid 1001 / gid 1000 |
//! |--------------|--------------|--------------|---------------------|
//! | `/cache`     | `10001:1000` | `drwxrwsr-x` | write OK            |
//! | `/mirror`    | `10001:1000` | `drwxrwsr-x` | write OK            |
//! | `/workspace` | `0:1000`     | `drwxrwsrwx` | write OK            |
//!
//! The child is *not* the owner of any of them — it writes through the group,
//! which is precisely what `fsGroup: ARTIFACT_GID` + the setgid bit exist to
//! provide, and what `child::prepare_child`'s `setgid(ARTIFACT_GID)` +
//! `umask 0002` are for. Note `prepare_child` also calls `setgroups(0, NULL)`,
//! so gid 1000 must be the child's PRIMARY group, not a supplementary one — it
//! is. `fsGroupChangePolicy: OnRootMismatch` is a no-op here because both PVC
//! roots already carry gid 1000, so no recursive re-own is triggered.
//!
//! # The eighth blocker: the child's `TMPDIR` is not the Pod's `TMPDIR`
//!
//! Everything above derives the launcher's mount set from paths **the Pod
//! renders**. That is not the whole set, and the gap is not theoretical: it is
//! the eighth blocker in this chain.
//!
//! `djinn_sandbox`'s Linux backend sets `djinn_sandbox::SANDBOX_TMPDIR`
//! (`/var/tmp`) on **every** shell command, overriding the pod's
//! `TMPDIR=/workspace`, so that a sandboxed tool's scratch lands inside the
//! Landlock writable set. `TMPDIR` is on the broker's forward allow-list and
//! `process_broker::child_environment` overlays `Command::get_envs()` *over* the
//! inherited environment — so the value a brokered child is actually born with
//! is the sandbox's, injected at spawn time, appearing nowhere in the manifest.
//!
//! `/var/tmp` lives in the image layer, which `readOnlyRootFilesystem: true`
//! takes away. Measured on the production node, in a launcher-shaped container
//! with the real child credentials:
//!
//! ```text
//! # launcher (readOnlyRootFilesystem, setpriv --reuid=1001 --regid=1000)
//! TMPDIR=/var/tmp mktemp -d
//!   mktemp: failed to create directory via template '/var/tmp/tmp.XXXXXXXXXX':
//!           Read-only file system                                      (rc 1)
//! findmnt /var/tmp -> (nothing: it is the image layer)
//!
//! # worker container, same pod, same image
//! TMPDIR=/var/tmp mktemp -d -> /var/tmp/tmp.vtramIerJV                 (rc 0)
//! ```
//!
//! So arming the launcher without [`LAUNCHER_VAR_TMP_DIR`] is a strict
//! regression against the unbrokered path for cargo, `cc`, Go and git — every
//! toolchain that honours `$TMPDIR`.
//!
//! It is **launcher-private**, like `/tmp`, and deliberately not shared with the
//! worker. A surface both containers can write is how a child that holds no
//! privilege reaches into the worker's uid, and nothing hands work off through
//! `/var/tmp`: the worker's own `TempDir` follows the pod's `TMPDIR=/workspace`.
//! The one-way worker→child handoff that genuinely exists gets its own channel;
//! see [`crate::private_dep_config`].

use k8s_openapi::api::core::v1::{EmptyDirVolumeSource, Volume, VolumeMount};

use crate::job::{
    CACHE_MOUNT_DIR, MIRROR_MOUNT_DIR, VOLUME_CACHE, VOLUME_MIRROR, VOLUME_WORKSPACE,
    WORKSPACE_MOUNT_DIR,
};
use crate::launcher::worker_launcher_ipc_mount;

/// Volume supplying the launcher container a writable `/tmp`.
pub const VOLUME_LAUNCHER_TMP: &str = "launcher-tmp";
/// Volume supplying the launcher container a writable `$HOME`.
pub const VOLUME_LAUNCHER_HOME: &str = "launcher-home";
/// Volume supplying the launcher container a writable [`LAUNCHER_VAR_TMP_DIR`].
pub const VOLUME_LAUNCHER_VAR_TMP: &str = "launcher-var-tmp";

/// Scratch directory a brokered child gets even when it ignores `TMPDIR`.
pub const LAUNCHER_TMP_DIR: &str = "/tmp";

/// The directory the sandbox pins `TMPDIR` to on every shell command.
///
/// Duplicated as a literal rather than imported because `djinn-k8s` does not
/// depend on `djinn-sandbox` outside dev — the same arrangement
/// [`LAUNCHER_HOME_DIR`] uses for the image's `$HOME`. It is not left to drift:
/// `spawn_time_injected_paths` in
/// `tests/launcher_child_filesystem_reachability.rs` derives the required value
/// by applying the REAL sandbox to a real `Command` and reading the environment
/// back off it, so a change on either side fails the guard rather than the pod.
pub const LAUNCHER_VAR_TMP_DIR: &str = "/var/tmp";

/// `$HOME` inside every djinn devcontainer image, set by
/// `djinn_image_builder::dockerfile` (`ENV HOME=/home/djinn`). Duplicated as a
/// literal rather than imported because `djinn-k8s` does not depend on the
/// image builder; the guard asserts the rendered value round-trips.
pub const LAUNCHER_HOME_DIR: &str = "/home/djinn";

/// Every volumeMount on the cgroup-launcher sidecar.
///
/// Two groups, and the distinction is the whole point of this module:
///
/// * the launcher's OWN surfaces — the IPC volume carrying the broker socket
///   and the worker-private credential; the RuntimeClass supplies the cgroup root.
/// * the CHILD's surfaces — [`launcher_data_mounts`], which exist purely
///   because a brokered command executes in this container's mount namespace
///   and not the worker's.
pub fn launcher_volume_mounts(mirror_read_only: bool, cache_read_only: bool) -> Vec<VolumeMount> {
    let mut mounts = vec![worker_launcher_ipc_mount()];
    mounts.extend(launcher_data_mounts(mirror_read_only, cache_read_only));
    mounts
}

/// The worker's data mounts, re-rendered for the launcher container.
///
/// `read_only` mirrors the worker's own flags for the same run so the two
/// containers cannot disagree about whether a volume is writable — an
/// evidence-spike run that denies the worker write access to `/mirror` and
/// `/cache` must deny its brokered children the same, or the isolation
/// contract holds only for commands that happen not to be brokered.
pub fn launcher_data_mounts(mirror_read_only: bool, cache_read_only: bool) -> Vec<VolumeMount> {
    vec![
        data_mount(VOLUME_MIRROR, MIRROR_MOUNT_DIR, Some(mirror_read_only)),
        data_mount(
            VOLUME_CACHE,
            CACHE_MOUNT_DIR,
            cache_read_only.then_some(true),
        ),
        data_mount(VOLUME_WORKSPACE, WORKSPACE_MOUNT_DIR, None),
        data_mount(VOLUME_LAUNCHER_TMP, LAUNCHER_TMP_DIR, None),
        data_mount(VOLUME_LAUNCHER_HOME, LAUNCHER_HOME_DIR, None),
        data_mount(VOLUME_LAUNCHER_VAR_TMP, LAUNCHER_VAR_TMP_DIR, None),
        // Read-only on purpose: this is the worker→child direction of the
        // private-dependency handoff and the child must not be able to rewrite
        // it. See [`crate::private_dep_config`].
        crate::private_dep_config::launcher_child_git_mount(),
    ]
}

/// The launcher-private scratch volumes. The shared data volumes are declared
/// by `job.rs` for the worker already; only these are new to the pod.
pub fn launcher_scratch_volumes() -> Vec<Volume> {
    [
        VOLUME_LAUNCHER_TMP,
        VOLUME_LAUNCHER_HOME,
        VOLUME_LAUNCHER_VAR_TMP,
    ]
    .into_iter()
    .map(|name| Volume {
        name: name.to_string(),
        // Disk-backed on purpose: a Memory emptyDir is charged to the pod's
        // memory limit, and the launcher container's limit is the *build's*
        // memory ceiling. A cargo build's scratch would eat it.
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    })
    .collect()
}

fn data_mount(name: &str, mount_path: &str, read_only: Option<bool>) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: mount_path.to_string(),
        read_only,
        ..VolumeMount::default()
    }
}

/// Is `path` inside `root` (or equal to it)?
///
/// Component-wise so `/cachexyz` is not treated as living under `/cache`.
pub fn is_under(path: &str, root: &str) -> bool {
    path == root
        || (path.starts_with(root)
            && (root.ends_with('/') || path.as_bytes().get(root.len()) == Some(&b'/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_is_component_wise_not_textual() {
        assert!(is_under("/cache", "/cache"));
        assert!(is_under("/cache/cargo", "/cache"));
        assert!(!is_under("/cachexyz", "/cache"));
        assert!(!is_under("/cach", "/cache"));
        assert!(is_under("/var/run/djinn/spec.bin", "/var/run/djinn"));
        assert!(!is_under("/var/run/djinn-other/x", "/var/run/djinn"));
    }

    /// The evidence-spike isolation contract may not be weaker for a brokered
    /// command than for an unbrokered one.
    #[test]
    fn launcher_data_mounts_track_the_workers_read_only_flags() {
        let normal = launcher_data_mounts(false, false);
        let spike = launcher_data_mounts(true, true);
        let flag = |mounts: &[VolumeMount], name: &str| {
            mounts
                .iter()
                .find(|mount| mount.name == name)
                .unwrap_or_else(|| panic!("{name} rendered"))
                .read_only
        };
        assert_eq!(flag(&normal, VOLUME_MIRROR), Some(false));
        assert_eq!(flag(&normal, VOLUME_CACHE), None);
        assert_eq!(flag(&spike, VOLUME_MIRROR), Some(true));
        assert_eq!(flag(&spike, VOLUME_CACHE), Some(true));
        // The scratch surfaces are never read-only: they exist because
        // `readOnlyRootFilesystem` already took the image's copies away.
        assert_eq!(flag(&spike, VOLUME_LAUNCHER_TMP), None);
        assert_eq!(flag(&spike, VOLUME_LAUNCHER_HOME), None);
        // Including the one an evidence spike would most plausibly be argued
        // into: a build's TMPDIR is not a durable resource, and a read-only
        // /var/tmp is the EROFS that blocker 8 was.
        assert_eq!(flag(&spike, VOLUME_LAUNCHER_VAR_TMP), None);
        // The private-dependency channel is the one mount that IS read-only for
        // the launcher, in both regimes — that is its security property, not a
        // spike concession.
        assert_eq!(
            flag(&normal, crate::private_dep_config::VOLUME_CHILD_GIT_CONFIG),
            Some(true)
        );
        assert_eq!(
            flag(&spike, crate::private_dep_config::VOLUME_CHILD_GIT_CONFIG),
            Some(true)
        );
        // Workspace is an ephemeral per-Pod emptyDir; it is writable even for
        // an evidence-spike run, exactly as it is for the worker.
        assert_eq!(flag(&spike, VOLUME_WORKSPACE), None);
    }

    /// The render must mount exactly what the sandbox pins, or blocker 8 is back
    /// under a different path. `djinn-sandbox` is a dev-dependency, so this is
    /// where the duplicated literal is reconciled with its source.
    #[test]
    fn the_var_tmp_mount_is_exactly_the_path_the_sandbox_pins() {
        assert_eq!(
            LAUNCHER_VAR_TMP_DIR,
            djinn_sandbox::SANDBOX_TMPDIR,
            "the launcher mounts one path and every sandboxed command writes to another"
        );
    }

    #[test]
    fn the_scratch_volumes_are_disk_backed_not_charged_to_the_memory_limit() {
        for volume in launcher_scratch_volumes() {
            let source = volume.empty_dir.expect("scratch volume is an emptyDir");
            assert_eq!(
                source.medium, None,
                "{} must not be Memory-backed: the launcher's memory limit is the build's ceiling",
                volume.name
            );
        }
    }
}
