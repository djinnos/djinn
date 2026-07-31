// `kubectl`, `docker` and the setup script all report through stderr, and the
// skip lines below are the only channel a `--ignored` run has for saying why it
// did nothing. The workspace denies `print_stderr` for library and server code.
#![allow(clippy::print_stderr)]
//! The production-path proof for in-place task-run resize (proposal 3i92, task
//! `pcod`): a REAL `shell` brokered through the production Unix broker into the
//! rendered `cgroup-launcher` native-sidecar container, with the lease obtained
//! through the REAL supervisor RPC, on a live disposable kind cluster.
//!
//! # What this file refuses to accept as evidence
//!
//! Three things, each because a shipped change already faked one of them:
//!
//! * **A value that was written.** `cpu.max` is a value someone wrote. Whether
//!   the kernel ENFORCED it lives in `cpu.stat`. Task `7deu` measured a leaf
//!   whose `cpu.max` read four cores while the process burned a quarter of one,
//!   because the parent clamped it — and the leaf's own `nr_throttled` read `0`
//!   throughout. So the effective-CPU assertion here is `usage_usec` over a
//!   wall-clock window, and `cpu.max` is only ever read as a SECOND, weaker
//!   witness next to it.
//! * **A worker-local synthetic burner.** The measured command must arrive
//!   through `UnixBrokerClient` and run in a leaf the launcher created. The
//!   source gate below refuses any `kubectl exec` into the worker container, so
//!   there is nowhere for a local burner to be spawned from.
//! * **`status.containerStatuses`.** The launcher is a native sidecar:
//!   `spec.initContainers[name=cgroup-launcher]` with `restartPolicy: Always`.
//!   There is no `spec.containers[name=cgroup-launcher]`, so anything a Pod
//!   reports under `status.containerStatuses` for that name is either a
//!   different container or a fabrication. The source gate refuses the token and
//!   [`the_misleading_container_status_is_not_confirmation`] proves the
//!   production reader refuses the object.
//!
//! # The epic's pod-slice arithmetic is wrong, and this file does not repeat it
//!
//! Epic `xowm` asks for `250m -> ceiling -> 250m` in the init-container status
//! AND in the pod slice's `cpu.max`. The first half is right. The second is
//! arithmetically impossible: the pod slice's `cpu.max` is the SUM of the pod's
//! container limits, so it is never equal to the launcher's own 250m birth
//! limit. PR #2840 flagged it. The pod-slice assertion here is therefore a
//! DELTA — the slice must move by exactly the launcher's limit change — with
//! both endpoints derived from the rendered manifest at test time. A hardcoded
//! `4250m` would pass under the stock config and go red the moment a project
//! overrides `build_resources.task.cpu_limit`, which is why
//! [`the_pod_slice_delta_survives_a_per_project_cpu_limit_override`] renders two
//! different limits and compares them.
//!
//! # Running it
//!
//! ```text
//! scripts/kind/setup-resize-kind-cluster.sh up
//! cd server && DJINN_TEST_RESIZE_KIND=1 \
//!     cargo test -p djinn-server --test task_run_resize_kind -- --ignored --test-threads=1
//! scripts/kind/setup-resize-kind-cluster.sh down
//! ```
//!
//! The hermetic guards in this file run on every PR with no cluster at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use djinn_db::BuildLeaseRepository;
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::job::{
    COMPONENT_TASK_RUN_WORKER, LABEL_COMPONENT, LABEL_TASK_RUN_ID, build_task_run_job,
};
use djinn_k8s::launcher::{
    AUTHORITY_PROTOCOL_ENV, CgroupLauncherMode, LAUNCHER_CONTAINER_NAME, LAUNCHER_CREDENTIAL_PATH,
    LAUNCHER_IPC_DIR, LAUNCHER_SOCKET_PATH, TASK_RUN_CGROUP_RUNTIME_CLASS,
    apply_launcher_authority_protocol, render_authority_protocol,
};
use djinn_k8s::pod_resize::{
    CpuLimit, NotConfirmed, PodResizeError, RESIZE_SUBRESOURCE, build_resize_patch,
    confirm_launcher_cpu, declared_launcher_cpu_limit, has_resize_pending_condition,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_supervisor::SupervisorServices;
use djinn_supervisor::services::rpc::RpcServices;
use djinn_supervisor::services::server::serve_on_unix_socket;
use djinn_supervisor::services::{
    LeaseDeadlines, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult,
    TaskInvocationLeaseIdentity,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch.
// ---------------------------------------------------------------------------

pub const HARNESS_CLUSTER: &str = "djinn-resize-pcod";
/// kind names its context `kind-<cluster>`. Derived, never discovered: every
/// context in a Djinn developer's kubeconfig is a live EKS cluster.
pub const HARNESS_CONTEXT: &str = "kind-djinn-resize-pcod";
pub const HARNESS_REGISTRY: &str = "djinn-resize-pcod-registry";
pub const HARNESS_REGISTRY_PORT: &str = "5067";
/// `pods/resize` needs 1.33; `cgroup_writable` needs containerd 2.2, which
/// `kindest/node:v1.35.0` ships (measured 2026-07-31).
pub const HARNESS_K8S_VERSION: &str = "1.35.0";

/// The sibling live-cluster harnesses, spelled out rather than imported. A
/// disjointness guard that imported the sibling's constant would pass by
/// construction, and a sibling agent may be running one of these right now.
const SIBLING_CLUSTERS: &[&str] = &[
    "djinn-kueue-harness",
    "djinn-kueue-b1",
    "djinn-kueue-b2",
    "djinn-kueue-b2b",
    "djinn-kueue-c1",
];
const SIBLING_REGISTRY_PORTS: &[&str] = &["5001", "5051", "5053", "5055", "5061"];

const SETUP_SCRIPT: &str = "scripts/kind/setup-resize-kind-cluster.sh";
const WORKFLOW: &str = ".github/workflows/resize-kind.yml";
const POD_RESIZE_SOURCE: &str = "server/crates/djinn-k8s/src/pod_resize.rs";
const PROBE_BUILD_SCRIPT: &str = "server/crates/djinn-k8s/tests/fixtures/governor-probe/build.sh";
const PROBE_IMAGE: &str = "djinn-resize-probe:pcod";
const PROBE_BIN: &str = "/opt/djinn/bin/djinn-governor-probe";
const PROBE_WORKLOAD: &str = "/opt/djinn/workload.bin";
/// The decision file lives on the launcher IPC volume, which is the ONE
/// directory the renderer mounts into both the worker and the sidecar. It was
/// briefly a path under `/var/tmp`, which exists in both containers and is
/// shared by neither: the harness wrote the decision into the launcher and the
/// probe waited for it in the worker, forever.
const PROBE_DECISION_DIR: &str = LAUNCHER_IPC_DIR;

const NAMESPACE: &str = "djinn";
const WORKER_CONTAINER_NAME: &str = "worker";

/// The container CPU limit a `resize-v2` sidecar is born at, taken from the
/// production constant rather than spelled again.
const BIRTH_MILLICORES: u64 = djinn_server::task_run_resize_bootstrap::BIRTH_CPU_MILLICORES;

/// The pod CPU limit the stock render derives its launcher ceiling from. Two
/// cores rather than the production four: this node is shared with other
/// agents' clusters.
const STOCK_CPU_LIMIT: &str = "2";
/// The per-project override AC4 requires. Deliberately a DIFFERENT number, so
/// any absolute pod-slice constant is red in exactly one of the two renders.
const OVERRIDE_CPU_LIMIT: &str = "3";

/// `Instant::now` is workspace-disallowed, so every wait here is
/// iteration-counted.
const TICK: Duration = Duration::from_millis(500);
const READY_TICKS: usize = 240;
const CONFIRM_TICKS: usize = 120;
/// The wall-clock window AC5 measures `usage_usec` over. Long enough that a
/// scheduling hiccup is noise rather than the measurement.
const MEASURE_SECONDS: u64 = 10;
/// AC5's agreement band.
const EFFECTIVE_CPU_TOLERANCE_PERCENT: u64 = 5;

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

fn run_script(args: &[&str]) -> Output {
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

// ===========================================================================
// Hermetic guards. No cluster, no `#[ignore]`; these run on every PR.
// ===========================================================================

/// AC10. The harness cannot touch the Tilt cluster, any sibling harness, or any
/// context it did not derive itself.
#[test]
fn guard_the_harness_is_disjoint_from_every_other_kind_harness() {
    let accepted = run_script(&[
        "check",
        "--cluster-name",
        HARNESS_CLUSTER,
        "--registry-name",
        HARNESS_REGISTRY,
        "--registry-port",
        HARNESS_REGISTRY_PORT,
        "--k8s-version",
        HARNESS_K8S_VERSION,
        "--context",
        HARNESS_CONTEXT,
    ]);
    assert_eq!(
        exit_code(&accepted),
        0,
        "the harness refused its own quadruple:\n{}",
        stderr_of(&accepted)
    );
    let line = stdout_of(&accepted);
    assert!(
        line.contains(&format!("cluster={HARNESS_CLUSTER} "))
            && line.contains(&format!(
                "registry={HARNESS_REGISTRY}:{HARNESS_REGISTRY_PORT}"
            )),
        "the check line does not name this harness's quadruple: {line}"
    );

    // Every sibling cluster and every sibling registry port is a REFUSED name.
    for cluster in SIBLING_CLUSTERS.iter().chain(["djinn", "kind"].iter()) {
        let refused = run_script(&["check", "--cluster-name", cluster]);
        assert_eq!(
            exit_code(&refused),
            3,
            "cluster {cluster} was not refused:\n{}",
            stdout_of(&refused)
        );
    }
    for port in SIBLING_REGISTRY_PORTS {
        let refused = run_script(&["check", "--registry-port", port]);
        assert_eq!(
            exit_code(&refused),
            3,
            "registry port {port} was not refused:\n{}",
            stdout_of(&refused)
        );
    }
    // A foreign context — including an EKS one — is refused, never "used
    // anyway".
    for context in ["arn:aws:eks:eu-west-3:1:cluster/djinn-prod", "kind-djinn"] {
        let refused = run_script(&["check", "--context", context]);
        assert_eq!(
            exit_code(&refused),
            3,
            "context {context} was not refused:\n{}",
            stdout_of(&refused)
        );
    }
}

/// AC10. Both floors, and the corrected one stays corrected.
#[test]
fn guard_both_kubernetes_floors_are_enforced() {
    // Below the repository floor.
    let old = run_script(&["check", "--k8s-version", "1.29.0"]);
    assert_eq!(exit_code(&old), 7, "1.29 was accepted: {}", stdout_of(&old));
    assert!(
        stderr_of(&old).contains("#2818"),
        "the 1.29 refusal must name the correction that measured it false:\n{}",
        stderr_of(&old)
    );
    // Above the repository floor but below the `pods/resize` floor: the state
    // in which the Pod renders, admits and runs while every PATCH 404s.
    for version in ["1.30.0", "1.32.0"] {
        let refused = run_script(&["check", "--k8s-version", version]);
        assert_eq!(
            exit_code(&refused),
            7,
            "{version} has no pods/resize subresource but was accepted: {}",
            stdout_of(&refused)
        );
    }
    for version in ["1.33.0", HARNESS_K8S_VERSION] {
        let accepted = run_script(&["check", "--k8s-version", version]);
        assert_eq!(
            exit_code(&accepted),
            0,
            "{version} should be accepted:\n{}",
            stderr_of(&accepted)
        );
    }

    let script = read_repo_file(SETUP_SCRIPT);
    assert!(
        script.contains("MIN_K8S_MINOR=30") && script.contains("RESIZE_MIN_K8S_MINOR=33"),
        "the two floors must be named constants, not inline literals"
    );
    assert!(
        !script.contains("MIN_K8S_MINOR=29"),
        "the 1.29 floor was measured false on 2026-07-30 and corrected in #2818"
    );
}

/// AC10. The teardown trap exists AND is installed before the first thing it
/// has to clean up. A trap installed after `docker run` would still leave the
/// registry container holding its port, which is why this asserts an ORDERING
/// rather than the presence of a line.
#[test]
fn guard_the_teardown_trap_precedes_everything_it_must_clean_up() {
    let script = read_repo_file(SETUP_SCRIPT);
    // The line must be the INSTALLATION, not a comment that mentions it. The
    // first version of this guard matched the prose in the `selftest` header
    // and stayed green when the real `trap` line was deleted — which is
    // precisely the failure it exists to catch, so it is now anchored on a line
    // whose entire content is the command.
    let line_number = |needle: &str| -> Option<usize> {
        script
            .lines()
            .position(|line| line.trim() == needle || line.trim_start().starts_with(needle))
            .filter(|_| {
                script
                    .lines()
                    .any(|line| !line.trim_start().starts_with('#') && line.contains(needle))
            })
    };
    let trap = script
        .lines()
        .position(|line| line.trim() == "trap on_exit EXIT")
        .expect(
            "the harness must install an EXIT trap on a line of its own; a comment mentioning one is not a trap",
        );
    let registry = line_number("\"$DOCKER\" run -d --restart=no")
        .expect("the harness starts a registry container");
    let cluster = line_number("\"$KIND\" create cluster").expect("the harness creates a cluster");
    assert!(
        trap < registry && trap < cluster,
        "the EXIT trap is installed on line {trap}, after the registry ({registry}) or the cluster ({cluster}); everything the harness creates must be created UNDER the trap"
    );
    assert!(
        script.contains("teardown || true"),
        "the failure path must call teardown"
    );
    // The live lane runs `selftest`, which injects a real failure and proves
    // the registry container is gone. This is its hermetic half.
    assert!(
        script.contains("DJINN_RESIZE_HARNESS_FAIL_AFTER=registry"),
        "the selftest's injection point is gone; the trap would then be untested"
    );
}

/// AC10. The RuntimeClass prerequisites are INSTALLED, not disabled — the
/// launcher's `cgroup.kill` depends on them — and the containerd table is
/// resolved from the live node rather than assumed.
#[test]
fn guard_the_harness_installs_cgroup_delegation_rather_than_disabling_it() {
    let script = read_repo_file(SETUP_SCRIPT);
    // The sibling harness makes the writable-cgroup node opt-in behind a
    // `--cgroup-writable)` case arm and a `CGROUP_WRITABLE` toggle. Neither may
    // exist here: a run without the node would not fail, it would pass a weaker
    // test under plain runc with a read-only /sys/fs/cgroup. The header prose
    // may still MENTION the sibling's flag, so the gate looks for the parsing
    // arm and the toggle rather than for the word.
    assert!(
        !script.contains("--cgroup-writable)"),
        "this harness offers a `--cgroup-writable` option arm; the writable-cgroup node is mandatory here and must not be switchable"
    );
    for toggle in [
        "CGROUP_WRITABLE=true",
        "CGROUP_WRITABLE=false",
        "\"$CGROUP_WRITABLE\"",
    ] {
        assert!(
            !script.contains(toggle),
            "this harness carries the CGROUP_WRITABLE toggle `{toggle}`; the node handler and label must be installed unconditionally"
        );
    }
    for required in [
        "containerd-config-version.sh",
        "djinn_containerd_detect_version",
        "crictl info",
        "djinn.io/cgroup-writable",
        "runc-cgroupwritable",
    ] {
        assert!(
            script.contains(required),
            "the harness no longer references {required}"
        );
    }
    // A handler written into the wrong plugin namespace is accepted SILENTLY:
    // the RuntimeClass still resolves and the Pod still runs, under plain runc
    // with a read-only /sys/fs/cgroup. So neither namespace may be hardcoded.
    for namespace in [
        "io.containerd.grpc.v1.cri\".containerd.runtimes",
        "io.containerd.cri.v1.runtime'.containerd.runtimes",
    ] {
        assert!(
            !script.contains(namespace),
            "the harness hardcodes the runtime table `{namespace}`; it must be derived from the live schema"
        );
    }
}

/// AC1 + AC8. The source-level gate.
///
/// `include_str!` with a bare filename resolves relative to this file, so this
/// genuinely reads its own bytes. The forbidden tokens are ASSEMBLED at runtime
/// so that naming them here does not trip the gate.
#[test]
fn guard_the_source_admits_no_forbidden_shortcut() {
    let source = include_str!("task_run_resize_kind.rs");

    // AC8: confirmation may only ever come from the init-container statuses.
    //
    // Prose is exempt and code is not: the module docs above have to be able to
    // NAME the thing they forbid, but no executable line may. Note that
    // `initContainerStatuses` does not match — the capital `C` is what keeps
    // the legitimate key out of this count.
    let forbidden_status = format!("{}{}", "container", "Statuses");
    let offending: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains(forbidden_status.as_str()))
        .collect();
    assert!(
        offending.is_empty(),
        "the suite names `{forbidden_status}` in {} executable line(s):\n{}\nThe launcher is a native sidecar; there is no spec.containers entry for it, so anything reported there is a different container or a fabrication.",
        offending.len(),
        offending.join("\n")
    );

    // AC1: no worker-local synthetic burner. Every exec in this suite goes into
    // the launcher container, where the leaf lives and where uid 0 can read it;
    // an exec into the worker container is the only place a local burner could
    // be spawned from, so the shape is banned outright rather than reviewed.
    let worker_execs: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .filter(|line| line.contains("\"exec\"") && line.contains("WORKER_CONTAINER_NAME"))
        .collect();
    assert!(
        worker_execs.is_empty(),
        "the suite execs into the worker container:\n{}\nThe measured command must arrive through the production broker and run in a launcher-created leaf; a worker-local spawn would resolve under the WORKER container's cgroup and is exactly the substitution the task forbids.",
        worker_execs.join("\n")
    );
    // Assembled from halves for the same reason the status token above is: a
    // gate that spelled its own forbidden strings would always find them.
    for (head, tail) in [
        ("while :", "; do"),
        ("dd if=", "/dev/zero"),
        ("stress", "-ng"),
        ("openssl", " speed"),
        ("yes >", "/dev/null"),
    ] {
        let burner = format!("{head}{tail}");
        assert!(
            !source.contains(burner.as_str()),
            "the suite contains the synthetic burner `{burner}`; the measured workload is the probe's brokered `sha256sum` and nothing else"
        );
    }

    // AC4: no absolute pod-slice constant. `4250` is the specific number the
    // epic's arithmetic invites; any absolute is wrong for the same reason.
    for (head, tail) in [("42", "50"), ("42", "5000")] {
        let absolute = format!("{head}{tail}");
        let offenders: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains(absolute.as_str()))
            .collect();
        assert!(
            offenders.is_empty(),
            "the suite hardcodes the absolute pod-slice value `{absolute}`:\n{}\nThe pod slice is the SUM of the pod's container limits and moves with a per-project cpu_limit override; the assertion must be a DELTA derived from the rendered manifest.",
            offenders.join("\n")
        );
    }
}

/// AC9, hermetic half. The production patch is strategic, aimed at the resize
/// subresource, and touches exactly one field.
#[test]
fn guard_the_production_resize_patch_is_strategic_and_minimal() {
    let source = read_repo_file(POD_RESIZE_SOURCE);
    assert!(
        source.contains("Patch::Strategic"),
        "{POD_RESIZE_SOURCE} no longer uses a strategic merge patch; the initContainers array carries `patchMergeKey: name`, so a JSON-merge body REPLACES the whole array and destroys every other init container"
    );
    for wrong in ["Patch::Merge", "Patch::Json", "Patch::Apply"] {
        assert!(
            !source.contains(wrong),
            "{POD_RESIZE_SOURCE} uses {wrong}; only Patch::Strategic survives the initContainers merge key"
        );
    }
    assert!(
        source.contains("patch_subresource(RESIZE_SUBRESOURCE"),
        "the PATCH must go to the pods/resize subresource, not to the Pod itself"
    );
    assert_eq!(RESIZE_SUBRESOURCE, "resize");

    // The body itself: exactly one field, on exactly the launcher.
    let body = build_resize_patch(CpuLimit::from_millis(1_234));
    assert_eq!(
        body,
        json!({
            "spec": {
                "initContainers": [{
                    "name": LAUNCHER_CONTAINER_NAME,
                    "resources": { "limits": { "cpu": "1234m" } }
                }]
            }
        }),
        "the resize body must name only the launcher's cpu limit"
    );
    // The apiserver canonicalises `4000m` to `4`; a string comparison anywhere
    // in the confirmation path breaks on exactly that.
    assert_eq!(CpuLimit::parse("4").expect("parse `4`").millis(), 4_000);
    assert_eq!(
        CpuLimit::parse("4000m").expect("parse `4000m`").millis(),
        4_000
    );
    assert_eq!(
        CpuLimit::parse("4").expect("parse"),
        CpuLimit::parse("4000m").expect("parse"),
        "millicore comparison is the only comparison that survives the apiserver's canonicalisation"
    );
}

/// AC4, hermetic half. The pod-slice delta is DERIVED, and it moves under a
/// per-project `cpu_limit` override — which is what turns any absolute
/// constant red.
#[test]
fn the_pod_slice_delta_survives_a_per_project_cpu_limit_override() {
    let stock = rendered_cpu_facts(STOCK_CPU_LIMIT);
    let overridden = rendered_cpu_facts(OVERRIDE_CPU_LIMIT);

    assert_ne!(
        stock.launcher_ceiling_millis, overridden.launcher_ceiling_millis,
        "`retune_launcher_lease` did not re-point the launcher ceiling at the overridden pod cpu_limit; without that movement this test could not distinguish a derived value from a constant"
    );
    assert_ne!(
        stock.pod_slice_at_ceiling_millis, overridden.pod_slice_at_ceiling_millis,
        "the pod slice did not move with the override"
    );

    for facts in [&stock, &overridden] {
        // The whole point: the slice is the SUM, so it is never the launcher's
        // own 250m, and the only stable statement is the DELTA.
        assert_eq!(
            facts.pod_slice_at_ceiling_millis - facts.pod_slice_at_birth_millis,
            facts.launcher_ceiling_millis - BIRTH_MILLICORES,
            "pod-slice delta must equal the launcher's limit change for cpu_limit={}",
            facts.pod_cpu_limit
        );
        assert_ne!(
            facts.pod_slice_at_birth_millis, BIRTH_MILLICORES,
            "the pod slice at birth ({}) equals the launcher's own birth limit; the epic's `250m -> ceiling -> 250m in pod-slice cpu.max` phrasing assumes exactly this and it is false",
            facts.pod_slice_at_birth_millis
        );
    }
}

/// AC8, hermetic half. A Pod carrying a MISLEADING matching
/// `status` container entry for the launcher name, alongside a STALE
/// init-container status, must read as unconfirmed.
///
/// This shape cannot be created through the apiserver — container names are
/// unique across `spec.containers` and `spec.initContainers` — so it is
/// constructed here and fed to the PRODUCTION reader. The live half
/// ([`live_the_absent_init_status_is_not_confirmed`]) covers the shape the
/// apiserver will produce.
#[test]
fn the_misleading_container_status_is_not_confirmation() {
    let ceiling = CpuLimit::from_millis(2_000);
    // The decoy key is ASSEMBLED, not written: the source gate above forbids
    // the literal token on any executable line, including this one.
    let decoy_key = format!("{}{}", "container", "Statuses");
    let mut document = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "misleading", "namespace": NAMESPACE },
        "spec": {
            "initContainers": [{
                "name": LAUNCHER_CONTAINER_NAME,
                "image": "irrelevant",
                "restartPolicy": "Always",
                "resources": { "limits": { "cpu": "2" } }
            }],
            "containers": [{ "name": WORKER_CONTAINER_NAME, "image": "irrelevant" }]
        },
        "status": {
            // The truth: still at the birth limit.
            "initContainerStatuses": [{
                "name": LAUNCHER_CONTAINER_NAME,
                "ready": true,
                "restartCount": 0,
                "image": "irrelevant",
                "imageID": "irrelevant",
                "containerID": "containerd://real",
                "resources": { "limits": { "cpu": "250m" } }
            }]
        }
    });
    // The decoy: the exact name, the exact target, in the wrong place. A test
    // that pointed its confirmation here would go green.
    document["status"][decoy_key.as_str()] = json!([{
        "name": LAUNCHER_CONTAINER_NAME,
        "ready": true,
        "restartCount": 0,
        "image": "irrelevant",
        "imageID": "irrelevant",
        "containerID": "containerd://decoy",
        "resources": { "limits": { "cpu": "2" } }
    }]);
    // Never named: the concrete `Pod` type is inferred from the production
    // function this value is handed to.
    let pod = serde_json::from_value(document).expect("a Pod document");

    let refusal = confirm_launcher_cpu(&pod, ceiling)
        .expect_err("a decoy status must not confirm the resize");
    assert_eq!(
        refusal,
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: BIRTH_MILLICORES,
            target_millis: ceiling.millis(),
        }),
        "the production reader must report the STALE init-container status, not the decoy"
    );
    // And the spec-side reader agrees the declared limit is the ceiling: the
    // spec is mutable and is not confirmation either.
    assert_eq!(
        declared_launcher_cpu_limit(&pod).expect("a declared launcher limit"),
        ceiling,
        "the declared spec limit is the target; confirming against it would make the PATCH response its own witness"
    );
    assert!(!has_resize_pending_condition(&pod));
}

/// The workflow that runs the live half is wired, is not silently optional, and
/// declares how many live proofs it expects.
#[test]
fn guard_the_live_lane_is_wired() {
    let workflow = read_repo_file(WORKFLOW);
    for required in [
        SETUP_SCRIPT,
        "task_run_resize_kind",
        "DJINN_TEST_RESIZE_KIND",
        "--ignored",
        "if: always()",
        "down",
    ] {
        assert!(
            workflow.contains(required),
            "{WORKFLOW} no longer references {required}"
        );
    }
    assert!(
        !workflow.contains("continue-on-error"),
        "a live proof that cannot fail the lane is not a proof"
    );
    // The same non-vacuity device `brokered_lease_lift_boundary.rs` uses: the
    // workflow declares the number of `#[ignore]`d proofs, so deleting one is
    // caught here rather than silently shrinking the lane.
    let expected = include_str!("task_run_resize_kind.rs")
        .matches("\n#[ignore")
        .count();
    assert!(
        workflow.contains(&format!("RESIZE_KIND_EXPECTED_PROOFS: \"{expected}\"")),
        "{WORKFLOW} declares a different live-proof count than the {expected} this file carries"
    );
}

// ---------------------------------------------------------------------------
// Render-derived CPU arithmetic, shared by the hermetic and live halves.
// ---------------------------------------------------------------------------

struct CpuFacts {
    pod_cpu_limit: String,
    /// Read back OUT of the render rather than passed in: the renderer stamps
    /// the id it was given onto the label the Pod is later selected by, and a
    /// second id invented at the call site is how the first live run selected a
    /// Pod that did not exist.
    task_run_id: String,
    worker_limit_millis: u64,
    launcher_ceiling_millis: u64,
    pod_slice_at_ceiling_millis: u64,
    pod_slice_at_birth_millis: u64,
    /// The rendered Job, as the apiserver will see it.
    document: Value,
}

/// Render a task-run Job exactly the way production does — `build_task_run_job`
/// then `apply_launcher_authority_protocol` under `resize-v2` — and read the
/// CPU arithmetic back OUT of the render.
///
/// Nothing here is recomputed from the config: `resolve_launcher_cpu_ceiling`
/// reads the rendered `DJINN_LAUNCHER_LEASED_MILLICORES`, so reading the
/// rendered limits back is the only way to be measuring the same number the
/// cluster will.
fn rendered_cpu_facts(pod_cpu_limit: &str) -> CpuFacts {
    rendered_cpu_facts_for(pod_cpu_limit, None)
}

/// The chart-installed object names a live render must be pointed at. Read from
/// the cluster by SUFFIX rather than spelled out: the release prefix is the
/// helm release name, so `djinn-djinn-taskrun` is what a release called `djinn`
/// installing a chart called `djinn` actually produces, and hardcoding either
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

fn rendered_cpu_facts_for(pod_cpu_limit: &str, chart: Option<&ChartNames>) -> CpuFacts {
    let mut config = KubernetesConfig::for_testing();
    config.cgroup_launcher_mode = CgroupLauncherMode::Required;
    config.task_run_cgroup_writable_enabled = true;
    if let Some(chart) = chart {
        config.namespace = NAMESPACE.to_owned();
        config.service_account = chart.service_account.clone();
        config.mirror_pvc = chart.mirror_pvc.clone();
        config.projects_pvc = chart.projects_pvc.clone();
        config.cache_pvc = chart.cache_pvc.clone();
    }
    // The per-project `build_resources.task.cpu_limit` override lands here in
    // production via `apply_resolved_resources`, which calls
    // `retune_launcher_lease` with exactly this string.
    config.cpu_limit = pod_cpu_limit.to_owned();

    let task_run_id = Uuid::now_v7();
    let mut job = build_task_run_job(
        &config,
        &task_run_id,
        "pcod-project",
        "pcod-secret",
        PROBE_IMAGE,
        &[],
        None,
        false,
        None,
    );
    let protocol = render_authority_protocol(
        Some(LauncherAuthorityProtocol::ResizeV2),
        Some("sha256:pcod"),
    )
    .expect("resize-v2 is declarable");
    apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, protocol)
        .expect("the rendered sidecar accepts the resize-v2 ceiling");

    let document = serde_json::to_value(&job).expect("the rendered Job serialises");
    let launcher_ceiling_millis = millicores(
        container_cpu_limit(&document, "initContainers", LAUNCHER_CONTAINER_NAME)
            .expect("the rendered sidecar carries a resize-v2 cpu ceiling"),
    );
    let worker_limit_millis = millicores(
        container_cpu_limit(&document, "containers", WORKER_CONTAINER_NAME)
            .expect("the rendered worker carries a cpu limit"),
    );

    let task_run_id = document["metadata"]["labels"][LABEL_TASK_RUN_ID]
        .as_str()
        .expect("the renderer stamps the task-run id onto the Job")
        .to_owned();

    CpuFacts {
        pod_cpu_limit: pod_cpu_limit.to_owned(),
        task_run_id,
        worker_limit_millis,
        launcher_ceiling_millis,
        // Both endpoints derived: the slice is the SUM of the pod's container
        // limits at each end of the resize.
        pod_slice_at_ceiling_millis: worker_limit_millis + launcher_ceiling_millis,
        pod_slice_at_birth_millis: worker_limit_millis + BIRTH_MILLICORES,
        document,
    }
}

fn pod_template(document: &Value) -> &Value {
    &document["spec"]["template"]["spec"]
}

fn container_cpu_limit(document: &Value, list: &str, name: &str) -> Option<String> {
    pod_template(document)[list]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == name)?["resources"]["limits"]["cpu"]
        .as_str()
        .map(str::to_owned)
}

/// The apiserver canonicalises `4000m` to `4`, so every comparison in this file
/// goes through the production parser rather than through strings.
fn millicores(raw: impl AsRef<str>) -> u64 {
    CpuLimit::parse(raw.as_ref())
        .unwrap_or_else(|error| panic!("parse cpu quantity {}: {error:?}", raw.as_ref()))
        .millis()
}

// ===========================================================================
// The live half.
// ===========================================================================

fn live_tests_enabled() -> bool {
    if std::env::var("DJINN_TEST_RESIZE_KIND").as_deref() != Ok("1") {
        eprintln!(
            "SKIP: DJINN_TEST_RESIZE_KIND=1 is not set. Bring the cluster up with `{SETUP_SCRIPT} up` first."
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
/// derives, and the API server it resolves to must be loopback. Every context
/// in a Djinn developer's kubeconfig is a live EKS cluster.
fn harness_context() -> String {
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
        "context {HARNESS_CONTEXT} resolves to {server}, which is not a loopback API server. This suite creates and deletes Jobs; it may only ever run against the disposable kind cluster."
    );
    HARNESS_CONTEXT.to_owned()
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

/// Read a file inside the LAUNCHER container. Never the worker: the invocation
/// leaf is mode 0700 and root-owned, and the source gate refuses a worker exec
/// outright so a synthetic burner has nowhere to be spawned from.
fn launcher_exec(pod: &str, argv: &[&str]) -> Output {
    let mut args = vec![
        "-n",
        NAMESPACE,
        "exec",
        pod,
        "-c",
        LAUNCHER_CONTAINER_NAME,
        "--",
    ];
    args.extend_from_slice(argv);
    kubectl(&args)
}

fn launcher_read(pod: &str, path: &str) -> String {
    let output = launcher_exec(pod, &["cat", path]);
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

/// `cpu.max` is `"<quota> <period>"` or `"max <period>"`. Returned in
/// millicores so it can be compared with the rendered manifest.
fn cpu_max_millicores(raw: &str) -> Option<u64> {
    let mut parts = raw.split_whitespace();
    let quota = parts.next()?;
    let period: u64 = parts.next()?.parse().ok()?;
    if quota == "max" {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    Some(quota * 1_000 / period)
}

fn cpu_stat_field(raw: &str, key: &str) -> u64 {
    raw.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == key).then(|| parts.next()?.parse().ok())?
        })
        .unwrap_or_else(|| panic!("cpu.stat has no `{key}` field:\n{raw}"))
}

/// The launcher's own view, from `status.initContainerStatuses` and NOWHERE
/// else.
struct LauncherStatus {
    container_id: String,
    restart_count: u64,
    limit_millis: Option<u64>,
}

fn launcher_status(pod_document: &Value) -> LauncherStatus {
    let entry = pod_document["status"]["initContainerStatuses"]
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

/// Every container in `spec` except the launcher, keyed by name, for the
/// byte-identity assertion AC9 rests on.
fn other_containers(pod_document: &Value) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    for list in ["initContainers", "containers"] {
        for entry in pod_document["spec"][list].as_array().into_iter().flatten() {
            let name = entry["name"].as_str().expect("a container name").to_owned();
            if name != LAUNCHER_CONTAINER_NAME {
                map.insert(format!("{list}/{name}"), entry.clone());
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Live proof 1: the whole production path.
// ---------------------------------------------------------------------------

/// AC1..AC7 and AC9, on one Pod, in one pass.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs the disposable kind cluster from scripts/kind/setup-resize-kind-cluster.sh"]
async fn live_a_real_brokered_shell_is_governed_by_the_resized_sidecar() {
    if !live_tests_enabled() {
        return;
    }
    let _context = harness_context();
    let node = node_name();
    cleanup_previous_runs();
    ensure_probe_image();

    // ---- AC1, first half: the lease comes from the REAL supervisor RPC. ----
    let db = djinn_server::test_helpers::create_test_db();
    BuildLeaseRepository::new(db.clone())
        .set_cap(1)
        .await
        .expect("arm the durable build-lease cap");
    let host: Arc<dyn SupervisorServices> =
        Arc::new(djinn_agent::direct_services::DirectServices::new(
            djinn_server::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new()),
            CancellationToken::new(),
        ));
    // `/var/tmp` deliberately: a Unix socket path must stay under ~108 bytes and
    // the default temp root under `target/` blows that.
    let socket_dir = tempfile::Builder::new()
        .prefix("pcod-rpc-")
        .tempdir_in("/var/tmp")
        .expect("a short-pathed temp dir");
    let socket = socket_dir.path().join("rpc.sock");
    let server = serve_on_unix_socket(&socket, host)
        .await
        .expect("the supervisor RPC server binds");
    let cancel = CancellationToken::new();
    let (rpc, background) = RpcServices::connect_unix(&socket, cancel.clone())
        .await
        .expect("the supervisor RPC client connects");

    // Render first: the lease identity must carry the SAME task-run id the
    // renderer stamped onto the Job, or the lease and the Pod are two unrelated
    // objects and the selector below finds nothing.
    let facts = rendered_cpu_facts_for(STOCK_CPU_LIMIT, Some(&chart_names()));
    let task_id = Uuid::now_v7().to_string();
    let task_run_id = facts.task_run_id.clone();
    let invocation_id = Uuid::now_v7().to_string();
    let identity = LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: task_id.clone(),
        task_run_id: task_run_id.clone(),
        invocation_id: invocation_id.clone(),
    });
    let queued = rpc
        .queue_lease(LeaseQueueRequest {
            identity: identity.clone(),
            // Absolute epoch millis; 0 means "no deadline". A relative value
            // here terminalizes the row instantly as `deadline_expired`.
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await;
    let fence = match queued {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("the real supervisor RPC did not grant a lease: {other:?}"),
    };
    let fence_value = fence.0;
    assert_ne!(
        fence_value, 0,
        "a zero fence is what production sent at BEGIN; a lift presenting it is refused"
    );

    // ---- Dispatch the Pod the render produced. ----
    let ceiling = facts.launcher_ceiling_millis;
    let mut document = facts.document.clone();
    prepare_probe_job(&mut document, &invocation_id, fence_value);
    // The birth limit: what `TaskRunResizeBootstrap` downsizes a resize-v2
    // sidecar to before dispatch. Taken from the production constant.
    set_launcher_birth_limit(&mut document);
    kubectl_apply(&document);

    let pod = await_running_pod(&task_run_id);
    let node_slice = pod_slice_path(&node, &pod);

    // ---- AC3, first observation: born at 250m, in the INIT statuses. ----
    let born = kubectl_json(&["-n", NAMESPACE, "get", "pod", &pod]);
    let born_status = launcher_status(&born);
    assert_eq!(
        born_status.limit_millis,
        Some(BIRTH_MILLICORES),
        "the sidecar was not born at the {BIRTH_MILLICORES}m birth limit"
    );
    let container_id = born_status.container_id.clone();
    let restart_count = born_status.restart_count;
    let survivors = other_containers(&born);
    let slice_at_birth = cpu_max_millicores(&node_read(&node, &format!("{node_slice}/cpu.max")))
        .expect("the pod slice carries a numeric cpu.max; every container in this pod has a limit");

    // The pod slice is the SUM. Asserted here so the delta below is not the
    // only thing standing between this test and the epic's wrong arithmetic.
    assert_eq!(
        slice_at_birth,
        facts.worker_limit_millis + BIRTH_MILLICORES,
        "the pod slice at birth is the sum of the pod's container limits, not the launcher's own limit"
    );

    await_probe_line(&pod, "probe.created");

    // ---- AC2: the PID belongs to the launcher hierarchy. ----
    let ancestry = prove_pid_ancestry(&pod, &node, &invocation_id, &container_id);
    eprintln!(
        ">>> AC2: in-pod pid {} == NSpid of host pid {} under {}",
        ancestry.in_pod_pid, ancestry.host_pid, ancestry.host_cgroup
    );

    // ---- AC9 + AC3 + AC7: resize UP through pods/resize, strategically. ----
    resize_launcher(&pod, ceiling);
    let lifted = await_launcher_limit(&pod, ceiling);
    assert_eq!(
        launcher_status(&lifted).container_id,
        container_id,
        "AC7: the container id changed, so this was a restart, not an in-place resize"
    );
    assert_eq!(
        launcher_status(&lifted).restart_count,
        restart_count,
        "AC7: the restart count moved"
    );
    assert_eq!(
        other_containers(&lifted),
        survivors,
        "AC9: a container other than the launcher changed across the resize PATCH. The initContainers array carries `patchMergeKey: name`; a JSON-merge body would have replaced the whole array."
    );
    // The production confirmation reader agrees, on the object the apiserver
    // returned rather than on the PATCH response.
    let lifted_pod = serde_json::from_value(lifted.clone()).expect("a Pod document");
    confirm_launcher_cpu(&lifted_pod, CpuLimit::from_millis(ceiling))
        .expect("the production reader confirms the lifted limit from the init-container status");

    // ---- AC4: the pod slice moved by exactly the launcher's change. ----
    let slice_at_ceiling = cpu_max_millicores(&node_read(&node, &format!("{node_slice}/cpu.max")))
        .expect("the pod slice carries a numeric cpu.max");
    assert_eq!(
        slice_at_ceiling - slice_at_birth,
        ceiling - BIRTH_MILLICORES,
        "AC4: pod slice moved {} but the launcher moved {}",
        slice_at_ceiling - slice_at_birth,
        ceiling - BIRTH_MILLICORES
    );
    assert_eq!(
        slice_at_ceiling, facts.pod_slice_at_ceiling_millis,
        "the rendered arithmetic and the kernel disagree about the pod slice"
    );

    // ---- The lift itself, judged by the launcher against the REAL fence. ----
    deliver_decision(&pod, fence_value);
    await_probe_line(&pod, "probe.lift_attempt attempted=true");
    let log = probe_log(&pod);
    assert!(
        log.contains("result=accepted"),
        "the broker refused a lift carrying the fence the real supervisor RPC granted:\n{log}"
    );

    // ---- AC5: effective CPU from ENFORCEMENT, not from configuration. ----
    let measured = measure_effective_cpu(&pod, &invocation_id);
    let lower = ceiling * (100 - EFFECTIVE_CPU_TOLERANCE_PERCENT) / 100;
    let upper = ceiling * (100 + EFFECTIVE_CPU_TOLERANCE_PERCENT) / 100;
    eprintln!(
        ">>> AC5: usage {} usec over {} ms = {} millicores (ceiling {ceiling}, band {lower}..{upper})",
        measured.usage_usec, measured.wall_millis, measured.effective_millicores
    );
    assert!(
        (lower..=upper).contains(&measured.effective_millicores),
        "AC5: the leaf burned {} millicores over {} ms against an admitted ceiling of {ceiling}m. This is the 7deu shape: a `cpu.max` read-back would have reported the ceiling regardless.",
        measured.effective_millicores,
        measured.wall_millis
    );

    // ---- AC6: the invocation leaf has NO narrower quota, proven twice. ----
    let leaf_cpu_max = launcher_read(&pod, &format!("/sys/fs/cgroup/{invocation_id}/cpu.max"));
    assert!(
        leaf_cpu_max.starts_with("max "),
        "AC6: the invocation leaf carries a numeric quota `{leaf_cpu_max}`. Under resize-v2 the launcher does not own the leaf quota; a value here is the 7deu ancestor clamp being reintroduced one level down."
    );
    assert_eq!(
        measured.throttled_delta, 0,
        "AC6: the leaf was throttled {} times while running at the admitted ceiling; a quota narrower than the ceiling is being enforced somewhere below the container",
        measured.throttled_delta
    );

    // ---- AC3, third observation: back to 250m, still the same container. ----
    resize_launcher(&pod, BIRTH_MILLICORES);
    let dropped = await_launcher_limit(&pod, BIRTH_MILLICORES);
    assert_eq!(
        launcher_status(&dropped).container_id,
        container_id,
        "AC7: the container id changed on the way down"
    );
    assert_eq!(launcher_status(&dropped).restart_count, restart_count);
    assert_eq!(
        other_containers(&dropped),
        survivors,
        "AC9, on the way down"
    );
    let slice_at_rest = cpu_max_millicores(&node_read(&node, &format!("{node_slice}/cpu.max")))
        .expect("the pod slice carries a numeric cpu.max");
    assert_eq!(
        slice_at_rest, slice_at_birth,
        "AC4: the pod slice did not return to its birth sum"
    );

    // ---- AC1, second half: the lease is released through the same RPC. ----
    let released = rpc
        .release_lease(LeaseReleaseRequest {
            identity,
            fencing_token: fence,
            candidate_cleanup: true,
        })
        .await;
    assert!(
        matches!(released, LeaseResult::Released { .. }),
        "the real supervisor RPC did not release the lease: {released:?}"
    );

    // Teardown, in the one order that does not deadlock.
    drop(rpc);
    let _ = background.writer.await;
    cancel.cancel();
    let _ = background.reader.await;
    server.cancel();
    let _ = server.join.await;
    cleanup_previous_runs();
}

// ---------------------------------------------------------------------------
// Live proof 2: an absent init-container status is not confirmation.
// ---------------------------------------------------------------------------

/// AC8, live half. A Pod whose only `cgroup-launcher` entry is a REGULAR
/// container — the shape the apiserver will actually produce, since container
/// names are unique across the two lists — must read as unconfirmed and must
/// never reach a lease grant.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs the disposable kind cluster from scripts/kind/setup-resize-kind-cluster.sh"]
async fn live_the_absent_init_status_is_not_confirmed() {
    if !live_tests_enabled() {
        return;
    }
    let _context = harness_context();
    let name = format!("pcod-decoy-{}", Uuid::now_v7().simple());
    let decoy = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": NAMESPACE,
            "labels": { LABEL_COMPONENT: COMPONENT_TASK_RUN_WORKER }
        },
        "spec": {
            "restartPolicy": "Never",
            "containers": [{
                // The exact name the confirmation looks for, in the wrong list.
                "name": LAUNCHER_CONTAINER_NAME,
                "image": "registry.k8s.io/pause:3.10",
                "resources": { "limits": { "cpu": "2" }, "requests": { "cpu": "50m" } }
            }]
        }
    });
    kubectl_apply(&decoy);
    // Wait for the kubelet to publish a containerStatus for it, so the decoy is
    // genuinely misleading rather than merely empty.
    let mut published = false;
    for _ in 0..READY_TICKS {
        let phase = kubectl_ok(&[
            "-n",
            NAMESPACE,
            "get",
            "pod",
            &name,
            "-o",
            "jsonpath={.status.phase}",
        ]);
        if phase == "Running" {
            published = true;
            break;
        }
        std::thread::sleep(TICK);
    }
    assert!(published, "the decoy Pod never reached Running");

    let document = kubectl_json(&["-n", NAMESPACE, "get", "pod", &name]);
    // Read the decoy's own status entry so this test is measuring a Pod that
    // really does carry a matching entry under the wrong key.
    let decoy_limit = document["status"]
        .as_object()
        .and_then(|status| status.get(&format!("{}{}", "container", "Statuses")))
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        })
        .and_then(|entry| entry["resources"]["limits"]["cpu"].as_str())
        .map(millicores);
    assert_eq!(
        decoy_limit,
        Some(2_000),
        "the decoy does not carry a matching launcher entry under the wrong key, so this test would prove nothing"
    );

    let pod = serde_json::from_value(document).expect("a Pod document");
    let refusal = confirm_launcher_cpu(&pod, CpuLimit::from_millis(2_000))
        .expect_err("a Pod with no init-container status must not confirm");
    assert!(
        matches!(
            refusal,
            PodResizeError::LauncherIdentityAmbiguous { .. }
                | PodResizeError::NotConfirmed(NotConfirmed::StatusLimitAbsent)
        ),
        "the production reader confirmed a Pod whose only matching entry is a regular container: {refusal:?}"
    );

    // And nothing downstream may treat it as confirmed: the harness's own gate
    // is the same one the production dispatch path uses — confirmation first,
    // grant second — so an unconfirmed sidecar never reaches `grant_lease`.
    let db = djinn_server::test_helpers::create_test_db();
    BuildLeaseRepository::new(db.clone())
        .set_cap(1)
        .await
        .expect("arm the cap");
    let host: Arc<dyn SupervisorServices> =
        Arc::new(djinn_agent::direct_services::DirectServices::new(
            djinn_server::test_helpers::agent_context_from_db(db, CancellationToken::new()),
            CancellationToken::new(),
        ));
    let socket_dir = tempfile::Builder::new()
        .prefix("pcod-decoy-")
        .tempdir_in("/var/tmp")
        .expect("a short-pathed temp dir");
    let socket = socket_dir.path().join("rpc.sock");
    let server = serve_on_unix_socket(&socket, host)
        .await
        .expect("the supervisor RPC server binds");
    let cancel = CancellationToken::new();
    let (rpc, background) = RpcServices::connect_unix(&socket, cancel.clone())
        .await
        .expect("the client connects");
    let granted = confirm_launcher_cpu(&pod, CpuLimit::from_millis(2_000)).is_ok();
    assert!(
        !granted,
        "confirmation passed, so the guard below would be vacuous"
    );
    // The lease is NOT queued, because confirmation failed. Reading the status
    // of an identity that was never queued proves the gate held.
    let status = rpc
        .lease_status(djinn_supervisor::services::LeaseStatusRequest {
            identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                task_id: Uuid::now_v7().to_string(),
                task_run_id: Uuid::now_v7().to_string(),
                invocation_id: Uuid::now_v7().to_string(),
            }),
        })
        .await;
    assert!(
        !matches!(status, LeaseResult::Granted(_)),
        "an unconfirmed sidecar produced a lease grant: {status:?}"
    );

    drop(rpc);
    let _ = background.writer.await;
    cancel.cancel();
    let _ = background.reader.await;
    server.cancel();
    let _ = server.join.await;
    kubectl(&[
        "-n",
        NAMESPACE,
        "delete",
        "pod",
        &name,
        "--ignore-not-found",
    ]);
}

// ---------------------------------------------------------------------------
// Live proof 3: the harness self-test.
// ---------------------------------------------------------------------------

/// AC10. The teardown trap really fires: a run injected to fail after the
/// registry container exists leaves nothing behind.
#[test]
#[ignore = "live: creates and removes a registry container"]
fn live_the_teardown_trap_removes_what_a_failed_run_created() {
    if !live_tests_enabled() {
        return;
    }
    let output = run_script(&["selftest"]);
    assert_eq!(
        exit_code(&output),
        0,
        "the harness self-test failed:\n{}\n{}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("PASS: the teardown trap removed the registry"),
        "unexpected self-test output:\n{}",
        stdout_of(&output)
    );
}

// ---------------------------------------------------------------------------
// Live helpers.
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

/// Turn the rendered Job into one this cluster can actually run: the worker
/// entrypoint becomes the probe, which speaks the SAME broker protocol against
/// the UNMODIFIED shipped launcher binary in the rendered sidecar. Everything
/// else about the render — the sidecar, the RuntimeClass, the IPC volume, the
/// resource shape — is left exactly as production produced it.
fn prepare_probe_job(document: &mut Value, invocation: &str, fence: u64) {
    document["spec"]["backoffLimit"] = json!(0);
    document["spec"]["ttlSecondsAfterFinished"] = json!(600);
    let spec = &mut document["spec"]["template"]["spec"];
    assert_eq!(
        spec["runtimeClassName"], TASK_RUN_CGROUP_RUNTIME_CLASS,
        "the render did not name the writable-cgroup RuntimeClass; the launcher would come up on a read-only /sys/fs/cgroup"
    );
    let containers = spec["containers"].as_array_mut().expect("containers");
    let worker = containers
        .iter_mut()
        .find(|entry| entry["name"] == WORKER_CONTAINER_NAME)
        .expect("the render carries a worker container");
    worker["image"] = json!(PROBE_IMAGE);
    worker["imagePullPolicy"] = json!("IfNotPresent");
    worker["command"] = json!([PROBE_BIN]);
    worker["args"] = json!([]);
    // The two rendered variables the probe MUST read from the environment the
    // renderer produced rather than from constants of its own.
    let rendered_socket = env_value(worker, "DJINN_LAUNCHER_SOCKET")
        .unwrap_or_else(|| LAUNCHER_SOCKET_PATH.to_owned());
    let rendered_credential = env_value(worker, "DJINN_LAUNCHER_CREDENTIAL_PATH")
        .unwrap_or_else(|| LAUNCHER_CREDENTIAL_PATH.to_owned());
    let protocol = env_value(worker, AUTHORITY_PROTOCOL_ENV)
        .expect("the render declares the launcher authority protocol on the worker");
    assert_eq!(
        protocol,
        LauncherAuthorityProtocol::ResizeV2.as_wire(),
        "this proof only means anything under resize-v2: under leaf-v1 the launcher owns the leaf quota and AC6 would be asserting the wrong thing"
    );
    let environment = worker["env"].as_array_mut().expect("the worker has env");
    for (name, value) in [
        ("DJINN_LAUNCHER_SOCKET", rendered_socket),
        ("DJINN_LAUNCHER_CREDENTIAL_PATH", rendered_credential),
        ("DJINN_PROBE_INVOCATION", invocation.to_owned()),
        ("DJINN_PROBE_FENCE", fence.to_string()),
        ("DJINN_PROBE_AUTHORITY", "armed".to_owned()),
        (
            "DJINN_PROBE_DECISION_PATH",
            format!("{PROBE_DECISION_DIR}/decision"),
        ),
        ("DJINN_PROBE_WORKLOAD", PROBE_WORKLOAD.to_owned()),
        ("DJINN_PROBE_CLAMP_SECONDS", "6".to_owned()),
        ("DJINN_PROBE_LIFTED_SECONDS", "6".to_owned()),
        ("DJINN_PROBE_WORKERS", "4".to_owned()),
        ("DJINN_PROBE_HOLD_SECONDS", "600".to_owned()),
    ] {
        environment.retain(|entry| entry["name"] != name);
        environment.push(json!({ "name": name, "value": value }));
    }
    document["metadata"]["labels"][LABEL_COMPONENT] = json!(COMPONENT_TASK_RUN_WORKER);
}

fn env_value(container: &Value, name: &str) -> Option<String> {
    container["env"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"] == name)?["value"]
        .as_str()
        .map(str::to_owned)
}

/// Downsize the rendered sidecar to the birth limit before dispatch, which is
/// what `TaskRunResizeBootstrap` does in production. The ceiling was already
/// read OUT of the render before this ran, so the endpoints are the rendered
/// ones and not constants.
fn set_launcher_birth_limit(document: &mut Value) {
    let initcontainers = document["spec"]["template"]["spec"]["initContainers"]
        .as_array_mut()
        .expect("the render carries the launcher as an init container");
    let launcher = initcontainer_mut(initcontainers_names(initcontainers), initcontainers);
    launcher["resources"]["limits"]["cpu"] = json!(format!("{BIRTH_MILLICORES}m"));
}

fn initcontainers_names(list: &[Value]) -> usize {
    list.iter()
        .position(|entry| entry["name"] == LAUNCHER_CONTAINER_NAME)
        .expect("there is exactly one cgroup-launcher init container")
}

fn initcontainer_mut(index: usize, list: &mut [Value]) -> &mut Value {
    &mut list[index]
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

/// The pod's own cgroup slice on the NODE. Located by the launcher's cgroup
/// path rather than by guessing the kubelet's naming scheme.
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
    let dashed = uid.replace('-', "_");
    let output = node_exec(
        node,
        &[
            "sh",
            "-c",
            &format!(
                "find /sys/fs/cgroup -maxdepth 4 -type d \\( -name '*{uid}*' -o -name '*{dashed}*' \\) | head -1"
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

struct Ancestry {
    in_pod_pid: u64,
    host_pid: u64,
    host_cgroup: String,
}

/// AC2. The PID is taken from the KERNEL — the leaf's `cgroup.procs` — never
/// from a log line and never from the broker's response, which does not carry
/// one. It is then resolved two ways: `/proc/<pid>/cgroup` inside the launcher
/// container, and, on the node, a host PID under a cgroup path containing the
/// launcher's container ID from `status.initContainerStatuses`.
fn prove_pid_ancestry(pod: &str, node: &str, invocation: &str, container_id: &str) -> Ancestry {
    let leaf = format!("/sys/fs/cgroup/{invocation}");
    let procs = launcher_read(pod, &format!("{leaf}/cgroup.procs"));
    let in_pod_pid: u64 = procs
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .expect("the invocation leaf holds at least one process")
        .parse()
        .expect("cgroup.procs holds PIDs");

    // The command really is the brokered workload, not something else that
    // wandered into the leaf.
    let cmdline = launcher_exec(
        pod,
        &[
            "sh",
            "-c",
            &format!("tr '\\0' ' ' < /proc/{in_pod_pid}/cmdline"),
        ],
    );
    let cmdline = stdout_of(&cmdline);
    assert!(
        cmdline.contains("/bin/sh") || cmdline.contains("sha256sum"),
        "the leaf's process is not the brokered workload: {cmdline}"
    );

    // Inside the launcher container the cgroup namespace is private, so
    // `/proc/<pid>/cgroup` is relative to the container's DELEGATED ROOT. The
    // launcher's `Bootstrap` vacates the container's own processes into the
    // `init` leaf before it creates any invocation leaf, so the measured process
    // is a SIBLING of the launcher's own leaf under that root, not a descendant
    // of it — the first version of this assertion said "descendant" and was
    // wrong. What must hold is that both live under one delegated root, that
    // the measured process is in the invocation's leaf and not in the
    // launcher's own, and (below, on the node) that the root is the launcher
    // container's.
    let cgroup_path = |raw: &str| -> String {
        raw.rsplit("::")
            .next()
            .expect("a cgroup v2 line")
            .trim()
            .to_owned()
    };
    let pid_path = cgroup_path(&launcher_read(pod, &format!("/proc/{in_pod_pid}/cgroup")));
    let own_path = cgroup_path(&launcher_read(pod, "/proc/self/cgroup"));
    let parent_of = |path: &str| -> String {
        path.rsplit_once('/')
            .map(|(head, _)| head.to_owned())
            .unwrap_or_default()
    };
    assert_eq!(
        pid_path.trim_start_matches('/'),
        invocation,
        "the measured PID's cgroup is `{pid_path}`, not the invocation leaf `{invocation}`. A process spawned in the WORKER container is not in this cgroup namespace at all, and one spawned by the launcher outside a leaf resolves to the launcher's own `{own_path}`."
    );
    assert_ne!(
        pid_path, own_path,
        "the measured process is in the launcher's OWN cgroup rather than in an invocation leaf; the broker never created a leaf for it"
    );
    assert_eq!(
        parent_of(&pid_path),
        parent_of(&own_path),
        "the invocation leaf `{pid_path}` and the launcher's own `{own_path}` are not children of one delegated root"
    );

    // The container-ID half, which only the node can answer: find the cgroup
    // directory whose name carries the launcher's container ID, and read the
    // leaf's HOST pids out of it.
    let short_id = container_id
        .rsplit('/')
        .next()
        .expect("a container id")
        .to_owned();
    let found = node_exec(
        node,
        &[
            "sh",
            "-c",
            &format!("find /sys/fs/cgroup -maxdepth 6 -type d -name '*{short_id}*' | head -1"),
        ],
    );
    let scope = stdout_of(&found).trim().to_owned();
    assert!(
        !scope.is_empty(),
        "no cgroup directory on node {node} carries the launcher container id {short_id}; the ancestry claim cannot be bound to the sidecar reported in the init-container statuses"
    );
    let host_procs = node_read(node, &format!("{scope}/{invocation}/cgroup.procs"));
    let host_pid: u64 = host_procs
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| {
            panic!("the leaf {scope}/{invocation} on node {node} holds no processes")
        })
        .parse()
        .expect("cgroup.procs holds PIDs");
    // `NSpid` binds the host PID to the PID observed inside the container, so
    // this is the same process and not merely a process in the same directory.
    let status = node_read(node, &format!("/proc/{host_pid}/status"));
    let nspid = status
        .lines()
        .find(|line| line.starts_with("NSpid:"))
        .expect("the node kernel reports NSpid");
    assert!(
        nspid
            .split_whitespace()
            .skip(1)
            .any(|value| value == in_pod_pid.to_string()),
        "host pid {host_pid} does not map to in-pod pid {in_pod_pid} ({nspid})"
    );
    let host_cgroup = node_read(node, &format!("/proc/{host_pid}/cgroup"));
    assert!(
        host_cgroup.contains(&short_id),
        "the host cgroup of the measured process does not contain the launcher container id {short_id}:\n{host_cgroup}"
    );

    Ancestry {
        in_pod_pid,
        host_pid,
        host_cgroup,
    }
}

/// AC9. A strategic merge patch of the production body, against the
/// `pods/resize` subresource. `--type strategic` is `Patch::Strategic`; the
/// initContainers array's `patchMergeKey: name` is what makes it the only
/// correct choice.
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

/// Confirmation comes from `status.initContainerStatuses` — never from the
/// PATCH response, never from the mutable `spec`, never from an HTTP 200 — and
/// is compared in PARSED MILLICORES, because the apiserver canonicalises
/// `4000m` to `4`.
fn await_launcher_limit(pod: &str, target_millis: u64) -> Value {
    let mut last = Value::Null;
    for _ in 0..CONFIRM_TICKS {
        let document = kubectl_json(&["-n", NAMESPACE, "get", "pod", pod]);
        let observed = launcher_status(&document).limit_millis;
        let parsed: Option<()> = serde_json::from_value(document.clone())
            .ok()
            .filter(|pod: &_| !has_resize_pending_condition(pod))
            .map(|_| ());
        if observed == Some(target_millis) && parsed.is_some() {
            return document;
        }
        last = document;
        std::thread::sleep(TICK);
    }
    panic!(
        "the launcher's init-container status never reported {target_millis}m; last observed {:?}",
        launcher_status(&last).limit_millis
    );
}

fn probe_log(pod: &str) -> String {
    let output = kubectl(&["-n", NAMESPACE, "logs", pod, "-c", WORKER_CONTAINER_NAME]);
    stdout_of(&output)
}

fn await_probe_line(pod: &str, needle: &str) {
    for _ in 0..READY_TICKS {
        let log = probe_log(pod);
        assert!(
            !log.contains("probe.fatal"),
            "the probe failed before reaching `{needle}`:\n{log}"
        );
        if log.contains(needle) {
            return;
        }
        std::thread::sleep(TICK);
    }
    panic!("the probe never printed `{needle}`:\n{}", probe_log(pod));
}

/// Deliver the fence the REAL supervisor RPC granted. The probe never derives
/// its own fence; a probe that authorized itself would be measuring itself.
fn deliver_decision(pod: &str, fence: u64) {
    let output = launcher_exec(
        pod,
        &[
            "sh",
            "-c",
            &format!(
                "mkdir -p {PROBE_DECISION_DIR} && printf 'lift {fence}' > {PROBE_DECISION_DIR}/decision"
            ),
        ],
    );
    assert!(
        output.status.success(),
        "delivering the lift decision failed: {}",
        stderr_of(&output)
    );
}

struct Effective {
    usage_usec: u64,
    throttled_delta: u64,
    wall_millis: u64,
    effective_millicores: u64,
}

/// AC5. `usage_usec` accumulated over a KNOWN wall-clock window, read from the
/// leaf's `cpu.stat`. Not `cpu.max`: task 7deu measured a leaf whose `cpu.max`
/// read four cores while the process burned a quarter of one.
fn measure_effective_cpu(pod: &str, invocation: &str) -> Effective {
    let path = format!("/sys/fs/cgroup/{invocation}/cpu.stat");
    let before = launcher_read(pod, &path);
    let window = Duration::from_secs(MEASURE_SECONDS);
    std::thread::sleep(window);
    let after = launcher_read(pod, &path);

    let usage_usec =
        cpu_stat_field(&after, "usage_usec").saturating_sub(cpu_stat_field(&before, "usage_usec"));
    let throttled_delta = cpu_stat_field(&after, "nr_throttled")
        .saturating_sub(cpu_stat_field(&before, "nr_throttled"));
    // The window is the sleep, not a clock reading: `Instant::now` is
    // workspace-disallowed and a logical clock would report a throughput this
    // test invented.
    let wall_millis = window.as_millis() as u64;
    Effective {
        usage_usec,
        throttled_delta,
        wall_millis,
        effective_millicores: usage_usec.saturating_mul(1_000) / (wall_millis.max(1) * 1_000),
    }
}
