// Test: eprintln is the skip-reason channel for the gated half, mirroring
// tests/kueue_cluster_harness.rs.
#![allow(clippy::print_stderr)]
//! Live-apiserver proof for the `resize-v2` birth downsize (0ppk-1b, AC8).
//!
//! # Two halves, on purpose
//!
//! * The `guard_*` tests are **hermetic** and are NOT `#[ignore]`d. They run in
//!   ordinary CI with no cluster and no Docker, and they assert the properties
//!   that make this harness safe to point at anything: the reserved names it
//!   refuses, and the fact that it derives its context instead of discovering
//!   one. A safety guard that only runs when someone remembers to spin up a
//!   cluster is not a safety guard.
//! * The `live_*` tests are `#[ignore]` + `DJINN_TEST_RESIZE_CLUSTER=1` gated
//!   and require the disposable cluster from
//!   `scripts/kind/setup-resize-cluster.sh`.
//!
//! # Running the live half
//!
//! ```text
//! scripts/kind/setup-resize-cluster.sh up
//! DJINN_TEST_RESIZE_CLUSTER=1 cargo test -p djinn-k8s --test pod_resize_kind -- --ignored
//! scripts/kind/setup-resize-cluster.sh down   # ALWAYS, pass or fail
//! ```
//!
//! # Why the context is pinned twice
//!
//! Guard 1 is the name: it must be the context of the cluster the setup script
//! creates. Guard 2 is the resolved API server URL, which catches what guard 1
//! cannot — a kubeconfig entry *named* `kind-djinn-resize-harness` that points
//! somewhere else entirely. kind always serves on loopback; no managed control
//! plane does, and every context in a Djinn developer's kubeconfig today is a
//! live EKS cluster.
//!
//! `kube::Client::try_default()` is deliberately never used here: it resolves
//! the CURRENT context, and these tests create, mutate and delete objects.
//!
//! # What is real
//!
//! The Pod is rendered by `build_task_run_job_with_read_sources` and
//! `apply_launcher_authority_protocol` — the production render — so the
//! launcher really is a native sidecar in `spec.initContainers` with
//! `restartPolicy: Always` and a `resize-v2` CPU ceiling. The resize goes
//! through `PodResizeClient`, i.e. `Patch::Strategic` against the `resize`
//! subresource, and confirmation goes through `confirm_launcher_cpu` reading
//! `status.initContainerStatuses`.
//!
//! Only the image *references* are rewritten, to a public pause image. The
//! launcher binary's own behaviour is not what this file tests: the kubelet's
//! actuation of a limits-only in-place resize is, and that is indifferent to
//! what the container runs. Leaving the real (unpullable) references in would
//! park every Pod in `ImagePullBackOff`, where the launcher never starts, no
//! status is ever published, and the test would report "not confirmed" for a
//! reason that has nothing to do with the code under test.

use std::env;
use std::path::Path;
use std::time::Duration;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_k8s::KubernetesConfig;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::pod_resize::{
    CpuLimit, KubePodResizeApi, PodResizeClient, PodResizeError, confirm_launcher_cpu,
    declared_launcher_cpu_limit, locate_launcher_spec, locate_launcher_status,
};
use djinn_k8s::runtime::{ObservedLauncherSidecar, TaskRunPodResizeSurface};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use uuid::Uuid;

mod support;

/// The only cluster this harness will ever talk to.
const HARNESS_CLUSTER: &str = "djinn-resize-harness";

/// The only context this harness will ever talk to.
const HARNESS_CONTEXT: &str = "kind-djinn-resize-harness";

/// Namespace the harness creates and deletes. Not `djinn`: a name collision
/// with a real deployment's namespace is exactly the kind of coincidence a
/// destructive harness must not rely on being impossible.
const HARNESS_NAMESPACE: &str = "djinn-resize-harness";

/// The birth limit under test.
const BIRTH_MILLICORES: u64 = 250;

/// The launcher CPU ceiling **as the apiserver stores it**, not as the render
/// emits it. `apply_launcher_cpu_ceiling` writes `4000m`; `Quantity` is
/// canonicalised on the way in and reads back as `4`. That gap is the whole
/// reason confirmation compares millicores, and it is asserted live below.
const RENDERED_CEILING: &str = "4";

/// Names this harness must never operate on, mirroring
/// `scripts/kind/setup-resize-cluster.sh`'s `RESERVED_*` arrays. Duplicated
/// deliberately: the script guards `kind delete`, this guards the API calls, and
/// a single shared source would have to be read at runtime by both — which is a
/// file the harness could then be pointed at.
const RESERVED_CLUSTER_NAMES: &[&str] = &["djinn", "kind", "djinn-kueue-harness"];
const RESERVED_REGISTRY_NAMES: &[&str] = &["kind-registry", "djinn-kueue-harness-registry"];
const RESERVED_REG_PORTS: &[u16] = &[5000, 5001, 5051];

// ── Hermetic half ─────────────────────────────────────────────────────────

#[test]
fn guard_harness_names_are_not_reserved() {
    assert!(
        !RESERVED_CLUSTER_NAMES.contains(&HARNESS_CLUSTER),
        "the harness cluster name collides with a reserved name; this harness \
         DELETES its target, so a collision destroys the developer's Tilt \
         cluster or the Kueue harness mid-run",
    );
    assert!(
        !RESERVED_REGISTRY_NAMES.contains(&"djinn-resize-harness-registry"),
        "the harness registry name collides with a reserved name",
    );
    assert!(
        !RESERVED_REG_PORTS.contains(&5052),
        "the harness registry port collides with a published port",
    );
}

#[test]
fn guard_harness_context_is_derived_from_the_cluster_name() {
    assert_eq!(
        HARNESS_CONTEXT,
        format!("kind-{HARNESS_CLUSTER}"),
        "the context must be DERIVED from the cluster name this harness creates, \
         never discovered from the ambient kubeconfig",
    );
}

#[test]
fn guard_a_non_local_apiserver_is_refused() {
    // The exact shapes a managed control plane presents, and the exact shapes
    // kind does. This is the predicate `harness_client` applies, exercised
    // without a cluster so it cannot rot unnoticed.
    for hostile in [
        "https://ABCDEF.gr7.eu-west-3.eks.amazonaws.com",
        "https://kubernetes.default.svc",
        "https://10.0.0.1:6443",
        "https://127.0.0.1.evil.example:6443",
    ] {
        assert!(
            !is_local_apiserver(hostile),
            "{hostile} must be refused: it is not a local kind API server",
        );
    }
    for benign in [
        "https://127.0.0.1:6443",
        "https://localhost:41234",
        "https://[::1]:6443",
    ] {
        assert!(
            is_local_apiserver(benign),
            "{benign} is a local kind server"
        );
    }
}

#[test]
fn guard_the_setup_script_exists_and_reserves_the_same_names() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/kind/setup-resize-cluster.sh")
        .canonicalize()
        .expect(
            "scripts/kind/setup-resize-cluster.sh must exist; this harness cannot \
                 create its own cluster and a missing script means the live half is \
                 unrunnable rather than merely skipped",
        );
    let body = std::fs::read_to_string(script).expect("read setup script");
    for reserved in RESERVED_CLUSTER_NAMES {
        assert!(
            body.contains(reserved),
            "the setup script no longer reserves cluster name `{reserved}`; the two \
             halves of this harness must refuse the same targets",
        );
    }
    assert!(
        body.contains(HARNESS_CLUSTER),
        "the setup script no longer creates `{HARNESS_CLUSTER}`",
    );
}

/// Whether a resolved API server URL belongs to a local kind cluster.
///
/// Host-anchored on purpose: `https://127.0.0.1.evil.example` starts with
/// `https://127.0.0.1` and is a remote host.
fn is_local_apiserver(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(head, _)| head);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

// ── Live half ─────────────────────────────────────────────────────────────

fn live_tests_enabled() -> bool {
    if env::var("DJINN_TEST_RESIZE_CLUSTER").is_err() {
        eprintln!("pod_resize_kind: DJINN_TEST_RESIZE_CLUSTER not set — skipping");
        return false;
    }
    true
}

/// A client pinned to [`HARNESS_CONTEXT`], after two independent refusals.
async fn harness_client() -> Client {
    // Before anything else: `kube::Client` builds its TLS config eagerly, and
    // this workspace enables both rustls providers, so without an explicit
    // install the very first client construction panics inside rustls with
    // "Could not automatically determine the process-level CryptoProvider" —
    // before a single byte reaches the apiserver. `server/src/main.rs` does this
    // for the production binary; a test binary has no `main` to do it here.
    support::install_crypto_provider();
    let requested =
        env::var("DJINN_TEST_RESIZE_CONTEXT").unwrap_or_else(|_| HARNESS_CONTEXT.to_owned());
    assert_eq!(
        requested, HARNESS_CONTEXT,
        "this harness only ever targets the context of the cluster \
         scripts/kind/setup-resize-cluster.sh creates and deletes",
    );
    let config = kube::Config::from_kubeconfig(&kube::config::KubeConfigOptions {
        context: Some(requested.clone()),
        ..Default::default()
    })
    .await
    .expect("resolve the harness context from the kubeconfig");
    let server = config.cluster_url.to_string();
    assert!(
        is_local_apiserver(&server),
        "refusing to run against {server}: context {requested} does not resolve to a \
         local kind API server, so it is not a cluster this harness created",
    );
    Client::try_from(config).expect("build a kube client for the harness context")
}

/// The production render of a `resize-v2` task-run Job, with only its image
/// references made pullable.
fn rendered_resize_v2_job(task_run_id: &Uuid) -> Job {
    let config = KubernetesConfig::from_env();
    let mut job = djinn_k8s::job::build_task_run_job_with_read_sources(
        &config,
        task_run_id,
        "resize-harness-project",
        "resize-harness-secret",
        "registry.k8s.io/pause:3.10",
        &[],
        None,
        false,
        None,
        None,
    );
    djinn_k8s::launcher::apply_launcher_authority_protocol(
        &mut job,
        config.cgroup_launcher_mode,
        LauncherAuthorityProtocol::ResizeV2,
    )
    .expect("the render must produce a launcher sidecar under resize-v2");
    job
}

/// The production render, with only its image references made pullable.
fn rendered_resize_v2_pod(task_run_id: &Uuid) -> Pod {
    let job = rendered_resize_v2_job(task_run_id);

    let template = job
        .spec
        .expect("job spec")
        .template
        .spec
        .expect("pod template spec");
    let mut pod = Pod {
        metadata: kube::api::ObjectMeta {
            name: Some(format!("resize-harness-{task_run_id}")),
            namespace: Some(HARNESS_NAMESPACE.to_owned()),
            ..Default::default()
        },
        spec: Some(template),
        status: None,
    };
    make_pullable(&mut pod);
    pod
}

/// Swap every image reference for a public pause image and drop the volume
/// mounts that reference cluster-specific volumes.
///
/// The launcher entry keeps its NAME, its `restartPolicy`, its env and its
/// resources — everything the resize protocol addresses. Only what the kubelet
/// would need a real cluster to satisfy is replaced.
fn make_pullable(pod: &mut Pod) {
    const PAUSE: &str = "registry.k8s.io/pause:3.10";
    let spec = pod.spec.as_mut().expect("pod spec");
    spec.restart_policy = Some("Never".to_owned());
    spec.service_account_name = None;
    spec.volumes = None;
    spec.node_selector = None;
    spec.tolerations = None;
    spec.affinity = None;
    // The production render carries `runtimeClassName: djinn-cgroup-writable`
    // (`job.rs`), and a Pod naming a RuntimeClass that does not exist is
    // REJECTED at admission — 403 "pod rejected: RuntimeClass
    // \"djinn-cgroup-writable\" not found" — so without this the Pod is never
    // created at all and nothing downstream runs. Cleared rather than installed
    // on purpose:
    //
    // * The claim under test is that the KUBELET actuates a limits-only
    //   in-place resize and reports it in `status.initContainerStatuses`. The
    //   kubelet issues that through CRI `UpdateContainerResources` against the
    //   same `io.containerd.runc.v2` shim either way; `cgroup_writable` changes
    //   only whether `/sys/fs/cgroup` is writable *inside* the container, which
    //   nothing here touches.
    // * Declaring a RuntimeClass named `djinn-cgroup-writable` backed by plain
    //   `runc` — the obvious shortcut — would be the exact silent-wrong-handler
    //   trap `scripts/kind/setup-kueue-cluster.sh --cgroup-writable` was built
    //   to catch: the class resolves, the Pod is admitted, and the sandbox comes
    //   up with a read-only `/sys/fs/cgroup` while every assertion still passes.
    //   Installing the REAL handler is that script's job, and this node image
    //   (1.33.1, containerd v2.1.1) is not the one it is verified against.
    //
    // Resize under the production runtime handler is therefore NOT proven here.
    // It is proven in `tests/kueue_cluster_harness.rs`'s cluster that the
    // handler loads at all; the intersection of the two remains open.
    spec.runtime_class_name = None;
    for container in spec.init_containers.iter_mut().flatten() {
        container.image = Some(PAUSE.to_owned());
        container.command = None;
        container.args = None;
        container.volume_mounts = None;
        container.env_from = None;
        container.liveness_probe = None;
        container.readiness_probe = None;
        container.startup_probe = None;
        container.security_context = None;
    }
    for container in spec.containers.iter_mut() {
        container.image = Some(PAUSE.to_owned());
        container.command = None;
        container.args = None;
        container.volume_mounts = None;
        container.env_from = None;
        container.liveness_probe = None;
        container.readiness_probe = None;
        container.startup_probe = None;
        container.security_context = None;
    }
}

async fn ensure_namespace(client: &Client) {
    use k8s_openapi::api::core::v1::Namespace;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let namespace = Namespace {
        metadata: kube::api::ObjectMeta {
            name: Some(HARNESS_NAMESPACE.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    };
    match namespaces.create(&PostParams::default(), &namespace).await {
        Ok(_) => {}
        Err(kube::Error::Api(response)) if response.code == 409 => {}
        Err(error) => panic!("create harness namespace: {error}"),
    }
}

/// Delete every Pod this harness left behind in its own namespace.
///
/// A failed run panics before its trailing `delete`, so without this each retry
/// leaves a 4-core-limit Pod parked on a single-node cluster. Two of those and
/// the next run's Pod never schedules — which surfaces as "launcher sidecar
/// never became admitted", i.e. quota exhaustion wearing the costume of the
/// defect under test.
///
/// Safe to make unconditional: `HARNESS_NAMESPACE` is created and owned by this
/// file, and `harness_client` has already refused any non-loopback apiserver.
async fn purge_harness_pods(pods: &Api<Pod>) {
    pods.delete_collection(&DeleteParams::default(), &Default::default())
        .await
        .expect("purge Pods left by a previous harness run");
}

/// Poll until the launcher sidecar is present and started in BOTH
/// `spec.initContainers` and `status.initContainerStatuses`.
async fn await_admitted_launcher(pods: &Api<Pod>, name: &str) -> Pod {
    // Iteration-counted rather than deadline-based: `Instant::now` is a
    // workspace-disallowed method (`clippy.toml`), and the Kueue harness polls
    // the same way.
    const TICKS: usize = 90;
    const TICK: Duration = Duration::from_secs(2);
    let mut last: Option<Pod> = None;
    for _ in 0..TICKS {
        let pod = pods.get(name).await.expect("get harness pod");
        let named = locate_launcher_spec(&pod).is_ok() && locate_launcher_status(&pod).is_ok();
        let started = locate_launcher_status(&pod)
            .ok()
            .and_then(|status| status.container_id.clone())
            .is_some();
        if named && started {
            return pod;
        }
        last = Some(pod);
        tokio::time::sleep(TICK).await;
    }
    panic!(
        "launcher sidecar never became admitted and started; last status: {:?}",
        last.and_then(|pod| pod.status),
    );
}

/// The birth downsize, driven exactly as production drives it.
///
/// `PodResizeClient::resize_launcher_cpu` performs one GET / PATCH / GET and
/// nothing more — by design; the retry budget belongs to `0ppk`'s
/// `TaskRunResizeAdmissionBridge`, which re-runs the whole bootstrap on a poll
/// interval until its budget expires. So does this, for the same reason and with
/// the same idempotence: resizing to a value the launcher already holds confirms
/// on the next read.
///
/// Returns what the *first* cycle observed, which is the interesting number: it
/// is the width of the window between "the apiserver accepted the PATCH" and
/// "the kubelet actuated it", and that window is the entire reason a dispatch
/// gate exists.
struct BirthDownsize {
    /// Cycles until `status.initContainerStatuses` agreed. 1 means the first
    /// GET / PATCH / GET confirmed.
    cycles: usize,
    /// `spec.initContainers[cgroup-launcher].resources.limits.cpu`, in
    /// millicores, read fresh immediately after the first PATCH was accepted.
    spec_millis_after_first_patch: Option<u64>,
    /// The same instant's
    /// `status.initContainerStatuses[cgroup-launcher].resources.limits.cpu`.
    status_millis_after_first_patch: Option<u64>,
}

async fn confirm_birth_downsize(client: &Client, pods: &Api<Pod>, name: &str) -> BirthDownsize {
    // Iteration-counted rather than deadline-based: `Instant::now` is a
    // workspace-disallowed method (`clippy.toml`).
    const TICKS: usize = 120;
    const TICK: Duration = Duration::from_millis(500);

    let resize = PodResizeClient::new(KubePodResizeApi::new(
        client.clone(),
        HARNESS_NAMESPACE,
        "djinn-resize-harness",
    ));
    let target = CpuLimit::from_millis(BIRTH_MILLICORES);
    let mut observed = BirthDownsize {
        cycles: 0,
        spec_millis_after_first_patch: None,
        status_millis_after_first_patch: None,
    };
    let mut last = String::new();

    for tick in 0..TICKS {
        observed.cycles = tick + 1;
        let outcome = resize.resize_launcher_cpu(name, target).await;
        if tick == 0 {
            // Snapshot both halves before the retry loop can blur them. The
            // apiserver updates `spec` synchronously with an accepted PATCH; the
            // kubelet updates `status` when it has actually moved the cgroup.
            let snapshot = pods.get(name).await.expect("snapshot after first patch");
            observed.spec_millis_after_first_patch = declared_launcher_cpu_limit(&snapshot)
                .ok()
                .map(CpuLimit::millis);
            observed.status_millis_after_first_patch = locate_launcher_status(&snapshot)
                .ok()
                .and_then(|status| status.resources.as_ref())
                .and_then(|resources| resources.limits.as_ref())
                .and_then(|limits| limits.get("cpu"))
                .and_then(|quantity| CpuLimit::parse(&quantity.0).ok())
                .map(CpuLimit::millis);
        }
        match outcome {
            Ok(()) => return observed,
            Err(PodResizeError::NotConfirmed(reason)) => last = reason.to_string(),
            Err(other) => panic!("the birth downsize failed outright: {other}"),
        }
        tokio::time::sleep(TICK).await;
    }
    panic!(
        "a real kubelet never confirmed the {BIRTH_MILLICORES}m birth limit in \
         status.initContainerStatuses within {TICKS} cycles; last refusal: {last}",
    );
}

/// **AC8, live.** Render a real `resize-v2` task-run Pod, capture the ceiling the
/// apiserver actually stored, downsize to the birth limit, and confirm from
/// `status.initContainerStatuses` — against a real kubelet.
///
/// # What each mutation costs
///
/// * Swap `Patch::Strategic` for `Patch::Merge` in
///   `KubePodResizeApi::patch_resize`: the apiserver rejects it outright with
///   `spec.initContainers[0].resources.limits: Forbidden: resource limits cannot
///   be removed`, because an RFC 7386 merge replaces the whole array.
/// * Point confirmation at `status.containerStatuses`: the assertions under
///   "the wrong array" below show what it would read on this very Pod — a
///   `cgroup-launcher` entry that does not exist, beside a `worker` entry
///   holding a *coincidentally rendered* `cpu: 4`.
/// * Compare `Quantity` strings instead of millicores: the ceiling assertions
///   below show the apiserver stored the string `4` for a render of `4000m`.
#[tokio::test]
#[ignore = "requires scripts/kind/setup-resize-cluster.sh up"]
async fn live_birth_downsize_is_confirmed_by_a_real_kubelet() {
    if !live_tests_enabled() {
        return;
    }
    let client = harness_client().await;
    ensure_namespace(&client).await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), HARNESS_NAMESPACE);
    purge_harness_pods(&pods).await;

    let task_run_id = Uuid::now_v7();
    let rendered = rendered_resize_v2_pod(&task_run_id);
    let name = rendered.metadata.name.clone().expect("pod name");
    // Teardown of the Pod is unconditional; teardown of the CLUSTER belongs to
    // scripts/kind/setup-resize-cluster.sh and must be run whether this passed
    // or failed.
    let _ = pods.delete(&name, &DeleteParams::default()).await;
    pods.create(&PostParams::default(), &rendered)
        .await
        .expect("create the rendered harness pod");

    let admitted = await_admitted_launcher(&pods, &name).await;

    // ── The ceiling, and why millicores are not a stylistic choice ──────────
    //
    // The render writes `4000m` (`apply_launcher_cpu_ceiling` formats
    // `{millicores}m`). What the apiserver STORED is asserted here, off the
    // stored Pod, never off the render input.
    let raw_ceiling = locate_launcher_spec(&admitted)
        .expect("launcher spec")
        .resources
        .as_ref()
        .and_then(|resources| resources.limits.as_ref())
        .and_then(|limits| limits.get("cpu"))
        .map(|quantity| quantity.0.clone())
        .expect("a resize-v2 render declares a launcher CPU ceiling");
    assert_eq!(
        raw_ceiling, RENDERED_CEILING,
        "the apiserver canonicalises Quantity on the way in",
    );
    assert_ne!(
        raw_ceiling, "4000m",
        "AC2, live and non-vacuous: the render emitted `4000m` and the apiserver \
         stored `{RENDERED_CEILING}`. A confirmation that compared Quantity \
         STRINGS would therefore never match a whole-core target, and would \
         report `never reported 4000m; last observed Some(4)` forever",
    );
    let ceiling =
        declared_launcher_cpu_limit(&admitted).expect("the stored ceiling parses through CpuLimit");
    assert_eq!(
        ceiling.millis(),
        4000,
        "parsed to millicores, `{RENDERED_CEILING}` and `4000m` are the same number",
    );
    assert!(
        ceiling.millis() > BIRTH_MILLICORES,
        "the rendered ceiling ({ceiling}) must be above the birth limit, or the \
         downsize would be an upsize and prove nothing",
    );

    let before = locate_launcher_status(&admitted)
        .expect("launcher status")
        .clone();
    let before_init_count = admitted
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .map_or(0, Vec::len);
    let before_requests = locate_launcher_spec(&admitted)
        .expect("launcher spec")
        .resources
        .clone()
        .and_then(|resources| resources.requests);
    let before_qos = admitted
        .status
        .as_ref()
        .and_then(|status| status.qos_class.clone());

    // ── The downsize, and the acceptance/actuation window it opens ──────────
    let downsize = confirm_birth_downsize(&client, &pods, &name).await;
    eprintln!(
        "pod_resize_kind: a real kubelet confirmed {BIRTH_MILLICORES}m after \
         {} cycle(s); after the first accepted PATCH spec={:?}m status={:?}m",
        downsize.cycles,
        downsize.spec_millis_after_first_patch,
        downsize.status_millis_after_first_patch,
    );

    // AC1, live and non-vacuous: whatever the kubelet's latency was on this run,
    // the apiserver had ALREADY stored the new spec by the time the first PATCH
    // returned. The PATCH response and the spec are therefore both available to
    // a caller that has no confirmation at all — which is precisely why neither
    // may be the confirmation source.
    assert_eq!(
        downsize.spec_millis_after_first_patch,
        Some(BIRTH_MILLICORES),
        "an accepted PATCH updates `spec` synchronously; if this is not already \
         250m the PATCH was not accepted and the rest of this test measures \
         nothing",
    );

    let after = pods.get(&name).await.expect("re-read the resized pod");
    confirm_launcher_cpu(&after, CpuLimit::from_millis(BIRTH_MILLICORES))
        .expect("a fresh read still confirms the birth limit");

    // ── The wrong array, on this very Pod ───────────────────────────────────
    //
    // AC1's non-vacuity, stated against live data rather than a fixture: point
    // confirmation at `status.containerStatuses` and there is no launcher entry
    // to find, while the entry that IS there reports a limit that is not the
    // birth limit. Reading the wrong array does not read nothing; it reads a
    // number, and that number is wrong.
    let regular_statuses = after
        .status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .expect("the live Pod publishes regular container statuses");
    assert!(
        !regular_statuses
            .iter()
            .any(|status| status.name == LAUNCHER_CONTAINER_NAME),
        "the launcher is a NATIVE SIDECAR: a `{LAUNCHER_CONTAINER_NAME}` entry in \
         status.containerStatuses is a documented failure mode, not a fallback",
    );
    let worker_limit = regular_statuses
        .iter()
        .find(|status| status.name == "worker")
        .and_then(|status| status.resources.as_ref())
        .and_then(|resources| resources.limits.as_ref())
        .and_then(|limits| limits.get("cpu"))
        .map(|quantity| quantity.0.clone())
        .expect("the worker container reports a cpu limit");
    assert_ne!(
        CpuLimit::parse(&worker_limit)
            .expect("the worker limit parses")
            .millis(),
        BIRTH_MILLICORES,
        "status.containerStatuses[worker] reports {worker_limit}, never the birth \
         limit; a confirmation that read this array would refuse forever, and a \
         render that happened to size the worker at 250m would make it confirm a \
         resize that never happened",
    );

    let after_status = locate_launcher_status(&after).expect("launcher status after");
    assert_eq!(
        after_status.container_id, before.container_id,
        "an in-place resize must not restart the launcher: the container ID moved",
    );
    assert_eq!(
        after_status.restart_count, before.restart_count,
        "an in-place resize must not restart the launcher: restartCount moved",
    );
    assert_eq!(
        after
            .spec
            .as_ref()
            .and_then(|spec| spec.init_containers.as_ref())
            .map_or(0, Vec::len),
        before_init_count,
        "a strategic merge patch keeps every other init container; an RFC 7386 \
         merge patch replaces the whole array and drops them",
    );
    assert_eq!(
        locate_launcher_spec(&after)
            .expect("launcher spec after")
            .resources
            .clone()
            .and_then(|resources| resources.requests),
        before_requests,
        "a limits-only resize must leave requests byte-identical: moving them \
         would change scheduling and Kueue accounting",
    );
    assert_eq!(
        after.status.as_ref().and_then(|s| s.qos_class.clone()),
        before_qos,
        "the QoS class must be byte-identical across the resize",
    );
    assert_eq!(
        locate_launcher_spec(&after)
            .expect("launcher spec after")
            .name,
        LAUNCHER_CONTAINER_NAME,
        "the resize target is still the launcher and only the launcher",
    );

    // ── AC2's non-vacuity, on the CONFIRMATION path ─────────────────────────
    //
    // 250m survives a round trip as the string `250m`, so the birth downsize
    // alone cannot show what comparing `Quantity` strings would cost. A
    // whole-core target can: the apiserver canonicalises it in BOTH `spec` and
    // `status`, so `raw == "1000m"` never matches and a string-comparing
    // confirmation waits out its budget reporting "never reported 1000m; last
    // observed 1". The lease lift this stack exists to perform moves the
    // launcher to whole cores, so this is the operating case, not a corner one.
    let whole_core = CpuLimit::from_millis(1000);
    confirm_whole_core(&client, &pods, &name, whole_core).await;
    let lifted = pods.get(&name).await.expect("re-read the lifted pod");
    let raw_lifted = locate_launcher_status(&lifted)
        .expect("launcher status after the lift")
        .resources
        .as_ref()
        .and_then(|resources| resources.limits.as_ref())
        .and_then(|limits| limits.get("cpu"))
        .map(|quantity| quantity.0.clone())
        .expect("the lifted launcher reports a cpu limit");
    assert_eq!(
        raw_lifted, "1",
        "the kubelet reports the CANONICALISED quantity, not the one we sent",
    );
    assert_ne!(
        raw_lifted,
        whole_core.as_quantity(),
        "AC2, live and non-vacuous: this client emits `{}` and the apiserver \
         reports `{raw_lifted}`. String equality is not merely fragile here, it \
         is never true — which is why `confirm_launcher_cpu` parses both sides",
        whole_core.as_quantity(),
    );
    assert_eq!(
        CpuLimit::parse(&raw_lifted)
            .expect("the canonicalised quantity parses")
            .millis(),
        whole_core.millis(),
        "parsed to millicores the two agree, and that is the only comparison \
         that survives canonicalisation",
    );

    pods.delete(&name, &DeleteParams::default())
        .await
        .expect("delete the harness pod");
}

/// Drive one further confirmed resize, with the same production client and the
/// same retry shape as [`confirm_birth_downsize`].
async fn confirm_whole_core(client: &Client, pods: &Api<Pod>, name: &str, target: CpuLimit) {
    const TICKS: usize = 120;
    const TICK: Duration = Duration::from_millis(500);

    let resize = PodResizeClient::new(KubePodResizeApi::new(
        client.clone(),
        HARNESS_NAMESPACE,
        "djinn-resize-harness",
    ));
    let mut last = String::new();
    for _ in 0..TICKS {
        match resize.resize_launcher_cpu(name, target).await {
            Ok(()) => {
                // Belt and braces: a fresh read outside the client agrees.
                let fresh = pods.get(name).await.expect("fresh read after the lift");
                confirm_launcher_cpu(&fresh, target).expect("a fresh read still confirms");
                return;
            }
            Err(PodResizeError::NotConfirmed(reason)) => last = reason.to_string(),
            Err(other) => panic!("the whole-core lift failed outright: {other}"),
        }
        tokio::time::sleep(TICK).await;
    }
    panic!("a real kubelet never confirmed {target}; last refusal: {last}");
}

// ── The production surface, live ──────────────────────────────────────────

/// Namespace for the surface test. Distinct from [`HARNESS_NAMESPACE`] because
/// both live tests purge every object in the namespace they own and cargo runs
/// them on separate threads: sharing one namespace would make each test's
/// cleanup the other's flake.
const SURFACE_NAMESPACE: &str = "djinn-resize-harness-surface";

/// **The type the server actually holds, against a real apiserver.**
///
/// `TaskRunPodResizeSurface` is what `TaskRunResizeAdmissionBridge` resolves and
/// drives; `server/tests/task_run_resize_dispatch_seam.rs` proves the bridge and
/// the `DispatchGate` are on the production dispatch path, but it substitutes
/// this surface for a fixture. So until now none of its three operations had
/// ever spoken to an apiserver — and the one of them this file DID exercise,
/// `resize_launcher_cpu`, turned out to be broken at the transport in a way no
/// hermetic test could see. Its two siblings deserve the same treatment rather
/// than the benefit of the doubt.
///
/// Driven from a real Job, not a hand-built Pod: `observe_launcher` resolves by
/// the `djinn.app/task-run-id` LABEL and `uid_fenced_delete` reads the Job's own
/// UID, so a Pod created directly would exercise neither.
#[tokio::test]
#[ignore = "requires scripts/kind/setup-resize-cluster.sh up"]
async fn live_production_resize_surface_observes_and_uid_fences_a_real_job() {
    if !live_tests_enabled() {
        return;
    }
    let client = harness_client().await;
    ensure_named_namespace(&client, SURFACE_NAMESPACE).await;
    let jobs: Api<Job> = Api::namespaced(client.clone(), SURFACE_NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), SURFACE_NAMESPACE);
    purge_harness_jobs(&jobs).await;
    purge_harness_pods(&pods).await;

    let task_run_uuid = Uuid::now_v7();
    let mut job = rendered_resize_v2_job(&task_run_uuid);
    job.metadata.namespace = Some(SURFACE_NAMESPACE.to_owned());
    {
        let template = job
            .spec
            .as_mut()
            .and_then(|spec| spec.template.spec.as_mut())
            .expect("pod template spec");
        // `make_pullable` takes a Pod; wrap the template in one so the Job's
        // template gets exactly the same treatment the standalone Pod gets.
        let mut carrier = Pod {
            spec: Some(template.clone()),
            ..Default::default()
        };
        make_pullable(&mut carrier);
        *template = carrier.spec.expect("carrier spec");
    }
    let created = jobs
        .create(&PostParams::default(), &job)
        .await
        .expect("create the rendered harness job");
    let job_uid = created.metadata.uid.clone().expect("the Job has a uid");

    let surface =
        TaskRunPodResizeSurface::new(client.clone(), SURFACE_NAMESPACE, "djinn-resize-harness");
    let task_run_id = task_run_uuid.to_string();

    // 1. `observe_launcher` — the label lookup, live.
    let observed = await_observed_launcher(&surface, &task_run_id).await;
    assert_eq!(
        observed.launcher_container_name, LAUNCHER_CONTAINER_NAME,
        "the observed sidecar is the launcher",
    );
    assert_eq!(
        observed.namespace, SURFACE_NAMESPACE,
        "the surface reports the namespace it is bound to",
    );
    assert_eq!(
        observed.observed_protocol.as_deref(),
        Some("resize-v2"),
        "the protocol is read off the STORED spec, which is where a live \
         mismatch between render and cluster would show",
    );
    assert_eq!(
        observed.admitted_cpu_millicores,
        Some(4000),
        "the admitted ceiling is the canonicalised `{RENDERED_CEILING}` parsed to \
         millicores, not the `4000m` the render emitted",
    );
    let pod_uid = observed.pod_uid.clone();
    assert!(!pod_uid.is_empty(), "the fence needs a real Pod UID");
    assert_ne!(
        pod_uid, job_uid,
        "the fence is the POD's uid, never the Job's — the Job's uid survives a \
         Pod recreate and would fence nothing",
    );

    // 2. `resize_launcher_cpu` — the same production call the bridge makes,
    //    reached through the surface rather than the raw client.
    let mut cycles = 0;
    loop {
        cycles += 1;
        assert!(cycles <= 120, "the surface never confirmed the birth limit");
        match surface
            .resize_launcher_cpu(&observed.pod_name, BIRTH_MILLICORES)
            .await
        {
            Ok(()) => break,
            Err(PodResizeError::NotConfirmed(_)) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(other) => panic!("the surface's resize failed outright: {other}"),
        }
    }
    let confirmed = pods
        .get(&observed.pod_name)
        .await
        .expect("re-read the resized pod");
    confirm_launcher_cpu(&confirmed, CpuLimit::from_millis(BIRTH_MILLICORES))
        .expect("the surface's resize is confirmed from status.initContainerStatuses");

    // 3. `uid_fenced_delete` — refuses a wrong UID, accepts the observed one.
    let wrong = surface
        .uid_fenced_delete(&task_run_id, "00000000-0000-0000-0000-000000000000")
        .await;
    assert!(
        wrong.is_err(),
        "a delete fenced to a UID this task run never had must refuse; accepting \
         it would mean the fence is decorative and a stale watchdog could destroy \
         a Pod belonging to a later attempt",
    );
    assert!(
        pods.get_opt(&observed.pod_name)
            .await
            .expect("re-read after the refused delete")
            .is_some(),
        "the refused delete must not have destroyed anything",
    );
    surface
        .uid_fenced_delete(&task_run_id, &pod_uid)
        .await
        .expect("a delete fenced to the OBSERVED uid is accepted");

    purge_harness_jobs(&jobs).await;
    purge_harness_pods(&pods).await;
}

async fn ensure_named_namespace(client: &Client, name: &str) {
    use k8s_openapi::api::core::v1::Namespace;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let namespace = Namespace {
        metadata: kube::api::ObjectMeta {
            name: Some(name.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    };
    match namespaces.create(&PostParams::default(), &namespace).await {
        Ok(_) => {}
        Err(kube::Error::Api(response)) if response.code == 409 => {}
        Err(error) => panic!("create harness namespace {name}: {error}"),
    }
}

async fn purge_harness_jobs(jobs: &Api<Job>) {
    jobs.delete_collection(&DeleteParams::background(), &Default::default())
        .await
        .expect("purge Jobs left by a previous harness run");
}

/// Poll until the surface reports a launcher the kubelet has actually started.
async fn await_observed_launcher(
    surface: &TaskRunPodResizeSurface,
    task_run_id: &str,
) -> ObservedLauncherSidecar {
    const TICKS: usize = 90;
    let mut last = String::from("no observation yet");
    for _ in 0..TICKS {
        match surface.observe_launcher(task_run_id).await {
            Ok(Some(observed)) if observed.launcher_container_id.is_some() => return observed,
            Ok(Some(_)) => last = "observed, but the kubelet has not started it".to_owned(),
            Ok(None) => last = "no Pod carries this task-run label yet".to_owned(),
            Err(error) => last = error.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("the production surface never observed a started launcher: {last}");
}
