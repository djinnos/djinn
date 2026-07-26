//! Behavioural proof for goxi's eighth and ninth launcher blockers.
//!
//! Seven blockers in this chain were invisible to manifest assertions, so the
//! sibling file [`launcher_child_filesystem_reachability`] derives the *shape* of
//! the render and this one drives the *real call paths* through it:
//!
//! * **blocker 9** — the private-dependency installation token. The token is
//!   written by the real writer (`djinn_git::exported_config::publish`, which is
//!   what `configure_private_dep_access` calls), exposed by the real launcher
//!   anchor writer (`djinn_cgroup_launcher::git_trust`), and read back by **real
//!   git**. The control reproduces the production failure by name: the same read
//!   against the pre-fix arrangement — the rewrite living only in the worker
//!   container's `$HOME` — must come back empty.
//! * **blocker 8** — the sandbox's `TMPDIR`. The value comes from the real
//!   sandbox applied to a real `Command`, and a real `mktemp -d` runs against it.
//!   The control models what `readOnlyRootFilesystem: true` leaves behind and
//!   requires `mktemp` to fail.
//!
//! # Guarding against a vacuous control
//!
//! `#2617`'s first control passed for a reason that had nothing to do with the
//! code under test: GitHub Actions runners ship `/etc/gitconfig` containing
//! `[safe] directory = *`, so "with the fix reverted, git must fail" succeeded
//! anyway. Every git invocation below therefore runs in a controlled
//! configuration: `GIT_CONFIG_SYSTEM` is the anchor this test wrote (and the
//! anchor chains to a controlled EMPTY file, never the host's `/etc/gitconfig`),
//! `GIT_CONFIG_GLOBAL` is pinned to the `$HOME` under test, and `HOME` is a
//! directory this test owns. And every positive assertion is paired with the same
//! command in the broken arrangement, so a read that "worked" for an ambient
//! reason would fail the pair.

use std::path::{Path, PathBuf};
use std::process::Command;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, PodSpec};
use uuid::Uuid;

use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::private_dep_config::{
    CHILD_GIT_CONFIG_DIR, CHILD_GIT_CONFIG_FILE, CHILD_GIT_CONFIG_PATH_ENV,
};

// ───────────────────────────── scratch plumbing ──────────────────────────────

/// A private scratch directory. Deliberately not `tempfile`: this crate does not
/// carry it as a dev-dependency, matching the sibling guard.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("goxi-handoff-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch dir");
        Self(base)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).expect("create scratch subdir");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn env_of<'a>(pod: &'a PodSpec, container_name: &str, key: &str) -> Option<&'a str> {
    container(pod, container_name)
        .env
        .iter()
        .flatten()
        .find(|env| env.name == key)
        .and_then(|env| env.value.as_deref())
}

// ─────────────────── the constants that must not drift apart ─────────────────

/// Three crates name this channel and none of them can import the others'
/// definition in a non-dev build: the renderer (`djinn-k8s`), the launcher
/// (`djinn-cgroup-launcher`, whose anchor includes the file), and the worker
/// (which imports the launcher's copy, so it is not a third source). A mismatch
/// would make the whole handoff a silent no-op — the exact failure mode of
/// blocker 9.
#[test]
fn the_renderer_and_the_launcher_agree_on_the_channel_env_var() {
    assert_eq!(
        CHILD_GIT_CONFIG_PATH_ENV,
        djinn_cgroup_launcher::git_trust::CHILD_GIT_CONFIG_PATH_ENV,
        "the renderer names the channel with one variable and the launcher reads another; the \
         anchor would include nothing and every brokered private-dependency fetch would go out \
         unauthenticated, silently"
    );
}

/// Both ends must be TOLD the path, or the worker writes somewhere the launcher
/// never looks.
#[test]
fn both_containers_are_told_the_channel_path() {
    let job = render();
    let pod = pod_of(&job);
    for container_name in ["worker", LAUNCHER_CONTAINER_NAME] {
        assert_eq!(
            env_of(pod, container_name, CHILD_GIT_CONFIG_PATH_ENV),
            Some(CHILD_GIT_CONFIG_FILE),
            "{container_name} must be told the channel path; the worker writes it and the \
             launcher includes it"
        );
    }
}

// ──────────────── blocker 9: the token really does cross the boundary ─────────

/// Run real git with a controlled configuration and return `(success, stdout)`.
///
/// `home` is the container's `$HOME` — the whole variable under test. Global
/// scope is pinned to `home/.gitconfig` explicitly rather than left to git's own
/// `$HOME` resolution: the value is identical, and pinning it means the host
/// user's real `~/.gitconfig` cannot leak into either the positive case or the
/// controls. The anchor already replaces the host's `/etc/gitconfig`.
fn git_get(anchor: &Path, home: &Path, key: &str) -> (bool, String) {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .env("GIT_CONFIG_SYSTEM", anchor)
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("HOME", home)
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_NOSYSTEM")
        .current_dir("/")
        .output()
        .expect("git must be available to this test");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

const REWRITE_KEY: &str = "url.https://x-access-token:goxi-token-canary@github.com/acme/.insteadOf";
const REWRITE_VALUE: &str = "https://github.com/acme/";

/// The whole of blocker 9, end to end, with real git on both sides.
///
/// 1. The worker publishes the rewrite with the production writer.
/// 2. The launcher materializes its anchor with the production writer, chaining
///    to the channel.
/// 3. A child reading through that anchor — with a `$HOME` that is NOT the
///    worker's, which is the entire point — resolves the rewrite.
///
/// And the control, which is the production failure by name: the same child, the
/// same anchor with the chain removed, resolves nothing, while a reader whose
/// `$HOME` *is* the worker's resolves it fine. That pair is what proves the
/// divergence — not the absence of the file — was the defect.
#[tokio::test]
async fn the_private_dep_rewrite_reaches_a_child_whose_home_is_not_the_workers() {
    let scratch = Scratch::new("crossing");
    let worker_home = scratch.dir("worker-home");
    let launcher_home = scratch.dir("launcher-home");
    let anchor_root = scratch.dir("anchor-root");
    // `#2617`: chain to a controlled EMPTY file rather than the host's
    // /etc/gitconfig, which on a GitHub runner trusts every repository and would
    // make the controls below succeed for the wrong reason.
    let system_config = scratch.0.join("etc-gitconfig");
    std::fs::write(&system_config, "").expect("controlled empty system config");

    // (1) The worker's two writes, both through real git. `--global` with
    //     HOME=worker_home is exactly what `git config --global` does in the
    //     worker container; `publish` is the channel write this change adds.
    let channel = worker_home.join("channel").join("gitconfig");
    Command::new("git")
        .args(["config", "--global", REWRITE_KEY, REWRITE_VALUE])
        .env("HOME", &worker_home)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir("/")
        .status()
        .expect("git must be available")
        .success()
        .then_some(())
        .expect("the worker's own --global write must succeed");
    djinn_git::exported_config::publish(&channel, REWRITE_KEY.into(), REWRITE_VALUE.into())
        .await
        .expect("the production channel writer must publish the rewrite");

    // (2) The launcher's anchor, written by the production writer, chaining to
    //     the channel.
    let anchor = djinn_cgroup_launcher::git_trust::materialize_in_with(
        &anchor_root,
        Some(&system_config),
        Some(&channel),
    )
    .expect("the launcher must materialize its anchor");

    // (3) The child. Its HOME is the launcher's volume, not the worker's.
    let (ok, value) = git_get(&anchor, &launcher_home, REWRITE_KEY);
    assert!(
        ok,
        "a brokered child must resolve the private-dependency rewrite; without it every fetch \
         of a private transitive dependency goes out unauthenticated and GitHub answers 404"
    );
    assert_eq!(value, REWRITE_VALUE);

    // ── Control A: the pre-fix arrangement. No chain to the channel, so the
    //    rewrite exists ONLY in the worker's $HOME — which is a different volume
    //    from the child's. This is the production failure.
    let unchained = djinn_cgroup_launcher::git_trust::materialize_in_with(
        &scratch.dir("anchor-unchained"),
        Some(&system_config),
        None,
    )
    .expect("materialize an anchor with no private-dep chain");
    let (ok, value) = git_get(&unchained, &launcher_home, REWRITE_KEY);
    assert!(
        !ok && value.is_empty(),
        "CONTROL FAILED TO FAIL: with no chain to the channel and the child's own $HOME, the \
         rewrite must not resolve — that is blocker 9. Got success={ok} value={value:?}. If \
         this passes, some ambient git configuration is supplying the answer and the positive \
         assertion above proves nothing."
    );

    // ── Control B: the same unchained anchor, read with the WORKER's $HOME,
    //    resolves it. So the file was written correctly all along and the
    //    container boundary is what broke it — not a bad write.
    let (ok, value) = git_get(&unchained, &worker_home, REWRITE_KEY);
    assert!(
        ok && value == REWRITE_VALUE,
        "the worker's own $HOME must still resolve the rewrite; if it does not, control A \
         above failed for an unrelated reason. Got success={ok} value={value:?}"
    );
}

/// The anchor may not grow the ability to carry arbitrary configuration just
/// because it now chains to a second file: `#2617`'s constraint was that the
/// launcher owns what it points at, and the channel is launcher-READ-ONLY at the
/// kubelet, not merely by convention. Assert the render, since that flag is the
/// entire control.
#[test]
fn the_channel_is_read_only_on_the_launcher_side_in_the_render() {
    let job = render();
    let pod = pod_of(&job);
    let mount_of = |container_name: &str| {
        container(pod, container_name)
            .volume_mounts
            .iter()
            .flatten()
            .find(|mount| mount.mount_path == CHILD_GIT_CONFIG_DIR)
            .cloned()
            .unwrap_or_else(|| panic!("{container_name} must mount {CHILD_GIT_CONFIG_DIR}"))
    };
    let worker = mount_of("worker");
    let launcher = mount_of(LAUNCHER_CONTAINER_NAME);
    assert_eq!(
        worker.name, launcher.name,
        "one volume, or it is not a channel"
    );
    assert_ne!(
        worker.read_only,
        Some(true),
        "the worker is the writer: a read-only mount makes the publish fail EROFS"
    );
    assert_eq!(
        launcher.read_only,
        Some(true),
        "the launcher's anchor includes this file at PROTECTED scope. A writable mount here \
         would hand repository-controlled code running as CHILD_UID the ability to put \
         core.sshCommand in it — the arbitrary-execution primitive #2617 closed for the \
         environment form of the same thing."
    );
    // And the pod must actually declare the volume, or both mounts are unbound.
    assert!(
        pod.volumes
            .iter()
            .flatten()
            .any(|volume| volume.name == worker.name && volume.empty_dir.is_some()),
        "the channel must be a pod-local emptyDir: a PVC would carry a live installation token \
         across pods and projects"
    );
}

// ─────────── blocker 8: the sandbox's TMPDIR, with a real mktemp ──────────────

/// The `TMPDIR` a brokered child is really born with, from the real sandbox.
fn sandbox_tmpdir() -> String {
    let mut command = Command::new("true");
    djinn_sandbox::SANDBOX
        .apply(
            djinn_sandbox::SandboxScope::Worktree(Path::new("/var/tmp")),
            &mut command,
        )
        .expect("the production sandbox must configure a command");
    command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("TMPDIR"))
        .and_then(|(_, value)| value)
        .expect("the sandbox pins TMPDIR on every shell command")
        .to_string_lossy()
        .into_owned()
}

fn mktemp_in(tmpdir: &Path) -> (bool, String) {
    let output = Command::new("sh")
        .args(["-c", "mktemp -d"])
        .env("TMPDIR", tmpdir)
        .current_dir("/")
        .output()
        .expect("sh must be available");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    )
}

/// Blocker 8, driven by the tool that failed in production.
///
/// The production measurement, in a launcher-shaped container on the production
/// node with the real child credentials:
///
/// ```text
/// TMPDIR=/var/tmp mktemp -d
///   mktemp: failed to create directory via template '/var/tmp/tmp.XXXXXXXXXX':
///           Read-only file system                                       (rc 1)
/// ```
///
/// A test process cannot mount a read-only filesystem, so the *refusal* is
/// modelled with a `0555` directory rather than `EROFS`. What is real here is the
/// tool, the environment variable, the value (from the sandbox itself), and the
/// fact that `mktemp` reports failure through the same channel — so the control
/// demonstrably fails, which is what makes the positive case mean something.
#[test]
fn the_sandbox_tmpdir_must_be_writable_or_a_real_mktemp_fails() {
    use std::os::unix::fs::PermissionsExt;

    let tmpdir = sandbox_tmpdir();
    assert_eq!(
        tmpdir,
        djinn_k8s::launcher_child_fs::LAUNCHER_VAR_TMP_DIR,
        "the render mounts one path and the sandbox pins another: that gap IS blocker 8"
    );

    let scratch = Scratch::new("mktemp");
    // The rendered emptyDir: a writable mount at the sandbox's TMPDIR.
    let mounted = scratch.dir("var-tmp-mounted");
    let (ok, stderr) = mktemp_in(&mounted);
    assert!(
        ok,
        "a writable mount at {tmpdir} must let a real mktemp succeed: {stderr}"
    );

    // The image layer under `readOnlyRootFilesystem: true`: the path is there and
    // the write is refused.
    //
    // Which refusal is available depends on the uid this test runs as. Mode bits
    // are ADVISORY for root, and CI containers frequently are root — so the
    // control probes whether it can actually deny itself a write, and falls back
    // to a refusal root does respect. Either way the control must fail; a
    // control that quietly could not fail is the whole trap #2617 walked into.
    let unwritable = scratch.dir("var-tmp-image-layer");
    std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o555))
        .expect("model an unwritable image-layer /var/tmp");
    let mode_bits_are_enforced = std::fs::File::create(unwritable.join(".probe")).is_err();
    let restored = std::fs::set_permissions(&unwritable, std::fs::Permissions::from_mode(0o755));

    let (denied_path, expected) = if mode_bits_are_enforced {
        (unwritable.clone(), "an unwritable directory")
    } else {
        // Root. Use a path that is not a directory at all: `mktemp -d` cannot
        // create under it for any uid. The production refusal is EROFS, which is
        // measured on the node rather than modelled here — see
        // `djinn_k8s::launcher_child_fs`.
        let not_a_dir = scratch.0.join("var-tmp-not-a-dir");
        std::fs::write(&not_a_dir, "").expect("model an unusable scratch path");
        (
            not_a_dir,
            "a path that is not a usable scratch dir (running as root)",
        )
    };
    if mode_bits_are_enforced {
        std::fs::set_permissions(&denied_path, std::fs::Permissions::from_mode(0o555))
            .expect("re-deny after the probe");
    }
    let (ok, stderr) = mktemp_in(&denied_path);
    if mode_bits_are_enforced {
        let _ = std::fs::set_permissions(&denied_path, std::fs::Permissions::from_mode(0o755));
    }
    assert!(
        !ok,
        "CONTROL FAILED TO FAIL: a real mktemp succeeded against {expected}, so the positive \
         case above proves nothing"
    );
    assert!(
        stderr.contains("failed to create directory"),
        "the refusal must come from mktemp itself, the way it did in production: {stderr}"
    );
    restored.expect("restore permissions so the scratch dir can be removed");
}
