#![allow(clippy::print_stderr)]
// Shared live-cluster helpers for the two `kueue_disruption_*` test targets.
//
// `#![allow(dead_code)]` is the standard `tests/<name>/mod.rs` idiom and is
// load-bearing here rather than lazy: this module is compiled SEPARATELY into
// each test binary, so every helper the other binary uses is genuinely unused
// in this one. Without it `RUSTFLAGS: -D warnings` fails the lane for helpers
// that are exercised — just from the sibling target.
#![allow(dead_code)]
//! Live-cluster helpers shared by `kueue_disruption_conformance` (AC1/AC2) and
//! `kueue_disruption_governor` (AC3/AC5).
//!
//! Split out of one file because the pair exceeded `scripts/check-file-size.sh`
//! (MAX_LINES 1500 / MAX_BYTES 51200) and the `djinn:allow-oversize` marker is
//! for source that genuinely cannot be divided — this could be.

use std::collections::BTreeSet;
use std::env;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use djinn_k8s::KubernetesConfig;
use djinn_k8s::job::{LABEL_TASK_RUN_ID, build_task_run_job};
use djinn_k8s::launcher::CgroupLauncherMode;
use k8s_openapi::api::batch::v1::Job;
use serde_json::Value;

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

/// DELIBERATELY NOT the script's default (`djinn-kueue-harness`, which is what
/// `tests/kueue_cluster_harness.rs` and `fbiy-B1` target). `down` deletes the
/// cluster it is given, so a shared name is a shared deletion.
pub const HARNESS_CLUSTER: &str = "djinn-kueue-b2";
/// kind names its context `kind-<cluster>`. Derived for the same reason the
/// script derives it: the CURRENT context is never consulted, because every
/// context in a Djinn developer's kubeconfig is a live EKS cluster.
pub const HARNESS_CONTEXT: &str = "kind-djinn-kueue-b2";
pub const HARNESS_REGISTRY: &str = "djinn-kueue-b2-registry";
pub const HARNESS_REGISTRY_PORT: &str = "5053";

pub const NAMESPACE: &str = "djinn";
/// `<djinn.fullname>-kueue` for release `djinn`, per
/// `deploy/helm/djinn/templates/kueue-topology.yaml`.
pub const CLUSTER_QUEUE: &str = "djinn-kueue";

pub const SETUP_SCRIPT: &str = "scripts/kind/setup-kueue-cluster.sh";
pub const VALUES_FIXTURE: &str = "deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml";

/// Exit code owned by `scripts/kind/setup-kueue-cluster.sh`: a refused context
/// or a reserved name.
pub const EXIT_REFUSED_TARGET: i32 = 3;

/// A workload image that exists, runs as uid 1000, and does nothing.
///
/// The real worker binary is not in play: this file measures Pod IDENTITY and
/// Job lifecycle, both of which are properties of the objects rather than of
/// what runs inside them. The Job is otherwise the real renderer's output, byte
/// for byte, including `backoffLimit: 0` — which is exactly the field that turns
/// out to decide finding 2 above.
pub const WORKLOAD_IMAGE: &str = "busybox:1.36";

pub fn repo_root() -> PathBuf {
    // <repo>/server/crates/djinn-k8s
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives three levels below the repository root")
        .to_path_buf()
}

pub fn run_setup_script(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join(SETUP_SCRIPT))
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("setup-kueue-cluster.sh is executable")
}

pub fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the script exits rather than dying on a signal")
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn yaml_at(relative: &str) -> serde_yaml::Value {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {relative}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {relative}: {e}"))
}

// ===========================================================================
// Live half — #[ignore] + DJINN_TEST_KUEUE_CLUSTER=1
// ===========================================================================

/// Returns `false` when the live half is disabled; callers `return` early.
pub fn live_tests_enabled() -> bool {
    if env::var("DJINN_TEST_KUEUE_CLUSTER").is_err() {
        eprintln!("kueue_disruption_conformance: DJINN_TEST_KUEUE_CLUSTER not set — skipping");
        return false;
    }
    for tool in ["kubectl", "docker", "kind"] {
        if !which(tool) {
            eprintln!("kueue_disruption_conformance: {tool} not found on PATH — skipping");
            return false;
        }
    }
    true
}

pub fn which(bin: &str) -> bool {
    env::var("PATH").is_ok_and(|path| {
        path.split(':')
            .any(|dir| Path::new(dir).join(bin).is_file())
    })
}

/// rustls 0.23 refuses to pick a provider when `workspace-hack` has unified both
/// `ring` and `aws-lc-rs` into the build, and `kube::Client::try_from` panics on
/// construction — for an `http://` URL as readily as an `https://` one, so there
/// is no TLS-free route around it. Installing `ring` here is process-global,
/// idempotent (the `Err` arm means somebody already installed one), and scoped
/// to this test binary.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The context every live call is pinned to, after TWO independent refusals of
/// anything else — the same discipline `tests/kueue_cluster_harness.rs` uses.
///
/// Guard 1 is the name. Guard 2 is the resolved API-server URL, which catches
/// what guard 1 cannot: a kubeconfig entry NAMED `kind-djinn-kueue-b2` that
/// points somewhere else. kind always serves on loopback; no managed control
/// plane does, and all three contexts in a Djinn developer's kubeconfig are EKS.
pub fn harness_context() -> String {
    let requested =
        env::var("DJINN_TEST_KUEUE_B2_CONTEXT").unwrap_or_else(|_| HARNESS_CONTEXT.into());
    assert_eq!(
        requested, HARNESS_CONTEXT,
        "this harness only ever targets the context of the cluster it creates and deletes",
    );
    let server = kubectl_raw(
        &requested,
        &[
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ],
    );
    assert!(
        server.starts_with("https://127.0.0.1:")
            || server.starts_with("https://localhost:")
            || server.starts_with("https://[::1]:"),
        "refusing to run against {server}: context {requested} does not resolve to a local kind \
         API server, so it is not a cluster this harness created",
    );
    requested
}

/// Run `kubectl --context <ctx> ...`, failing the test on a nonzero exit.
///
/// A nonzero status is never read as an empty result: `get workloads` against a
/// cluster with no Kueue CRDs exits nonzero, and treating that as "zero
/// Workloads" would make every negative assertion below pass against a cluster
/// with no Kueue at all.
pub fn kubectl_raw(context: &str, args: &[&str]) -> String {
    let output = Command::new("kubectl")
        .arg("--context")
        .arg(context)
        .args(args)
        .output()
        .expect("kubectl is on PATH");
    assert!(
        output.status.success(),
        "kubectl --context {context} {args:?} failed: {}",
        stderr(&output),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn kubectl_json(context: &str, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.extend_from_slice(&["-o", "json"]);
    serde_json::from_str(&kubectl_raw(context, &args)).expect("kubectl -o json emits JSON")
}

pub fn kubectl_apply(context: &str, namespace: &str, object: &Value) {
    let mut child = Command::new("kubectl")
        .args(["--context", context, "-n", namespace, "apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kubectl is on PATH");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(
            serde_json::to_string(object)
                .expect("object serializes")
                .as_bytes(),
        )
        .expect("write manifest to kubectl");
    let output = child.wait_with_output().expect("kubectl apply completes");
    assert!(
        output.status.success(),
        "kubectl apply into {namespace} failed: {}",
        stderr(&output),
    );
}

pub fn delete_job(context: &str, job_name: &str) {
    let _ = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            NAMESPACE,
            "delete",
            "job",
            job_name,
            "--ignore-not-found",
            "--wait=false",
        ])
        .output();
}

/// Delete every task-run Job this file may have left behind, so a test that
/// starts finds an empty quota rather than the previous test's occupancy.
pub fn clear_task_run_jobs(context: &str) {
    let _ = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            NAMESPACE,
            "delete",
            "job",
            "-l",
            "djinn.app/component=task-run-worker",
            "--ignore-not-found",
            "--wait=true",
            "--timeout=90s",
        ])
        .output();
}

// ---------------------------------------------------------------------------
// The live cluster's own object names
// ---------------------------------------------------------------------------

/// The ServiceAccount and PersistentVolumeClaims the CHART actually created.
///
/// `KubernetesConfig::for_testing()` carries the unprefixed development names
/// (`djinn-taskrun`, `djinn-mirror`), while the chart renders `djinn.fullname`
/// ones (`djinn-djinn-taskrun`, `djinn-mirrors`). A Pod referencing a
/// nonexistent ServiceAccount or PVC never leaves `Pending`, and every timing
/// measurement below would then be a measurement of that mistake. So they are
/// READ off the cluster rather than assumed.
pub struct LiveNames {
    service_account: String,
    mirror_pvc: String,
    cache_pvc: String,
}

pub fn live_names(context: &str) -> LiveNames {
    let accounts = kubectl_json(context, &["-n", NAMESPACE, "get", "serviceaccounts"]);
    let service_account = accounts["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter_map(|item| item["metadata"]["name"].as_str())
        .find(|name| name.ends_with("-taskrun"))
        .expect("the chart installs a task-run ServiceAccount")
        .to_owned();

    let claims = kubectl_json(context, &["-n", NAMESPACE, "get", "persistentvolumeclaims"]);
    let named = |suffix: &str| -> String {
        claims["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a PVC ending in {suffix}"))
            .to_owned()
    };
    LiveNames {
        service_account,
        mirror_pvc: named("-mirrors"),
        cache_pvc: named("-cache"),
    }
}

/// The armed `KubernetesConfig` the live Jobs render with.
///
/// `cgroup_launcher_mode: Disabled` + `task_run_cgroup_writable_enabled: false`
/// mirror the values fixture exactly: `build_task_run_job` PANICS if a required
/// launcher is rendered without the RuntimeClass, and this harness deliberately
/// does not install it (that is `fbiy-C1`'s job).
///
/// The requests are lowered from the production defaults (1 cpu / 2Gi) so that
/// `kueue.buildPods` stays the BINDING constraint on a single kind node. If cpu
/// or memory bound admission instead, "two Workloads admitted" would be a
/// statement about the node rather than about the quota under test.
pub fn armed_harness_config(image: &str) -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        kueue_local_queue_prefix: "djinn".into(),
        cgroup_launcher_mode: CgroupLauncherMode::Disabled,
        task_run_cgroup_writable_enabled: false,
        image: image.into(),
        image_pull_policy: "IfNotPresent".into(),
        cpu_request: "100m".into(),
        cpu_limit: "500m".into(),
        memory_request: "64Mi".into(),
        memory_limit: "256Mi".into(),
        ..KubernetesConfig::for_testing()
    }
}

pub fn config_for(context: &str, image: &str) -> KubernetesConfig {
    let names = live_names(context);
    KubernetesConfig {
        service_account: names.service_account,
        mirror_pvc: names.mirror_pvc,
        cache_pvc: names.cache_pvc,
        ..armed_harness_config(image)
    }
}

/// A task-run Job straight out of the real renderer, plus its task-run id.
pub fn rendered_task_run_job(config: &KubernetesConfig) -> (Job, String) {
    let task_run_id = uuid::Uuid::now_v7();
    let job = build_task_run_job(
        config,
        &task_run_id,
        "harness-project",
        &format!("djinn-taskrun-{task_run_id}"),
        &config.image,
        &[],
        None,
        false,
        None,
    );
    (job, task_run_id.to_string())
}

pub fn job_as_json(job: &Job) -> Value {
    let mut value = serde_json::to_value(job).expect("Job serializes");
    // `k8s-openapi` omits apiVersion/kind on the typed struct; the API server
    // needs both.
    value["apiVersion"] = Value::String("batch/v1".into());
    value["kind"] = Value::String("Job".into());
    value
}

/// Replace the worker entrypoint with a sleep.
///
/// The ONLY mutation this file makes to the renderer's output, and it is
/// confined to `command`. Everything that decides the behaviour under test —
/// `suspend`, `backoffLimit: 0`, the queue-name label, the task-run-id label the
/// watch selects on, `restartPolicy: Never`, the resource requests — is the
/// renderer's. A real worker image would exit non-zero against this cluster's
/// absent server within seconds, and every Pod-identity measurement below would
/// become a measurement of that crash instead.
pub fn sleep_instead_of_the_worker(manifest: &mut Value) {
    let containers = manifest
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
        .expect("the rendered Job has containers");
    for container in containers {
        container["command"] = serde_json::json!(["sh", "-c", "sleep 100000"]);
        container["args"] = Value::Null;
    }
}

// ---------------------------------------------------------------------------
// Live finding 1: repairing the topology the chart cannot admit through
// ---------------------------------------------------------------------------

/// What [`repair_cluster_queue_for_admission`] had to change.
#[derive(Debug, Default)]
pub struct RepairReport {
    namespace_selector_was_absent: bool,
    uncovered_resources: Vec<String>,
}

impl RepairReport {
    pub fn needed(&self) -> bool {
        self.namespace_selector_was_absent || !self.uncovered_resources.is_empty()
    }
}

/// Make the chart's ClusterQueue capable of admitting the Job the chart's own
/// renderer produces, and report what had to be changed.
///
/// Both changes are live finding 1. Neither is a preference:
///
/// * `namespaceSelector: {}` — Kueue's default of `null` matches NO namespace.
/// * `cpu` and `memory` added to `coveredResources` at quotas far above what
///   two Pods request, so `pods` REMAINS the binding constraint. Asserted right
///   after, because a repair that also relaxed the pods bound would silently
///   destroy the very quota AC3 measures `K` against.
///
/// Idempotent, and a no-op the day the chart renders both itself.
pub fn repair_cluster_queue_for_admission(context: &str) -> RepairReport {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let mut report = RepairReport {
        namespace_selector_was_absent: queue["spec"]["namespaceSelector"].is_null(),
        ..RepairReport::default()
    };
    let covered: BTreeSet<&str> = queue["spec"]["resourceGroups"][0]["coveredResources"]
        .as_array()
        .expect("the ClusterQueue covers resources")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for required in ["cpu", "memory"] {
        if !covered.contains(required) {
            report.uncovered_resources.push(required.to_owned());
        }
    }

    let pods_quota = pods_nominal_quota(context);
    if report.needed() {
        eprintln!(
            "LIVE FINDING (fbiy-B2): the chart's armed ClusterQueue cannot admit a Job its own \
             renderer produces. namespaceSelector absent: {} (Kueue treats null as 'no namespace \
             matches'). Requested-but-uncovered resources: {:?}. Repairing the DISPOSABLE cluster \
             so the containment underneath can be measured at all; the chart fix belongs to \
             deploy/helm/djinn/templates/kueue-topology.yaml.",
            report.namespace_selector_was_absent, report.uncovered_resources,
        );
        let patch = serde_json::json!({
            "spec": {
                "namespaceSelector": {},
                "resourceGroups": [{
                    "coveredResources": ["pods", "cpu", "memory"],
                    "flavors": [{
                        "name": CLUSTER_QUEUE,
                        "resources": [
                            { "name": "pods", "nominalQuota": pods_quota.to_string() },
                            { "name": "cpu", "nominalQuota": "8" },
                            { "name": "memory", "nominalQuota": "8Gi" },
                        ],
                    }],
                }],
            }
        });
        let output = Command::new("kubectl")
            .args([
                "--context",
                context,
                "patch",
                "clusterqueues.kueue.x-k8s.io",
                CLUSTER_QUEUE,
                "--type=merge",
                "-p",
                &serde_json::to_string(&patch).expect("patch serializes"),
            ])
            .output()
            .expect("kubectl is on PATH");
        assert!(
            output.status.success(),
            "repairing the ClusterQueue failed: {}",
            stderr(&output),
        );
    }

    let expected_quota = yaml_at(VALUES_FIXTURE)["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number");
    assert_eq!(
        pods_nominal_quota(context),
        expected_quota,
        "the repair must not move the `pods` nominalQuota: it is the bound M that AC3's cap K is \
         asserted to be strictly below, and relaxing it would make that comparison meaningless",
    );
    report
}

pub fn pods_nominal_quota(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let field = queue["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
        .as_array()
        .expect("the flavor covers resources")
        .iter()
        .find(|resource| resource["name"] == "pods")
        .map(|resource| resource["nominalQuota"].clone())
        .expect("the ClusterQueue bounds the pods resource");
    // `nominalQuota` is a `resource.Quantity`: the API server round-trips it as
    // a STRING even though the chart writes a bare number.
    field
        .as_u64()
        .or_else(|| field.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("pods nominalQuota is not an integer quantity: {field}"))
}

// ---------------------------------------------------------------------------
// Live observation helpers
// ---------------------------------------------------------------------------

pub const TICK: Duration = Duration::from_millis(500);
/// Ticks a POSITIVE observation waits before concluding something will never
/// happen: 240 x 500ms = 120s. Generous because a kind node pulling nothing
/// still has to bind two `local-path` PVCs.
pub const AWAIT_TICKS: usize = 240;

/// Every Pod carrying this run's task-run-id label, as `(name, uid, phase)`.
///
/// The same label selector `watch_infra_death` uses, deliberately: an
/// observation made through a different selector would not be evidence about
/// what the watch can see.
pub fn pods_of(context: &str, task_run_id: &str) -> Vec<(String, String, String)> {
    let list = kubectl_json(
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "pods",
            "-l",
            &format!("{LABEL_TASK_RUN_ID}={task_run_id}"),
        ],
    );
    list["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .map(|pod| {
            (
                pod["metadata"]["name"].as_str().unwrap_or("").to_owned(),
                pod["metadata"]["uid"].as_str().unwrap_or("").to_owned(),
                pod["status"]["phase"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect()
}

/// Every live task-run worker Pod in the namespace, regardless of run.
///
/// This is the population AC1 asks to be RECORDED: the maximum number of Pods
/// alive at once across the disruption, which is the thing a quota is supposed
/// to bound.
pub fn live_task_run_pods(context: &str) -> Vec<String> {
    let list = kubectl_json(
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "pods",
            "-l",
            "djinn.app/component=task-run-worker",
        ],
    );
    list["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter(|pod| {
            // A Pod with a deletionTimestamp is on its way out but still holds
            // its slot until the kubelet acknowledges; counting it is the
            // conservative choice for a MAXIMUM.
            !matches!(
                pod["status"]["phase"].as_str(),
                Some("Succeeded") | Some("Failed")
            )
        })
        .filter_map(|pod| pod["metadata"]["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

/// Poll until this run has a `Running` Pod, returning `(name, uid)`.
pub fn await_running_pod(context: &str, task_run_id: &str) -> (String, String) {
    for _ in 0..AWAIT_TICKS {
        if let Some((name, uid, _)) = pods_of(context, task_run_id)
            .into_iter()
            .find(|(_, uid, phase)| !uid.is_empty() && phase == "Running")
        {
            return (name, uid);
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "no Running Pod appeared for task-run {task_run_id}; workloads: {}",
        workload_summary(context),
    );
}

/// A one-line summary of every Workload, for failure messages. A test that
/// timed out waiting for admission must say WHY Kueue refused, or the next
/// person re-derives live finding 1 from scratch.
pub fn workload_summary(context: &str) -> String {
    let list = kubectl_json(
        context,
        &["-n", NAMESPACE, "get", "workloads.kueue.x-k8s.io"],
    );
    list["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .map(|workload| {
            let conditions = workload["status"]["conditions"]
                .as_array()
                .map(|conditions| {
                    conditions
                        .iter()
                        .map(|condition| {
                            format!(
                                "{}={} ({})",
                                condition["type"].as_str().unwrap_or("?"),
                                condition["status"].as_str().unwrap_or("?"),
                                condition["message"].as_str().unwrap_or(""),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            format!(
                "{} [{conditions}]",
                workload["metadata"]["name"].as_str().unwrap_or("?"),
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Whether the named Workload carries `type == kind` with status `True`.
pub fn workload_condition(context: &str, job_name: &str, kind: &str) -> bool {
    let list = kubectl_json(
        context,
        &["-n", NAMESPACE, "get", "workloads.kueue.x-k8s.io"],
    );
    list["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter(|workload| {
            workload["metadata"]["ownerReferences"]
                .as_array()
                .is_some_and(|owners| {
                    owners
                        .iter()
                        .any(|owner| owner["kind"] == "Job" && owner["name"] == job_name)
                })
        })
        .any(|workload| {
            workload["status"]["conditions"]
                .as_array()
                .is_some_and(|conditions| {
                    conditions
                        .iter()
                        .any(|condition| condition["type"] == kind && condition["status"] == "True")
                })
        })
}

/// The ClusterQueue's currently reserved `pods` usage.
pub fn pods_usage(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    queue["status"]["flavorsUsage"][0]["resources"]
        .as_array()
        .map(|resources| {
            resources
                .iter()
                .find(|resource| resource["name"] == "pods")
                .and_then(|resource| resource["total"].as_str())
                .and_then(|total| total.parse().ok())
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

pub fn admitted_workloads(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    queue["status"]["admittedWorkloads"].as_u64().unwrap_or(0)
}

pub fn job_exists(context: &str, job_name: &str) -> bool {
    let output = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            NAMESPACE,
            "get",
            "job",
            job_name,
            "-o",
            "name",
        ])
        .output()
        .expect("kubectl is on PATH");
    output.status.success()
}

/// Make the node able to run [`WORKLOAD_IMAGE`] without reaching a registry.
///
/// `kind load` rather than a push to the harness registry: the registry mirror
/// is `fbiy-B0`'s to exercise, and a pull failure here would surface as "the Pod
/// never ran" — indistinguishable from the admission defect this file is
/// measuring.
pub fn ensure_workload_image_on_the_node() {
    if !Command::new("docker")
        .args(["image", "inspect", WORKLOAD_IMAGE])
        .output()
        .is_ok_and(|output| output.status.success())
    {
        let pulled = Command::new("docker")
            .args(["pull", WORKLOAD_IMAGE])
            .output()
            .expect("docker is on PATH");
        assert!(
            pulled.status.success(),
            "could not pull {WORKLOAD_IMAGE}: {}",
            stderr(&pulled),
        );
    }
    let loaded = Command::new("kind")
        .args([
            "load",
            "docker-image",
            WORKLOAD_IMAGE,
            "--name",
            HARNESS_CLUSTER,
        ])
        .output()
        .expect("kind is on PATH");
    assert!(
        loaded.status.success(),
        "could not load {WORKLOAD_IMAGE} into {HARNESS_CLUSTER}: {}",
        stderr(&loaded),
    );
}

/// Apply a freshly rendered task-run Job and return `(task_run_id, job_name)`.
pub fn launch_task_run(context: &str, config: &KubernetesConfig) -> (String, String) {
    let (job, task_run_id) = rendered_task_run_job(config);
    assert_eq!(
        job.spec.as_ref().and_then(|spec| spec.suspend),
        Some(true),
        "the armed renderer must create the Job suspended so Kueue owns admission",
    );
    assert_eq!(
        job.spec.as_ref().and_then(|spec| spec.backoff_limit),
        Some(0),
        "backoffLimit: 0 is the field that decides live finding 2 — a Pod loss is an immediate \
         Job failure, which is why no replacement Pod is ever minted by a force-delete",
    );
    let job_name = job
        .metadata
        .name
        .clone()
        .expect("the renderer names the Job");
    let mut manifest = job_as_json(&job);
    sleep_instead_of_the_worker(&mut manifest);
    kubectl_apply(context, NAMESPACE, &manifest);
    (task_run_id, job_name)
}

/// Evict (or release) every admitted Workload in the ClusterQueue.
///
/// `HoldAndDrain` is the smallest live producer of Pod-absent-plus-Job-
/// nonterminal. Preemption and a quota reduction reach the same Kueue code path;
/// this one is the only one an operator can trigger on demand.
pub fn set_stop_policy(context: &str, policy: &str) {
    let output = Command::new("kubectl")
        .args([
            "--context",
            context,
            "patch",
            "clusterqueues.kueue.x-k8s.io",
            CLUSTER_QUEUE,
            "--type=merge",
            "-p",
            &format!(r#"{{"spec":{{"stopPolicy":"{policy}"}}}}"#),
        ])
        .output()
        .expect("kubectl is on PATH");
    assert!(
        output.status.success(),
        "setting stopPolicy={policy} failed: {}",
        stderr(&output),
    );
}

/// A `kube::Client` pinned to the harness context.
///
/// Deliberately NOT `Client::try_default()`: that resolves the CURRENT context,
/// and these tests DELETE objects. [`harness_context`] has already proven the
/// name resolves to a loopback API server before this is called.
///
/// `async` because `Client::try_from` builds a `tower` buffer whose worker needs
/// a running reactor — constructing one outside a runtime panics with "there is
/// no reactor running", measured 2026-07-30.
pub async fn kube_client(context: &str) -> kube::Client {
    let config = kube::Config::from_custom_kubeconfig(
        kube::config::Kubeconfig::read().expect("read the kubeconfig"),
        &kube::config::KubeConfigOptions {
            context: Some(context.to_owned()),
            ..kube::config::KubeConfigOptions::default()
        },
    )
    .await
    .expect("resolve the harness context out of the kubeconfig");
    assert!(
        config
            .cluster_url
            .host()
            .is_some_and(|host| host == "127.0.0.1" || host == "localhost" || host == "::1"),
        "refusing to build a client for {}: not a local kind API server",
        config.cluster_url,
    );
    kube::Client::try_from(config).expect("build a kube client for the harness context")
}

/// Drive one full Kueue eviction of this run and then release the queue.
///
/// The release is NOT on a timer. Measured 2026-07-30: after `HoldAndDrain` the
/// evicted Pod is still `Running` 10 seconds later and only disappears at ~15s,
/// so a fixed sleep releases the queue before the Pod is gone, the Workload is
/// never re-admitted, and the "replacement" observed is the ORIGINAL Pod — a
/// test that would then fail for a reason that has nothing to do with the fence.
/// So this waits on the fenced Pod's actual disappearance.
pub fn evict_and_release(context: &str, task_run_id: &str, fenced_uid: &str) {
    set_stop_policy(context, "HoldAndDrain");
    let mut gone = false;
    for _ in 0..AWAIT_TICKS {
        if !pods_of(context, task_run_id)
            .iter()
            .any(|(_, uid, _)| uid == fenced_uid)
        {
            gone = true;
            break;
        }
        std::thread::sleep(TICK);
    }
    set_stop_policy(context, "None");
    assert!(
        gone,
        "Kueue never evicted the fenced Pod {fenced_uid}; workloads: {}",
        workload_summary(context),
    );
}
