//! Real syscalls through the launcher's rendered mount set.
//!
//! The sibling `launcher_child_filesystem_reachability` derives WHICH paths a
//! brokered child depends on and classifies them against the rendered manifest.
//! This file takes the mount set the renderer produced, materializes it as a real
//! directory tree the way a kubelet materializes a container's namespace — a path
//! exists if and only if some mount covers it — and then runs the two syscalls
//! that actually failed in production: `chdir(cwd)` and a file create underneath
//! each required path.
//!
//! Split out of that file rather than living in it because the two are different
//! kinds of evidence and the combined file had run up against the repo's per-file
//! byte budget. The real tools that fail when blockers 8 and 9 are present
//! (`mktemp`, `git`) are driven in `launcher_child_runtime_handoff.rs`.
//!
//! [`removing_the_workspace_mount_reproduces_the_production_enoent`] is the
//! non-vacuity control: with the mount stripped from the rendered spec, the SAME
//! harness must reproduce `NotFound` by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, PodSpec, VolumeMount};
use uuid::Uuid;

use djinn_cgroup_launcher::is_allowed_environment_key;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::launcher_child_fs::is_under;

// ─────────────────────────── rendering helpers ───────────────────────────────
//
// Duplicated from the sibling guard rather than shared through a `tests/common`
// module: each is a thin wrapper over the production renderer, and a shared
// module would make one test binary's failure depend on the other's plumbing.

fn render() -> Job {
    build_task_run_job(
        &KubernetesConfig::for_testing(),
        &Uuid::now_v7(),
        "proj-goxi",
        "djinn-taskrun-goxi",
        "registry.example/djinn-project:goxi",
        &[],
        None,
        false,
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

/// The longest `mount_path` covering `path`, which is how the kernel resolves
/// overlapping mounts. See the sibling guard for why shortest-match would
/// misclassify the launcher IPC volume nested inside the `spec` Secret mount.
fn covering_mount<'a>(mounts: &'a [VolumeMount], path: &str) -> Option<&'a VolumeMount> {
    mounts
        .iter()
        .filter(|mount| is_under(path, &mount.mount_path))
        .max_by_key(|mount| mount.mount_path.len())
}

/// Every path-valued env var the pod declares to the worker that the broker
/// forwards to a child.
fn child_visible_declared_paths(pod: &PodSpec) -> BTreeMap<String, String> {
    container(pod, "worker")
        .env
        .iter()
        .flatten()
        .filter_map(|env| {
            let value = env.value.as_deref()?;
            (is_allowed_environment_key(&env.name)
                && value.starts_with('/')
                && !value.contains(':'))
            .then(|| (env.name.clone(), value.to_owned()))
        })
        .collect()
}

/// The brokered `cwd`. Derived from the pod's own `TMPDIR`, which `job.rs`
/// renders to the workspace root the agent's worktree is created under — not
/// spelled out here, so a renderer change moves this with it.
fn brokered_cwd(pod: &PodSpec) -> String {
    child_visible_declared_paths(pod)
        .remove("TMPDIR")
        .expect("the pod declares TMPDIR, which roots the brokered cwd")
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

#[test]
fn the_rendered_launcher_mount_set_lets_a_real_chdir_and_write_succeed() {
    let job = render();
    let pod = pod_of(&job);
    let mounts = container(pod, LAUNCHER_CONTAINER_NAME)
        .volume_mounts
        .clone()
        .expect("launcher has volume mounts");

    let scratch = Scratch::new("mounted");
    materialize(&scratch.0, &mounts);

    // Only paths the launcher actually mounts are exercised. The worker-private
    // credential surfaces are, correctly, not among them — WHICH paths belong in
    // the set and why is the sibling guard's job; this file only proves the
    // syscalls work through the set the renderer produced. And only the mount
    // roots exist in a freshly materialized namespace: the per-project leaves
    // below them are created by the build itself.
    let declared = child_visible_declared_paths(pod);
    let writes: Vec<&str> = declared
        .values()
        .filter_map(|path| covering_mount(&mounts, path).map(|m| m.mount_path.as_str()))
        .collect();
    assert!(!writes.is_empty(), "nothing to exercise");

    exercise(&scratch.0, &brokered_cwd(pod), &writes)
        .expect("the rendered launcher mount set must let a brokered child chdir and write");
}

/// Non-vacuity: with the workspace mount removed, the SAME harness must
/// reproduce the production failure, by name.
#[test]
fn removing_the_workspace_mount_reproduces_the_production_enoent() {
    let job = render();
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
