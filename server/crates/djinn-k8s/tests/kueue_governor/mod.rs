#![allow(clippy::print_stderr)]
// Shared live-cluster helpers for the two `kueue_governor_*` test targets.
//
// `#![allow(dead_code)]` is the standard `tests/<name>/mod.rs` idiom and is
// load-bearing rather than lazy: this module is compiled SEPARATELY into each
// test binary, so every helper the other binary uses is genuinely unused in
// this one. Without it `RUSTFLAGS: -D warnings` fails the lane for helpers that
// ARE exercised — just from the sibling target.
#![allow(dead_code)]
//! Live-cluster helpers shared by `kueue_governor_conformance` (AC1/AC2, plus
//! every hermetic guard) and `kueue_governor_cap` (AC3/AC4).
//!
//! Split out of one file because the pair exceeded `scripts/check-file-size.sh`
//! (MAX_LINES 1500 / MAX_BYTES 51200) and the `djinn:allow-oversize` marker is
//! for source that genuinely cannot be divided — this could be. The module docs
//! for the whole harness live in `kueue_governor_conformance.rs`.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    GrantNextBuildLeaseResult, InvocationLeaseAuthorityRepository, InvocationLeaseMode,
    QueueBuildLeaseInput,
};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::{LABEL_TASK_RUN_ID, build_task_run_job};
use djinn_k8s::launcher::{CgroupLauncherMode, LAUNCHER_CONTAINER_NAME};
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};
use k8s_openapi::api::core::v1::Container;
use serde_json::Value;

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

pub const HARNESS_CLUSTER: &str = "djinn-kueue-c1";
/// kind names its context `kind-<cluster>`. Derived, never discovered: every
/// context in a Djinn developer's kubeconfig is a live EKS cluster.
pub const HARNESS_CONTEXT: &str = "kind-djinn-kueue-c1";
pub const HARNESS_REGISTRY: &str = "djinn-kueue-c1-registry";
pub const HARNESS_REGISTRY_PORT: &str = "5061";
/// Kueue 0.19 on a containerd that supports `cgroup_writable`. Measured
/// 2026-07-31: `kindest/node:v1.35.0` ships containerd 2.2.0. The 1.31 image the
/// sibling harnesses use ships containerd 1.7, which has no such runtime
/// property, and the setup script fails closed on it rather than producing a
/// node whose RuntimeClass resolves to a read-only cgroup mount.
pub const HARNESS_K8S_VERSION: &str = "1.35.0";

/// The sibling harnesses' clusters. Named so the disjointness guard below is a
/// statement about them and not only about the script's defaults.
pub const SIBLING_CLUSTERS: &[&str] = &["djinn-kueue-harness", "djinn-kueue-b2", "djinn-kueue-b2b"];

pub const NAMESPACE: &str = "djinn";
pub const CLUSTER_QUEUE: &str = "djinn-kueue";
pub const WORKER_CONTAINER_NAME: &str = "worker";

pub const SETUP_SCRIPT: &str = "scripts/kind/setup-kueue-cluster.sh";
pub const GOVERNOR_VALUES: &str = "deploy/helm/djinn/tests/fixtures/kueue-governor-values.yaml";
pub const B0_VALUES: &str = "deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml";
pub const PROBE_BUILD_SCRIPT: &str =
    "server/crates/djinn-k8s/tests/fixtures/governor-probe/build.sh";
pub const PROBE_IMAGE: &str = "djinn-governor-probe:fbiy-c1";
pub const PROBE_BIN: &str = "/opt/djinn/bin/djinn-governor-probe";
pub const PROBE_WORKLOAD: &str = "/opt/djinn/workload.bin";
pub const PROBE_DECISION_PATH: &str = "/var/tmp/djinn-probe/decision";

/// The pod CPU limit the lifted quota is derived from
/// (`launcher_cpu::launcher_leased_millicores`). Two cores rather than the
/// production four: the node is shared with other agents' clusters, and 2000m
/// against a 250m clamp is still an 8x ceiling — comfortably above the 3x this
/// file asserts, and small enough that three concurrent lifted invocations
/// cannot saturate the host.
pub const POD_CPU_LIMIT: &str = "2";

/// The multiplication AC1 demands across the lift transition.
pub const REQUIRED_THROUGHPUT_MULTIPLE_MILLI: u64 = 3_000;

pub const TICK: Duration = Duration::from_millis(500);
pub const AWAIT_TICKS: usize = 360;

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives three levels below the repository root")
        .to_path_buf()
}

pub fn run_script(relative: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join(relative))
        .args(args)
        .current_dir(repo_root())
        .output()
        .unwrap_or_else(|error| panic!("{relative} is executable: {error}"))
}

pub fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the script exits rather than dying on a signal")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn yaml_at(relative: &str) -> serde_yaml::Value {
    let text = std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"));
    serde_yaml::from_str(&text).unwrap_or_else(|error| panic!("parse {relative}: {error}"))
}

// ---------------------------------------------------------------------------
// Shared render helpers (used by both halves).
// ---------------------------------------------------------------------------

/// Capabilities a task-run container must never hold.
///
/// `SYS_ADMIN` is the mount privilege the launcher used to need before the
/// RuntimeClass delegated its cgroup root; `SYS_RESOURCE` would let a container
/// raise its own limits. Both would make the clamp advisory.
pub fn privileged_capabilities(container: &Container) -> Vec<String> {
    const FORBIDDEN: &[&str] = &["SYS_ADMIN", "SYS_RESOURCE"];
    container
        .security_context
        .as_ref()
        .and_then(|context| context.capabilities.as_ref())
        .and_then(|capabilities| capabilities.add.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|granted| {
            FORBIDDEN
                .iter()
                .any(|name| granted.eq_ignore_ascii_case(name))
        })
        .collect()
}

/// `post_rate / clamp_rate` in thousandths. Zero when the clamp phase produced
/// no work, because "infinite improvement over nothing" is a measurement
/// failure, not a pass.
pub fn throughput_multiple_milli(clamp_rate_milli: u64, post_rate_milli: u64) -> u64 {
    if clamp_rate_milli == 0 {
        return 0;
    }
    post_rate_milli.saturating_mul(1_000) / clamp_rate_milli
}

pub fn armed_config(image: &str) -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        kueue_local_queue_prefix: "djinn".into(),
        // The arming pair. `build_task_run_job` asserts them together.
        cgroup_launcher_mode: CgroupLauncherMode::Required,
        task_run_cgroup_writable_enabled: true,
        image: image.into(),
        image_pull_policy: "IfNotPresent".into(),
        // Requests low so `kueue.buildPods` stays the binding constraint on one
        // kind node; the LIMIT is what the lifted quota is derived from.
        cpu_request: "100m".into(),
        cpu_limit: POD_CPU_LIMIT.into(),
        memory_request: "64Mi".into(),
        memory_limit: "512Mi".into(),
        ..KubernetesConfig::for_testing()
    }
}

/// `(clamp, leased)` millicores as they appear on the RENDERED sidecar, read
/// back off the container rather than recomputed from the config.
pub fn rendered_quotas(config: &KubernetesConfig) -> (u32, u32) {
    let job = render_probe_job(config, "harness-project").0;
    let sidecar = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .and_then(|pod| pod.init_containers.as_ref())
        .and_then(|containers| {
            containers
                .iter()
                .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        })
        .expect("the armed renderer emits the cgroup-launcher sidecar")
        .clone();
    let read = |key: &str| -> u32 {
        sidecar
            .env
            .as_ref()
            .expect("the sidecar carries env")
            .iter()
            .find(|entry| entry.name == key)
            .and_then(|entry| entry.value.as_deref())
            .unwrap_or_else(|| panic!("the sidecar carries {key}"))
            .parse()
            .unwrap_or_else(|_| panic!("{key} is a number"))
    };
    (
        read("DJINN_LAUNCHER_UNLEASED_MILLICORES"),
        read("DJINN_LAUNCHER_LEASED_MILLICORES"),
    )
}

pub fn render_probe_job(
    config: &KubernetesConfig,
    project: &str,
) -> (k8s_openapi::api::batch::v1::Job, String) {
    let task_run_id = uuid::Uuid::now_v7();
    let job = build_task_run_job(
        config,
        &task_run_id,
        project,
        &format!("djinn-taskrun-{task_run_id}"),
        &config.image,
        &[],
        None,
        false,
        None,
    );
    (job, task_run_id.to_string())
}

// ===========================================================================
// The live half.
// ===========================================================================

pub fn live_tests_enabled() -> bool {
    if env::var("DJINN_TEST_KUEUE_CLUSTER").as_deref() != Ok("1") {
        eprintln!("SKIP: set DJINN_TEST_KUEUE_CLUSTER=1 to run the live governor conformance");
        return false;
    }
    for tool in ["kubectl", "kind", "docker"] {
        if Command::new(tool)
            .arg("--help")
            .output()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            panic!("DJINN_TEST_KUEUE_CLUSTER=1 but {tool} is not on PATH");
        }
    }
    true
}

/// Refuse to run against anything that is not this harness's local kind cluster.
pub fn harness_context() -> String {
    let output = Command::new("kubectl")
        .args([
            "--context",
            HARNESS_CONTEXT,
            "config",
            "view",
            "--minify",
            "-o",
            "jsonpath={.clusters[0].cluster.server}",
        ])
        .output()
        .expect("kubectl is on PATH");
    let server = stdout(&output);
    assert!(
        server.contains("127.0.0.1") || server.contains("localhost"),
        "refusing to run against {HARNESS_CONTEXT}: its API server is {server:?}, not a local \
         kind cluster. Every context in a Djinn developer's kubeconfig is a live EKS cluster.",
    );
    HARNESS_CONTEXT.to_owned()
}

pub fn kubectl(context: &str, args: &[&str]) -> Output {
    Command::new("kubectl")
        .arg("--context")
        .arg(context)
        .args(args)
        .output()
        .expect("kubectl is on PATH")
}

pub fn kubectl_ok(context: &str, args: &[&str]) -> String {
    let output = kubectl(context, args);
    assert!(
        output.status.success(),
        "kubectl {args:?} failed: {}",
        stderr(&output),
    );
    stdout(&output)
}

pub fn kubectl_json(context: &str, args: &[&str]) -> Value {
    let mut full = args.to_vec();
    full.extend(["-o", "json"]);
    serde_json::from_str(&kubectl_ok(context, &full)).expect("kubectl -o json emits JSON")
}

/// The chart-installed names the renderer must be pointed at.
pub fn config_for(context: &str) -> KubernetesConfig {
    let named = |kind: &str, suffix: &str| -> String {
        kubectl_json(context, &["-n", NAMESPACE, "get", kind])["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a {kind} ending in {suffix}"))
            .to_owned()
    };
    KubernetesConfig {
        service_account: named("serviceaccounts", "-taskrun"),
        mirror_pvc: named("persistentvolumeclaims", "-mirrors"),
        cache_pvc: named("persistentvolumeclaims", "-cache"),
        ..armed_config(PROBE_IMAGE)
    }
}

pub fn ensure_probe_image(context: &str) {
    let built = run_script(PROBE_BUILD_SCRIPT, &[PROBE_IMAGE, HARNESS_CLUSTER]);
    assert!(
        built.status.success(),
        "building/loading {PROBE_IMAGE} failed: {}\n{}",
        stdout(&built),
        stderr(&built),
    );
    let _ = context;
}

/// One live invocation: the Job that carries it, the fence it was BEGUN with,
/// and the identities the harness needs to speak to it.
pub struct Probe {
    pub task_run_id: String,
    pub job_name: String,
    pub invocation: String,
    pub fence: u64,
    pub pod: String,
    pub pod_uid: String,
}

/// Render a task-run Job, replace ONLY the worker container's command, apply it.
///
/// Everything that decides the behaviour under test is the renderer's:
/// `suspend`, `runtimeClassName`, `shareProcessNamespace`, both security
/// contexts, the sidecar's command/env/capabilities and the absence of a sidecar
/// CPU limit. The `command` swap is the same single mutation
/// `sleep_instead_of_the_worker` makes in the sibling harness, for the same
/// reason, and the probe env rides alongside it.
pub fn launch_probe(
    context: &str,
    config: &KubernetesConfig,
    fence: u64,
    clamp_s: u64,
    post_s: u64,
) -> Probe {
    let (job, task_run_id) = render_probe_job(config, "harness-project");
    assert_eq!(
        job.spec.as_ref().and_then(|spec| spec.suspend),
        Some(true),
        "the armed renderer must create the Job suspended so Kueue owns admission",
    );
    let job_name = job
        .metadata
        .name
        .clone()
        .expect("the renderer names the Job");
    // The leaf name IS the invocation id in production (`process_broker`), and
    // `validate_leaf_name` rejects anything that is not a direct child name.
    let invocation = format!("probe-{task_run_id}");

    let mut manifest = serde_json::to_value(&job).expect("Job serializes");
    manifest["apiVersion"] = Value::String("batch/v1".into());
    manifest["kind"] = Value::String("Job".into());
    let containers = manifest
        .pointer_mut("/spec/template/spec/containers")
        .and_then(Value::as_array_mut)
        .expect("the rendered Job has containers");
    let worker = containers
        .iter_mut()
        .find(|container| container["name"] == WORKER_CONTAINER_NAME)
        .expect("the rendered Job has a worker container");
    worker["command"] = serde_json::json!([PROBE_BIN]);
    worker["args"] = Value::Null;
    let env = worker["env"]
        .as_array_mut()
        .expect("the renderer sets worker env");
    for (name, value) in [
        ("DJINN_PROBE_INVOCATION", invocation.clone()),
        ("DJINN_PROBE_FENCE", fence.to_string()),
        // Armed: the birth authority `birth_authority(Lift|Shadow)` produces.
        // An `unarmed` leaf is born UNCLAMPED and there would be nothing to lift.
        ("DJINN_PROBE_AUTHORITY", "armed".to_owned()),
        ("DJINN_PROBE_DECISION_PATH", PROBE_DECISION_PATH.to_owned()),
        ("DJINN_PROBE_WORKLOAD", PROBE_WORKLOAD.to_owned()),
        ("DJINN_PROBE_CLAMP_SECONDS", clamp_s.to_string()),
        ("DJINN_PROBE_LIFTED_SECONDS", post_s.to_string()),
    ] {
        env.push(serde_json::json!({ "name": name, "value": value }));
    }

    let manifest_text = serde_json::to_string(&manifest).expect("manifest serializes");
    let applied = Command::new("kubectl")
        .args(["--context", context, "-n", NAMESPACE, "apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("stdin is piped")
                .write_all(manifest_text.as_bytes())?;
            child.wait_with_output()
        })
        .expect("kubectl apply runs");
    assert!(
        applied.status.success(),
        "applying the rendered Job failed: {}",
        stderr(&applied),
    );

    let (pod, pod_uid) = await_running_pod(context, &task_run_id);
    Probe {
        task_run_id,
        job_name,
        invocation,
        fence,
        pod,
        pod_uid,
    }
}

pub fn pods_of(context: &str, task_run_id: &str) -> Vec<(String, String, String)> {
    kubectl_json(
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "pods",
            "-l",
            &format!("{LABEL_TASK_RUN_ID}={task_run_id}"),
        ],
    )["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .map(|item| {
            (
                item["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                item["metadata"]["uid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                item["status"]["phase"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect()
}

pub fn await_running_pod(context: &str, task_run_id: &str) -> (String, String) {
    for _ in 0..AWAIT_TICKS {
        if let Some((name, uid, _)) = pods_of(context, task_run_id)
            .into_iter()
            .find(|(_, _, phase)| phase == "Running")
        {
            return (name, uid);
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "no Pod of task-run {task_run_id} reached Running. Pods: {:?}. Workloads: {}",
        pods_of(context, task_run_id),
        kubectl_ok(
            context,
            &[
                "-n",
                NAMESPACE,
                "get",
                "workloads.kueue.x-k8s.io",
                "-o",
                "wide"
            ],
        ),
    )
}

/// Read the invocation leaf's `cpu.max` from INSIDE the pod, out of the
/// launcher container's own delegated cgroup root.
///
/// This is the kernel's value, read through the same `/sys/fs/cgroup` the
/// launcher writes — never a value this test wrote and never one the probe
/// reported. The leaf directory is mode 0700 and root-owned, which is why this
/// runs in the sidecar (uid 0) rather than the worker (uid 1000).
pub fn leaf_cpu_max(context: &str, probe: &Probe) -> String {
    let path = format!("/sys/fs/cgroup/{}/cpu.max", probe.invocation);
    let output = kubectl(
        context,
        &[
            "-n",
            NAMESPACE,
            "exec",
            &probe.pod,
            "-c",
            LAUNCHER_CONTAINER_NAME,
            "--",
            "cat",
            &path,
        ],
    );
    assert!(
        output.status.success(),
        "reading {path} in {} failed: {}",
        probe.pod,
        stderr(&output),
    );
    stdout(&output).trim().to_owned()
}

pub fn probe_log(context: &str, probe: &Probe) -> String {
    stdout(&kubectl(
        context,
        &[
            "-n",
            NAMESPACE,
            "logs",
            &probe.pod,
            "-c",
            WORKER_CONTAINER_NAME,
        ],
    ))
}

/// Block until the probe emits `record`, then return the whole log.
pub fn await_probe_record(context: &str, probe: &Probe, record: &str) -> String {
    for _ in 0..AWAIT_TICKS {
        let log = probe_log(context, probe);
        if log.contains(&format!("probe.{record}")) {
            return log;
        }
        if let Some(line) = log.lines().find(|line| line.starts_with("probe.fatal")) {
            panic!("the probe in {} failed: {line}", probe.pod);
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "the probe in {} never emitted probe.{record}. Log:\n{}",
        probe.pod,
        probe_log(context, probe),
    );
}

/// Parse one `probe.<record> k=v ...` line into a map.
pub fn probe_record(log: &str, record: &str) -> BTreeMap<String, String> {
    let prefix = format!("probe.{record} ");
    let line = log
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no probe.{record} line in:\n{log}"));
    line.trim_start_matches(&prefix)
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

pub fn phase_record(log: &str, phase: &str) -> BTreeMap<String, String> {
    let line = log
        .lines()
        .find(|line| line.starts_with("probe.phase ") && line.contains(&format!("phase={phase} ")))
        .unwrap_or_else(|| panic!("no probe.phase phase={phase} line in:\n{log}"));
    line.trim_start_matches("probe.phase ")
        .split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

pub fn phase_number(log: &str, phase: &str, key: &str) -> u64 {
    phase_record(log, phase)
        .get(key)
        .unwrap_or_else(|| panic!("phase {phase} carries no {key}"))
        .parse()
        .unwrap_or_else(|error| panic!("phase {phase} {key} is not a number: {error}"))
}

/// Hand the probe the fence the governor authorized — or a fence it did not.
pub fn deliver_decision(context: &str, probe: &Probe, fence: Option<u64>) {
    let body = match fence {
        Some(value) => format!("lift {value}"),
        None => "skip".to_owned(),
    };
    let script = format!(
        "mkdir -p $(dirname {PROBE_DECISION_PATH}); printf '%s' '{body}' > {PROBE_DECISION_PATH}"
    );
    let output = kubectl(
        context,
        &[
            "-n",
            NAMESPACE,
            "exec",
            &probe.pod,
            "-c",
            WORKER_CONTAINER_NAME,
            "--",
            "sh",
            "-c",
            &script,
        ],
    );
    assert!(
        output.status.success(),
        "delivering the decision to {} failed: {}",
        probe.pod,
        stderr(&output),
    );
}

pub fn delete_job(context: &str, job_name: &str) {
    let _ = kubectl(
        context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "job",
            job_name,
            "--cascade=foreground",
            "--wait=true",
            "--timeout=120s",
        ],
    );
}

pub fn clear_task_run_jobs(context: &str) {
    let _ = kubectl(
        context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "jobs",
            "-l",
            // The renderer's own component label (`job.rs::job_labels`). NOT
            // `app.kubernetes.io/component`: measured 2026-07-31, a selector on
            // that key matches nothing here, leaves the previous run's Jobs
            // holding the `pods` quota, and the next run then fails with "no Pod
            // reached Running" — a `pods`-quota exhaustion wearing the costume
            // of a scheduling bug.
            "djinn.app/component=task-run-worker",
            "--cascade=foreground",
            "--wait=true",
            "--timeout=180s",
        ],
    );
}

pub fn admitted_workloads(context: &str) -> u64 {
    kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    )["status"]["admittedWorkloads"]
        .as_u64()
        .unwrap_or(0)
}

pub fn pods_nominal_quota(context: &str) -> u64 {
    kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    )["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
        .as_array()
        .expect("the ClusterQueue covers resources")
        .iter()
        .find(|resource| resource["name"] == "pods")
        // A Kubernetes `Quantity` serializes as a STRING ("4"), not a number.
        .and_then(|resource| resource["nominalQuota"].as_str())
        .and_then(|quota| quota.parse().ok())
        .expect("the ClusterQueue bounds `pods` with a parseable quantity")
}

// ---------------------------------------------------------------------------
// The durable governor, driven exactly as `kueue_disruption_governor` drives it.
// ---------------------------------------------------------------------------

pub fn invocation_key(pod_uid: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{pod_uid}"),
    }
}

/// Ask the REAL governor which of `pod_uids` may lift, at a cap it reads back
/// out of the durable authority row.
///
/// Returns `(K, the subset of pod UIDs holding a bound lease)`. Nothing here is
/// a constant: the cap is written through the operator API, read back, and only
/// the read-back value is used; `grant_next` enforces it inside a locked
/// transaction; `bind` fences each grant to the live Pod UID it was queued for.
pub fn authorize(pod_uids: &[String], m: u64) -> (i64, Vec<String>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the durable governor");
    runtime.block_on(async move {
        let db = Database::open_in_memory()
            .expect("a real PostgreSQL test database (DJINN_TEST_DATABASE_URL)");
        let authority = InvocationLeaseAuthorityRepository::new(db.clone());
        let seeded = authority.seed_baseline().await.expect("seed the authority");
        authority
            .set_mode_and_cap(
                seeded.epoch,
                InvocationLeaseMode::Enforce,
                Some(i64::try_from(m).expect("M fits an i64") - 1),
            )
            .await
            .expect("arm the invocation-lease authority");

        // READ BACK. Everything below uses this, never the value written.
        let live = authority
            .read()
            .await
            .expect("read the durable authority")
            .expect("the authority row exists once seeded");
        let k = live
            .cap
            .expect("an armed authority carries a reference cap");
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(live.clone()))),
            InvocationLiftDecision::Lift,
            "the live authority must authorize lifts at all, or the cap bounds a population that \
             never lifts",
        );
        assert!(
            k >= 1 && k < i64::try_from(m).expect("M fits an i64"),
            "the cap K={k} must be at least 1 and strictly below M={m}, or it bounds nothing",
        );

        let repository = std::sync::Arc::new(BuildLeaseRepository::new(db.clone()));
        for uid in pod_uids {
            repository
                .queue(&QueueBuildLeaseInput {
                    key: invocation_key(uid),
                    immutable_identity: format!("pod:{uid}"),
                    queue_deadline: None,
                    launch_deadline: None,
                    weight: 1,
                })
                .await
                .unwrap_or_else(|error| panic!("queue an invocation lease for {uid}: {error:?}"));
        }

        // Every grant the FIFO will make at the live cap, taken concurrently so
        // the bound is a property of the locked transaction rather than of the
        // order this test happened to call in.
        let mut attempts = tokio::task::JoinSet::new();
        for _ in 0..pod_uids.len() {
            let repository = repository.clone();
            attempts.spawn(async move {
                repository
                    .grant_next(k, "2026-07-31T00:00:00.000Z", None)
                    .await
                    .expect("grant_next must not error")
            });
        }
        let mut authorized = Vec::new();
        while let Some(result) = attempts.join_next().await {
            if let GrantNextBuildLeaseResult::Granted(row) = result.expect("grant task panicked") {
                let uid = row
                    .immutable_identity
                    .strip_prefix("pod:")
                    .expect("the identity carries its Pod UID")
                    .to_owned();
                let token = row.fencing_token.expect("a granted lease carries a token");
                let bound = repository
                    .bind(&invocation_key(&uid), token, &uid, None)
                    .await
                    .unwrap_or_else(|error| panic!("bind the lease for {uid}: {error:?}"));
                assert_eq!(bound.state, BuildLeaseState::Bound);
                assert_eq!(bound.bound_pod_uid.as_deref(), Some(uid.as_str()));
                authorized.push(uid);
            }
        }
        (k, authorized)
    })
}

/// `cpu.max` line for `millicores` over the launcher's 100ms period.
pub fn cpu_max_line(millicores: u32) -> String {
    format!("{} 100000", u64::from(millicores) * 100_000 / 1_000)
}
