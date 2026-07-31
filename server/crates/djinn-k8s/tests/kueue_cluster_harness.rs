// Test: eprintln is the skip-reason channel for the gated half, mirroring
// tests/kind_smoke.rs.
#![allow(clippy::print_stderr)]
//! Live conformance harness for the DISPOSABLE armed-Kueue cluster (fbiy-B0).
//!
//! WHY THIS FILE IS NOT A FIXTURE TEST
//! -----------------------------------
//! Epic `fbiy` asks whether Kueue mutates only `spec.suspend`, whether the
//! `pods` nominalQuota actually bounds admission, and what force-deleting a
//! Workload does. None of those can be answered by a render: they are
//! statements about a controller's behaviour against an API server. On
//! 2026-07-30 four separate pieces of green-but-inert work in this repository
//! were caught only by real clusters — a gate nothing could satisfy, a harness
//! that could not execute, a documented k8s floor wrong by five minor
//! versions, and a launcher path unreachable from its own binary. This file
//! exists so `fbiy`'s claims cannot join them.
//!
//! TWO HALVES, AND ONLY ONE OF THEM NEEDS A CLUSTER
//! ------------------------------------------------
//! * The `guard_*` tests are HERMETIC and NOT `#[ignore]`d. They run in the
//!   ordinary `cargo test -p djinn-k8s` lane, on every PR, with no cluster and
//!   no network. They exercise the refusals in
//!   `scripts/kind/setup-kueue-cluster.sh` that keep this harness off
//!   production, plus the non-vacuity of the live assertions' own inputs.
//! * The `live_*` tests are `#[ignore]` + `DJINN_TEST_KUEUE_CLUSTER=1` gated,
//!   mirroring `tests/kind_smoke.rs:89-103`.
//!
//! That split is deliberate, and it is the answer to the thing
//! `tests/kind_smoke.rs` gets wrong: that file is `#[ignore]` +
//! `DJINN_TEST_KIND`-gated and **no CI lane runs it** (`grep DJINN_TEST_KIND
//! .github/workflows/` is empty), so its assertions have never protected
//! anything automatically. The live half here has the same property — see
//! `.github/workflows/kueue-cluster-harness.yml`, which is `workflow_dispatch`
//! only and NOT a required check — so the guards that must never regress were
//! put where they run by default instead.
//!
//! WHY THE LIVE HALF DRIVES `kubectl` AND NOT `kube::Client`
//! ---------------------------------------------------------
//! Originally, because `kube::Client` did not work in this workspace's test
//! binaries at all — finding that out was one of this task's results.
//!
//! `workspace-hack` unifies `rustls` 0.23 with BOTH the `ring` and the
//! `aws-lc-rs` providers enabled, and at the time nothing in a test binary
//! called `CryptoProvider::install_default()`. The first TLS handshake
//! therefore panicked:
//!
//! ```text
//! Could not automatically determine the process-level CryptoProvider from
//! Rustls crate features.
//! ```
//!
//! MEASURED 2026-07-30: `DJINN_TEST_KIND=1 cargo test -p djinn-k8s --test
//! kind_smoke -- --ignored` panicked exactly there, against a live kind
//! cluster, before its first API call. The file 6knv points at as the pattern
//! to mirror was not merely un-run by CI — it was UNRUNNABLE, and had been
//! silently so. That is the "harness that could not execute" failure class,
//! found again.
//!
//! RESOLVED 2026-07-31 by task `d2ae`: `tests/support/mod.rs` installs the
//! `ring` provider explicitly, `kind_smoke.rs` uses it, and a `kube::Client`
//! now builds in a djinn-k8s test binary. No dev-dependency was needed —
//! `djinn-k8s` already carries a dev-only `rustls`, so `workspace-hack` and
//! `cargo hakari generate` stayed untouched.
//!
//! This file still drives `kubectl` anyway, and deliberately: the reason that
//! survives is pinned-context discipline, not the provider. Every live
//! assertion below reads or writes through
//! `kubectl --context kind-djinn-kueue-harness`, the same discipline
//! `deploy/kueue/zero-capture-gate.sh` and `deploy/kueue/preflight.sh` already
//! use, and which cannot silently start targeting a cluster this harness did
//! not create. A future rewrite onto `kube::Client` is now possible but must
//! carry an equivalent guard — see `kind_smoke.rs`'s loopback check.
//!
//! The objects still come from the REAL renderer: `build_task_run_job` produces
//! the Job, it is serialized and handed to the API server unmodified, and what
//! Kueue does with it is read back off that same API server.
//!
//! RUNNING THE LIVE HALF
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s --test kueue_cluster_harness -- --ignored
//! scripts/kind/setup-kueue-cluster.sh down     # cluster AND registry
//! ```

use std::collections::BTreeSet;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use djinn_k8s::config::{KubernetesConfig, LABEL_KUEUE_BUILD_OBJECT, LABEL_KUEUE_QUEUE_NAME};
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::CgroupLauncherMode;
use k8s_openapi::api::batch::v1::Job;
use serde_json::Value;

/// The one cluster this harness may ever touch. It must equal
/// `CLUSTER_NAME`'s default in `scripts/kind/setup-kueue-cluster.sh`; the
/// script's `check` action is used below to keep the two from drifting.
const HARNESS_CLUSTER: &str = "djinn-kueue-harness";
/// kind names its context `kind-<cluster>`. Derived here for the same reason
/// the script derives it: the CURRENT context is never consulted, because
/// every context in a Djinn developer's kubeconfig is a live EKS cluster.
const HARNESS_CONTEXT: &str = "kind-djinn-kueue-harness";
const NAMESPACE: &str = "djinn";
const KUEUE_MANAGED_LABEL: &str = "djinn.io/kueue-managed";
/// `<djinn.fullname>-kueue` for release `djinn`, per
/// `deploy/helm/djinn/templates/kueue-topology.yaml`.
const CLUSTER_QUEUE: &str = "djinn-kueue";
/// The RuntimeClass the harness deliberately does NOT install — see
/// [`live_cutover_preflight_exits_10_against_the_armed_harness`].
const CGROUP_WRITABLE_RUNTIME_CLASS: &str = "djinn-cgroup-writable";

const SETUP_SCRIPT: &str = "scripts/kind/setup-kueue-cluster.sh";
const VALUES_FIXTURE: &str = "deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml";
const CHART_VALUES: &str = "deploy/helm/djinn/values.yaml";
const PREFLIGHT_SCRIPT: &str = "deploy/kueue/preflight.sh";

/// Exit codes owned by `scripts/kind/setup-kueue-cluster.sh`.
const EXIT_REFUSED_TARGET: i32 = 3;
const EXIT_VERSION_FLOOR: i32 = 7;
/// Exit code owned by `deploy/kueue/preflight.sh`: RuntimeClass absent.
const PREFLIGHT_EXIT_MISSING_RUNTIME_CLASS: i32 = 10;

fn repo_root() -> PathBuf {
    // <repo>/server/crates/djinn-k8s
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives three levels below the repository root")
        .to_path_buf()
}

fn run_setup_script(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join(SETUP_SCRIPT))
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("setup-kueue-cluster.sh is executable")
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the script exits rather than dying on a signal")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
// ===========================================================================

/// AC3, first half: the harness never targets a context it did not create.
///
/// `check` runs every guard and then stops without creating anything, so this
/// is a real exercise of the production-safety refusal rather than a
/// re-statement of it in Rust.
///
/// The accepting case is the non-vacuity: a `check` that refused everything
/// (or a script that had been renamed out from under this test) would satisfy
/// the refusals alone.
#[test]
fn guard_setup_script_refuses_a_context_it_did_not_create() {
    let accepted = run_setup_script(&["check", "--context", HARNESS_CONTEXT]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "the harness must accept the context of the cluster it creates; stderr: {}",
        stderr(&accepted),
    );

    for foreign in [
        // The three contexts actually present in a Djinn developer's
        // kubeconfig today. All three are EKS.
        "demo",
        "staging",
        "prod",
        "arn:aws:eks:us-east-1:482965429208:cluster/eks-134-prod",
        // The developer's live Tilt cluster.
        "kind-djinn",
        // A near-miss, because a prefix check would let this through.
        "kind-djinn-kueue-harness-2",
    ] {
        let refused = run_setup_script(&["check", "--context", foreign]);
        assert_eq!(
            exit_code(&refused),
            EXIT_REFUSED_TARGET,
            "context {foreign} must be refused with exit {EXIT_REFUSED_TARGET}; stderr: {}",
            stderr(&refused),
        );
        assert!(
            stderr(&refused).contains(HARNESS_CONTEXT),
            "the refusal must name the only context this harness may target; stderr: {}",
            stderr(&refused),
        );
    }
}

/// AC3, second half: the names belonging to the developer's Tilt environment
/// are refused outright, not merely defaulted away from. `down` DELETES what
/// it is given, so "we would never pass that" is not a safety property.
#[test]
fn guard_setup_script_refuses_the_tilt_cluster_registry_and_port() {
    for args in [
        ["check", "--cluster-name", "djinn"],
        ["check", "--registry-name", "kind-registry"],
        ["check", "--registry-port", "5001"],
    ] {
        let refused = run_setup_script(&args);
        assert_eq!(
            exit_code(&refused),
            EXIT_REFUSED_TARGET,
            "{args:?} must be refused with exit {EXIT_REFUSED_TARGET}; stderr: {}",
            stderr(&refused),
        );
    }
}

/// The Kueue 0.19 floor is 1.30, and the 1.29 floor this repository documented
/// until #2818 was measured false. A harness that silently created a 1.29
/// cluster would reproduce that bug as a test environment.
#[test]
fn guard_setup_script_refuses_a_kubernetes_below_the_kueue_floor() {
    let refused = run_setup_script(&["check", "--k8s-version", "1.29.0"]);
    assert_eq!(
        exit_code(&refused),
        EXIT_VERSION_FLOOR,
        "1.29 must be refused with exit {EXIT_VERSION_FLOOR}; stderr: {}",
        stderr(&refused),
    );
    // Non-vacuity: the floor is 1.30, not "refuse everything".
    let accepted = run_setup_script(&["check", "--k8s-version", "1.30.0"]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "1.30 is the floor and must be accepted; stderr: {}",
        stderr(&accepted),
    );
}

/// The script's default cluster name and this file's constant are two copies
/// of one fact, and the live tests below would target a cluster that does not
/// exist if they drifted apart.
#[test]
fn guard_harness_cluster_name_matches_the_setup_script_default() {
    let defaults = run_setup_script(&["check"]);
    assert_eq!(exit_code(&defaults), 0, "stderr: {}", stderr(&defaults));
    let stdout = String::from_utf8_lossy(&defaults.stdout).into_owned();
    assert!(
        stdout.contains(&format!("cluster={HARNESS_CLUSTER} ")),
        "the script's default cluster must be {HARNESS_CLUSTER}, got: {stdout}",
    );
    assert!(
        stdout.contains(&format!("context={HARNESS_CONTEXT} ")),
        "the script's derived context must be {HARNESS_CONTEXT}, got: {stdout}",
    );
}

fn yaml_at(relative: &str) -> serde_yaml::Value {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {relative}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {relative}: {e}"))
}

fn build_pods_in(values: &serde_yaml::Value) -> u64 {
    values["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number")
}

/// AC1's quota assertion compares the live ClusterQueue against the FIXTURE's
/// `kueue.buildPods`. If the fixture happened to carry the chart's own
/// default, that comparison would pass just as well against an install that
/// never read the fixture — green against nothing.
///
/// This runs hermetically so the divergence cannot be lost in a future values
/// edit without the ordinary PR lane saying so.
#[test]
fn guard_fixture_build_pods_differs_from_the_chart_default() {
    let fixture = build_pods_in(&yaml_at(VALUES_FIXTURE));
    let chart_default = build_pods_in(&yaml_at(CHART_VALUES));
    assert_ne!(
        fixture, chart_default,
        "the harness fixture's kueue.buildPods ({fixture}) must differ from the chart default \
         ({chart_default}), or the live nominalQuota assertion cannot tell an install that read \
         the fixture from one that ignored it",
    );
}

/// The fixture must NOT install the RuntimeClass, because AC4 asserts the
/// cutover preflight exits 10 ("RuntimeClass djinn-cgroup-writable is absent")
/// against the cluster it produces. Enabling it in the fixture would retire
/// that assertion silently — the live test would simply start failing for a
/// reason nobody would connect to this file.
#[test]
fn guard_fixture_leaves_the_cgroup_writable_runtime_class_uninstalled() {
    let fixture = yaml_at(VALUES_FIXTURE);
    assert_eq!(
        fixture["cgroupWritable"]["runtimeClass"]["enabled"].as_bool(),
        Some(false),
        "the harness fixture must leave cgroupWritable.runtimeClass disabled",
    );
    assert_eq!(
        fixture["cgroupLauncher"]["mode"].as_str(),
        Some("disabled"),
        "the chart refuses kueue.armed + cgroupLauncher.mode=required without the RuntimeClass, \
         so proving the class absent requires the launcher disabled — the two are one fact",
    );
    assert_eq!(
        fixture["kueue"]["armed"].as_bool(),
        Some(true),
        "a fixture that did not arm Kueue would make every live assertion below vacuous",
    );
}

// ===========================================================================
// ===========================================================================
// Live half — #[ignore] + DJINN_TEST_KUEUE_CLUSTER=1
// ===========================================================================

/// Returns `false` when the live half is disabled; callers `return` early.
/// Mirrors `tests/kind_smoke.rs:89-103`, including printing the skip reason so
/// a developer knows which gate they hit.
fn live_tests_enabled() -> bool {
    if env::var("DJINN_TEST_KUEUE_CLUSTER").is_err() {
        eprintln!("kueue_cluster_harness: DJINN_TEST_KUEUE_CLUSTER not set — skipping");
        return false;
    }
    if !which("kubectl") {
        eprintln!("kueue_cluster_harness: kubectl not found on PATH — skipping");
        return false;
    }
    true
}

/// Minimal PATH-based which(1), copied from `tests/kind_smoke.rs` rather than
/// pulling a crate in for one call site.
fn which(bin: &str) -> bool {
    env::var("PATH").is_ok_and(|path| {
        path.split(':')
            .any(|dir| Path::new(dir).join(bin).is_file())
    })
}

/// The context every live call below is pinned to, after TWO independent
/// refusals of anything else.
///
/// Guard 1 is the name: it must be the context of the cluster
/// `scripts/kind/setup-kueue-cluster.sh` creates. Guard 2 is the resolved API
/// server URL, which catches what guard 1 cannot — a kubeconfig entry NAMED
/// `kind-djinn-kueue-harness` that points somewhere else entirely. kind always
/// serves on loopback; no managed control plane does, and all three contexts
/// in a Djinn developer's kubeconfig are EKS.
///
/// Deliberately NOT `kubectl` with no `--context` and NOT
/// `kube::Client::try_default()`: both resolve the CURRENT context, and these
/// tests CREATE AND DELETE objects.
fn harness_context() -> String {
    let requested = env::var("DJINN_TEST_KUEUE_CONTEXT").unwrap_or_else(|_| HARNESS_CONTEXT.into());
    assert_eq!(
        requested, HARNESS_CONTEXT,
        "this harness only ever targets the context of the cluster \
         scripts/kind/setup-kueue-cluster.sh creates and deletes",
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

/// Run `kubectl --context <ctx> ...` and return stdout, failing the test on a
/// nonzero exit.
///
/// A nonzero status is never read as an empty result. `get workloads` on a
/// cluster whose Kueue CRDs are missing exits nonzero, and treating that as
/// "zero Workloads" would make every negative assertion below pass against a
/// cluster that has no Kueue at all — the same fail-closed reasoning
/// `deploy/kueue/preflight.sh` documents at its `get_kubectl` helper.
fn kubectl_raw(context: &str, args: &[&str]) -> String {
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

fn kubectl_json(context: &str, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.extend_from_slice(&["-o", "json"]);
    serde_json::from_str(&kubectl_raw(context, &args)).expect("kubectl -o json emits JSON")
}

/// Apply an object by piping it to `kubectl apply -f -`.
fn kubectl_apply(context: &str, namespace: &str, object: &Value) {
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

/// The armed `KubernetesConfig` the live Job assertions render with.
///
/// `cgroup_launcher_mode: Disabled` + `task_run_cgroup_writable_enabled: false`
/// are not a convenience: they mirror the values fixture exactly, and
/// `build_task_run_job` asserts (i.e. PANICS) if a required launcher is
/// rendered without the RuntimeClass. The harness proves that RuntimeClass
/// ABSENT for AC4, so this is the only internally consistent pairing.
/// fbiy-C1 flips both together once the kind node can run the class.
fn armed_harness_config() -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        // Matches the chart's `<djinn.fullname>-<kind>` LocalQueue naming for
        // release `djinn`. Not merely asserted as a string below: a wrong
        // prefix names a LocalQueue that does not exist, and the capture
        // assertion is what would notice.
        kueue_local_queue_prefix: "djinn".into(),
        cgroup_launcher_mode: CgroupLauncherMode::Disabled,
        task_run_cgroup_writable_enabled: false,
        ..KubernetesConfig::for_testing()
    }
}

/// A task-run Job straight out of the real renderer.
fn rendered_task_run_job() -> (Job, String) {
    let task_run_id = uuid::Uuid::now_v7();
    let job = build_task_run_job(
        &armed_harness_config(),
        &task_run_id,
        "harness-project",
        &format!("djinn-taskrun-{task_run_id}"),
        "registry.example/project:harness",
        &[],
        None,
        false,
        None,
    );
    let name = job
        .metadata
        .name
        .clone()
        .expect("the renderer names the Job");
    (job, name)
}

fn job_as_json(job: &Job) -> Value {
    let mut value = serde_json::to_value(job).expect("Job serializes");
    // `k8s-openapi` omits apiVersion/kind on the typed struct; the API server
    // needs both.
    value["apiVersion"] = Value::String("batch/v1".into());
    value["kind"] = Value::String("Job".into());
    value
}

fn strip_label(job: &mut Value, label: &str) {
    for pointer in ["/metadata/labels", "/spec/template/metadata/labels"] {
        if let Some(labels) = job.pointer_mut(pointer).and_then(Value::as_object_mut) {
            labels.remove(label);
        }
    }
}

/// Names of the Workloads in `namespace` owned by the Job `job_name`.
fn workloads_owned_by(context: &str, namespace: &str, job_name: &str) -> Vec<String> {
    let list = kubectl_json(
        context,
        &["-n", namespace, "get", "workloads.kueue.x-k8s.io"],
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
        .filter_map(|workload| workload["metadata"]["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

/// How long a positive capture assertion waits before concluding a Workload
/// will never appear: 120 ticks of 500ms.
const CAPTURE_TICKS: usize = 120;
/// The budget a NEGATIVE assertion waits before concluding absence. It is a
/// named constant, not a smaller literal at each call site, because "zero
/// Workloads" is only a statement about capture if the wait was long enough to
/// have seen one. The positive assertions above measured capture inside the
/// first few ticks.
const ABSENCE_TICKS: usize = 60;
const TICK: Duration = Duration::from_millis(500);

/// Poll until `job_name` owns at least one Workload, or `ticks` elapse.
///
/// Iteration-counted rather than deadline-based: `Instant::now` is a
/// workspace-disallowed method (`clippy.toml`), and `tests/kind_smoke.rs`
/// polls the same way.
fn await_owned_workloads(
    context: &str,
    namespace: &str,
    job_name: &str,
    ticks: usize,
) -> Vec<String> {
    for _ in 0..ticks {
        let owned = workloads_owned_by(context, namespace, job_name);
        if !owned.is_empty() {
            return owned;
        }
        std::thread::sleep(TICK);
    }
    workloads_owned_by(context, namespace, job_name)
}

fn delete_job(context: &str, namespace: &str, job_name: &str) {
    let _ = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            namespace,
            "delete",
            "job",
            job_name,
            "--wait=false",
        ])
        .output();
}

/// AC1 — asserted against the LIVE API server, never against a render.
///
/// Three separate facts, because one "the topology is there" verdict would be
/// satisfied by any one of them:
///
/// 1. the `djinn` namespace carries `djinn.io/kueue-managed=true` — without it
///    Kueue's positive `managedJobsNamespaceSelector` matches nothing and the
///    whole armed install is inert. That the label DOES this rather than merely
///    existing is proven separately by
///    [`live_the_namespace_label_is_what_makes_capture_happen`];
/// 2. the ClusterQueue's `pods` nominalQuota EQUALS the values fixture's
///    `kueue.buildPods`, read out of that file at test time rather than
///    hard-coded here (and `guard_fixture_build_pods_differs_from_the_chart_default`
///    keeps that number off the chart default, so the comparison can tell an
///    install that read the fixture from one that ignored it);
/// 3. all three LocalQueues exist, in the namespace, pointing at that
///    ClusterQueue.
#[test]
#[ignore]
fn live_armed_cluster_carries_the_label_the_quota_and_three_local_queues() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();

    // 1. The namespace label.
    let namespace = kubectl_json(&context, &["get", "namespace", NAMESPACE]);
    assert_eq!(
        namespace["metadata"]["labels"][KUEUE_MANAGED_LABEL].as_str(),
        Some("true"),
        "namespace {NAMESPACE} must carry {KUEUE_MANAGED_LABEL}=true on an armed cluster",
    );

    // 2. The ClusterQueue quota, compared against the fixture on disk.
    let expected_quota = build_pods_in(&yaml_at(VALUES_FIXTURE));
    let queue = kubectl_json(
        &context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let quota_field = queue["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
        .as_array()
        .expect("the flavor covers resources")
        .iter()
        .find(|resource| resource["name"] == "pods")
        .map(|resource| resource["nominalQuota"].clone())
        .expect("the ClusterQueue bounds the pods resource");
    // `nominalQuota` is a `resource.Quantity`, which the API server round-trips
    // as a STRING ("2") even though the chart writes it as a bare number. A
    // test that only handled the number form would fail against the very thing
    // it is supposed to read.
    let quota = quota_field
        .as_u64()
        .or_else(|| quota_field.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("pods nominalQuota is not an integer quantity: {quota_field}"));
    assert_eq!(
        quota, expected_quota,
        "the live ClusterQueue's pods nominalQuota must equal the fixture's kueue.buildPods",
    );

    // 3. The three LocalQueues, each pointing at that ClusterQueue.
    let local_queues = kubectl_json(
        &context,
        &["-n", NAMESPACE, "get", "localqueues.kueue.x-k8s.io"],
    );
    let found: BTreeSet<String> = local_queues["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter(|queue| queue["spec"]["clusterQueue"] == CLUSTER_QUEUE)
        .filter_map(|queue| queue["metadata"]["name"].as_str().map(ToOwned::to_owned))
        .collect();
    let expected: BTreeSet<String> = ["djinn-task-run", "djinn-warm", "djinn-scip"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(
        found, expected,
        "all three LocalQueues must exist in {NAMESPACE} and point at {CLUSTER_QUEUE}",
    );
}

/// AC2 — a Job produced by the REAL renderer, applied to the REAL cluster,
/// produces exactly ONE Workload, and that Workload's `ownerReferences` names
/// that Job.
///
/// Two negative controls follow. The second one contradicts this task's own
/// acceptance criterion; see
/// [`live_stripping_the_build_object_label_does_not_stop_capture`].
#[test]
#[ignore]
fn live_real_task_run_renderer_produces_exactly_one_owned_workload() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    let (job, job_name) = rendered_task_run_job();

    // Assert the renderer's own output first. If the armed renderer ever
    // stopped stamping these, this test must fail HERE, naming the cause,
    // rather than silently observing zero Workloads later.
    assert_eq!(
        job.metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL_KUEUE_QUEUE_NAME))
            .map(String::as_str),
        Some("djinn-task-run"),
        "the armed renderer must target the chart's task-run LocalQueue",
    );
    assert_eq!(
        job.spec.as_ref().and_then(|spec| spec.suspend),
        Some(true),
        "the armed renderer must create the Job suspended so Kueue owns admission",
    );

    kubectl_apply(&context, NAMESPACE, &job_as_json(&job));

    let owned = await_owned_workloads(&context, NAMESPACE, &job_name, CAPTURE_TICKS);
    assert_eq!(
        owned.len(),
        1,
        "exactly one Kueue Workload must own the rendered Job {job_name}, got {owned:?}",
    );

    delete_job(&context, NAMESPACE, &job_name);
}

/// AC2's non-vacuity, corrected to the fence that actually exists.
///
/// 6knv's AC2 says to strip `djinn.io/kueue-build-object` and expect zero
/// Workloads. MEASURED AGAINST THIS CLUSTER ON 2026-07-30, THAT IS FALSE — see
/// [`live_stripping_the_build_object_label_does_not_stop_capture`], which
/// asserts the contradiction instead of describing it. The label stopped being
/// a capture fence when the byte-vendored fork's
///
/// ```yaml
/// objectSelector:
///   matchLabels:
///     djinn.io/kueue-build-object: "true"
/// ```
///
/// was retired for the pinned upstream chart, which exposes no `objectSelector`
/// hook at any version (`deploy/kueue/README.md`, "Scope reduction:
/// objectSelector is gone").
///
/// What DOES decide capture is `kueue.x-k8s.io/queue-name`: with
/// `manageJobsWithoutQueueName` left at its upstream default (commented out in
/// `deploy/helm/djinn-prereqs/values.yaml`, i.e. off), a Job without that label
/// is not managed. So that is the label this control removes.
#[test]
#[ignore]
fn live_stripping_the_queue_name_label_produces_zero_workloads() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    let (job, job_name) = rendered_task_run_job();
    let mut manifest = job_as_json(&job);
    strip_label(&mut manifest, LABEL_KUEUE_QUEUE_NAME);

    kubectl_apply(&context, NAMESPACE, &manifest);

    // The same budget the positive case gets before concluding absence. A
    // shorter wait would make "zero Workloads" a statement about latency
    // rather than about capture.
    let owned = await_owned_workloads(&context, NAMESPACE, &job_name, ABSENCE_TICKS);
    assert!(
        owned.is_empty(),
        "a Job with no {LABEL_KUEUE_QUEUE_NAME} must not be captured, got {owned:?}",
    );

    delete_job(&context, NAMESPACE, &job_name);
}

/// The correction to 6knv's AC2, asserted rather than argued.
///
/// This PASSES while stripping `djinn.io/kueue-build-object` still yields a
/// Workload — i.e. it FAILS if the label ever regains fence semantics. Either
/// outcome is informative, and neither is reachable by reading a chart.
#[test]
#[ignore]
fn live_stripping_the_build_object_label_does_not_stop_capture() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    let (job, job_name) = rendered_task_run_job();
    let mut manifest = job_as_json(&job);
    strip_label(&mut manifest, LABEL_KUEUE_BUILD_OBJECT);

    kubectl_apply(&context, NAMESPACE, &manifest);

    let owned = await_owned_workloads(&context, NAMESPACE, &job_name, CAPTURE_TICKS);
    assert_eq!(
        owned.len(),
        1,
        "measured 2026-07-30: {LABEL_KUEUE_BUILD_OBJECT} is NOT a capture fence — the pinned \
         upstream chart has no objectSelector hook, so capture is decided by the namespace label \
         plus {LABEL_KUEUE_QUEUE_NAME} alone. If this now fails with zero Workloads the fence was \
         restored and 6knv's AC2 became true; record it in deploy/kueue/README.md's \
         scope-reduction section. Got {owned:?}",
    );

    delete_job(&context, NAMESPACE, &job_name);
}

/// AC1's label, proven to DO something rather than merely to exist.
///
/// The same armed Job, in a namespace without `djinn.io/kueue-managed`, must
/// not be captured. This is what turns assertion 1 of
/// [`live_armed_cluster_carries_the_label_the_quota_and_three_local_queues`]
/// from "a string is present on an object" into the admission fact it stands
/// for.
#[test]
#[ignore]
fn live_the_namespace_label_is_what_makes_capture_happen() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    let unlabelled = format!("kueue-harness-unlabelled-{}", uuid::Uuid::now_v7().simple());

    kubectl_apply(
        &context,
        "default",
        &serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": unlabelled },
        }),
    );

    let (job, job_name) = rendered_task_run_job();
    let mut manifest = job_as_json(&job);
    // The renderer stamps `metadata.namespace` from the config (job.rs:619), so
    // the object has to be re-homed to be applied anywhere else. Only the
    // namespace changes: every label, the `suspend` flag and the queue-name
    // stay exactly as the renderer produced them, which is what makes the
    // absence of capture attributable to the namespace and nothing else.
    manifest["metadata"]["namespace"] = Value::String(unlabelled.clone());
    kubectl_apply(&context, &unlabelled, &manifest);

    let owned = await_owned_workloads(&context, &unlabelled, &job_name, ABSENCE_TICKS);
    assert!(
        owned.is_empty(),
        "Kueue must capture nothing in a namespace without {KUEUE_MANAGED_LABEL}, got {owned:?}",
    );

    let _ = Command::new("kubectl")
        .args([
            "--context",
            &context,
            "delete",
            "namespace",
            &unlabelled,
            "--wait=false",
        ])
        .output();
}

/// AC4 — the harness reaches the REAL operator script, and that script's
/// ordering gate is live.
///
/// `deploy/kueue/preflight.sh --mode cutover` must exit **10**: "RuntimeClass
/// djinn-cgroup-writable is absent". That gate took production down twice
/// (v0.7.25, and again on 2026-07-30) — labelling a namespace
/// `djinn.io/kueue-managed=true` on a cluster with no RuntimeClass wedges ALL
/// dispatch, because the Job renderer panics while Kueue holds the Job. This
/// cluster is in exactly that shape (armed namespace, no RuntimeClass), which
/// is why the preflight has something real to refuse.
///
/// The API-server check beside it is the non-vacuity: exit 10 must be the
/// consequence of a genuinely absent RuntimeClass, not of a script that
/// returns 10 for reasons of its own.
#[test]
#[ignore]
fn live_cutover_preflight_exits_10_against_the_armed_harness() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();

    let classes = kubectl_json(&context, &["get", "runtimeclasses.node.k8s.io"]);
    let present: Vec<&str> = classes["items"]
        .as_array()
        .expect("a List has items")
        .iter()
        .filter_map(|class| class["metadata"]["name"].as_str())
        .collect();
    assert!(
        !present.contains(&CGROUP_WRITABLE_RUNTIME_CLASS),
        "the harness must NOT install {CGROUP_WRITABLE_RUNTIME_CLASS} — that is fbiy-C1's job, \
         and its absence is what AC4 measures. Present: {present:?}",
    );

    let output = Command::new("bash")
        .arg(repo_root().join(PREFLIGHT_SCRIPT))
        .args(["--context", &context, "--mode", "cutover"])
        // Required by the preflight before it reads anything else. It is never
        // dialled: the RuntimeClass gate is checked first and exits before the
        // ledger fence is reached, which is itself part of what "the ordering
        // gate is live" means.
        .env(
            "DJINN_PREFLIGHT_DATABASE_URL",
            "postgres://harness:harness@127.0.0.1:5432/djinn-kueue-harness?sslmode=disable",
        )
        .current_dir(repo_root())
        .output()
        .expect("deploy/kueue/preflight.sh is executable");

    assert_eq!(
        exit_code(&output),
        PREFLIGHT_EXIT_MISSING_RUNTIME_CLASS,
        "preflight --mode cutover must exit {PREFLIGHT_EXIT_MISSING_RUNTIME_CLASS} against the \
         armed harness; stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output),
    );
    assert!(
        stderr(&output).contains(CGROUP_WRITABLE_RUNTIME_CLASS),
        "the refusal must name the missing RuntimeClass; stderr: {}",
        stderr(&output),
    );
}
