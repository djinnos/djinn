// Test: eprintln is how the RECORDED (not asserted) measurements of this file
// reach the operator reading the run, and the skip-reason channel for the gated
// half. Mirrors tests/kueue_cluster_harness.rs.
#![allow(clippy::print_stderr)]
//! Disruption containment on the LIVE armed-Kueue cluster (fbiy-B2 / `c6ej`).
//!
//! WHAT THIS PROVES, AND WHY IT IS NOT `runtime_pod_fence_tests.rs`
//! ----------------------------------------------------------------
//! `fbiy-A1` (PR #2833) built the containment: `watch_infra_death` binds one
//! immutable `metadata.uid` on first observation and never re-binds, the
//! Pod-absent-plus-Job-nonterminal arm resolves the watch and foreground-deletes
//! the Job, a replacement Pod carrying a different UID is never adopted, and a
//! cleanly `Complete` Job is excluded so the reaper cannot race a terminal
//! report still on the wire. A1 proved every one of those against an in-process
//! `FakeCluster` and said, in its own module docs, that the fake "does not
//! substitute for the live-cluster proof, which is `fbiy-B2`'s job".
//!
//! This is that proof, and it did not confirm the fake. It measured four things
//! the fake could not represent, three of which contradict the shape both A1 and
//! this task's acceptance criteria assumed. They are recorded here because a
//! model that disagrees with the cluster is the exact failure class `fbiy`
//! exists to catch, and burying the disagreement in a commit message would leave
//! the next reader trusting the model.
//!
//! ### 1. The chart's armed Kueue topology admits NOTHING (two defects)
//!
//! Measured 2026-07-30 against a cluster `scripts/kind/setup-kueue-cluster.sh`
//! had just installed from `deploy/helm/djinn`:
//!
//! * `deploy/helm/djinn/templates/kueue-topology.yaml` renders the ClusterQueue
//!   with **no `namespaceSelector`**. Kueue defaults that field to `null`, which
//!   is *a selector that matches no namespace*. Every captured Workload sat at
//!   `QuotaReserved=False`, `reason: Pending`, `message: workload namespace
//!   doesn't match ClusterQueue selector`. `namespaceSelector: {}` — the empty
//!   selector, which matches everything — is what the chart must render.
//! * That same ClusterQueue covers `["pods"]` alone, while EVERY Job the real
//!   renderer produces requests `cpu` and `memory` (asserted hermetically by
//!   [`guard_the_task_run_renderer_requests_cpu_and_memory`]). Kueue refuses to
//!   assign flavors to a pod set requesting a resource the ClusterQueue does not
//!   cover: `couldn't assign flavors to pod set main: resource memory
//!   unavailable in ClusterQueue`.
//!
//! Either one alone wedges the armed install completely: Jobs are captured,
//! suspended, and never admitted. Both are live-only — a render test sees a
//! ClusterQueue and three LocalQueues and calls it a topology. This file
//! REPAIRS both on the disposable cluster before it can measure anything else
//! (see [`repair_cluster_queue_for_admission`]), records whether the repair was
//! needed, and asserts the post-condition — so when the chart is fixed the
//! repair silently becomes a no-op and nothing here has to change.
//!
//! ### 2. Force-deleting an admitted task-run's Pod mints NO replacement
//!
//! This task's AC1 asks for the disjunction of what MAY happen, and AC2 then
//! assumes a replacement Pod exists to refuse. On a real cluster, with the real
//! render, it does not. `build_task_run_job` sets `backoffLimit: 0`, so the
//! first Pod loss exceeds the backoff limit immediately: the Job goes
//! `FailureTarget` → `Failed` within ~5s, the Workload becomes `Finished`,
//! ClusterQueue `pods` usage falls to 0, and no second Pod is ever created. The
//! maximum transient live-Pod count is **1**, not `kueue.buildPods`. Recorded by
//! [`live_force_deleting_an_admitted_pod_records_the_permitted_disjunction`] as
//! a number, exactly as this task requires, never asserted equal to the quota.
//!
//! A consequence worth stating plainly: because the Job is `Failed` by the time
//! any 15-second poll observes the missing Pod, `watch_infra_death` resolves
//! through the PRE-EXISTING `job_failed_reason` arm. A1's new containment arm is
//! **not reached by a force-deleted Pod at all**.
//!
//! ### 3. The arm A1 built IS reachable — through a Kueue EVICTION
//!
//! Pod-absent-plus-Job-nonterminal has exactly one live producer in this
//! topology, and it is not force-delete: it is Kueue evicting an admitted
//! Workload (measured via `stopPolicy: HoldAndDrain`; preemption, a `Hold`, and
//! a quota reduction are the same code path). Kueue re-suspends the Job and
//! deletes its Pod, leaving `spec.suspend: true`, no `Failed` condition, and the
//! Workload `Evicted` but NOT `Finished`. That is precisely
//! `job_failed_reason() == None && !job_completed_cleanly()`, so the watch takes
//! A1's arm and **foreground-deletes the Job**.
//!
//! Measured immediately afterwards: restoring `stopPolicy: None` makes Kueue
//! re-admit that same Workload and the Job controller mint a NEW Pod with a
//! DIFFERENT UID. So the eviction is recoverable and the Job is owed a Pod —
//! and the containment destroys it. That is the live subject AC2 needed, and
//! [`live_a_kueue_eviction_produces_the_replacement_uid_the_watch_refuses`]
//! drives the REAL `SessionRuntime::watch_infra_death` against it.
//!
//! ### 4. `kube::Client` needs a `CryptoProvider` installed, or it panics
//!
//! `tests/kueue_cluster_harness.rs` measured this and routed around it by
//! driving `kubectl`. That is not available here: proving "the coordinator never
//! adopts the replacement" means calling the coordinator's own watch, which
//! takes a `kube::Client`. So this binary installs the `ring` provider itself
//! (see [`install_crypto_provider`]) behind a dev-only `rustls` dependency —
//! the same line `server/Cargo.toml` already carries for `server/src/main.rs`.
//! Reproduced here on the first line of every live test that builds a client;
//! it changes nothing outside this test binary.
//!
//! TWO HALVES, ONE OF THEM HERMETIC
//! --------------------------------
//! Same split as `tests/kueue_cluster_harness.rs`, for the same reason: the
//! `live_*` tests are `#[ignore]` + `DJINN_TEST_KUEUE_CLUSTER=1` and NO CI lane
//! runs them, so every assertion that must not regress silently was written as a
//! `guard_*` test with no `#[ignore]`, which runs in the ordinary
//! `cargo test -p djinn-k8s` lane on every PR.
//!
//! ISOLATION FROM THE OTHER LIVE HARNESSES
//! ---------------------------------------
//! `fbiy-B1` runs against the same script concurrently. This file therefore owns
//! its own cluster, registry and registry port ([`HARNESS_CLUSTER`] and
//! friends), all distinct from the script's defaults, and
//! [`guard_this_harness_is_disjoint_from_the_b0_defaults_and_the_tilt_cluster`]
//! keeps them that way — `down` DELETES what it is given, so two harnesses
//! sharing a name is two harnesses deleting each other's cluster mid-run.
//!
//! WHAT THIS FILE DOES NOT COVER
//! -----------------------------
//! `c6ej`'s AC4 — the coordinator-restart interleavings — is **not here**. It is
//! the split this task sanctions ("split at the restart boundary"), because
//! killing a coordinator between Job creation and Workload admission requires a
//! coordinator, and this harness deliberately has none: the values fixture
//! disables Postgres and the djinn-server Deployment sits in `ImagePullBackOff`
//! by design. It is `fbiy-B2b`.
//!
//! RUNNING THE LIVE HALF
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up \
//!     --cluster-name djinn-kueue-b2 \
//!     --registry-name djinn-kueue-b2-registry \
//!     --registry-port 5053
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_disruption_conformance -- --ignored --test-threads=1
//! scripts/kind/setup-kueue-cluster.sh down \
//!     --cluster-name djinn-kueue-b2 --registry-name djinn-kueue-b2-registry
//! ```

use std::collections::BTreeSet;
use std::env;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    GrantNextBuildLeaseResult, InvocationLeaseAuthorityRepository, InvocationLeaseAuthorityRow,
    InvocationLeaseMode, QueueBuildLeaseInput,
};
use djinn_k8s::KubernetesConfig;
use djinn_k8s::job::{LABEL_TASK_RUN_ID, build_task_run_job};
use djinn_k8s::launcher::CgroupLauncherMode;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_runtime::{RunHandle, SessionRuntime};
use djinn_supervisor::ConnectionRegistry;
use djinn_supervisor::services::{InvocationLiftDecision, evaluate_invocation_lift};
use k8s_openapi::api::batch::v1::Job;
use serde_json::Value;

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

/// DELIBERATELY NOT the script's default (`djinn-kueue-harness`, which is what
/// `tests/kueue_cluster_harness.rs` and `fbiy-B1` target). `down` deletes the
/// cluster it is given, so a shared name is a shared deletion.
const HARNESS_CLUSTER: &str = "djinn-kueue-b2";
/// kind names its context `kind-<cluster>`. Derived for the same reason the
/// script derives it: the CURRENT context is never consulted, because every
/// context in a Djinn developer's kubeconfig is a live EKS cluster.
const HARNESS_CONTEXT: &str = "kind-djinn-kueue-b2";
const HARNESS_REGISTRY: &str = "djinn-kueue-b2-registry";
const HARNESS_REGISTRY_PORT: &str = "5053";

const NAMESPACE: &str = "djinn";
/// `<djinn.fullname>-kueue` for release `djinn`, per
/// `deploy/helm/djinn/templates/kueue-topology.yaml`.
const CLUSTER_QUEUE: &str = "djinn-kueue";

const SETUP_SCRIPT: &str = "scripts/kind/setup-kueue-cluster.sh";
const VALUES_FIXTURE: &str = "deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml";

/// Exit code owned by `scripts/kind/setup-kueue-cluster.sh`: a refused context
/// or a reserved name.
const EXIT_REFUSED_TARGET: i32 = 3;

/// A workload image that exists, runs as uid 1000, and does nothing.
///
/// The real worker binary is not in play: this file measures Pod IDENTITY and
/// Job lifecycle, both of which are properties of the objects rather than of
/// what runs inside them. The Job is otherwise the real renderer's output, byte
/// for byte, including `backoffLimit: 0` — which is exactly the field that turns
/// out to decide finding 2 above.
const WORKLOAD_IMAGE: &str = "busybox:1.36";

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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn yaml_at(relative: &str) -> serde_yaml::Value {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {relative}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {relative}: {e}"))
}

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
// ===========================================================================

/// This harness must not be able to touch `fbiy-B0`/`fbiy-B1`'s cluster, or the
/// developer's Tilt environment.
///
/// Three separate facts, because any one of them alone is satisfiable by a bug:
/// the script ACCEPTS this file's triple (so the live half targets something
/// that can exist); the script's own DEFAULT cluster is a different one (so the
/// two harnesses are genuinely disjoint rather than accidentally equal today);
/// and a foreign context against this cluster name is refused outright.
#[test]
fn guard_this_harness_is_disjoint_from_the_b0_defaults_and_the_tilt_cluster() {
    let accepted = run_setup_script(&[
        "check",
        "--cluster-name",
        HARNESS_CLUSTER,
        "--registry-name",
        HARNESS_REGISTRY,
        "--registry-port",
        HARNESS_REGISTRY_PORT,
        "--context",
        HARNESS_CONTEXT,
    ]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "the script must accept this file's cluster/registry/port/context; stderr: {}",
        stderr(&accepted),
    );
    assert!(
        stdout(&accepted).contains(&format!("cluster={HARNESS_CLUSTER} "))
            && stdout(&accepted).contains(&format!("context={HARNESS_CONTEXT} ")),
        "the script must derive this file's context from its cluster name, got: {}",
        stdout(&accepted),
    );

    // Disjointness from B0/B1, which run the script with its defaults.
    let defaults = run_setup_script(&["check"]);
    assert_eq!(exit_code(&defaults), 0, "stderr: {}", stderr(&defaults));
    assert!(
        !stdout(&defaults).contains(&format!("cluster={HARNESS_CLUSTER} ")),
        "this harness must NOT share the script's default cluster: `down` deletes what it is \
         given, so a shared name lets two concurrent harnesses destroy each other. Script \
         defaults: {}",
        stdout(&defaults),
    );
    assert!(
        !stdout(&defaults).contains(&format!(
            "registry={HARNESS_REGISTRY}:{HARNESS_REGISTRY_PORT}"
        )),
        "this harness must not share the script's default registry name or published port; \
         script defaults: {}",
        stdout(&defaults),
    );

    // A context this cluster name does not derive is refused, not "used anyway".
    for foreign in ["kind-djinn", "kind-djinn-kueue-harness", "staging", "prod"] {
        let refused = run_setup_script(&[
            "check",
            "--cluster-name",
            HARNESS_CLUSTER,
            "--context",
            foreign,
        ]);
        assert_eq!(
            exit_code(&refused),
            EXIT_REFUSED_TARGET,
            "context {foreign} must be refused with exit {EXIT_REFUSED_TARGET}; stderr: {}",
            stderr(&refused),
        );
    }
}

/// The renderer requests `cpu` and `memory` on every task-run Job.
///
/// This is the hermetic half of live finding 1: the chart's ClusterQueue covers
/// `["pods"]` alone, and Kueue refuses to assign flavors to a pod set that
/// requests a resource the queue does not cover. The live repair
/// ([`repair_cluster_queue_for_admission`]) exists ONLY because this is true, so
/// if the renderer ever stopped requesting them the repair — and the reasoning
/// behind it — would be stale, and this is where that shows up.
///
/// Deliberately NOT phrased as "the chart does not cover them": that assertion
/// would start failing the moment somebody fixes the chart, which is the
/// opposite of what a regression guard should do.
#[test]
fn guard_the_task_run_renderer_requests_cpu_and_memory() {
    let (job, _) = rendered_task_run_job(&armed_harness_config(WORKLOAD_IMAGE));
    let requests = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .map(|pod| &pod.containers)
        .and_then(|containers| containers.first())
        .and_then(|container| container.resources.as_ref())
        .and_then(|resources| resources.requests.as_ref())
        .expect("the task-run renderer sets container resource requests");
    let named: BTreeSet<&str> = requests.keys().map(String::as_str).collect();
    assert!(
        named.contains("cpu") && named.contains("memory"),
        "every rendered task-run Job requests cpu and memory, which is why a ClusterQueue \
         covering only `pods` can never admit one; requested: {named:?}",
    );
}

/// The invocation-lease governor's arming decision fails closed on every input
/// that is not an explicitly enforcing authority row.
///
/// AC3's bound is "the count holding a lifted `cpu.max` never exceeds K". That
/// count is the number of leases the FIFO granted AND whose authority says
/// `Lift`. If the authority projection could answer `Lift` for a missing or
/// unreadable row, the cap would be bounding a population that no longer needs
/// permission to lift, and the live assertion would be measuring nothing.
#[test]
fn guard_evaluate_invocation_lift_fails_closed_unless_the_authority_enforces() {
    assert_eq!(
        evaluate_invocation_lift(Err(())),
        InvocationLiftDecision::Unleased
    );
    assert_eq!(
        evaluate_invocation_lift(Ok(None)),
        InvocationLiftDecision::Unleased
    );
    for (mode, expected) in [
        (InvocationLeaseMode::Off, InvocationLiftDecision::Unleased),
        (InvocationLeaseMode::Shadow, InvocationLiftDecision::Shadow),
        (InvocationLeaseMode::Enforce, InvocationLiftDecision::Lift),
    ] {
        assert_eq!(
            evaluate_invocation_lift(Ok(Some(InvocationLeaseAuthorityRow {
                epoch: 1,
                mode,
                cap: Some(1),
                updated_at: "2026-07-30T00:00:00Z".into(),
            }))),
            expected,
            "authority mode {mode:?} must project to {expected:?}",
        );
    }
}

/// The values fixture must leave room for a cap that BINDS.
///
/// AC3 requires `K < M`, where `M` is the number of admitted Workloads the
/// cluster can hold — bounded by the fixture's `kueue.buildPods` — and `K` is
/// the live invocation-lease cap. The live test derives `K = M - 1`. If the
/// fixture ever dropped `buildPods` to 1 then `K` would be 0, and a cap of 0
/// denies EVERY lease before `grant_next` even reads the queue: the live
/// assertion would go green while proving nothing about a cap at all.
#[test]
fn guard_the_fixture_pods_quota_leaves_room_for_a_binding_cap() {
    let build_pods = yaml_at(VALUES_FIXTURE)["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number");
    assert!(
        build_pods >= 2,
        "the harness fixture must admit at least 2 Workloads so the live invocation-lease cap \
         K = M - 1 is at least 1; a K of 0 denies every lease before the queue is read and would \
         make AC3 vacuous. kueue.buildPods = {build_pods}",
    );
}

// ===========================================================================
// Live half — #[ignore] + DJINN_TEST_KUEUE_CLUSTER=1
// ===========================================================================

/// Returns `false` when the live half is disabled; callers `return` early.
fn live_tests_enabled() -> bool {
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

fn which(bin: &str) -> bool {
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
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The context every live call is pinned to, after TWO independent refusals of
/// anything else — the same discipline `tests/kueue_cluster_harness.rs` uses.
///
/// Guard 1 is the name. Guard 2 is the resolved API-server URL, which catches
/// what guard 1 cannot: a kubeconfig entry NAMED `kind-djinn-kueue-b2` that
/// points somewhere else. kind always serves on loopback; no managed control
/// plane does, and all three contexts in a Djinn developer's kubeconfig are EKS.
fn harness_context() -> String {
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

fn delete_job(context: &str, job_name: &str) {
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
fn clear_task_run_jobs(context: &str) {
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
struct LiveNames {
    service_account: String,
    mirror_pvc: String,
    cache_pvc: String,
}

fn live_names(context: &str) -> LiveNames {
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
fn armed_harness_config(image: &str) -> KubernetesConfig {
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

fn config_for(context: &str, image: &str) -> KubernetesConfig {
    let names = live_names(context);
    KubernetesConfig {
        service_account: names.service_account,
        mirror_pvc: names.mirror_pvc,
        cache_pvc: names.cache_pvc,
        ..armed_harness_config(image)
    }
}

/// A task-run Job straight out of the real renderer, plus its task-run id.
fn rendered_task_run_job(config: &KubernetesConfig) -> (Job, String) {
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

fn job_as_json(job: &Job) -> Value {
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
fn sleep_instead_of_the_worker(manifest: &mut Value) {
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
struct RepairReport {
    namespace_selector_was_absent: bool,
    uncovered_resources: Vec<String>,
}

impl RepairReport {
    fn needed(&self) -> bool {
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
fn repair_cluster_queue_for_admission(context: &str) -> RepairReport {
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

fn pods_nominal_quota(context: &str) -> u64 {
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

const TICK: Duration = Duration::from_millis(500);
/// Ticks a POSITIVE observation waits before concluding something will never
/// happen: 240 x 500ms = 120s. Generous because a kind node pulling nothing
/// still has to bind two `local-path` PVCs.
const AWAIT_TICKS: usize = 240;

/// Every Pod carrying this run's task-run-id label, as `(name, uid, phase)`.
///
/// The same label selector `watch_infra_death` uses, deliberately: an
/// observation made through a different selector would not be evidence about
/// what the watch can see.
fn pods_of(context: &str, task_run_id: &str) -> Vec<(String, String, String)> {
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
fn live_task_run_pods(context: &str) -> Vec<String> {
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
fn await_running_pod(context: &str, task_run_id: &str) -> (String, String) {
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
fn workload_summary(context: &str) -> String {
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
fn workload_condition(context: &str, job_name: &str, kind: &str) -> bool {
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
fn pods_usage(context: &str) -> u64 {
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

fn admitted_workloads(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    queue["status"]["admittedWorkloads"].as_u64().unwrap_or(0)
}

fn job_exists(context: &str, job_name: &str) -> bool {
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
fn ensure_workload_image_on_the_node() {
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
fn launch_task_run(context: &str, config: &KubernetesConfig) -> (String, String) {
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

// ===========================================================================
// AC1 — force-delete the Pod, RETAIN the Job, record the disjunction
// ===========================================================================

/// The permitted disjunction, asserted as a disjunction, plus the maximum
/// transient live-Pod count as a REPORTED NUMBER.
///
/// The proposal hedges deliberately: the Workload MAY become `Finished`, usage
/// MAY fall, the old Job MAY create a replacement Pod, a newly admitted Job MAY
/// overlap. Asserting any single branch produces a flaky test that somebody
/// later weakens, so this asserts membership in the permitted set and RECORDS
/// which branch the cluster actually took.
///
/// Measured 2026-07-30 (Kubernetes 1.31, Kueue 0.19.0): Workload `Finished`,
/// `pods` usage falls to 0, NO replacement Pod, a new Job admits into the freed
/// quota — and the maximum transient live-Pod count is **1**. See finding 2 in
/// the module docs; it is why `backoffLimit: 0` is asserted at launch.
///
/// The load-bearing assertion is the one nothing about the disjunction can
/// satisfy by accident: `pods` usage never exceeds the nominal quota at any
/// sample, across the whole disruption.
#[test]
#[ignore]
fn live_force_deleting_an_admitted_pod_records_the_permitted_disjunction() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    let repair = repair_cluster_queue_for_admission(&context);
    eprintln!("repair needed: {} ({repair:?})", repair.needed());
    ensure_workload_image_on_the_node();

    let config = config_for(&context, WORKLOAD_IMAGE);
    let quota = pods_nominal_quota(&context);

    let (task_run_id, job_name) = launch_task_run(&context, &config);
    let (pod_name, bound_uid) = await_running_pod(&context, &task_run_id);
    assert!(
        workload_condition(&context, &job_name, "Admitted"),
        "the Job must be ADMITTED before it can be disrupted; workloads: {}",
        workload_summary(&context),
    );

    // Force-delete the worker Pod while RETAINING the Job.
    let deleted = Command::new("kubectl")
        .args([
            "--context",
            &context,
            "-n",
            NAMESPACE,
            "delete",
            "pod",
            &pod_name,
            "--grace-period=0",
            "--force",
        ])
        .output()
        .expect("kubectl is on PATH");
    assert!(
        deleted.status.success(),
        "force-deleting {pod_name} failed: {}",
        stderr(&deleted),
    );

    // A second run, launched into whatever capacity the disruption released.
    // This is the "a newly admitted Job MAY overlap" branch, and it is also what
    // makes the maximum-live-Pod census a measurement of something.
    let (overlap_task_run_id, overlap_job_name) = launch_task_run(&context, &config);

    let mut max_live_pods = 0usize;
    let mut max_pods_usage = 0u64;
    let mut replacement_uids: BTreeSet<String> = BTreeSet::new();
    let mut overlap_admitted = false;
    for _ in 0..120 {
        max_live_pods = max_live_pods.max(live_task_run_pods(&context).len());
        max_pods_usage = max_pods_usage.max(pods_usage(&context));
        for (_, uid, _) in pods_of(&context, &task_run_id) {
            if !uid.is_empty() && uid != bound_uid {
                replacement_uids.insert(uid);
            }
        }
        overlap_admitted |= workload_condition(&context, &overlap_job_name, "Admitted");
        std::thread::sleep(TICK);
    }

    let workload_finished = workload_condition(&context, &job_name, "Finished");
    let usage_fell = pods_usage(&context) < quota;
    let job_retained = job_exists(&context, &job_name);

    eprintln!(
        "AC1 RECORDED (never asserted equal to kueue.buildPods={quota}): maximum transient \
         live-Pod count = {max_live_pods}; maximum ClusterQueue pods usage = {max_pods_usage}"
    );
    eprintln!(
        "AC1 disjunction observed: workload_finished={workload_finished} usage_fell={usage_fell} \
         replacement_pod_uids={replacement_uids:?} overlapping_job_admitted={overlap_admitted} \
         old_job_object_retained={job_retained}"
    );

    // The disjunction: at least one of the permitted consequences of losing the
    // Pod must have happened. A cluster where the Workload stayed admitted, the
    // quota stayed spent, no replacement appeared AND nothing else could be
    // admitted has LEAKED the slot, which is the failure mode `fbiy` exists to
    // rule out.
    assert!(
        workload_finished || usage_fell || !replacement_uids.is_empty() || overlap_admitted,
        "losing an admitted task-run's Pod must have at least one permitted consequence — \
         otherwise the quota is leaked. workloads: {}",
        workload_summary(&context),
    );

    // The absolute invariant, sampled throughout rather than once at the end:
    // Kueue never reserves more `pods` than it nominally has.
    assert!(
        max_pods_usage <= quota,
        "ClusterQueue pods usage reached {max_pods_usage}, above its own nominalQuota {quota}",
    );
    assert!(
        max_live_pods >= 1,
        "the census never saw a live task-run Pod, so it measured nothing",
    );

    delete_job(&context, &job_name);
    delete_job(&context, &overlap_job_name);
    let _ = overlap_task_run_id;
}

// ===========================================================================
// AC2 — the replacement UID, and the REAL watch that refuses it
// ===========================================================================

/// The live subject AC2 needs, produced by the only thing that produces it.
///
/// A force-deleted Pod mints no replacement (finding 2). A Kueue EVICTION does:
/// it re-suspends the Job, deletes the Pod, and — when capacity returns —
/// re-admits the same Workload and the Job controller creates a NEW Pod with a
/// DIFFERENT `metadata.uid` under the very same labels. That is the object
/// `fenced_worker_pod` must refuse to adopt, and this drives the REAL
/// `SessionRuntime::watch_infra_death` at it rather than a copy of its logic.
///
/// Three assertions, each of which fails on a different removal:
///
/// 1. the watch RESOLVES — reverting A1's Pod-absent-plus-Job-nonterminal arm
///    makes it hang until the timeout, because the evicted Job is neither
///    `Failed` nor `Complete` and the pre-existing arms have nothing to say;
/// 2. the reason names the ORIGINAL Pod UID — removing the UID comparison from
///    `fenced_worker_pod` makes the watch adopt the re-admitted Pod, see it
///    healthy, and never resolve at all;
/// 3. the Job is GONE from the live API server — a reap that builds
///    `DeleteParams` and never sends them leaves it there.
#[test]
#[ignore]
fn live_a_kueue_eviction_produces_the_replacement_uid_the_watch_refuses() {
    if !live_tests_enabled() {
        return;
    }
    install_crypto_provider();
    let context = harness_context();
    clear_task_run_jobs(&context);
    repair_cluster_queue_for_admission(&context);
    ensure_workload_image_on_the_node();
    let config = config_for(&context, WORKLOAD_IMAGE);

    let (task_run_id, job_name) = launch_task_run(&context, &config);
    let (_, bound_uid) = await_running_pod(&context, &task_run_id);
    assert!(
        workload_condition(&context, &job_name, "Admitted"),
        "the run must be admitted before eviction can mean anything; workloads: {}",
        workload_summary(&context),
    );

    let handle = RunHandle {
        task_run_id: task_run_id.clone(),
        container_id: None,
        // The K8s runtime carries the JOB name here; see
        // `KubernetesRuntime::prepare`.
        pod_ref: Some(job_name.clone()),
        // `SystemTime::now` is workspace-disallowed (`clippy.toml`); this is the
        // same clock `KubernetesRuntime::prepare` stamps a real handle with.
        started_at: SystemClock::new().now(),
    };

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the live watch");
    let reason = tokio_runtime.block_on({
        let context = context.clone();
        let task_run_id = task_run_id.clone();
        let bound_uid = bound_uid.clone();
        let config = config.clone();
        async move {
            let runtime = KubernetesRuntime::from_client(
                kube_client(&context).await,
                config,
                std::sync::Arc::new(ConnectionRegistry::new()),
            );
            let evicting = tokio::task::spawn_blocking(move || {
                // Let the watch bind its immutable fence to the Pod that exists
                // NOW. A watch disrupted before it has ever observed a Pod has
                // nothing to be fenced to and would pass trivially.
                std::thread::sleep(Duration::from_secs(20));
                evict_and_release(&context, &task_run_id, &bound_uid);
            });
            let reason =
                tokio::time::timeout(Duration::from_secs(300), runtime.watch_infra_death(&handle))
                    .await;
            let _ = evicting.await;
            reason
        }
    });
    // Whatever happened, do not leave the queue stopped for the next test.
    set_stop_policy(&context, "None");

    let reason = reason.unwrap_or_else(|_| {
        panic!(
            "the infra-death watch never resolved across a Kueue eviction. The evicted Job is \
             suspended, NOT Failed and NOT Complete, so only fbiy-A1's \
             Pod-absent-plus-Job-nonterminal arm can resolve it. Job still present: {}; \
             workloads: {}",
            job_exists(&context, &job_name),
            workload_summary(&context),
        )
    });
    eprintln!("AC2 live death reason: {reason}");

    assert!(
        reason.contains(&bound_uid),
        "the run stays bound to the Pod UID it launched — the watch must name it rather than \
         adopt whatever Pod the re-admission created. bound_uid={bound_uid}, reason: {reason}",
    );

    // The replacement, when the re-admission got one in front of a poll. This is
    // RECORDED rather than required, because the watch may legitimately resolve
    // inside the eviction window before Kueue re-admits — both orderings are
    // correct behaviour and asserting one produces a flaky test.
    let observed_uids: BTreeSet<String> = pods_of(&context, &task_run_id)
        .into_iter()
        .map(|(_, uid, _)| uid)
        .filter(|uid| !uid.is_empty() && *uid != bound_uid)
        .collect();
    let refused_by_name = reason.contains("refused to adopt replacement Pod UID(s)");
    eprintln!(
        "AC2 RECORDED: replacement Pod UIDs seen after re-admission = {observed_uids:?}; \
         the watch reported them refused by name = {refused_by_name}"
    );
    for uid in &observed_uids {
        assert_ne!(
            uid, &bound_uid,
            "a replacement Pod must carry a different immutable UID",
        );
    }

    // The reap, observed on the API server rather than in a log line.
    let mut reaped = false;
    for _ in 0..AWAIT_TICKS {
        if !job_exists(&context, &job_name) {
            reaped = true;
            break;
        }
        std::thread::sleep(TICK);
    }
    assert!(
        reaped,
        "reconciliation must foreground-delete the old Job so it stops holding quota before the \
         task-run is retried; {job_name} is still on the API server",
    );

    delete_job(&context, &job_name);
}

/// Evict (or release) every admitted Workload in the ClusterQueue.
///
/// `HoldAndDrain` is the smallest live producer of Pod-absent-plus-Job-
/// nonterminal. Preemption and a quota reduction reach the same Kueue code path;
/// this one is the only one an operator can trigger on demand.
fn set_stop_policy(context: &str, policy: &str) {
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
async fn kube_client(context: &str) -> kube::Client {
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
fn evict_and_release(context: &str, task_run_id: &str, fenced_uid: &str) {
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

// ===========================================================================
// AC3 + AC5 — the cap bound, driven from three LIVE Pod UIDs
// ===========================================================================

fn invocation_key(pod_uid: &str) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("invocation-{pod_uid}"),
    }
}

/// Simultaneous lift attempts from the ORIGINAL, the REPLACEMENT and a NEWLY
/// ADMITTED Pod never authorize more than the live invocation-lease cap.
///
/// Both numbers are read live and neither is a constant in this test:
///
/// * `M` is the ClusterQueue's `admittedWorkloads` at its live peak, bounded by
///   the `pods` nominalQuota the chart installed;
/// * `K` is read back out of the durable invocation-lease authority — the row
///   `BuildLeaseService` adopts at runtime and `djinn-server epoch set-cap`
///   writes — after being armed through the real operator API.
///
/// `K < M` is asserted, because a cap at or above the number of admissible
/// Workloads bounds nothing and every assertion under it would be vacuous.
///
/// AC5's four deletions each fail a DIFFERENT assertion here, which is what
/// makes this suite non-vacuous rather than merely green:
///
/// * delete the **invocation queue** (`grant_next`'s queue read) → nothing is
///   ever granted → `authorized == K` fails at 0;
/// * delete the **weighted occupancy SUM** → every queued lease fits → all `M`
///   are granted → `authorized <= K` fails at `M`;
/// * delete the **cap** comparison → same, `authorized <= K` fails;
/// * delete **`bound_pod_uid` authorization** → the replacement UID's lift is
///   accepted → the rejection assertion fails.
///
/// Driven against a real PostgreSQL database rather than a mock repository
/// because the refusal is a locked read-modify-write plus a trigger-enforced
/// immutable column: a mock proves only that the Rust `if` was written.
#[test]
#[ignore]
fn live_simultaneous_lifts_from_three_pod_uids_never_exceed_the_live_cap() {
    if !live_tests_enabled() {
        return;
    }
    let context = harness_context();
    clear_task_run_jobs(&context);
    repair_cluster_queue_for_admission(&context);
    ensure_workload_image_on_the_node();
    let config = config_for(&context, WORKLOAD_IMAGE);

    // --- Three LIVE Pod UIDs -----------------------------------------------
    //
    // Not invented strings: an invented UID cannot show that the fence rejects
    // the object Kubernetes actually created.
    let (first_task_run, first_job) = launch_task_run(&context, &config);
    let (_, original_uid) = await_running_pod(&context, &first_task_run);

    let (second_task_run, second_job) = launch_task_run(&context, &config);
    let (_, newly_admitted_uid) = await_running_pod(&context, &second_task_run);

    let peak_admitted = admitted_workloads(&context);

    // The replacement: evict, then release, and take the UID of the Pod the
    // re-admission creates for the SAME Job.
    evict_and_release(&context, &first_task_run, &original_uid);
    let (_, replacement_uid) = await_running_pod(&context, &first_task_run);

    assert_ne!(
        replacement_uid, original_uid,
        "the re-admitted Pod must carry a different immutable UID, or there is nothing for the \
         fence to refuse",
    );
    let live_uids = [
        original_uid.clone(),
        replacement_uid.clone(),
        newly_admitted_uid.clone(),
    ];
    eprintln!("AC3 live Pod UIDs: {live_uids:?}");

    // --- K and M, both read live -------------------------------------------
    let m = peak_admitted.max(admitted_workloads(&context));
    assert!(
        m >= 2,
        "the live ClusterQueue admitted only {m} Workload(s); a cap cannot be strictly below 1",
    );

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("build a tokio runtime for the durable governor");
    tokio_runtime.block_on(async move {
        let db = Database::open_in_memory().expect("real Postgres test database");
        let authority = InvocationLeaseAuthorityRepository::new(db.clone());
        let seeded = authority.seed_baseline().await.expect("seed the authority");
        // Armed through the real operator API, at a cap derived from the LIVE
        // admitted-Workload count rather than from a literal.
        authority
            .set_mode_and_cap(
                seeded.epoch,
                InvocationLeaseMode::Enforce,
                Some(i64::try_from(m).expect("M fits an i64") - 1),
            )
            .await
            .expect("arm the invocation-lease authority");

        // READ BACK. Everything below uses this value, never the one written.
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
            "the live authority must actually authorize lifts, or the cap bounds a population \
             that never lifts and AC3 measures nothing",
        );
        assert!(
            k < i64::try_from(m).expect("M fits an i64"),
            "the invocation-lease cap K={k} must be strictly below the M={m} Workloads the live \
             ClusterQueue admits, or it bounds nothing",
        );
        eprintln!(
            "AC3 live governor configuration: K={k} (durable authority), M={m} (live ClusterQueue)"
        );

        // --- Drive the lifts, all three at once -----------------------------
        let repository = std::sync::Arc::new(BuildLeaseRepository::new(db.clone()));
        for uid in &live_uids {
            repository
                .queue(&QueueBuildLeaseInput {
                    key: invocation_key(uid),
                    immutable_identity: format!("pod:{uid}"),
                    queue_deadline: None,
                    launch_deadline: None,
                    weight: 1,
                })
                .await
                .unwrap_or_else(|e| panic!("queue an invocation lease for {uid}: {e:?}"));
        }

        // Every grant the FIFO will make at the live cap, taken concurrently so
        // the bound is a property of the locked transaction rather than of the
        // order this test happened to call in.
        let mut granted = Vec::new();
        let mut attempts = tokio::task::JoinSet::new();
        for _ in 0..live_uids.len() {
            let repository = repository.clone();
            attempts.spawn(async move {
                repository
                    .grant_next(k, "2026-07-30T00:00:00.000Z", None)
                    .await
                    .expect("grant_next must not error")
            });
        }
        while let Some(result) = attempts.join_next().await {
            if let GrantNextBuildLeaseResult::Granted(row) = result.expect("grant task panicked") {
                granted.push(row);
            }
        }

        // Bind each grant to the live Pod UID it was queued for. `bind` is the
        // sole operation that carries a Pod identity, so this is the complete
        // lift surface.
        let mut authorized = 0i64;
        for row in &granted {
            let uid = row
                .immutable_identity
                .strip_prefix("pod:")
                .expect("the identity carries its Pod UID")
                .to_owned();
            let token = row.fencing_token.expect("a granted lease carries a token");
            let bound = repository
                .bind(&invocation_key(&uid), token, &uid, None)
                .await
                .unwrap_or_else(|e| panic!("bind the lease for {uid}: {e:?}"));
            assert_eq!(bound.state, BuildLeaseState::Bound);
            assert_eq!(bound.bound_pod_uid.as_deref(), Some(uid.as_str()));
            authorized += 1;
        }

        eprintln!("AC3 authorized lifts = {authorized} against live cap K={k} (M={m})");
        assert!(
            authorized <= k,
            "{authorized} invocations hold a lifted cpu.max, above the live cap K={k}",
        );
        assert_eq!(
            authorized, k,
            "the cap must be REACHED as well as respected: {authorized} of a possible K={k} were \
             authorized, which is what a deleted invocation queue looks like",
        );

        // --- The replacement UID, refused --------------------------------
        //
        // The original's lease, presented with the UID of the Pod the
        // re-admission created. This is the fence AC2 asks for, driven from a
        // UID Kubernetes minted rather than one this test made up.
        let original_key = invocation_key(&original_uid);
        if let Some(row) = repository
            .get(&original_key)
            .await
            .expect("read the original's lease row")
            .filter(|row| row.bound_pod_uid.is_some())
        {
            let token = row.fencing_token.expect("a bound lease carries a token");
            let rejected = repository
                .bind(&original_key, token, &replacement_uid, None)
                .await
                .expect_err(
                    "a lift presenting the LIVE replacement Pod's UID against the original's \
                     lease must be rejected",
                );
            assert!(
                format!("{rejected:?}").contains("pod UID does not match build lease"),
                "the refusal must be the pod-UID fence, not an unrelated error: {rejected:?}",
            );
            let after = repository
                .get(&original_key)
                .await
                .expect("re-read the lease row")
                .expect("the lease row exists");
            assert_eq!(
                after.bound_pod_uid.as_deref(),
                Some(original_uid.as_str()),
                "no rejected lift may have moved the durable Pod binding",
            );
        }
    });

    delete_job(&context, &first_job);
    delete_job(&context, &second_job);
}
