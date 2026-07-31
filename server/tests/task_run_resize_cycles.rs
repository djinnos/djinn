// djinn:allow-oversize — task 1j64's design names `server/tests/task_run_resize_cycles.rs`
// as the single exclusive path for this proof, so the live helpers cannot move
// into a `tests/<name>/mod.rs` sibling without leaving that set. The size is
// deliberate and recorded here rather than left unacknowledged in the report.
//
// `kubectl`, `docker` and the setup script all report through stderr, and the
// skip lines below are the only channel a `--ignored` run has for saying why it
// did nothing. The workspace denies `print_stderr` for library and server code.
#![allow(clippy::print_stderr)]
//! Repeated lift/drop cycle invariants for in-place task-run resize (proposal
//! 3i92, epic `xowm`, task `1j64`), plus the always-on wiring of the epic's
//! no-retirement gate.
//!
//! # The property
//!
//! A limits-only resize moves ONE number: the launcher sidecar's
//! `resources.limits.cpu`. It must leave everything that scheduling, Kueue
//! accounting and CPU shares are computed from exactly where it found it. Over
//! [`CYCLES`] lift/drop cycles against one live Pod, five things must not move:
//!
//! 1. `resources.requests` on every container and init container,
//! 2. `status.*.allocatedResources`,
//! 3. the Pod's QoS class,
//! 4. the NODE's allocated CPU **requests**,
//! 5. the cgroup `cpu.weight` at the pod slice and at the launcher container,
//!
//! while the status-confirmed limit moves correctly on every single cycle.
//!
//! # What this file refuses to accept as evidence
//!
//! * **A written value.** `cpu.weight` is read from the cgroup FILE, on the node
//!   and inside the launcher container. Deriving it from `resources.requests`
//!   would assert that the number we asked for equals the number we asked for.
//!   That is the `cpu.max` versus `cpu.stat` distinction that cost task 7deu a
//!   week: a leaf whose `cpu.max` read four cores while the process burned a
//!   quarter of one, `nr_throttled` at 0 throughout.
//! * **A Pod spec standing in for node accounting.** The Node's allocated
//!   requests are read from the Node's own allocated-resources view. The Pod
//!   spec is the INPUT to that accounting, not the accounting.
//! * **`status.containerStatuses` as confirmation.** The launcher is a native
//!   sidecar: `spec.initContainers[name=cgroup-launcher]`. Confirmation comes
//!   from `status.initContainerStatuses[name=cgroup-launcher]` through the
//!   PRODUCTION reader [`confirm_launcher_cpu`] and from nowhere else. The
//!   source gate below holds that line.
//! * **A loop that reports 40 without running 40.** The completion counter is
//!   incremented only after a cycle's DROP is status-confirmed, and the final
//!   assertion is on the counter, not on the loop bound. A `break` after the
//!   first cycle leaves it at 1.
//! * **An invariant that holds because nothing moved.** Every "did not move"
//!   assertion is paired with a "DID move" witness: the per-cycle confirmations
//!   and the node's allocated LIMITS, which do change under a resize.
//!
//! # Running it
//!
//! ```text
//! scripts/kind/setup-resize-kind-cluster.sh up --cluster-name djinn-resize-1j64 \
//!     --registry-name djinn-resize-1j64-registry --registry-port 5071
//! cd server && DJINN_TEST_RESIZE_CYCLES=1 \
//!     cargo test -p djinn-server --test task_run_resize_cycles -- --ignored --nocapture
//! scripts/kind/setup-resize-kind-cluster.sh down --cluster-name djinn-resize-1j64 \
//!     --registry-name djinn-resize-1j64-registry --registry-port 5071
//! ```
//!
//! The hermetic guards run on every PR with no cluster at all, and they are
//! where the no-retirement gate is invoked by an always-on lane.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::{
    COMPONENT_TASK_RUN_WORKER, LABEL_COMPONENT, LABEL_TASK_RUN_ID, build_task_run_job,
};
use djinn_k8s::launcher::{
    CgroupLauncherMode, LAUNCHER_CONTAINER_NAME, LAUNCHER_IPC_DIR, TASK_RUN_CGROUP_RUNTIME_CLASS,
    apply_launcher_authority_protocol, render_authority_protocol,
};
use djinn_k8s::pod_resize::{
    CpuLimit, RESIZE_SUBRESOURCE, build_resize_patch, confirm_launcher_cpu,
    has_resize_pending_condition,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use serde_json::{Value, json};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
//
// Isolated from every sibling harness. `djinn-resize-pcod`/5067 belongs to task
// pcod, `djinn-kueue-c1`/5061 and `djinn-kueue-b2b`/5055 to the Kueue harnesses,
// and a sibling agent may be running any of them right now.
// ---------------------------------------------------------------------------

const HARNESS_CLUSTER: &str = "djinn-resize-1j64";
/// kind names its context `kind-<cluster>`. DERIVED, never discovered: every
/// context in a Djinn developer's kubeconfig is a live EKS cluster.
const HARNESS_CONTEXT: &str = "kind-djinn-resize-1j64";
const HARNESS_REGISTRY: &str = "djinn-resize-1j64-registry";
const HARNESS_REGISTRY_PORT: &str = "5071";
/// `pods/resize` needs 1.33; the cgroup-writable RuntimeClass needs containerd
/// 2.2, which `kindest/node:v1.35.0` ships.
const HARNESS_K8S_VERSION: &str = "1.35.0";

/// Every OTHER live-cluster harness in this repository, spelled out rather than
/// imported. A disjointness guard that imported the sibling's constant would
/// pass by construction.
const SIBLING_CLUSTERS: &[&str] = &[
    "djinn",
    "djinn-resize-pcod",
    "djinn-kueue-harness",
    "djinn-kueue-b1",
    "djinn-kueue-b2",
    "djinn-kueue-b2b",
    "djinn-kueue-c1",
];
const SIBLING_REGISTRY_PORTS: &[&str] = &["5000", "5001", "5051", "5053", "5055", "5061", "5067"];

const SETUP_SCRIPT: &str = "scripts/kind/setup-resize-kind-cluster.sh";
const WORKFLOW: &str = ".github/workflows/resize-kind.yml";
const THIS_FILE: &str = "server/tests/task_run_resize_cycles.rs";
/// pcod's suite. Read, never edited: it owns the production-path proof and the
/// misleading-status fixture this suite defers to for the live half of AC6.
const PCOD_SUITE: &str = "server/tests/task_run_resize_kind.rs";
const RETENTION_GATE: &str = "scripts/check-resize-cutover-retention.sh";
const RETENTION_SELFTEST: &str = "scripts/test-resize-cutover-retention.mjs";
const PROBE_BUILD_SCRIPT: &str = "server/crates/djinn-k8s/tests/fixtures/governor-probe/build.sh";
const PROBE_IMAGE: &str = "djinn-resize-probe:1j64";
const PROBE_WORKLOAD: &str = "/opt/djinn/workload.bin";
/// How long the probe holds its broker connection open. Comfortably longer than
/// [`CYCLES`] cycles take, and bounded, because a probe that never exits turns a
/// failed run into a hung one.
const PROBE_HOLD_SECONDS: &str = "2400";

const NAMESPACE: &str = "djinn";
const WORKER_CONTAINER_NAME: &str = "worker";

/// The number of complete lift/drop cycles the live proof performs. The task
/// floor is 40; this is a named constant so the floor is asserted against a
/// number rather than against a loop that happens to be written `0..40`.
const CYCLES: usize = 40;
/// The floor itself, separate from the count, so raising `CYCLES` never
/// silently lowers what is being PROVEN.
const MIN_CYCLES: usize = 40;
const _: () = assert!(CYCLES >= MIN_CYCLES);

/// The container CPU limit a `resize-v2` sidecar is born at, taken from the
/// production constant rather than spelled again.
const BIRTH_MILLICORES: u64 = djinn_server::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES;

/// The pod CPU limit the render derives its launcher ceiling from. Two cores
/// rather than the production four: this node is shared with other agents'
/// clusters.
const STOCK_CPU_LIMIT: &str = "2";

/// `Instant::now` is workspace-disallowed, so every wait here is
/// iteration-counted.
const TICK: Duration = Duration::from_millis(400);
const READY_TICKS: usize = 300;
const CONFIRM_TICKS: usize = 150;

/// The two status lists AC3's `allocatedResources` snapshot covers.
///
/// This array is the ONLY place in this file that spells the regular
/// container-status list, and `guard_confirmation_never_reads_container_statuses`
/// enforces that. Limit CONFIRMATION comes from
/// `status.initContainerStatuses[name=cgroup-launcher]` through the production
/// reader and from nowhere else; this snapshot is a byte-identity witness over
/// both lists, which is a different question with a different answer.
const STATUS_LISTS: [&str; 2] = ["initContainerStatuses", "containerStatuses"];

// ---------------------------------------------------------------------------
// Repository helpers.
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("the server package lives one level below the repository root")
        .to_path_buf()
}

fn read_repo_file(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

/// `text` with whole-line comments removed.
///
/// Copied from `server/tests/task_run_resize_kind.rs`, whose doc comment is the
/// canonical statement of this defect class. These are separate test binaries,
/// so the helper is duplicated rather than shared. The rule: a comment cannot
/// call a function, set a variable or arm a workflow step, so it must neither
/// satisfy a positive assertion nor trip a negative one. Both directions have
/// bitten this campaign — this suite's own teardown-trap guard matched prose in
/// a header and stayed green when the real `trap` line was deleted.
///
/// Only WHOLE-line comments are dropped, deliberately: a trailing comment must
/// not be able to launder the code in front of it.
fn code_lines(text: &str, comment_prefix: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with(comment_prefix))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`code_lines`] for Rust. Covers `//`, `///` and `//!`.
fn rust_code(source: &str) -> String {
    code_lines(source, "//")
}

/// [`code_lines`] for shell and YAML, which share the `#` comment marker.
fn script_code(text: &str) -> String {
    code_lines(text, "#")
}

// ---------------------------------------------------------------------------
// The predicates the source gates below rest on.
//
// Each is split out from its guard so the guard's logic is testable against
// synthetic input: otherwise "ignores comments" and "ignores everything" are
// indistinguishable from a green run. Every needle is assembled at compile time
// because the subject of these gates is THE FILE THEY LIVE IN — a needle spelled
// literally on its own assertion line is satisfied by that line and can never
// fail. The `ban` needles here already used this trick; the `presence` ones did
// not, which is the defect this split repairs.
// ---------------------------------------------------------------------------

/// The CODE lines of `source` that name the regular container-status list.
///
/// `initContainerStatuses` is deliberately not a match: the forbidden spelling
/// has a lower-case `c` where that token has a capital one.
fn regular_status_list_code_lines(source: &str) -> Vec<String> {
    let needle = concat!("container", "Statuses");
    rust_code(source)
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// Whether `source` confirms a limit through the PRODUCTION reader, in code.
fn calls_the_production_confirmation_reader(source: &str) -> bool {
    rust_code(source).contains(concat!("confirm_launcher", "_cpu("))
}

/// Whether `source` reads `cpu.weight` as a PATH under a cgroup directory.
fn reads_cpu_weight_as_a_cgroup_path(source: &str) -> bool {
    rust_code(source).contains(concat!("/cpu", ".weight"))
}

/// The CODE line, if any, that derives `cpu.weight` from a request value.
fn cpu_weight_derived_from_a_request(source: &str) -> Option<String> {
    let needle = concat!("cpu", ".weight");
    rust_code(source)
        .lines()
        .find(|line| line.contains(needle) && line.contains("requests"))
        .map(str::to_owned)
}

/// Whether `source` reads the weight at BOTH the node and the launcher.
///
/// The two helper names must appear on a line that also names the cgroup file,
/// so the helpers' own definitions do not satisfy this: only a call that reads
/// `cpu.weight` through them does.
fn reads_the_weight_at_both_sites(source: &str) -> bool {
    let weight = concat!("cpu", ".weight");
    let code = rust_code(source);
    let reads: Vec<&str> = code.lines().filter(|line| line.contains(weight)).collect();
    reads
        .iter()
        .any(|line| line.contains(concat!("node", "_read(")))
        && reads
            .iter()
            .any(|line| line.contains(concat!("launcher", "_read(")))
}

/// Whether `source` parses the NODE's own allocation table.
fn parses_the_node_allocation_table(source: &str) -> bool {
    let code = rust_code(source);
    code.contains(concat!("\"describe\", ", "\"node\""))
        && code.contains(concat!("Allocated ", "resources"))
}

/// Whether `source` INVOKES the shared setup script rather than merely naming it.
///
/// The predecessor of this predicate was `source.contains(SETUP_SCRIPT)`, which
/// the constant's own declaration satisfies: it could not fail while the file
/// compiled.
fn invokes_the_shared_setup_script(source: &str) -> bool {
    rust_code(source).contains(concat!(".arg(repo_root().join(SETUP", "_SCRIPT))"))
}

fn run_setup_script(args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join(SETUP_SCRIPT))
        .args(args)
        .current_dir(repo_root())
        .output()
        .unwrap_or_else(|error| panic!("{SETUP_SCRIPT} is executable: {error}"))
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the script exits rather than dying on a signal")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The isolated-name arguments every invocation of the shared harness carries.
fn harness_args() -> Vec<&'static str> {
    vec![
        "--cluster-name",
        HARNESS_CLUSTER,
        "--registry-name",
        HARNESS_REGISTRY,
        "--registry-port",
        HARNESS_REGISTRY_PORT,
        "--context",
        HARNESS_CONTEXT,
    ]
}

// ===========================================================================
// Hermetic guards. No cluster, no `#[ignore]`; these run on every PR.
// ===========================================================================

/// AC1's first half. The floor is a named constant and it is at least 40.
#[test]
fn guard_the_cycle_count_is_a_named_constant_of_at_least_forty() {
    // Constant, so it is a COMPILE error rather than a test failure: lowering
    // the floor below 40 must not be something a run can get past.
    const { assert!(MIN_CYCLES >= 40) };
    const { assert!(CYCLES >= MIN_CYCLES) };

    let source = read_repo_file(THIS_FILE);
    // The completion counter, not the loop bound, is what the final assertion
    // rests on. Exactly one increment site, and it is asserted against
    // MIN_CYCLES.
    //
    // Every needle is built by concatenation so this guard's OWN text cannot
    // satisfy it. A guard whose subject is the file it lives in and which
    // spells its needle literally passes because of itself.
    let increment = concat!("completed_cycles", " += 1");
    assert_eq!(
        source.matches(increment).count(),
        1,
        "there must be exactly ONE place a cycle is counted as complete. Two increment sites is how a loop reports 40 having run 20."
    );
    assert!(
        source.contains(concat!("completed_cycles", ", CYCLES,")),
        "the completion counter must be compared against the cycle count itself, so a `break` after the first cycle is red rather than merely quieter"
    );
    assert!(
        source.contains(concat!("completed_cycles", " >= MIN_CYCLES")),
        "the completion counter must also be asserted against the task floor, so lowering CYCLES below 40 cannot make the suite pass"
    );
}

/// AC12. The harness cannot touch the Tilt cluster, any sibling harness, or any
/// context it did not derive itself — and it is not a fork of the shared script,
/// it INVOKES it.
#[test]
fn guard_the_harness_is_disjoint_and_reuses_the_shared_script() {
    assert!(
        repo_root().join(SETUP_SCRIPT).is_file(),
        "{SETUP_SCRIPT} must exist; this suite invokes it rather than forking it"
    );
    let source = read_repo_file(THIS_FILE);
    assert!(
        invokes_the_shared_setup_script(&source),
        "this suite must INVOKE the shared setup script, passing it to a `Command`. Merely NAMING it was the previous assertion, and the constant's own declaration satisfied that unconditionally."
    );
    assert!(
        !repo_root()
            .join("scripts/kind/setup-resize-cycles-cluster.sh")
            .exists(),
        "a second copy of the harness is exactly what this task was told not to create"
    );

    for sibling in SIBLING_CLUSTERS {
        assert_ne!(
            HARNESS_CLUSTER, *sibling,
            "this harness DELETES its cluster on teardown; sharing a name with {sibling} would delete a sibling agent's live cluster"
        );
        assert_ne!(HARNESS_REGISTRY, format!("{sibling}-registry"));
    }
    for port in SIBLING_REGISTRY_PORTS {
        assert_ne!(
            HARNESS_REGISTRY_PORT, *port,
            "registry port {port} is published by a sibling harness that may be running right now"
        );
    }
    assert_eq!(
        HARNESS_CONTEXT,
        format!("kind-{HARNESS_CLUSTER}"),
        "the context is DERIVED from the cluster name. A discovered context is an EKS cluster."
    );

    // The shared script accepts this harness's own names ...
    let accepted = run_setup_script(&[&["check"], harness_args().as_slice()].concat());
    assert_eq!(
        exit_code(&accepted),
        0,
        "the shared harness refused this suite's isolated names:\n{}",
        stderr_of(&accepted)
    );

    // ... and refuses a sibling's, so the disjointness above is enforced by the
    // script and not merely asserted by this test.
    let refused = run_setup_script(&[
        "check",
        "--cluster-name",
        "djinn-kueue-c1",
        "--registry-name",
        HARNESS_REGISTRY,
        "--registry-port",
        HARNESS_REGISTRY_PORT,
    ]);
    assert_eq!(
        exit_code(&refused),
        3,
        "the shared harness accepted a sibling's cluster name:\n{}",
        stdout_of(&refused)
    );

    // And it refuses a context this suite did not derive. Every context in a
    // Djinn developer's kubeconfig is a live EKS cluster.
    let foreign = run_setup_script(
        &[
            &["check"],
            harness_args().as_slice(),
            &["--context", "arn:aws:eks:eu-west-3:1:cluster/djinn-prod"],
        ]
        .concat(),
    );
    assert_eq!(
        exit_code(&foreign),
        3,
        "the shared harness accepted an externally supplied context:\n{}",
        stdout_of(&foreign)
    );
}

/// AC12. The 1.30 repository floor is enforced, and the "1.29" older docs
/// carried was measured false and corrected in #2818.
#[test]
fn guard_the_kubernetes_floor_is_enforced() {
    let below = run_setup_script(
        &[
            &["check"],
            harness_args().as_slice(),
            &["--k8s-version", "1.29.0"],
        ]
        .concat(),
    );
    assert_eq!(
        exit_code(&below),
        7,
        "1.29 was accepted; the repository floor is 1.30 (Kueue 0.19 / k8s-openapi v1_30)"
    );
    assert!(
        stderr_of(&below).contains("2818"),
        "the refusal must name the correction so nobody restores the measured-false 1.29 floor:\n{}",
        stderr_of(&below)
    );

    let accepted = run_setup_script(
        &[
            &["check"],
            harness_args().as_slice(),
            &["--k8s-version", HARNESS_K8S_VERSION],
        ]
        .concat(),
    );
    assert_eq!(
        exit_code(&accepted),
        0,
        "this suite's own Kubernetes version was refused:\n{}",
        stderr_of(&accepted)
    );
}

/// AC6's second mutation, source half. Confirmation may never read
/// `status.containerStatuses`: the launcher is a native sidecar and has no entry
/// there, so anything found under that name is a different container or a
/// fabrication.
#[test]
fn guard_confirmation_never_reads_container_statuses() {
    let source = read_repo_file(THIS_FILE);
    let hits = regular_status_list_code_lines(&source);
    assert_eq!(
        hits.len(),
        1,
        "the regular container-status list may be named exactly once, in STATUS_LISTS, and only for the allocatedResources byte-identity snapshot. Found:\n{hits:#?}"
    );
    assert!(
        hits[0].contains("STATUS_LISTS"),
        "the one permitted mention must be the STATUS_LISTS constant, not a confirmation read: {}",
        hits[0]
    );
    assert!(
        calls_the_production_confirmation_reader(&source),
        "confirmation must go through the PRODUCTION reader, which reads the init-container status list and nothing else"
    );
}

/// AC6's second mutation, live half — asserted here rather than duplicated.
///
/// The misleading-status fixture already exists in `pcod`'s suite: a hermetic
/// proof that the production reader refuses a Pod whose only `cgroup-launcher`
/// entry is a REGULAR container, and a live one that applies exactly that Pod to
/// the cluster. Rebuilding either here would be a second copy of the same
/// evidence, so this suite DEFERS to them — and a deferral to a proof nobody
/// checks still exists is an assumption. This guard checks.
#[test]
fn guard_the_misleading_status_fixture_still_exists_and_is_run() {
    // Both proof names are also spelled in that suite's doc comments, so a raw
    // `contains` stays green after the tests themselves are deleted. Only the
    // `fn` definitions count.
    let sibling = rust_code(&read_repo_file(PCOD_SUITE));
    for proof in [
        "the_misleading_container_status_is_not_confirmation",
        "live_the_absent_init_status_is_not_confirmed",
    ] {
        assert!(
            sibling.contains(proof),
            "{PCOD_SUITE} no longer carries `{proof}`. This suite's confirmation reads defer to it for the live half of AC6; if it moved, point this guard at its new home rather than deleting the guard."
        );
    }
    // The workflow's own header comment names that suite too; a commented-out
    // step must not stand in for an armed one.
    let workflow = script_code(&read_repo_file(WORKFLOW));
    assert!(
        workflow.contains("task_run_resize_kind"),
        "the live misleading-status fixture is no longer invoked by any lane"
    );
}

/// AC4's mutation, source half. `cpu.weight` is a cgroup FILE. Deriving it from
/// `resources.requests` asserts that what we asked for equals what we asked for,
/// and stays green even when the kernel applied something else.
#[test]
fn guard_cpu_weight_is_read_from_the_cgroup_file() {
    let source = read_repo_file(THIS_FILE);
    assert!(
        reads_cpu_weight_as_a_cgroup_path(&source),
        "the weight must be read as a PATH under a cgroup directory"
    );
    assert_eq!(
        cpu_weight_derived_from_a_request(&source),
        None,
        "the weight must never be derived from a request value"
    );
    assert!(
        reads_the_weight_at_both_sites(&source),
        "the weight must be read at BOTH the pod slice (on the node) and the launcher container"
    );
}

/// AC5's mutation, source half. Node allocation is read from the Node, not from
/// a Pod spec.
#[test]
fn guard_node_allocation_is_read_from_the_node() {
    let source = read_repo_file(THIS_FILE);
    assert!(
        parses_the_node_allocation_table(&source),
        "the Node's own allocated-resources view is what must be read, and the parser must be anchored on that section's heading. A sum over Pod specs is the INPUT to that accounting, not the accounting."
    );
}

/// AC2's mutation, hermetic half. The production resize body touches ONLY the
/// launcher's `limits.cpu`. A body that also moved `requests` — by as little as
/// 1m — would change scheduling and Kueue accounting, and the live snapshot
/// comparison would catch it; this catches it without a cluster.
#[test]
fn guard_the_resize_patch_touches_only_the_launcher_cpu_limit() {
    let body = build_resize_patch(CpuLimit::from_millis(1_234));
    let document = serde_json::to_value(&body).expect("the resize patch serialises");
    let rendered = serde_json::to_string(&document).expect("serialise");

    assert!(
        !rendered.contains("requests"),
        "the resize body names `requests`. A limits-only resize must not: {rendered}"
    );
    assert!(
        !rendered.contains("\"containers\""),
        "the resize body names the regular container list. The launcher is an init container: {rendered}"
    );
    let init = document["spec"]["initContainers"]
        .as_array()
        .expect("the resize body targets the initContainers array");
    assert_eq!(
        init.len(),
        1,
        "the body must name exactly the launcher, so the strategic merge on `patchMergeKey: name` leaves every other container untouched"
    );
    assert_eq!(init[0]["name"], LAUNCHER_CONTAINER_NAME);
    assert_eq!(init[0]["resources"]["limits"]["cpu"], json!("1234m"));
    assert!(
        init[0]["resources"]["requests"].is_null(),
        "the body carries requests for the launcher: {rendered}"
    );

    // Strategic, not merge: `Patch::Merge` on this shape answers
    // `limits: Forbidden: resource limits cannot be removed`.
    let source = rust_code(&read_repo_file(THIS_FILE));
    assert!(
        source.contains("\"strategic\""),
        "the PATCH must be strategic; the initContainers array carries patchMergeKey: name and a JSON-merge body would replace the whole array"
    );
}

/// AC11. The no-retirement gate is INVOKED by an always-on lane — this one — and
/// its exit code is the contract.
#[test]
fn the_no_retirement_gate_passes_and_its_self_test_is_green() {
    let gate = repo_root().join(RETENTION_GATE);
    assert!(gate.is_file(), "{RETENTION_GATE} is missing");
    let selftest = repo_root().join(RETENTION_SELFTEST);
    assert!(selftest.is_file(), "{RETENTION_SELFTEST} is missing");

    let checked = Command::new("sh")
        .arg(&gate)
        .current_dir(repo_root())
        .output()
        .expect("the retention gate is executable");
    assert_eq!(
        exit_code(&checked),
        0,
        "the no-retirement gate rejects this tree. Nothing in the resize cutover retires the containment or credential boundary:\n{}\n{}",
        stdout_of(&checked),
        stderr_of(&checked)
    );

    // The gate's own fixtures, through the gate's own shell entry point. This is
    // what makes "stub the script to exit 0" red rather than quietly fine.
    let tested = Command::new("node")
        .args(["--test", RETENTION_SELFTEST])
        .current_dir(repo_root())
        .output()
        .expect("node is required to run the retention gate's self-test");
    assert!(
        tested.status.success(),
        "the retention gate's fixture self-test failed:\n{}\n{}",
        stdout_of(&tested),
        stderr_of(&tested)
    );
}

/// A live proof that never runs is not a proof. The workflow must carry this
/// suite's job, its isolated names, an `--ignored` invocation, and an
/// unconditional teardown.
#[test]
fn guard_the_live_lane_is_wired() {
    // Comment-stripped: a step that is commented out arms nothing, and this
    // workflow's prose already names the job, the suite and the cluster.
    let workflow = script_code(&read_repo_file(WORKFLOW));
    for needle in [
        "resize-cycles",
        HARNESS_CLUSTER,
        HARNESS_REGISTRY,
        HARNESS_REGISTRY_PORT,
        "task_run_resize_cycles",
        "--ignored",
        SETUP_SCRIPT,
        RETENTION_SELFTEST,
    ] {
        assert!(
            workflow.contains(needle),
            "{WORKFLOW} does not mention `{needle}`; the live lane is not wired"
        );
    }
    assert!(
        workflow.contains("DJINN_TEST_RESIZE_CYCLES"),
        "without the enabling variable the `--ignored` run skips every live proof and reports success"
    );
    assert!(
        workflow.contains("if: always()"),
        "teardown must run pass or fail; a cluster left standing holds this node's disk and the next run's quota"
    );

    // The suite's own count of live proofs, so deleting one fails the ordinary
    // PR lane rather than silently shrinking the dispatch lane.
    let source = rust_code(&read_repo_file(THIS_FILE));
    let live_proofs = source.matches("#[ignore = \"live:").count();
    assert_eq!(
        live_proofs, 1,
        "this suite declares {live_proofs} live proofs; the workflow and this guard both expect 1"
    );
}

// ===========================================================================
// The gates' own self-tests.
//
// A source gate that ignores comments and a source gate that ignores everything
// look identical from a green run. Each predicate above is therefore driven
// against synthetic input here, in BOTH directions.
//
// Two mutations, not one. A PRESENCE assertion is mutated by REMOVING the
// required call — swapping it for a different valid token proves nothing about a
// ban. A BAN is mutated by ADDING the banned token while LEAVING the required
// one in place, or it is the presence arm that goes red and the ban is still
// untested.
// ===========================================================================

#[test]
fn a_whole_line_comment_is_not_code_but_a_trailing_one_does_not_launder_the_line() {
    assert_eq!(rust_code("a\n// b\n  /// c\n//! d\ne"), "a\ne");
    assert_eq!(
        rust_code("call(); // and why"),
        "call(); // and why",
        "a trailing comment must NOT drop the code in front of it: `foo(); // banned` still calls foo"
    );
    assert_eq!(
        script_code("run: x\n# note\n  # indented\nrun: y"),
        "run: x\nrun: y"
    );
}

#[test]
fn the_confirmation_reader_gate_reads_calls_and_not_prose() {
    let call = concat!("confirm_launcher", "_cpu(parsed, target).unwrap();");
    assert!(
        calls_the_production_confirmation_reader(&format!("fn f() {{\n    {call}\n}}\n")),
        "a real call must satisfy the gate"
    );
    assert!(
        !calls_the_production_confirmation_reader(&format!("fn f() {{\n    // {call}\n}}\n")),
        "a commented-out call must NOT satisfy the gate; that is how a deleted confirmation stays green"
    );
    assert!(
        !calls_the_production_confirmation_reader(&format!(
            "//! confirmation goes through {call}\nfn f() {{}}\n"
        )),
        "a doc comment naming the reader must NOT satisfy the gate"
    );
    assert!(
        calls_the_production_confirmation_reader(&format!("fn f() {{\n    {call} // AC6\n}}\n")),
        "a trailing comment must not hide the real call in front of it"
    );
}

#[test]
fn the_regular_status_list_gate_fires_on_code_and_not_on_a_comment() {
    // The BAN mutation ADDS the forbidden read; the permitted STATUS_LISTS
    // mention stays exactly where it is, so what goes red can only be the ban.
    let permitted = concat!(
        "const STATUS_LISTS: [&str; 2] = [\"initContainer",
        "Statuses\", \"container",
        "Statuses\"];"
    );
    let offending = concat!("    let s = &pod[\"status\"][\"container", "Statuses\"];");

    assert_eq!(
        regular_status_list_code_lines(permitted).len(),
        1,
        "the permitted constant is one hit"
    );
    assert_eq!(
        regular_status_list_code_lines(&format!("{permitted}\n{offending}\n")).len(),
        2,
        "a real read of the regular list must be a SECOND hit, which is what reds the gate"
    );
    assert_eq!(
        regular_status_list_code_lines(&format!("{permitted}\n    // {offending}\n")).len(),
        1,
        "a comment naming the forbidden list must not red the gate"
    );
    assert_eq!(
        regular_status_list_code_lines(&format!("{permitted}\n{offending} // stale\n")).len(),
        2,
        "a trailing comment must not launder a real read"
    );
    assert!(
        regular_status_list_code_lines(concat!(
            "let s = &pod[\"status\"][\"initContainer",
            "Statuses\"];"
        ))
        .is_empty(),
        "the init-container list is the PERMITTED confirmation site and must never be a hit"
    );
}

#[test]
fn the_cpu_weight_gates_read_the_cgroup_file_and_ignore_comments() {
    let real = concat!(
        "    let w = node",
        "_read(node, \"/sys/fs/cgroup/cpu",
        ".weight\");"
    );
    let derived = concat!("    let w = spec.resources.requests.cpu", ".weight;");

    assert!(reads_cpu_weight_as_a_cgroup_path(real));
    assert!(
        !reads_cpu_weight_as_a_cgroup_path(&format!("// {real}")),
        "a commented-out read must not satisfy the path requirement"
    );
    assert!(
        reads_cpu_weight_as_a_cgroup_path(&format!("{real} // AC4")),
        "a trailing comment must not hide the real read"
    );

    // The BAN mutation ADDS the request-derived line and KEEPS the cgroup read.
    assert_eq!(cpu_weight_derived_from_a_request(real), None);
    assert!(
        cpu_weight_derived_from_a_request(&format!("{real}\n{derived}\n")).is_some(),
        "deriving the weight from a request value must be caught even when a real cgroup read is also present"
    );
    assert_eq!(
        cpu_weight_derived_from_a_request(&format!("{real}\n// {derived}\n")),
        None,
        "a comment explaining why requests are the wrong source must not red the gate"
    );
    assert!(
        cpu_weight_derived_from_a_request(&format!("{derived} // legacy\n")).is_some(),
        "a trailing comment must not launder a request-derived weight"
    );
}

#[test]
fn the_both_sites_and_node_table_gates_ignore_comments() {
    let both = concat!(
        "    a = node",
        "_read(n, &format!(\"{slice}/cpu",
        ".weight\"));\n    b = launcher",
        "_read(pod, \"/sys/fs/cgroup/cpu",
        ".weight\");"
    );
    assert!(reads_the_weight_at_both_sites(both));
    assert!(
        !reads_the_weight_at_both_sites(&both.replace("    b = launcher", "    // b = launcher")),
        "commenting out the launcher-side read must red the gate"
    );
    assert!(
        !reads_the_weight_at_both_sites(concat!(
            "fn node",
            "_read(n: &str, p: &str) -> String {}\nfn launcher",
            "_read(pod: &str, p: &str) -> String {}"
        )),
        "the helpers' own DEFINITIONS must not satisfy the gate; only a call that reads the cgroup file does"
    );

    let table = concat!(
        "    let d = kubectl_ok(&[\"describe\", ",
        "\"node\", node]);\n    if line.starts_with(\"Allocated ",
        "resources\") {}"
    );
    assert!(parses_the_node_allocation_table(table));
    assert!(
        !parses_the_node_allocation_table(&table.replace("    if line", "    // if line")),
        "commenting out the section anchor must red the gate"
    );
    assert!(
        !parses_the_node_allocation_table(concat!(
            "//! the parser is anchored on the node's Allocated ",
            "resources section, read via \"describe\", ",
            "\"node\""
        )),
        "a doc comment describing the parser must NOT stand in for the parser"
    );
}

#[test]
fn the_setup_script_gate_wants_an_invocation_not_a_mention() {
    let naming = "const SETUP_SCRIPT: &str = \"scripts/kind/setup-resize-kind-cluster.sh\";";
    let invoking = concat!("        .arg(repo_root().join(SETUP", "_SCRIPT))");
    assert!(
        !invokes_the_shared_setup_script(naming),
        "declaring the constant is not invoking the script; this is the assertion that could never fail"
    );
    assert!(invokes_the_shared_setup_script(&format!(
        "{naming}\nfn r() {{\n    Command::new(\"bash\")\n{invoking}\n}}\n"
    )));
    assert!(
        !invokes_the_shared_setup_script(&format!("{naming}\n    // {invoking}")),
        "a commented-out invocation must not satisfy the gate"
    );
}

// ===========================================================================
// The live half.
// ===========================================================================

fn live_tests_enabled() -> bool {
    if std::env::var("DJINN_TEST_RESIZE_CYCLES").as_deref() != Ok("1") {
        eprintln!(
            "SKIP: DJINN_TEST_RESIZE_CYCLES=1 is not set. Bring the cluster up with `{SETUP_SCRIPT} up --cluster-name {HARNESS_CLUSTER} --registry-name {HARNESS_REGISTRY} --registry-port {HARNESS_REGISTRY_PORT}` first."
        );
        return false;
    }
    for tool in ["kubectl", "kind", "docker"] {
        assert!(
            Command::new("which")
                .arg(tool)
                .output()
                .is_ok_and(|output| output.status.success()),
            "{tool} must be on PATH for the live lane"
        );
    }
    true
}

/// Two independent refusals: the context name must be the one this harness
/// derives, and the API server it resolves to must be loopback.
fn assert_harness_context() {
    let server = kubectl_ok(&[
        "config",
        "view",
        "--minify",
        "-o",
        "jsonpath={.clusters[0].cluster.server}",
    ]);
    assert!(
        server.starts_with("https://127.0.0.1:")
            || server.starts_with("https://localhost:")
            || server.starts_with("https://[::1]:"),
        "context {HARNESS_CONTEXT} resolves to {server}, which is not a loopback API server. This suite creates and deletes Jobs; it may only run against the disposable kind cluster."
    );
}

fn kubectl(args: &[&str]) -> Output {
    Command::new("kubectl")
        .arg("--context")
        .arg(HARNESS_CONTEXT)
        .args(args)
        .output()
        .expect("kubectl is executable")
}

fn kubectl_ok(args: &[&str]) -> String {
    let output = kubectl(args);
    assert!(
        output.status.success(),
        "kubectl {args:?} failed: {}",
        stderr_of(&output)
    );
    stdout_of(&output).trim().to_owned()
}

fn kubectl_json(args: &[&str]) -> Value {
    let mut full = args.to_vec();
    full.extend_from_slice(&["-o", "json"]);
    serde_json::from_str(&kubectl_ok(&full)).expect("kubectl -o json returns JSON")
}

fn kubectl_apply(document: &Value) {
    let mut child = Command::new("kubectl")
        .arg("--context")
        .arg(HARNESS_CONTEXT)
        .args(["-n", NAMESPACE, "apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("kubectl apply spawns");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("apply has a stdin");
        stdin
            .write_all(
                serde_json::to_string(document)
                    .expect("serialise")
                    .as_bytes(),
            )
            .expect("write the manifest");
    }
    let output = child.wait_with_output().expect("kubectl apply completes");
    assert!(
        output.status.success(),
        "kubectl apply failed: {}",
        stderr_of(&output)
    );
}

fn launcher_read(pod: &str, path: &str) -> String {
    let output = kubectl(&[
        "-n",
        NAMESPACE,
        "exec",
        pod,
        "-c",
        LAUNCHER_CONTAINER_NAME,
        "--",
        "cat",
        path,
    ]);
    assert!(
        output.status.success(),
        "reading {path} in {pod} failed: {}",
        stderr_of(&output)
    );
    stdout_of(&output).trim_end().to_owned()
}

fn node_name() -> String {
    let output = Command::new("kind")
        .args(["get", "nodes", "--name", HARNESS_CLUSTER])
        .output()
        .expect("kind is executable");
    assert!(
        output.status.success(),
        "kind get nodes: {}",
        stderr_of(&output)
    );
    stdout_of(&output)
        .lines()
        .next()
        .expect("the harness cluster has a node")
        .trim()
        .to_owned()
}

fn node_exec(node: &str, argv: &[&str]) -> Output {
    let mut args = vec!["exec", node];
    args.extend_from_slice(argv);
    Command::new("docker")
        .args(&args)
        .output()
        .expect("docker is executable")
}

fn node_read(node: &str, path: &str) -> String {
    let output = node_exec(node, &["cat", path]);
    assert!(
        output.status.success(),
        "reading {path} on node {node} failed: {}",
        stderr_of(&output)
    );
    stdout_of(&output).trim_end().to_owned()
}

/// The apiserver canonicalises `2000m` to `2`, so every comparison here goes
/// through the production parser rather than through strings. A string
/// comparison reports `never reported 2000m; last observed Some(2000)`.
fn millicores(raw: impl AsRef<str>) -> u64 {
    CpuLimit::parse(raw.as_ref())
        .unwrap_or_else(|error| panic!("parse cpu quantity {}: {error:?}", raw.as_ref()))
        .millis()
}

/// Canonical serialized bytes: object keys sorted at every depth, so the
/// comparison is over the whole subtree rather than over the fields somebody
/// remembered to name. A field-by-field comparison stays green under a change
/// to the field it omits.
fn canonical(value: &Value) -> String {
    fn walk(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: BTreeMap<String, Value> = map
                    .iter()
                    .map(|(key, inner)| (key.clone(), walk(inner)))
                    .collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(walk).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&walk(value)).expect("canonical serialisation")
}

// ---------------------------------------------------------------------------
// The five invariants, each read from the place that would actually drift.
// ---------------------------------------------------------------------------

/// AC2. Every container's and init container's `resources.requests`, keyed by
/// list and name, as canonical bytes.
fn requests_snapshot(pod: &Value) -> String {
    let mut map = BTreeMap::new();
    for list in ["initContainers", "containers"] {
        for entry in pod["spec"][list].as_array().into_iter().flatten() {
            let name = entry["name"].as_str().expect("a container name");
            map.insert(
                format!("{list}/{name}"),
                entry["resources"]["requests"].clone(),
            );
        }
    }
    assert!(
        map.len() >= 2,
        "a rendered task-run Pod has a worker and a launcher; snapshotting {} containers means the traversal found nothing",
        map.len()
    );
    canonical(&json!(map))
}

/// AC3, first half. `status.*.allocatedResources` over BOTH status lists.
fn allocated_resources_snapshot(pod: &Value) -> String {
    let mut map = BTreeMap::new();
    for list in STATUS_LISTS {
        for entry in pod["status"][list].as_array().into_iter().flatten() {
            let name = entry["name"].as_str().expect("a container name");
            map.insert(
                format!("{list}/{name}"),
                entry["allocatedResources"].clone(),
            );
        }
    }
    assert!(
        !map.is_empty(),
        "no container status carried allocatedResources; the traversal found nothing and the comparison would be vacuous"
    );
    canonical(&json!(map))
}

/// AC3, second half. Read from the LIVE Pod status, never inferred from the
/// spec.
fn qos_class(pod: &Value) -> String {
    pod["status"]["qosClass"]
        .as_str()
        .expect("a running Pod reports a QoS class in its status")
        .to_owned()
}

/// AC5. The NODE's own allocated-resources view, in millicores.
///
/// `kubectl describe node` renders the node's allocation table:
///
/// ```text
///   Resource   Requests     Limits
///   --------   --------     ------
///   cpu        1050m (13%)  2350m (29%)
/// ```
///
/// Returned as `(requests, limits)`. The LIMITS half is not decoration: it is
/// the witness that something moved at all, so the requests half cannot pass
/// vacuously on a Pod that never resized.
fn node_allocated_cpu(node: &str) -> (u64, u64) {
    let described = kubectl_ok(&["describe", "node", node]);
    let mut in_section = false;
    for line in described.lines() {
        if line.starts_with("Allocated resources") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("cpu ") || trimmed == "cpu" {
            let fields: Vec<&str> = trimmed.split_whitespace().collect();
            // `cpu <requests> (<pct>) <limits> (<pct>)`
            assert!(
                fields.len() >= 4,
                "unparsed node allocation row: {trimmed}\n{described}"
            );
            return (millicores(fields[1]), millicores(fields[3]));
        }
        if trimmed.starts_with("Events") {
            break;
        }
    }
    panic!("no `cpu` row in the node's Allocated resources section:\n{described}");
}

/// AC4. `cpu.weight` from the kernel's own files: the pod slice on the node and
/// the launcher container's own cgroup root.
struct Weights {
    pod_slice: String,
    launcher: String,
}

fn cpu_weights(node: &str, node_slice: &str, pod: &str) -> Weights {
    Weights {
        pod_slice: node_read(node, &format!("{node_slice}/cpu.weight")),
        launcher: launcher_read(pod, "/sys/fs/cgroup/cpu.weight"),
    }
}

/// The launcher's own view, from `status.initContainerStatuses` and NOWHERE
/// else. The launcher is a native sidecar; it has no regular container status.
struct LauncherStatus {
    container_id: String,
    restart_count: u64,
    limit_millis: Option<u64>,
}

fn launcher_status(pod: &Value) -> LauncherStatus {
    let entry = pod["status"]["initContainerStatuses"]
        .as_array()
        .expect("the launcher is a native sidecar; its status lives in the init-container statuses")
        .iter()
        .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        .unwrap_or_else(|| panic!("no init-container status named {LAUNCHER_CONTAINER_NAME}"));
    LauncherStatus {
        container_id: entry["containerID"]
            .as_str()
            .expect("a running sidecar has a container id")
            .to_owned(),
        restart_count: entry["restartCount"].as_u64().expect("a restart count"),
        limit_millis: entry["resources"]["limits"]["cpu"].as_str().map(millicores),
    }
}

// ---------------------------------------------------------------------------
// Dispatch helpers.
// ---------------------------------------------------------------------------

/// The renderer emits `djinn.app/component=task-run-worker`. A cleanup selector
/// on `app.kubernetes.io/component=task-run` matches NOTHING, and the previous
/// run's Jobs then keep holding the `pods` quota — which surfaces on the next
/// run as "no Pod reached Running", quota exhaustion wearing the costume of a
/// scheduling bug.
fn cleanup_previous_runs() {
    let selector = format!("{LABEL_COMPONENT}={COMPONENT_TASK_RUN_WORKER}");
    for kind in ["job", "pod"] {
        let _ = kubectl(&[
            "-n",
            NAMESPACE,
            "delete",
            kind,
            "-l",
            &selector,
            "--ignore-not-found",
            "--wait=true",
            "--timeout=120s",
        ]);
    }
}

/// The chart-installed object names a live render must be pointed at, found by
/// SUFFIX: the release prefix is the helm release name, so hardcoding either
/// spelling breaks the moment the release is renamed.
struct ChartNames {
    service_account: String,
    mirror_pvc: String,
    projects_pvc: String,
    cache_pvc: String,
}

fn chart_names() -> ChartNames {
    let named = |kind: &str, suffix: &str| -> String {
        kubectl_json(&["-n", NAMESPACE, "get", kind])["items"]
            .as_array()
            .expect("a List has items")
            .iter()
            .filter_map(|item| item["metadata"]["name"].as_str())
            .find(|name| name.ends_with(suffix))
            .unwrap_or_else(|| panic!("the chart installs a {kind} ending in {suffix}"))
            .to_owned()
    };
    ChartNames {
        service_account: named("serviceaccounts", "-taskrun"),
        mirror_pvc: named("persistentvolumeclaims", "-mirrors"),
        projects_pvc: named("persistentvolumeclaims", "-projects"),
        cache_pvc: named("persistentvolumeclaims", "-cache"),
    }
}

struct Rendered {
    task_run_id: String,
    launcher_ceiling_millis: u64,
    document: Value,
}

/// Render a task-run Job exactly the way production does — `build_task_run_job`
/// then `apply_launcher_authority_protocol` under `resize-v2` — and read the
/// ceiling back OUT of the render rather than recomputing it from the config.
fn render_task_run_job(chart: &ChartNames) -> Rendered {
    let mut config = KubernetesConfig::for_testing();
    config.cgroup_launcher_mode = CgroupLauncherMode::Required;
    config.task_run_cgroup_writable_enabled = true;
    config.namespace = NAMESPACE.to_owned();
    config.service_account = chart.service_account.clone();
    config.mirror_pvc = chart.mirror_pvc.clone();
    config.projects_pvc = chart.projects_pvc.clone();
    config.cache_pvc = chart.cache_pvc.clone();
    config.cpu_limit = STOCK_CPU_LIMIT.to_owned();

    let task_run_id = Uuid::now_v7();
    let mut job = build_task_run_job(
        &config,
        &task_run_id,
        "cycles-project",
        "cycles-secret",
        PROBE_IMAGE,
        &[],
        None,
        false,
        None,
    );
    let protocol = render_authority_protocol(
        Some(LauncherAuthorityProtocol::ResizeV2),
        Some("sha256:1j64"),
    )
    .expect("resize-v2 is declarable");
    apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, protocol)
        .expect("the rendered sidecar accepts the resize-v2 ceiling");

    let document = serde_json::to_value(&job).expect("the rendered Job serialises");
    let launcher_ceiling_millis = millicores(
        document["spec"]["template"]["spec"]["initContainers"]
            .as_array()
            .expect("initContainers")
            .iter()
            .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
            .expect("the rendered sidecar")["resources"]["limits"]["cpu"]
            .as_str()
            .expect("the rendered sidecar carries a resize-v2 cpu ceiling"),
    );
    let task_run_id = document["metadata"]["labels"][LABEL_TASK_RUN_ID]
        .as_str()
        .expect("the renderer stamps the task-run id onto the Job")
        .to_owned();

    Rendered {
        task_run_id,
        launcher_ceiling_millis,
        document,
    }
}

/// The image the sidecar runs is the SHIPPED launcher binary at the path the
/// renderer expects; only the worker's entrypoint is replaced. This suite proves
/// resource-accounting invariants, not brokered execution — pcod's suite proves
/// the latter against the same image.
fn ensure_probe_image() {
    let output = Command::new("bash")
        .arg(repo_root().join(PROBE_BUILD_SCRIPT))
        .args([PROBE_IMAGE, HARNESS_CLUSTER])
        .current_dir(repo_root())
        .output()
        .expect("the probe build script is executable");
    assert!(
        output.status.success(),
        "building {PROBE_IMAGE} failed:\n{}",
        stderr_of(&output)
    );
}

fn prepare_holding_job(document: &mut Value, invocation: &str) {
    document["spec"]["backoffLimit"] = json!(0);
    document["spec"]["ttlSecondsAfterFinished"] = json!(600);
    let spec = &mut document["spec"]["template"]["spec"];
    assert_eq!(
        spec["runtimeClassName"], TASK_RUN_CGROUP_RUNTIME_CLASS,
        "the render did not name the writable-cgroup RuntimeClass; the launcher would come up on a read-only /sys/fs/cgroup"
    );
    let worker = spec["containers"]
        .as_array_mut()
        .expect("containers")
        .iter_mut()
        .find(|entry| entry["name"] == WORKER_CONTAINER_NAME)
        .expect("the render carries a worker container");
    worker["image"] = json!(PROBE_IMAGE);
    worker["imagePullPolicy"] = json!("IfNotPresent");
    // The worker must complete the broker HANDSHAKE, not merely exist. The
    // launcher's startup readiness contract gives the worker a bounded window to
    // connect and then exits 1 — measured on the first live run of this suite,
    // where a `sleep` stand-in produced `worker handshake did not complete in
    // time` and a launcher restart mid-cycle-16. That restart is real evidence
    // (the container-id assertion caught it), but it is evidence about the
    // stand-in, not about resize. So the worker is the SHIPPED probe, speaking
    // the real protocol to the unmodified launcher binary in the rendered
    // sidecar.
    //
    // `skip` is written before the probe starts: this suite proves
    // resource-accounting invariants across repeated resizes, so the probe
    // creates its leaf, runs a short workload, declines the lift and holds the
    // connection open for the whole cycle run. pcod's suite is where the lift
    // itself is judged.
    worker["command"] = json!([
        "/bin/sh",
        "-c",
        format!(
            "set -eu; mkdir -p {LAUNCHER_IPC_DIR}; printf 'skip' > {LAUNCHER_IPC_DIR}/decision; exec /opt/djinn/bin/djinn-governor-probe"
        )
    ]);
    worker["args"] = json!([]);

    let environment = worker["env"].as_array_mut().expect("the worker has env");
    for (name, value) in [
        ("DJINN_PROBE_INVOCATION", invocation.to_owned()),
        // Unused on the `skip` arm, but the probe requires it to be declarable.
        ("DJINN_PROBE_FENCE", "0".to_owned()),
        ("DJINN_PROBE_AUTHORITY", "armed".to_owned()),
        (
            "DJINN_PROBE_DECISION_PATH",
            format!("{LAUNCHER_IPC_DIR}/decision"),
        ),
        ("DJINN_PROBE_WORKLOAD", PROBE_WORKLOAD.to_owned()),
        ("DJINN_PROBE_CLAMP_SECONDS", "2".to_owned()),
        ("DJINN_PROBE_LIFTED_SECONDS", "2".to_owned()),
        ("DJINN_PROBE_WORKERS", "1".to_owned()),
        // Longer than 40 cycles can take. The probe's own hold is bounded so a
        // failed run is a failed run rather than a hung one.
        ("DJINN_PROBE_HOLD_SECONDS", PROBE_HOLD_SECONDS.to_owned()),
    ] {
        environment.retain(|entry| entry["name"] != name);
        environment.push(json!({ "name": name, "value": value }));
    }

    // The birth downsize `TaskRunResizeBootstrap` performs before dispatch, so
    // cycle 1 lifts from the same place every later cycle does.
    let launcher = document["spec"]["template"]["spec"]["initContainers"]
        .as_array_mut()
        .expect("the render carries the launcher as an init container")
        .iter_mut()
        .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        .expect("exactly one cgroup-launcher init container");
    launcher["resources"]["limits"]["cpu"] = json!(format!("{BIRTH_MILLICORES}m"));

    document["metadata"]["labels"][LABEL_COMPONENT] = json!(COMPONENT_TASK_RUN_WORKER);
}

/// Wait for a line on the worker's stdout. `probe.fatal` is a hard stop: a probe
/// that failed will never print the line being waited for, and a timeout would
/// report that as "the cluster was slow".
fn await_probe_line(pod: &str, needle: &str) {
    for _ in 0..READY_TICKS {
        let log = stdout_of(&kubectl(&[
            "-n",
            NAMESPACE,
            "logs",
            pod,
            "-c",
            WORKER_CONTAINER_NAME,
        ]));
        assert!(
            !log.contains("probe.fatal"),
            "the brokered probe failed before reaching `{needle}`:\n{log}"
        );
        if log.contains(needle) {
            return;
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "the worker never printed `{needle}`:\n{}",
        stdout_of(&kubectl(&[
            "-n",
            NAMESPACE,
            "logs",
            pod,
            "-c",
            WORKER_CONTAINER_NAME
        ]))
    );
}

fn await_running_pod(task_run_id: &str) -> String {
    let selector = format!("{LABEL_TASK_RUN_ID}={task_run_id}");
    for _ in 0..READY_TICKS {
        let names = kubectl_ok(&[
            "-n",
            NAMESPACE,
            "get",
            "pods",
            "-l",
            &selector,
            "-o",
            "jsonpath={range .items[*]}{.metadata.name}{\" \"}{.status.phase}{\"\\n\"}{end}",
        ]);
        for line in names.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(name), Some("Running")) = (parts.next(), parts.next()) {
                return name.to_owned();
            }
        }
        std::thread::sleep(TICK);
    }
    panic!(
        "no Pod reached Running for {selector}. If a previous run's Jobs were left behind they hold the `pods` quota and this is quota exhaustion, not scheduling:\n{}",
        kubectl_ok(&["-n", NAMESPACE, "get", "pods,jobs"])
    );
}

/// The pod's own cgroup slice on the NODE, located by the Pod uid rather than by
/// guessing the kubelet's naming scheme.
fn pod_slice_path(node: &str, pod: &str) -> String {
    let uid = kubectl_ok(&[
        "-n",
        NAMESPACE,
        "get",
        "pod",
        pod,
        "-o",
        "jsonpath={.metadata.uid}",
    ]);
    let underscored = uid.replace('-', "_");
    let output = node_exec(
        node,
        &[
            "sh",
            "-c",
            &format!(
                "find /sys/fs/cgroup -maxdepth 4 -type d \\( -name '*{uid}*' -o -name '*{underscored}*' \\) | head -1"
            ),
        ],
    );
    let path = stdout_of(&output).trim().to_owned();
    assert!(
        !path.is_empty(),
        "could not locate the cgroup slice of pod {pod} (uid {uid}) on node {node}"
    );
    path
}

/// `Patch::Strategic` is mandatory: the initContainers array carries
/// `patchMergeKey: name`, and `Patch::Merge` answers
/// `limits: Forbidden: resource limits cannot be removed`.
fn resize_launcher(pod: &str, target_millis: u64) {
    let body = build_resize_patch(CpuLimit::from_millis(target_millis));
    let body = serde_json::to_string(&body).expect("serialise the resize patch");
    let output = kubectl(&[
        "-n",
        NAMESPACE,
        "patch",
        "pod",
        pod,
        "--subresource",
        RESIZE_SUBRESOURCE,
        "--type",
        "strategic",
        "--patch",
        &body,
    ]);
    assert!(
        output.status.success(),
        "the resize PATCH failed. A 404 here means this cluster has no pods/resize subresource:\n{}",
        stderr_of(&output)
    );
}

/// Confirmation from `status.initContainerStatuses[name=cgroup-launcher]` in
/// PARSED MILLICORES — never from the PATCH response, never from the mutable
/// `spec`, never from an HTTP 200 — and cross-checked against the PRODUCTION
/// reader on the object the apiserver returned.
fn await_confirmed_limit(pod: &str, target_millis: u64, cycle: usize, phase: &str) -> Value {
    let mut last = Value::Null;
    for _ in 0..CONFIRM_TICKS {
        let document = kubectl_json(&["-n", NAMESPACE, "get", "pod", pod]);
        let observed = launcher_status(&document).limit_millis;
        // The type is inferred from `has_resize_pending_condition`, which takes
        // the production `Pod`. Deserialising into it is itself part of the
        // check: a status shape the production reader cannot parse is not a
        // confirmation.
        let settled = serde_json::from_value(document.clone())
            .ok()
            .filter(|parsed| !has_resize_pending_condition(parsed));
        if observed == Some(target_millis)
            && let Some(parsed) = &settled
        {
            confirm_launcher_cpu(parsed, CpuLimit::from_millis(target_millis)).unwrap_or_else(
                |error| {
                    panic!(
                        "cycle {cycle} {phase}: the production reader refused a limit this suite read as confirmed: {error:?}"
                    )
                },
            );
            return document;
        }
        last = document;
        std::thread::sleep(TICK);
    }
    panic!(
        "cycle {cycle} {phase}: the launcher's init-container status never reported {target_millis}m; last observed {:?}. Millicores, not strings: the apiserver canonicalises 2000m to 2.",
        launcher_status(&last).limit_millis
    );
}

// ---------------------------------------------------------------------------
// The proof.
// ---------------------------------------------------------------------------

/// AC1 through AC6, on one Pod, over [`CYCLES`] complete lift/drop cycles.
#[test]
#[ignore = "live: needs the disposable kind cluster from scripts/kind/setup-resize-kind-cluster.sh"]
fn live_repeated_cycles_move_only_the_launcher_limit() {
    if !live_tests_enabled() {
        return;
    }
    assert_harness_context();
    let node = node_name();
    cleanup_previous_runs();
    ensure_probe_image();

    let chart = chart_names();
    let rendered = render_task_run_job(&chart);
    let ceiling = rendered.launcher_ceiling_millis;
    assert!(
        ceiling > BIRTH_MILLICORES,
        "the rendered ceiling ({ceiling}m) is not above the birth limit ({BIRTH_MILLICORES}m); every cycle would be a no-op and every invariant would hold vacuously"
    );

    let invocation_id = Uuid::now_v7().to_string();
    let mut document = rendered.document.clone();
    prepare_holding_job(&mut document, &invocation_id);
    kubectl_apply(&document);

    let pod = await_running_pod(&rendered.task_run_id);
    // The worker must have completed the broker handshake and created its leaf
    // before the cycles start. Otherwise the launcher's startup readiness
    // contract expires mid-run and the sidecar RESTARTS, which the container-id
    // assertion would report as a failed resize.
    await_probe_line(&pod, "probe.holding");
    let node_slice = pod_slice_path(&node, &pod);

    // ---- The BEFORE snapshot, taken once, before cycle 1. ----
    let born = kubectl_json(&["-n", NAMESPACE, "get", "pod", &pod]);
    assert_eq!(
        launcher_status(&born).limit_millis,
        Some(BIRTH_MILLICORES),
        "the sidecar was not born at the {BIRTH_MILLICORES}m birth limit"
    );
    let container_id = launcher_status(&born).container_id;
    let restart_count = launcher_status(&born).restart_count;

    let requests_before = requests_snapshot(&born);
    let allocated_before = allocated_resources_snapshot(&born);
    let qos_before = qos_class(&born);
    let (node_requests_before, node_limits_at_rest) = node_allocated_cpu(&node);
    let weights_before = cpu_weights(&node, &node_slice, &pod);

    eprintln!(">>> before: qos={qos_before} node allocated cpu={node_requests_before}m");
    eprintln!(
        ">>> before: pod slice cpu.weight={} launcher cpu.weight={}",
        weights_before.pod_slice, weights_before.launcher
    );

    // ---- The cycles. ----
    let mut completed_cycles = 0usize;
    let mut lift_confirmations = 0usize;
    let mut drop_confirmations = 0usize;
    let mut node_limits_saw_a_lift = false;

    for cycle in 1..=CYCLES {
        // Lift.
        resize_launcher(&pod, ceiling);
        let lifted = await_confirmed_limit(&pod, ceiling, cycle, "lift");
        lift_confirmations += 1;

        assert_eq!(
            launcher_status(&lifted).container_id,
            container_id,
            "cycle {cycle}: the container id changed, so this was a RESTART, not an in-place resize"
        );
        assert_eq!(
            launcher_status(&lifted).restart_count,
            restart_count,
            "cycle {cycle}: the restart count moved"
        );
        assert_eq!(
            qos_class(&lifted),
            qos_before,
            "cycle {cycle}: the Pod left its starting QoS class mid-lift"
        );
        assert_eq!(
            requests_snapshot(&lifted),
            requests_before,
            "cycle {cycle}: a lift moved `resources.requests`"
        );

        // The witness that this cycle actually did something: the node's
        // allocated LIMITS reflect the lift while its allocated REQUESTS do not.
        let (node_requests_lifted, node_limits_lifted) = node_allocated_cpu(&node);
        assert_eq!(
            node_requests_lifted, node_requests_before,
            "cycle {cycle}: the Node's allocated CPU REQUESTS moved on a limits-only resize. This is the assertion that protects Kueue accounting."
        );
        if node_limits_lifted != node_limits_at_rest {
            node_limits_saw_a_lift = true;
        }

        // Drop.
        resize_launcher(&pod, BIRTH_MILLICORES);
        let dropped = await_confirmed_limit(&pod, BIRTH_MILLICORES, cycle, "drop");
        drop_confirmations += 1;

        assert_eq!(
            launcher_status(&dropped).container_id,
            container_id,
            "cycle {cycle}: the container id changed on the way down"
        );
        assert_eq!(
            launcher_status(&dropped).restart_count,
            restart_count,
            "cycle {cycle}: the restart count moved on the way down"
        );

        // Counted ONLY here: after this cycle's drop was status-confirmed.
        completed_cycles += 1;

        if cycle % 10 == 0 {
            eprintln!(">>> {completed_cycles} cycles complete");
        }
    }

    // ---- AC1. The counter, not the loop bound. ----
    assert_eq!(
        completed_cycles, CYCLES,
        "the loop ran {CYCLES} times but only {completed_cycles} cycles completed a status-confirmed drop"
    );
    assert!(
        completed_cycles >= MIN_CYCLES,
        "only {completed_cycles} complete lift/drop cycles; the floor is {MIN_CYCLES}"
    );
    // ---- AC6. Every cycle confirmed individually, both ends. ----
    assert_eq!(lift_confirmations, completed_cycles);
    assert_eq!(drop_confirmations, completed_cycles);
    assert!(
        node_limits_saw_a_lift,
        "the Node's allocated CPU LIMITS never moved across {completed_cycles} cycles. Nothing resized, so every invariant below would hold vacuously."
    );

    // ---- The AFTER snapshot. ----
    let settled = kubectl_json(&["-n", NAMESPACE, "get", "pod", &pod]);
    let (node_requests_after, _) = node_allocated_cpu(&node);
    let weights_after = cpu_weights(&node, &node_slice, &pod);

    // AC2, canonical bytes over the whole subtree.
    assert_eq!(
        requests_snapshot(&settled),
        requests_before,
        "`resources.requests` moved across {completed_cycles} lift/drop cycles"
    );
    // AC3.
    assert_eq!(
        allocated_resources_snapshot(&settled),
        allocated_before,
        "`status.*.allocatedResources` moved across {completed_cycles} lift/drop cycles"
    );
    assert_eq!(
        qos_class(&settled),
        qos_before,
        "the Pod's QoS class moved across {completed_cycles} lift/drop cycles"
    );
    // AC4, from the cgroup FILES.
    assert_eq!(
        weights_after.pod_slice, weights_before.pod_slice,
        "the pod slice's cpu.weight moved. This is read from the kernel's file, not derived from a request."
    );
    assert_eq!(
        weights_after.launcher, weights_before.launcher,
        "the launcher container's cpu.weight moved"
    );
    // AC5, from the Node.
    assert_eq!(
        node_requests_after, node_requests_before,
        "the Node's allocated CPU requests moved across {completed_cycles} lift/drop cycles"
    );

    // The Pod ends where it started, and the launcher never restarted.
    assert_eq!(
        launcher_status(&settled).limit_millis,
        Some(BIRTH_MILLICORES)
    );
    assert_eq!(launcher_status(&settled).container_id, container_id);
    assert_eq!(launcher_status(&settled).restart_count, restart_count);

    eprintln!(
        ">>> {completed_cycles} complete cycles: {BIRTH_MILLICORES}m <-> {ceiling}m, {lift_confirmations} lift and {drop_confirmations} drop confirmations"
    );
    eprintln!(
        ">>> unmoved across all of them: requests, allocatedResources, QoS class, node allocated cpu, and both cgroup weight files"
    );

    cleanup_previous_runs();
}
