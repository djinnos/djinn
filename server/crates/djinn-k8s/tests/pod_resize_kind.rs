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
use std::time::{Duration, Instant};

use djinn_k8s::KubernetesConfig;
use djinn_k8s::launcher::LAUNCHER_CONTAINER_NAME;
use djinn_k8s::pod_resize::{
    CpuLimit, KubePodResizeApi, PodResizeClient, confirm_launcher_cpu, declared_launcher_cpu_limit,
    locate_launcher_spec, locate_launcher_status,
};
use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, PostParams};
use kube::{Api, Client};
use uuid::Uuid;

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
        assert!(is_local_apiserver(benign), "{benign} is a local kind server");
    }
}

#[test]
fn guard_the_setup_script_exists_and_reserves_the_same_names() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/kind/setup-resize-cluster.sh")
        .canonicalize()
        .expect("scripts/kind/setup-resize-cluster.sh must exist; this harness cannot \
                 create its own cluster and a missing script means the live half is \
                 unrunnable rather than merely skipped");
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

/// The production render, with only its image references made pullable.
fn rendered_resize_v2_pod(task_run_id: &Uuid) -> Pod {
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

/// Poll until the launcher sidecar is present and started in BOTH
/// `spec.initContainers` and `status.initContainerStatuses`.
async fn await_admitted_launcher(pods: &Api<Pod>, name: &str) -> Pod {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let pod = pods.get(name).await.expect("get harness pod");
        let named = locate_launcher_spec(&pod).is_ok() && locate_launcher_status(&pod).is_ok();
        let started = locate_launcher_status(&pod)
            .ok()
            .and_then(|status| status.container_id.clone())
            .is_some();
        if named && started {
            return pod;
        }
        assert!(
            Instant::now() < deadline,
            "launcher sidecar never became admitted and started; last status: {:?}",
            pod.status,
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// **AC8.** Render a real `resize-v2` task-run Pod, capture the ceiling the
/// apiserver actually stored, downsize to the birth limit, and confirm from
/// `status.initContainerStatuses`.
///
/// Swap `Patch::Strategic` for `Patch::Merge` in
/// `KubePodResizeApi::patch_resize` and the init-container count assertion
/// fails: an RFC 7386 merge replaces the whole array and drops every other init
/// container.
#[tokio::test]
#[ignore = "requires scripts/kind/setup-resize-cluster.sh up"]
async fn live_birth_downsize_is_confirmed_by_a_real_kubelet() {
    if !live_tests_enabled() {
        return;
    }
    let client = harness_client().await;
    ensure_namespace(&client).await;
    let pods: Api<Pod> = Api::namespaced(client.clone(), HARNESS_NAMESPACE);

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

    // The ceiling is read off the STORED Pod, never off the render input.
    let ceiling = declared_launcher_cpu_limit(&admitted)
        .expect("a resize-v2 render declares a launcher CPU ceiling");
    assert!(
        ceiling.millis() > BIRTH_MILLICORES,
        "the fixture ceiling ({ceiling}) must be above the birth limit, or the \
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

    PodResizeClient::new(KubePodResizeApi::new(
        client.clone(),
        HARNESS_NAMESPACE,
        "djinn-resize-harness",
    ))
    .resize_launcher_cpu(&name, CpuLimit::from_millis(BIRTH_MILLICORES))
    .await
    .expect("the birth downsize must confirm from status.initContainerStatuses");

    let after = pods.get(&name).await.expect("re-read the resized pod");
    confirm_launcher_cpu(&after, CpuLimit::from_millis(BIRTH_MILLICORES))
        .expect("a fresh read still confirms the birth limit");

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
        after.spec
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
        locate_launcher_spec(&after).expect("launcher spec after").name,
        LAUNCHER_CONTAINER_NAME,
        "the resize target is still the launcher and only the launcher",
    );

    pods.delete(&name, &DeleteParams::default())
        .await
        .expect("delete the harness pod");
}
