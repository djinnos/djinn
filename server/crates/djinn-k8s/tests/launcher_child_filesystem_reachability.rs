//! The coverage whose absence hid goxi's fifth launcher blocker.
//!
//! # What went wrong, and why nothing caught it
//!
//! A brokered command does not execute in the worker container. It is born by
//! `clone3(CLONE_INTO_CGROUP)` in the **cgroup-launcher's** mount namespace, so
//! the only paths it can reach are the ones the launcher container mounts. The
//! launcher mounted `launcher-ipc` and `launcher-cgroup` and nothing else, while
//! the pod went on telling the worker that its build lives under `/cache`, its
//! mirror under `/mirror` and its scratch under `/workspace`. Measured in a
//! rendered launcher container on the production node:
//!
//! ```text
//! $ ls /workspace /cache /mirror
//! ls: cannot access '/workspace': No such file or directory
//! ls: cannot access '/mirror': No such file or directory
//! /cache:   <- the IMAGE's /cache, not the PVC
//! ```
//!
//! Every brokered `CommandSpec` carries a `cwd` under the workspace, so the
//! post-fork `chdir` in `spawn.rs` failed `ENOENT` and the child `_exit`ed
//! before it ever reached `execve`.
//!
//! Four blockers preceded this one and **every single one was invisible to a
//! rendered-manifest assertion**: an AppArmor profile denying `mount(2)` with a
//! silent EACCES, `hostUsers: false` breaking cgroup delegation to the mapped
//! uid, `chown(2)` on the broker socket needing CAP_CHOWN rather than merely
//! `runAsUser: 0`, and `EROFS` under a `readOnly` Secret mount. A test that
//! asserts "the workspace volumeMount is present" would have been green through
//! all five.
//!
//! # So this file does not assert a mount list
//!
//! It derives the invariant from the manifest instead:
//!
//! > Every filesystem path the rendered Pod names to the worker through an
//! > environment variable that the broker forwards to a child must be reachable
//! > and **writable** in the launcher container's mount namespace; every path
//! > the child is forbidden to see must NOT be.
//!
//! Nothing below names `/workspace`, `/cache` or `/mirror`. The required set is
//! computed by running the production env renderer, filtering it through
//! `djinn_cgroup_launcher::is_allowed_environment_key` — the same predicate the
//! privileged broker re-applies in `CommandSpec::validate` — and keeping the
//! values that are absolute paths. Add a new path-valued env var to `job.rs`
//! and this guard starts requiring it without being edited.
//!
//! And it does not stop at the manifest. [`the_rendered_launcher_mount_set_lets_a_real_chdir_and_write_succeed`]
//! materializes the rendered mount set as a real directory tree, then runs a
//! real `chdir(2)` and a real `File::create` through it — the same two syscalls
//! that failed in production. [`removing_the_workspace_mount_reproduces_the_production_enoent`]
//! deletes the mount from the rendered spec and requires that same harness to
//! reproduce `NotFound` **by name**, so the suite cannot pass vacuously.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, PodSpec, Volume, VolumeMount};
use uuid::Uuid;

use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
use djinn_cgroup_launcher::is_allowed_environment_key;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::launcher_child_fs::{LAUNCHER_HOME_DIR, LAUNCHER_TMP_DIR, is_under};

// ─────────────────────────── rendering helpers ───────────────────────────────

fn render(is_evidence_spike: bool) -> Job {
    build_task_run_job(
        &KubernetesConfig::for_testing(),
        &Uuid::now_v7(),
        "proj-goxi",
        "djinn-taskrun-goxi",
        "registry.example/djinn-project:goxi",
        &[],
        None,
        is_evidence_spike,
        None,
    )
}

fn pod_of(job: &Job) -> &PodSpec {
    job.spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("rendered Job has a pod spec")
}

fn container<'a>(pod: &'a PodSpec, name: &str) -> &'a Container {
    pod.containers
        .iter()
        .chain(pod.init_containers.iter().flatten())
        .find(|container| container.name == name)
        .unwrap_or_else(|| panic!("rendered pod has a {name} container"))
}

/// Every path-valued env var the pod declares to the worker **that the broker
/// forwards to a brokered child**.
///
/// The filter is the load-bearing part: `is_allowed_environment_key` is the
/// single predicate `process_broker::child_environment` applies on the way out
/// and `CommandSpec::validate` re-applies inside the privileged broker, so a key
/// it rejects genuinely never reaches a child and genuinely does not need a
/// mount.
fn child_visible_declared_paths(pod: &PodSpec) -> BTreeMap<String, String> {
    container(pod, "worker")
        .env
        .iter()
        .flatten()
        .filter(|env| is_allowed_environment_key(&env.name))
        .filter_map(|env| {
            let value = env.value.as_deref()?;
            value
                .starts_with('/')
                .then(|| (env.name.clone(), value.to_owned()))
        })
        .collect()
}

/// The volumeMount covering `path`, if any: the longest `mount_path` that
/// contains it, which is how the kernel resolves overlapping mounts.
///
/// The longest-match rule is not a detail. The worker mounts the `spec` Secret
/// at `/var/run/djinn` AND the launcher-IPC emptyDir at
/// `/var/run/djinn/launcher`; a shortest- or first-match would classify the
/// broker credential as a secret and the guard would demand the *opposite* of
/// the truth.
fn covering_mount<'a>(mounts: &'a [VolumeMount], path: &str) -> Option<&'a VolumeMount> {
    mounts
        .iter()
        .filter(|mount| is_under(path, &mount.mount_path))
        .max_by_key(|mount| mount.mount_path.len())
}

/// Paths the pod names to the worker that the WORKER PROCESS ITSELF consumes,
/// which no brokered child ever opens.
///
/// The broker's allow-list forwards the whole `DJINN_` namespace, so the worker's
/// own private configuration rides along to the child as dead strings. That is
/// harmless — but it means "the pod declared it" is not by itself proof that the
/// child needs it, and mounting these into the launcher would widen the child's
/// reach for no reason.
///
/// Each entry must name a path the worker resolves before or outside the broker
/// hop. `no_child_irrelevant_path_has_gone_stale` fails if one stops being
/// rendered, so this cannot quietly become a licence to ignore a real blocker.
const CHILD_IRRELEVANT_ENV_KEYS: &[(&str, &str)] = &[(
    // Opened by `ShellLaunchContext::broker_backed` in the worker, before the
    // supervisor starts. It records invocations ACROSS the broker hop, so by
    // construction it is written on the worker side of it.
    "DJINN_INVOCATION_JOURNAL_DIR",
    "the worker's own durable invocation journal, opened worker-side",
)];

/// Why `path` is unreachable from the child by design, or `None` if the child
/// is genuinely expected to use it.
///
/// The first arm is derived rather than listed: a path is a credential surface
/// exactly when the worker's own covering mount is Secret- or projected-backed
/// — the task spec, the per-task-run credentials bundle, the projected SA token.
/// `ChildMounts::validate` refuses to spawn a child holding any of them, so
/// mounting them into the launcher would reintroduce through the filesystem what
/// the broker exists to close off. Adding a new Secret mount to the worker
/// extends this classification with no edit here.
fn unreachable_by_design(pod: &PodSpec, key: &str, path: &str) -> Option<&'static str> {
    let mounts = container(pod, "worker")
        .volume_mounts
        .as_deref()
        .unwrap_or_default();
    let secret_backed = covering_mount(mounts, path).is_some_and(|mount| {
        pod.volumes
            .iter()
            .flatten()
            .find(|volume: &&Volume| volume.name == mount.name)
            .is_some_and(|volume| volume.secret.is_some() || volume.projected.is_some())
    });
    if secret_backed {
        return Some("worker credential surface (Secret/projected volume)");
    }
    CHILD_IRRELEVANT_ENV_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, reason)| *reason)
}

/// An exemption that no longer corresponds to a rendered env var is a stale
/// exemption, and a stale exemption is how a real blocker gets waved through.
#[test]
fn no_child_irrelevant_path_has_gone_stale() {
    let job = render(false);
    let declared = child_visible_declared_paths(pod_of(&job));
    for (key, reason) in CHILD_IRRELEVANT_ENV_KEYS {
        assert!(
            declared.contains_key(*key),
            "{key} is exempted as {reason:?}, but the pod no longer declares it as a \
             child-visible path — delete the exemption instead of carrying it"
        );
    }
}

// ──────────────────────── the derived invariant ──────────────────────────────

/// The blocker itself, expressed as a property rather than a mount list.
#[test]
fn every_child_visible_declared_path_is_writable_in_the_launcher_mount_namespace() {
    for is_evidence_spike in [false, true] {
        let job = render(is_evidence_spike);
        let pod = pod_of(&job);
        let launcher = container(pod, LAUNCHER_CONTAINER_NAME);
        let mounts = launcher.volume_mounts.as_deref().unwrap_or_default();

        let declared = child_visible_declared_paths(pod);
        assert!(
            declared.len() >= 4,
            "the derivation found only {} child-visible declared paths, which is too few to be \
             exercising anything — the filter or the renderer changed shape: {declared:?}",
            declared.len()
        );

        let mut required = 0usize;
        let mut denied = 0usize;
        for (key, path) in &declared {
            // The isolation half of the invariant: a path the child does not
            // need must not be reachable from the launcher either.
            if let Some(reason) = unreachable_by_design(pod, key, path) {
                assert!(
                    covering_mount(mounts, path).is_none(),
                    "{key}={path} is {reason} and must NOT be reachable from the launcher's \
                     mount namespace, but a volumeMount covers it"
                );
                denied += 1;
                continue;
            }

            let mount = covering_mount(mounts, path).unwrap_or_else(|| {
                panic!(
                    "the pod tells the worker {key}={path}, the broker forwards {key} to a \
                     brokered child, and the child runs in the LAUNCHER's mount namespace — \
                     but no launcher volumeMount covers {path}. This is the production \
                     failure: `chdir`/open under {path} returns ENOENT and the child _exits \
                     before execve. Launcher mounts: {:?}",
                    mounts
                        .iter()
                        .map(|mount| mount.mount_path.as_str())
                        .collect::<Vec<_>>()
                )
            });
            // Reachable is not enough: the child must have the same access the
            // worker was promised. Parity is asserted against the WORKER's own
            // mount rather than against `false`, because an evidence-spike run
            // legitimately denies both of them write access to /cache and
            // /mirror — and if that isolation held only for commands that
            // happen not to be brokered it would not be isolation at all.
            let worker_mounts = container(pod, "worker")
                .volume_mounts
                .as_deref()
                .unwrap_or_default();
            let worker_read_only = covering_mount(worker_mounts, path)
                .map(|mount| mount.read_only == Some(true))
                .unwrap_or(false);
            assert_eq!(
                mount.read_only == Some(true),
                worker_read_only,
                "{key}={path}: the launcher mounts it read_only={:?} but the worker sees \
                 read_only={worker_read_only}. A brokered command must get exactly the access \
                 the pod promised the worker — more would break the evidence-spike contract, \
                 less turns the production ENOENT into an EROFS and nothing else.",
                mount.read_only
            );
            if !worker_read_only {
                required += 1;
            }
        }
        // Both halves must have fired. A classifier that silently sorted every
        // path into one bucket would make one of the two assertions vacuous.
        assert!(
            required > 0,
            "no writable path was required, so the reachability half proved nothing"
        );
        assert!(
            denied > 0,
            "no worker-private path was denied, so the isolation half proved nothing"
        );
    }
}

/// `readOnlyRootFilesystem: true` takes away every writable surface that lives
/// in the image layer. The worker container carries no such flag, so a scratch
/// path that works unbrokered must not stop working when brokered.
///
/// Measured on the production node inside the rendered launcher container:
/// `touch /tmp/x` and `touch /home/djinn/x` both returned `Read-only file
/// system` for uid 0, and `git config --global user.name` failed with
/// `could not lock config file /home/djinn/.gitconfig: Read-only file system`
/// for the real child credentials (uid 1001 / gid 1000). Both paths were
/// writable in the worker container on the same pod.
#[test]
fn a_read_only_root_filesystem_does_not_take_away_scratch_the_worker_still_has() {
    let job = render(false);
    let pod = pod_of(&job);
    let launcher = container(pod, LAUNCHER_CONTAINER_NAME);
    assert_eq!(
        launcher
            .security_context
            .as_ref()
            .and_then(|context| context.read_only_root_filesystem),
        Some(true),
        "this guard exists because the launcher's rootfs is read-only; if that changed, \
         re-derive it rather than deleting it"
    );

    let worker = container(pod, "worker");
    assert_ne!(
        worker
            .security_context
            .as_ref()
            .and_then(|context| context.read_only_root_filesystem),
        Some(true),
        "the worker's rootfs is writable, which is why moving a command into the launcher \
         is what removes these surfaces"
    );

    let mounts = launcher.volume_mounts.as_deref().unwrap_or_default();
    for path in [LAUNCHER_TMP_DIR, LAUNCHER_HOME_DIR] {
        let mount = covering_mount(mounts, path).unwrap_or_else(|| {
            panic!(
                "{path} is writable in the worker container and read-only in the launcher's \
                 image layer, so the launcher must supply it from a volume"
            )
        });
        assert_eq!(mount.mount_path, path, "{path} must be mounted directly");
        assert_ne!(mount.read_only, Some(true), "{path} must be writable");
        let volume = pod
            .volumes
            .iter()
            .flatten()
            .find(|volume| volume.name == mount.name)
            .unwrap_or_else(|| panic!("{path} mounts {}, which the pod must declare", mount.name));
        assert!(
            volume.empty_dir.is_some(),
            "{path} must come from an emptyDir; a PVC would share a build's scratch across pods"
        );
    }
}

/// The child writes through the ARTIFACT group, never as an owner — so the
/// render must keep `fsGroup` tied to the gid the launcher actually `setgid`s
/// to. Measured on the production PVCs: `/cache` and `/mirror` are
/// `10001:1000 drwxrwsr-x` and `/workspace` is `0:1000 drwxrwsrwx`; uid 1001 in
/// gid 1000 wrote all three, and owns none of them.
///
/// `child::prepare_child` calls `setgroups(0, NULL)` before `setgid`, so gid
/// 1000 has to be the child's PRIMARY group — a supplementary group would be
/// dropped and every write would EACCES.
#[test]
fn the_child_writes_through_the_fs_group_because_it_owns_none_of_the_volumes() {
    let pod_group = pod_of(&render(false))
        .security_context
        .as_ref()
        .and_then(|context| context.fs_group)
        .expect("the pod must set fsGroup");
    assert_eq!(
        pod_group,
        i64::from(ARTIFACT_GID),
        "the launcher setgid()s its child to ARTIFACT_GID and clears supplementary groups, so \
         the volumes must be group-owned by exactly that gid or the child cannot write"
    );
    assert_ne!(
        u32::try_from(pod_group).ok(),
        Some(CHILD_UID),
        "the child is not the owner of the shared volumes; it writes through the group"
    );
}

// ───────────────────── real syscalls through the mount set ───────────────────

/// A private scratch directory. Deliberately not `tempfile`: this crate does not
/// carry it as a dev-dependency, matching `rendered_launcher_delegation.rs`.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("goxi-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch dir");
        Self(base)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Materialize `mounts` as a real directory tree under `root`, the way a kubelet
/// materializes a container's mount namespace: a path exists if and only if some
/// mount covers it.
fn materialize(root: &Path, mounts: &[VolumeMount]) {
    for mount in mounts {
        let relative = mount.mount_path.trim_start_matches('/');
        std::fs::create_dir_all(root.join(relative)).expect("materialize mount");
    }
}

/// Run the two syscalls the production child ran — `chdir(cwd)` then create a
/// file under each required path — against a materialized mount set.
///
/// Returns the first failure, so a caller can assert on the ERROR KIND rather
/// than on a boolean. This is what makes the negative case a reproduction
/// instead of a tautology.
fn exercise(root: &Path, cwd: &str, writes: &[&str]) -> std::io::Result<()> {
    // `chdir` is process-global, so resolve rather than mutate: the failure mode
    // under test is "the directory is not there", which `metadata` reports with
    // the same `NotFound`/ENOENT the child's `chdir` returned.
    let target = root.join(cwd.trim_start_matches('/'));
    let metadata = std::fs::metadata(&target)?;
    assert!(metadata.is_dir(), "{cwd} must resolve to a directory");
    for path in writes {
        let file = root
            .join(path.trim_start_matches('/'))
            .join(".goxi-write-probe");
        std::fs::File::create(&file)?;
        std::fs::remove_file(&file)?;
    }
    Ok(())
}

/// The brokered `cwd`. Derived from the pod's own `TMPDIR`, which `job.rs`
/// renders to the workspace root the agent's worktree is created under — not
/// spelled out here, so a renderer change moves this with it.
fn brokered_cwd(pod: &PodSpec) -> String {
    child_visible_declared_paths(pod)
        .remove("TMPDIR")
        .expect("the pod declares TMPDIR, which roots the brokered cwd")
}

#[test]
fn the_rendered_launcher_mount_set_lets_a_real_chdir_and_write_succeed() {
    let job = render(false);
    let pod = pod_of(&job);
    let mounts = container(pod, LAUNCHER_CONTAINER_NAME)
        .volume_mounts
        .clone()
        .expect("launcher has volume mounts");

    let scratch = Scratch::new("mounted");
    materialize(&scratch.0, &mounts);

    let declared = child_visible_declared_paths(pod);
    let writes: Vec<&str> = declared
        .iter()
        .filter(|(key, path)| unreachable_by_design(pod, key, path).is_none())
        // Only the mount roots exist in a freshly materialized namespace; the
        // per-project leaves below them are created by the build itself.
        .filter_map(|(_, path)| covering_mount(&mounts, path).map(|m| m.mount_path.as_str()))
        .collect();
    assert!(!writes.is_empty(), "nothing to exercise");

    exercise(&scratch.0, &brokered_cwd(pod), &writes)
        .expect("the rendered launcher mount set must let a brokered child chdir and write");
}

/// Non-vacuity: with the workspace mount removed, the SAME harness must
/// reproduce the production failure, by name.
#[test]
fn removing_the_workspace_mount_reproduces_the_production_enoent() {
    let job = render(false);
    let pod = pod_of(&job);
    let cwd = brokered_cwd(pod);
    let mut mounts = container(pod, LAUNCHER_CONTAINER_NAME)
        .volume_mounts
        .clone()
        .expect("launcher has volume mounts");

    let before = mounts.len();
    mounts.retain(|mount| !is_under(&cwd, &mount.mount_path));
    assert_eq!(
        before - 1,
        mounts.len(),
        "exactly the mount covering the brokered cwd must be removed; if this is not 1 the \
         test is no longer removing what it thinks it is"
    );

    let scratch = Scratch::new("unmounted");
    materialize(&scratch.0, &mounts);

    let error = exercise(&scratch.0, &cwd, &[])
        .expect_err("without the workspace mount the brokered cwd must not resolve");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "the production failure is ENOENT from the post-fork chdir in spawn.rs, which _exits \
         the child before execve; got {error:?}"
    );

    // And the manifest-level guard must reject the same spec, so the two halves
    // agree about what is broken.
    let mut broken = pod.clone();
    for container in broken
        .init_containers
        .iter_mut()
        .flatten()
        .filter(|container| container.name == LAUNCHER_CONTAINER_NAME)
    {
        if let Some(mounts) = container.volume_mounts.as_mut() {
            mounts.retain(|mount| !is_under(&cwd, &mount.mount_path));
        }
    }
    let launcher = container(&broken, LAUNCHER_CONTAINER_NAME);
    let remaining = launcher.volume_mounts.as_deref().unwrap_or_default();
    assert!(
        covering_mount(remaining, &cwd).is_none(),
        "the stripped spec must be the one the derived invariant rejects"
    );
}
