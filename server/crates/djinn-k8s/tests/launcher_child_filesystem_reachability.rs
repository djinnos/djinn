//! The coverage whose absence hid goxi's fifth, eighth and ninth launcher
//! blockers.
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
//! It derives the invariant instead:
//!
//! > Every filesystem path a brokered child will actually depend on **at exec
//! > time** must be reachable in the launcher container's mount namespace with
//! > the access the child needs; every path the child is forbidden to see must
//! > NOT be; and where the two containers resolve the same path to *different*
//! > filesystems, that has to be a decision somebody wrote down.
//!
//! Nothing below names `/workspace`, `/cache` or `/mirror`.
//!
//! # Why the first version of this guard could not see blockers 8 and 9
//!
//! It derived its required set from **one** source: paths the rendered Pod puts
//! in the worker container's env. That is not the environment a brokered child
//! is born with. `process_broker::child_environment` builds that environment by
//! filtering the worker's **inherited process env** through
//! `is_allowed_environment_key` and then overlaying `Command::get_envs()` on top
//! — so a child's environment has three sources, and the manifest is only one:
//!
//! | source | who injects it | example | visible in the manifest? |
//! |--------|----------------|---------|--------------------------|
//! | image  | `ENV` in the generated Dockerfile | `HOME=/home/djinn` | no |
//! | render | `job.rs`'s worker env | `TMPDIR=/workspace` | yes |
//! | spawn  | `djinn_sandbox` on the `Command` | `TMPDIR=/var/tmp` | no |
//!
//! Blocker 8 was a *spawn*-time injection: the sandbox pins `TMPDIR=/var/tmp` on
//! every shell command, `/var/tmp` is in the image layer, and
//! `readOnlyRootFilesystem: true` makes it `EROFS` for a brokered child while it
//! stays writable for the worker. Blocker 9 was an *image*-time value: `HOME` is
//! never rendered, both containers resolve it to `/home/djinn`, and those are
//! two different volumes — so the installation token
//! `configure_private_dep_access` writes into the worker's `$HOME/.gitconfig`
//! was invisible to the cargo/go/pnpm processes that exist to read it.
//!
//! [`child_visible_paths`] therefore derives from all three, each by running the
//! real producer: the real Pod renderer, the real `djinn_image_builder`
//! Dockerfile emitter, and the real `djinn_sandbox` backend applied to a real
//! `std::process::Command`. Add a path-valued `ENV` to the image, an env var to
//! `job.rs`, or an env override to the sandbox, and this guard starts requiring
//! it without being edited.
//!
//! # What it still cannot see
//!
//! Honest boundaries, because a guard that looks total is worse than one with a
//! stated edge:
//!
//! * **Paths no environment variable names.** A tool that hardcodes `/tmp` is
//!   covered only because [`a_read_only_root_filesystem_does_not_take_away_scratch_the_worker_still_has`]
//!   names it explicitly. A tool that hardcodes something else is not covered at
//!   all. Nothing derivable from djinn's own code can predict that.
//! * **List-valued variables.** `PATH` is excluded (see [`path_valued`]): it is
//!   a `:`-separated list, not a path handle, and its entries are image-layer
//!   directories that need read+exec rather than a mount.
//! * **Runtime writes that do not travel over a rendered channel.** This is the
//!   half of blocker 9 that no derivation could have caught, so the *fix* moved
//!   the fact into a place a derivation can see: the worker→child handoff is now
//!   an explicitly rendered path (`djinn_k8s::private_dep_config`), which means
//!   it arrives through the `render` source above like any other. A future
//!   runtime write that invents its own private convention instead would be
//!   invisible again — which is why
//!   [`launcher_child_runtime_handoff`](../launcher_child_runtime_handoff.rs)
//!   asserts the shape of the channel rather than trusting it.
//! * **Whether a read-only image-layer path is good enough.** The guard can
//!   prove a path is writable in the worker and read-only in the launcher; it
//!   cannot prove no tool wants to write it. So it forces every such path to be
//!   named in [`ACKNOWLEDGED_ROOTFS_PARITY_BREAKS`] with a reason, and a new one
//!   fails the build. `RUSTUP_HOME` sits in that table today and is a real, open
//!   blocker — not a resolved one.
//!
//! # Proving it can fail
//!
//! Two layers of control, because the classifier and the filesystem can each be
//! vacuous on their own.
//!
//! * The classifier is driven over deliberately broken specs, by
//!   [`a_path_the_launcher_cannot_reach_is_reported_as_the_production_enoent`],
//!   [`removing_the_sandbox_tmpdir_mount_is_reported_as_an_unacknowledged_rootfs_write`],
//!   [`an_undeclared_divergence_between_the_two_containers_is_reported`] and
//!   [`an_unacknowledged_image_layer_path_is_reported`] — one per arm — with
//!   [`the_unmodified_render_is_clean_so_the_controls_are_not_measuring_a_red_baseline`]
//!   ruling out a red baseline underneath them.
//! * It does not stop at the manifest.
//!   [`the_rendered_launcher_mount_set_lets_a_real_chdir_and_write_succeed`]
//!   materializes the rendered mount set as a real directory tree and runs a real
//!   `chdir(2)` and a real `File::create` through it — the two syscalls that
//!   failed in production — and
//!   [`removing_the_workspace_mount_reproduces_the_production_enoent`] deletes the
//!   mount and requires the same harness to reproduce `NotFound` **by name**. The
//!   real tools that fail when blockers 8 and 9 are present (`mktemp`, `git`) are
//!   driven in the sibling file.

use std::collections::BTreeMap;
use std::path::Path;

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec, Volume, VolumeMount};
use uuid::Uuid;

use djinn_cgroup_launcher::child::{ARTIFACT_GID, CHILD_UID};
use djinn_cgroup_launcher::is_allowed_environment_key;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::launcher_child_fs::{
    LAUNCHER_HOME_DIR, LAUNCHER_TMP_DIR, LAUNCHER_VAR_TMP_DIR, is_under,
};

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

// ───────────────────────── the three derivations ─────────────────────────────

/// Where a value in a brokered child's environment came from.
///
/// Ordered by the overlay order `process_broker::child_environment` applies, so
/// a later source shadowing an earlier one is the real behaviour and not an
/// artifact of this file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Source {
    /// `ENV` in the generated Dockerfile. Invisible to the Pod manifest.
    Image,
    /// The rendered worker container env.
    Render,
    /// Set on the `Command` by `djinn_sandbox` immediately before spawn.
    /// Invisible to the Pod manifest.
    Spawn,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Image => "image ENV (djinn_image_builder)",
            Source::Render => "rendered worker env (job.rs)",
            Source::Spawn => "spawn-time sandbox override (djinn_sandbox)",
        }
    }
}

/// One path a brokered child depends on, and where it learned about it.
#[derive(Clone, Debug)]
struct Required {
    source: Source,
    key: String,
    path: String,
}

/// Is `key=value` a single filesystem path a brokered child can actually
/// receive?
///
/// Three conditions, each load-bearing:
///
/// * `is_allowed_environment_key` — the single predicate
///   `process_broker::child_environment` applies on the way out and
///   `CommandSpec::validate` re-applies inside the privileged broker. A key it
///   rejects genuinely never reaches a child and genuinely does not need a
///   mount. (It also means the image's lowercase `npm_config_cache` is *not*
///   forwarded, so `/cache/npm` is correctly absent from the required set — a
///   brokered `npm install` caches in `$HOME` instead. That is a cold cache, not
///   a broken build, and widening the broker's allow-list is not this change's
///   business.)
/// * absolute — a relative value is not a mount path.
/// * no `:` — `PATH` and friends are lists of directories, not path handles.
///   Their entries live in the image layer and need read+exec, which the
///   launcher's own rootfs supplies from the same image.
fn path_valued(key: &str, value: &str) -> bool {
    is_allowed_environment_key(key) && value.starts_with('/') && !value.contains(':')
}

/// Every path-valued env var the pod declares to the worker.
fn render_time_paths(pod: &PodSpec) -> Vec<Required> {
    container(pod, "worker")
        .env
        .iter()
        .flatten()
        .filter_map(|env| {
            let value = env.value.as_deref()?;
            path_valued(&env.name, value).then(|| Required {
                source: Source::Render,
                key: env.name.clone(),
                path: value.to_owned(),
            })
        })
        .collect()
}

/// Every path-valued `ENV` the **real** image builder bakes into the container.
///
/// Derived by running `generate_dockerfile` and parsing the `ENV` lines it
/// emits, rather than by listing them: `HOME`, `RUSTUP_HOME`, `GOPATH`,
/// `GOMODCACHE`, `PNPM_HOME` and the rest are exactly the values a brokered
/// child inherits and the manifest never mentions. This is the source blocker 9
/// lived in.
fn image_time_paths() -> Vec<Required> {
    let context = djinn_image_builder::generate_dockerfile(
        &djinn_stack::environment::EnvironmentConfig::default(),
        &djinn_image_builder::AgentWorkerImage::new("registry.example/djinn-agent-worker", "test"),
    )
    .expect("the baseline image config must render a Dockerfile");
    let mut required = Vec::new();
    for line in context.dockerfile.lines() {
        let Some(assignments) = line.strip_prefix("ENV ") else {
            continue;
        };
        for token in assignments.split_whitespace() {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            let value = value.trim_matches('"');
            if path_valued(key, value) {
                required.push(Required {
                    source: Source::Image,
                    key: key.to_owned(),
                    path: value.to_owned(),
                });
            }
        }
    }
    assert!(
        !required.is_empty(),
        "the image ENV derivation found nothing; `generate_dockerfile`'s output shape changed \
         and this source has silently stopped contributing"
    );
    required
}

/// Every path-valued env var the **real** sandbox sets on a command at spawn.
///
/// `djinn_sandbox::SANDBOX.apply` is what `extension::handlers::workspace` calls
/// on the very `Command` that becomes a `CommandSpec`, so whatever it puts on
/// that command is what a brokered child is born with. Harvested off a real
/// `Command` rather than read from a constant: that is what makes this source
/// track the sandbox instead of a copy of it.
fn spawn_time_paths() -> Vec<Required> {
    let mut command = std::process::Command::new("true");
    djinn_sandbox::SANDBOX
        .apply(
            djinn_sandbox::SandboxScope::Worktree(Path::new(LAUNCHER_VAR_TMP_DIR)),
            &mut command,
        )
        .expect("the production sandbox must configure a command");
    let required: Vec<Required> = command
        .get_envs()
        .filter_map(|(key, value)| {
            let key = key.to_str()?;
            let value = value?.to_str()?;
            path_valued(key, value).then(|| Required {
                source: Source::Spawn,
                key: key.to_owned(),
                path: value.to_owned(),
            })
        })
        .collect();
    assert!(
        !required.is_empty(),
        "the sandbox injected no path-valued environment; if it stopped overriding TMPDIR this \
         source is now vacuous and blocker 8's class is unguarded again"
    );
    required
}

/// The union of all three sources, deduplicated by `(key, path)`.
///
/// A key present in more than one source with DIFFERENT values yields more than
/// one entry on purpose: `TMPDIR` is `/workspace` at render time (the worker's
/// own `TempDir`, and the brokered `cwd`) and `/var/tmp` at spawn time (every
/// sandboxed shell command). Both have to work, so both are required.
fn child_visible_paths(pod: &PodSpec) -> Vec<Required> {
    let mut all = image_time_paths();
    all.extend(render_time_paths(pod));
    all.extend(spawn_time_paths());
    all.sort_by(|a, b| (&a.key, &a.path, a.source).cmp(&(&b.key, &b.path, b.source)));
    all.dedup_by(|a, b| a.key == b.key && a.path == b.path);
    all
}

/// Render-time paths only, keyed by env var.
///
/// Retained for the two consumers that are specifically about what the *pod*
/// promised: the brokered `cwd` and the worker-private classification.
fn child_visible_declared_paths(pod: &PodSpec) -> BTreeMap<String, String> {
    render_time_paths(pod)
        .into_iter()
        .map(|required| (required.key, required.path))
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

// ─────────────────── deliberate divergences, each with a reason ───────────────

/// Paths the launcher supplies from a DIFFERENT filesystem than the worker's.
///
/// Not an escape hatch: a path lands here only when the launcher does mount it
/// writable, and the entry has to say why the two containers must not share it.
/// A new divergence fails the guard until somebody decides which it is, because
/// "the same path resolves to two different volumes" is precisely the shape of
/// blocker 9 and is invisible to every reachability check.
const DELIBERATELY_UNSHARED_SCRATCH: &[(&str, &str)] = &[
    (
        LAUNCHER_HOME_DIR,
        "$HOME is per-container ON PURPOSE. `$HOME/.gitconfig` is global-scope git \
         configuration for whoever reads it, and the WORKER reads it too — every git the \
         worker runs loads it. One shared writable $HOME would therefore let \
         repository-controlled code running as CHILD_UID plant `core.sshCommand` in a file \
         the worker executes as uid 1000, which can read the credentials Secret: exactly \
         the hop the broker exists to prevent. The one worker→child handoff that genuinely \
         has to cross goes over the one-way channel in `djinn_k8s::private_dep_config` \
         instead (RW in the worker, readOnly in the launcher).",
    ),
    (
        LAUNCHER_VAR_TMP_DIR,
        "the sandbox's spawn-time TMPDIR (blocker 8), launcher-private for the same reason. \
         The worker's own TempDir follows the pod's TMPDIR=/workspace, so there is nothing \
         to share.",
    ),
];

/// Paths that live in the image layer in BOTH containers, and are therefore
/// writable for the worker and read-only for the launcher.
///
/// The guard can prove the asymmetry; it cannot prove no tool wants to write
/// there. So every one of them has to be named with a reason, and a new one
/// fails the build rather than joining the silent majority. **`RUSTUP_HOME` is
/// an open blocker recorded here, not a resolved one.**
const ACKNOWLEDGED_ROOTFS_PARITY_BREAKS: &[(&str, &str)] = &[
    (
        "/usr/local/rustup",
        "OPEN BLOCKER (goxi, tenth). The image chmods RUSTUP_HOME 0777 specifically so a \
         session can `rustup install` a toolchain a repo pins in rust-toolchain.toml — see \
         `emit_cleanup` in djinn_image_builder::dockerfile. Measured on the production node: \
         writable in the worker, `Read-only file system` in the launcher, with only \
         `stable-x86_64-unknown-linux-gnu` baked. A repo pinning anything else fails when \
         brokered. It is NOT fixed here: an emptyDir would mask the baked toolchain and the \
         cargo/rustc proxies that PATH resolves through, so it needs its own change.",
    ),
    (
        "/usr/local/cargo",
        "the image's CARGO_HOME, which `job.rs` overrides at runtime to /cache/cargo — so \
         cargo's registry and data dirs are on the PVC and what remains here is the rustup \
         proxy binaries on PATH, which need read+exec only.",
    ),
    (
        "/go",
        "GOPATH. Every Go store that is actually written at runtime is redirected off it by \
         image ENV (GOMODCACHE, GOCACHE, GOBIN all land on /cache), and the build-time \
         `go install` of scip-go into /go/bin happens in the image layer. Measured: the \
         directory does not even exist in a non-Go image.",
    ),
];

/// Volumes both containers mount, where the ASYMMETRY of access is the point.
///
/// A one-way channel: read-write in the worker, `readOnly` in the launcher. It
/// exists so a value the worker computes at runtime can reach the child without
/// the child being able to influence what the worker reads — the property that
/// makes a shared `$HOME` unacceptable and makes this acceptable.
///
/// The read_only parity rule that governs every other shared volume would reject
/// this by construction, so the exception is named here and the direction is
/// asserted rather than assumed.
const DECLARED_ONE_WAY_CHANNELS: &[(&str, &str)] = &[(
    djinn_k8s::private_dep_config::CHILD_GIT_CONFIG_FILE,
    "the private-dependency installation token (blocker 9). The worker writes the \
     `url.<...>.insteadOf` rewrite here and the launcher's git trust anchor `[include]`s it, \
     so a brokered cargo/go/pnpm fetch of a private dependency is authenticated. readOnly on \
     the launcher side is what keeps repository-controlled code from rewriting \
     protected-scope git configuration - see `djinn_k8s::private_dep_config`.",
)];

fn declared_reason(table: &[(&'static str, &'static str)], path: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(candidate, _)| *candidate == path)
        .map(|(_, reason)| *reason)
}

/// A declaration that no longer corresponds to anything the derivation produces
/// is a stale declaration, and a stale declaration is how a real blocker gets
/// waved through.
#[test]
fn no_divergence_declaration_has_gone_stale() {
    let job = render(false);
    let derived: Vec<String> = child_visible_paths(pod_of(&job))
        .into_iter()
        .map(|required| required.path)
        .collect();
    for (table, label) in [
        (DELIBERATELY_UNSHARED_SCRATCH, "unshared scratch"),
        (ACKNOWLEDGED_ROOTFS_PARITY_BREAKS, "rootfs parity break"),
        (DECLARED_ONE_WAY_CHANNELS, "one-way handoff channel"),
    ] {
        for (path, _) in table {
            assert!(
                derived.iter().any(|candidate| candidate == path),
                "{path} is declared as a {label}, but no source (image ENV, rendered worker \
                 env, sandbox spawn override) names it any more — delete the declaration \
                 instead of carrying it. Derived: {derived:?}"
            );
        }
    }
}

// ──────────────────────── the derived invariant ──────────────────────────────

/// What [`classify`] found. Violations are collected rather than asserted so the
/// non-vacuity controls below can drive the REAL classifier over a broken spec
/// and assert on what it says, instead of re-deriving the check by hand.
#[derive(Default, Debug)]
struct Classification {
    violations: Vec<String>,
    shared: usize,
    unshared: usize,
    image_layer: usize,
    denied: usize,
}

/// Apply the invariant to `pod` and report every way it is broken.
fn classify(pod: &PodSpec) -> Classification {
    let mounts = container(pod, LAUNCHER_CONTAINER_NAME)
        .volume_mounts
        .as_deref()
        .unwrap_or_default();
    let worker_mounts = container(pod, "worker")
        .volume_mounts
        .as_deref()
        .unwrap_or_default();
    let mut found = Classification::default();

    for Required { source, key, path } in child_visible_paths(pod) {
        let whence = format!("{key}={path} ({})", source.label());

        // The isolation half of the invariant: a path the child does not need
        // must not be reachable from the launcher either.
        if let Some(reason) = unreachable_by_design(pod, &key, &path) {
            if covering_mount(mounts, &path).is_some() {
                found.violations.push(format!(
                    "{whence} is {reason} and must NOT be reachable from the launcher's mount \
                     namespace, but a volumeMount covers it"
                ));
            }
            found.denied += 1;
            continue;
        }

        // A declared one-way channel: same volume, deliberately asymmetric.
        if declared_reason(DECLARED_ONE_WAY_CHANNELS, &path).is_some() {
            let launcher_mount = covering_mount(mounts, &path);
            let worker_mount = covering_mount(worker_mounts, &path);
            match (launcher_mount, worker_mount) {
                (Some(launcher), Some(worker))
                    if launcher.name == worker.name
                        && launcher.read_only == Some(true)
                        && worker.read_only != Some(true) => {}
                _ => found.violations.push(format!(
                    "{whence} is a declared one-way worker->child channel, so it must be ONE \
                     volume mounted read-write in the worker and readOnly in the launcher. Got \
                     launcher={launcher_mount:?} worker={worker_mount:?}. A writable \
                     launcher-side mount here hands repository-controlled code the ability to \
                     rewrite git configuration the trust anchor loads at protected scope."
                )),
            }
            found.shared += 1;
            continue;
        }

        match (
            covering_mount(mounts, &path),
            covering_mount(worker_mounts, &path),
        ) {
            // Same volume in both containers: the child gets literally the
            // filesystem the worker was promised. Access must match too.
            (Some(launcher_mount), Some(worker_mount))
                if launcher_mount.name == worker_mount.name =>
            {
                // Parity is asserted against the WORKER's own mount rather than
                // against `false`, because an evidence-spike run legitimately
                // denies both of them write access to /cache and /mirror — and
                // if that isolation held only for commands that happen not to be
                // brokered it would not be isolation at all.
                if (launcher_mount.read_only == Some(true))
                    != (worker_mount.read_only == Some(true))
                {
                    found.violations.push(format!(
                        "{whence}: the launcher mounts it read_only={:?} but the worker sees \
                         read_only={:?}. A brokered command must get exactly the access the pod \
                         promised the worker — more would break the evidence-spike contract, \
                         less turns the production ENOENT into an EROFS.",
                        launcher_mount.read_only, worker_mount.read_only
                    ));
                }
                found.shared += 1;
            }
            // A launcher mount, but not the worker's filesystem. Must be
            // writable — it exists because `readOnlyRootFilesystem` took the
            // image's copy away — and must be declared, because a divergence is
            // blocker 9's whole shape.
            (Some(launcher_mount), _) => {
                if launcher_mount.read_only == Some(true) {
                    found.violations.push(format!(
                        "{whence} is launcher-private scratch and must be writable; a \
                         read-only mount here is the EROFS blocker 8 was made of"
                    ));
                }
                if declared_reason(DELIBERATELY_UNSHARED_SCRATCH, &path).is_none() {
                    found.violations.push(format!(
                        "{whence}: the launcher mounts volume `{}` here while the worker \
                         resolves the SAME path to {}. That is not automatically wrong — but \
                         it means a runtime write on one side is invisible on the other, which \
                         is exactly blocker 9 (the private-dependency token in \
                         $HOME/.gitconfig). Either share the volume or add {path} to \
                         DELIBERATELY_UNSHARED_SCRATCH with the reason it must not be shared.",
                        launcher_mount.name,
                        covering_mount(worker_mounts, &path)
                            .map(|mount| format!("volume `{}`", mount.name))
                            .unwrap_or_else(|| "its own image layer".to_owned()),
                    ));
                }
                found.unshared += 1;
            }
            // The pod mounts it for the worker and the launcher cannot reach it
            // at all. This is blocker 5, exactly.
            (None, Some(worker_mount)) => found.violations.push(format!(
                "{whence}: the worker gets it from volume `{}`, the broker forwards {key} to a \
                 brokered child, and the child runs in the LAUNCHER's mount namespace — but no \
                 launcher volumeMount covers {path}. `chdir`/open under it returns ENOENT and \
                 the child _exits before execve. Launcher mounts: {:?}",
                worker_mount.name,
                mounts
                    .iter()
                    .map(|mount| mount.mount_path.as_str())
                    .collect::<Vec<_>>()
            )),
            // Image layer in both containers: present, but writable for the
            // worker and read-only for the launcher. Must be acknowledged.
            (None, None) => {
                if declared_reason(ACKNOWLEDGED_ROOTFS_PARITY_BREAKS, &path).is_none() {
                    found.violations.push(format!(
                        "{whence} is covered by no mount in either container, so it comes from \
                         the image layer — writable in the worker, and READ-ONLY in the \
                         launcher because of `readOnlyRootFilesystem: true`. If a brokered tool \
                         writes there it gets EROFS, silently, exactly like blocker 8. Either \
                         render a mount for it or add {path} to \
                         ACKNOWLEDGED_ROOTFS_PARITY_BREAKS with the reason read-only is \
                         acceptable."
                    ));
                }
                found.image_layer += 1;
            }
        }
    }
    found
}

/// The blocker itself, expressed as a property rather than a mount list.
#[test]
fn every_child_visible_path_is_reachable_in_the_launcher_mount_namespace() {
    for is_evidence_spike in [false, true] {
        let job = render(is_evidence_spike);
        let pod = pod_of(&job);

        let derived = child_visible_paths(pod);
        assert!(
            derived.len() >= 8,
            "the derivation found only {} child-visible paths across three sources, which is \
             too few to be exercising anything — a source or the filter changed shape: \
             {derived:?}",
            derived.len()
        );
        // Every source has to be contributing, or a whole injection point has
        // gone dark exactly the way blockers 8 and 9 did.
        for source in [Source::Image, Source::Render, Source::Spawn] {
            assert!(
                derived.iter().any(|required| required.source == source),
                "no path came from {}; that source is no longer being derived",
                source.label()
            );
        }

        let found = classify(pod);
        assert!(
            found.violations.is_empty(),
            "spike={is_evidence_spike}: {}",
            found.violations.join("\n\n")
        );
        // Every bucket must have fired. A classifier that silently sorted every
        // path into one of them would make the others vacuous.
        for (count, what) in [
            (found.shared, "shared-volume"),
            (found.unshared, "launcher-private scratch"),
            (found.image_layer, "image-layer"),
            (found.denied, "worker-private"),
        ] {
            assert!(
                count > 0,
                "no {what} path was classified, so that arm of the invariant proved nothing"
            );
        }
    }
}

// ───────────── the classifier can actually fail: four controls ────────────────

/// Add a worker env var naming `path`, so the derivation picks it up the way it
/// picks up any rendered path.
fn with_probe_env(pod: &mut PodSpec, key: &str, path: &str) {
    for container in pod.containers.iter_mut() {
        if container.name == "worker" {
            container.env.get_or_insert_with(Vec::new).push(EnvVar {
                name: key.to_string(),
                value: Some(path.to_string()),
                ..EnvVar::default()
            });
        }
    }
}

fn launcher_mounts_of(pod: &mut PodSpec) -> &mut Vec<VolumeMount> {
    pod.init_containers
        .iter_mut()
        .flatten()
        .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        .expect("rendered pod has a launcher container")
        .volume_mounts
        .get_or_insert_with(Vec::new)
}

/// Blocker 5's shape: a path the worker gets from a volume that the launcher
/// cannot reach at all.
#[test]
fn a_path_the_launcher_cannot_reach_is_reported_as_the_production_enoent() {
    let job = render(false);
    let mut pod = pod_of(&job).clone();
    let cwd = brokered_cwd(&pod);
    launcher_mounts_of(&mut pod).retain(|mount| !is_under(&cwd, &mount.mount_path));

    let violations = classify(&pod).violations;
    assert!(
        violations.iter().any(|violation| violation.contains(&cwd)
            && violation.contains("no launcher volumeMount covers")),
        "stripping the mount covering {cwd} must be reported as the production ENOENT; got \
         {violations:?}"
    );
}

/// Blocker 8's shape: the sandbox's spawn-time `TMPDIR` left on the image layer.
///
/// The mount removed here is the one this change added. With it gone the path is
/// covered by nothing in either container — which is precisely the state the
/// production probe measured, where `TMPDIR=/var/tmp mktemp -d` returned
/// `Read-only file system` in the launcher and succeeded in the worker.
#[test]
fn removing_the_sandbox_tmpdir_mount_is_reported_as_an_unacknowledged_rootfs_write() {
    let sandbox_tmpdir = spawn_time_paths()
        .into_iter()
        .find(|required| required.key == "TMPDIR")
        .expect("the sandbox pins TMPDIR")
        .path;
    assert_eq!(
        sandbox_tmpdir, LAUNCHER_VAR_TMP_DIR,
        "the render must mount exactly what the sandbox pins; these two constants are what \
         blocker 8 was the gap between"
    );

    let job = render(false);
    let mut pod = pod_of(&job).clone();
    let before = launcher_mounts_of(&mut pod).len();
    launcher_mounts_of(&mut pod).retain(|mount| mount.mount_path != sandbox_tmpdir);
    assert_eq!(
        before - 1,
        launcher_mounts_of(&mut pod).len(),
        "exactly the {sandbox_tmpdir} mount must be removed"
    );

    let violations = classify(&pod).violations;
    assert!(
        violations.iter().any(
            |violation| violation.contains(&format!("TMPDIR={sandbox_tmpdir}"))
                && violation.contains("ACKNOWLEDGED_ROOTFS_PARITY_BREAKS")
        ),
        "with the mount gone, {sandbox_tmpdir} is an image-layer path a brokered tool writes \
         to — the guard must say so; got {violations:?}"
    );
}

/// Blocker 9's shape: the two containers resolve the same path to two different
/// filesystems, and nobody wrote down why.
///
/// This is the arm the previous guard did not have at all. `$HOME` passes today
/// only because [`DELIBERATELY_UNSHARED_SCRATCH`] carries an explicit reason;
/// an undeclared divergence must fail.
#[test]
fn an_undeclared_divergence_between_the_two_containers_is_reported() {
    let job = render(false);
    let mut pod = pod_of(&job).clone();
    with_probe_env(&mut pod, "DJINN_PROBE_SCRATCH_DIR", "/probe-scratch");
    // Mounted in the launcher from a launcher-private volume, and nowhere in the
    // worker: reachable, writable, and silently a different filesystem.
    launcher_mounts_of(&mut pod).push(VolumeMount {
        name: djinn_k8s::launcher_child_fs::VOLUME_LAUNCHER_TMP.to_string(),
        mount_path: "/probe-scratch".to_string(),
        ..VolumeMount::default()
    });

    let violations = classify(&pod).violations;
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("/probe-scratch")
                && violation.contains("DELIBERATELY_UNSHARED_SCRATCH")),
        "a path the launcher supplies from its own volume while the worker resolves it to the \
         image layer must be declared or rejected; got {violations:?}"
    );
}

/// And the image-layer arm: a path nothing mounts, which is read-only for the
/// child and writable for the worker.
#[test]
fn an_unacknowledged_image_layer_path_is_reported() {
    let job = render(false);
    let mut pod = pod_of(&job).clone();
    with_probe_env(&mut pod, "DJINN_PROBE_IMAGE_DIR", "/probe-image-layer");

    let violations = classify(&pod).violations;
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("/probe-image-layer")
                && violation.contains("ACKNOWLEDGED_ROOTFS_PARITY_BREAKS")),
        "got {violations:?}"
    );
}

/// The control that matters most: the unmodified render must be clean, so none
/// of the four controls above is passing because the baseline is already red.
#[test]
fn the_unmodified_render_is_clean_so_the_controls_are_not_measuring_a_red_baseline() {
    assert_eq!(
        classify(pod_of(&render(false))).violations,
        Vec::<String>::new()
    );
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
    for path in [LAUNCHER_TMP_DIR, LAUNCHER_HOME_DIR, LAUNCHER_VAR_TMP_DIR] {
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

/// The brokered `cwd`. Derived from the pod's own `TMPDIR`, which `job.rs`
/// renders to the workspace root the agent's worktree is created under — not
/// spelled out here, so a renderer change moves this with it.
fn brokered_cwd(pod: &PodSpec) -> String {
    child_visible_declared_paths(pod)
        .remove("TMPDIR")
        .expect("the pod declares TMPDIR, which roots the brokered cwd")
}
