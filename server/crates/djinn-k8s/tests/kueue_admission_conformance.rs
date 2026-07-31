// djinn:allow-oversize — one test binary is the whole unit here: an integration
// test cannot share helpers with a sibling `tests/*.rs` without a module file,
// and the live tests must run in ONE process because they serialise on a
// process-wide lock over one shared ClusterQueue. Splitting them into two
// binaries would let two of them mutate the same quota concurrently, which is
// the one failure mode this file cannot tolerate. Roughly a quarter of the
// length is the measured evidence for two chart defects; see the module doc.
//
// Test: eprintln is the skip-reason channel for the gated half, mirroring
// tests/kueue_cluster_harness.rs and tests/kind_smoke.rs.
#![allow(clippy::print_stderr)]
//! Ordinary admission conformance for an ARMED Kueue install, measured against
//! a live controller and a live API server (fbiy-B1).
//!
//! WHAT THIS FILE MEASURED, AND WHY IT COULD NOT HAVE BEEN A FIXTURE
//! -----------------------------------------------------------------
//! Against the disposable armed cluster this file targets, the chart's own
//! ClusterQueue admitted **nothing**. Twice, for two independent reasons, each
//! of which alone would have wedged every build Job in an armed production
//! install — and neither of which any render test could see.
//!
//! FIRST: no `namespaceSelector`. Every captured Workload sat Pending with
//!
//! ```text
//! QuotaReserved=False reason=Pending
//! "workload namespace doesn't match ClusterQueue selector"
//! ```
//!
//! The CRD documents the trap: an absent selector means *no namespaces are
//! eligible*, not *all of them*.
//!
//! SECOND, once that was fixed, a Job from the REAL renderer still sat Pending
//! with
//!
//! ```text
//! "couldn't assign flavors to pod set main: resource cpu unavailable in ClusterQueue"
//! ```
//!
//! because the ClusterQueue covered only `pods` while every build Pod requests
//! cpu and memory, and Kueue refuses to assign a flavor when any requested
//! resource is uncovered. That one is the sharper lesson: a synthetic Job with
//! no `resources:` block admits perfectly well, so the defect was invisible to
//! anything short of the real renderer against a real controller — and the
//! chart's render test asserted "CPU/memory quota is forbidden", which is what
//! locked it in.
//!
//! Both fixes are in this PR, both are guarded hermetically below, and an armed
//! install would have captured every task-run, warm and SCIP Job and unsuspended
//! none of them — with no error, no event, and a green chart render test.
//!
//! That is the whole argument for this task being live: whether Kueue mutates
//! only `spec.suspend`, whether the `pods` quota actually bounds admission, and
//! what deleting a Job or a Workload does to usage are properties of a
//! controller reconciling against an API server. Nothing that renders YAML can
//! attest them.
//!
//! TWO HALVES, AND ONLY ONE NEEDS A CLUSTER
//! ----------------------------------------
//! * `guard_*` — HERMETIC, NOT `#[ignore]`d, no cluster and no network. They
//!   run in the ordinary `cargo test -p djinn-k8s` lane on every PR. They hold
//!   the *premises* the live assertions depend on: that the renderer still
//!   emits every field the live diff compares (a renderer that stopped emitting
//!   `fsGroup` would make the live comparison `null == null`), that every field
//!   AC1 enumerates is actually probed, that a rendered Workload costs exactly
//!   one `pods` (the premise that makes BestEffortFIFO ordering-equivalent to
//!   StrictFIFO here), and that the chart still carries the `namespaceSelector`
//!   whose absence wedged admission.
//! * `live_*` — `#[ignore]` + `DJINN_TEST_KUEUE_CLUSTER=1`, mirroring
//!   `tests/kueue_cluster_harness.rs`.
//!
//! DOES ANY OF THIS RUN AUTOMATICALLY? The `guard_*` half does, on every PR.
//! The `live_*` half does NOT: `.github/workflows/kueue-cluster-harness.yml` is
//! `workflow_dispatch` only and is not a required check, exactly like
//! `DJINN_TEST_KIND` before it. That asymmetry is why anything that must not
//! regress silently was written as a guard rather than as a live assertion.
//!
//! WHY THE LIVE HALF DRIVES `kubectl` AND NOT `kube::Client`
//! ---------------------------------------------------------
//! Same reason `tests/kueue_cluster_harness.rs` documents: `workspace-hack`
//! unifies `rustls` 0.23 with both the `ring` and `aws-lc-rs` providers and
//! nothing installs a process default, so the first TLS handshake in a
//! `djinn-k8s` test binary panics before its first API call. Fixing that means
//! editing `workspace-hack`, which is a shared file and a separate PR. Every
//! call below is `kubectl --context kind-djinn-kueue-b1`, pinned, never
//! discovered — every context in a Djinn developer's kubeconfig is a live EKS
//! cluster.
//!
//! ISOLATION FROM fbiy-B2
//! ----------------------
//! This task's cluster, registry and registry port are all distinct from
//! `scripts/kind/setup-kueue-cluster.sh`'s defaults so B1 and B2 can hold two
//! disposable clusters at once. `guard_this_task_uses_names_of_its_own` asserts
//! the divergence rather than trusting it.
//!
//! RUNNING THE LIVE HALF
//!
//! ```bash
//! scripts/kind/setup-kueue-cluster.sh up \
//!     --cluster-name djinn-kueue-b1 \
//!     --registry-name djinn-kueue-b1-registry \
//!     --registry-port 5061
//! DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s \
//!     --test kueue_admission_conformance -- --ignored
//! scripts/kind/setup-kueue-cluster.sh down \
//!     --cluster-name djinn-kueue-b1 --registry-name djinn-kueue-b1-registry
//! ```
//!
//! The live tests mutate one shared ClusterQueue, so they serialise on a
//! process-wide lock rather than relying on the caller to pass
//! `--test-threads=1`. A budget that has to be remembered is a budget that is
//! forgotten.

use std::env;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use djinn_k8s::config::{KubernetesConfig, LABEL_KUEUE_BUILD_OBJECT, LABEL_KUEUE_QUEUE_NAME};
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::CgroupLauncherMode;
use djinn_k8s::sidecar::BackingServiceSpec;
use k8s_openapi::api::batch::v1::Job;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The one cluster this file may ever touch, and the names that keep it out of
// fbiy-B2's way.
// ---------------------------------------------------------------------------

const HARNESS_CLUSTER: &str = "djinn-kueue-b1";
const HARNESS_CONTEXT: &str = "kind-djinn-kueue-b1";
const HARNESS_REGISTRY: &str = "djinn-kueue-b1-registry";
const HARNESS_REGISTRY_PORT: &str = "5061";

/// `scripts/kind/setup-kueue-cluster.sh`'s defaults, which fbiy-B0 documents
/// and fbiy-B2 may well be using. Named here so the divergence is asserted.
const SETUP_SCRIPT_DEFAULT_CLUSTER: &str = "djinn-kueue-harness";
const SETUP_SCRIPT_DEFAULT_REGISTRY: &str = "djinn-kueue-harness-registry";
const SETUP_SCRIPT_DEFAULT_REG_PORT: &str = "5051";

const NAMESPACE: &str = "djinn";
/// `<djinn.fullname>-kueue` for release `djinn`.
const CLUSTER_QUEUE: &str = "djinn-kueue";
/// The LocalQueue the armed task-run renderer targets.
const TASK_RUN_LOCAL_QUEUE: &str = "djinn-task-run";
/// `printf "%s-%s" <fullname> .Values.serviceAccount.taskrun` for release
/// `djinn`. This one is load-bearing in a way that is easy to miss: the
/// ServiceAccount admission plugin REJECTS Pod creation outright when the named
/// SA does not exist, and a rejected Pod is indistinguishable from a Pod the
/// quota withheld. Every "there is no Pod" assertion below would pass for the
/// wrong reason. `guard_task_run_service_account_matches_the_chart` pins it.
const TASK_RUN_SERVICE_ACCOUNT: &str = "djinn-djinn-taskrun";

const SETUP_SCRIPT: &str = "scripts/kind/setup-kueue-cluster.sh";
const VALUES_FIXTURE: &str = "deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml";
const CHART_DIR: &str = "deploy/helm/djinn";
const TOPOLOGY_TEMPLATE: &str = "deploy/helm/djinn/templates/kueue-topology.yaml";
const CHART_VALUES: &str = "deploy/helm/djinn/values.yaml";

/// Mutating-webhook wiring for AC1's non-vacuity. The port is B1-specific for
/// the same reason the cluster name is.
const WEBHOOK_PORT: u16 = 18463;
const WEBHOOK_CONFIG_NAME: &str = "djinn-kueue-b1-nodeselector-injector";
/// Only Pods carrying this label are mutated, so the webhook cannot perturb any
/// other assertion in this file (or Kueue's own controllers) while it is
/// registered.
const WEBHOOK_TARGET_LABEL: &str = "djinn.io/harness-webhook-target";
/// The single `nodeSelector` key the webhook injects. AC1's diff must name it.
const INJECTED_NODE_SELECTOR_KEY: &str = "djinn.io/harness-injected";
const INJECTED_NODE_SELECTOR_VALUE: &str = "true";

fn repo_root() -> PathBuf {
    // <repo>/server/crates/djinn-k8s
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives three levels below the repository root")
        .to_path_buf()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the process exits rather than dying on a signal")
}

// ===========================================================================
// The field table AC1 enumerates.
//
// Every probe reads a PodTemplateSpec-SHAPED object: `{metadata, spec}`. A Pod
// has exactly that shape, and so does `Job.spec.template`, which is what lets
// one table diff the submitted template against BOTH the Job the API server
// stored and the Pod the kubelet was handed.
// ===========================================================================

enum Probe {
    /// JSON pointer, relative to the PodTemplateSpec.
    Pointer(&'static str),
    /// A field whose comparable form is assembled rather than pointed at.
    Derived(fn(&Value) -> Value),
}

struct PodField {
    /// AC1's own name for the field.
    name: &'static str,
    probe: Probe,
    /// The value the API SERVER substitutes when the submitted template leaves
    /// this field unset. `None` means "absent must stay absent" — which is a
    /// real assertion here, because the mutation this task is looking for is
    /// Kueue ADDING something (a ResourceFlavor's `nodeLabels` land in exactly
    /// these fields).
    api_server_default: Option<&'static str>,
}

/// `initContainers[*]` reduced to the pair AC1 cares about. A bare pointer
/// would compare image pull policies and probe timings too, which the API
/// server defaults and which say nothing about sidecar restartability.
fn init_sidecar_restart_policies(template: &Value) -> Value {
    let Some(containers) = template
        .pointer("/spec/initContainers")
        .and_then(Value::as_array)
    else {
        return Value::Null;
    };
    Value::Array(
        containers
            .iter()
            .map(|container| {
                json!({
                    "name": container["name"],
                    "restartPolicy": container["restartPolicy"],
                })
            })
            .collect(),
    )
}

/// The worker container's downward-API reference to `metadata.uid`.
///
/// This one cannot be a value comparison: the Pod's uid does not exist when the
/// Job is built, so what must survive admission is the REFERENCE. Returning the
/// whole `valueFrom` (rather than just the field path) is deliberate — a
/// mutation that swapped `fieldRef` for a `secretKeyRef` would otherwise pass.
///
/// `fieldRef.apiVersion` is normalised to `v1` on BOTH sides rather than
/// stripped. The API server defaults it (the renderer leaves it unset, and the
/// live diff reported exactly that difference the first time this ran, which is
/// how the default was found). Normalising up to the default keeps the rest of
/// the reference under comparison; dropping the key would have thrown away a
/// field a mutation could target.
fn worker_pod_uid_env_ref(template: &Value) -> Value {
    let Some(containers) = template
        .pointer("/spec/containers")
        .and_then(Value::as_array)
    else {
        return Value::Null;
    };
    containers
        .iter()
        .find(|container| container["name"] == "worker")
        .and_then(|worker| worker["env"].as_array())
        .and_then(|env| {
            env.iter()
                .find(|var| var["name"] == "DJINN_TASK_RUN_POD_UID")
        })
        .map(|var| {
            let mut value_from = var["valueFrom"].clone();
            if let Some(field_ref) = value_from
                .get_mut("fieldRef")
                .and_then(Value::as_object_mut)
            {
                field_ref.entry("apiVersion").or_insert_with(|| json!("v1"));
            }
            value_from
        })
        .unwrap_or(Value::Null)
}

/// AC1's list, in AC1's order. `guard_every_field_ac1_enumerates_is_probed`
/// holds this table against a literal copy of the acceptance criterion.
const AC1_POD_FIELDS: &[PodField] = &[
    PodField {
        name: "nodeSelector",
        probe: Probe::Pointer("/spec/nodeSelector"),
        api_server_default: None,
    },
    PodField {
        name: "schedulingGates",
        probe: Probe::Pointer("/spec/schedulingGates"),
        api_server_default: None,
    },
    PodField {
        name: "schedulerName",
        probe: Probe::Pointer("/spec/schedulerName"),
        // The renderer leaves this unset and the API server writes
        // `default-scheduler`. Declaring the default here is what turns "the
        // Pod says default-scheduler" into "Kueue did not redirect this Pod to
        // a scheduler of its own" — Kueue ships a `manageJobsWithoutQueueName`
        // /scheduler-name integration that would show up precisely here.
        api_server_default: Some("\"default-scheduler\""),
    },
    PodField {
        name: "runtimeClassName",
        probe: Probe::Pointer("/spec/runtimeClassName"),
        api_server_default: None,
    },
    PodField {
        name: "shareProcessNamespace",
        probe: Probe::Pointer("/spec/shareProcessNamespace"),
        api_server_default: None,
    },
    PodField {
        name: "automountServiceAccountToken",
        probe: Probe::Pointer("/spec/automountServiceAccountToken"),
        api_server_default: None,
    },
    PodField {
        name: "restartPolicy",
        probe: Probe::Pointer("/spec/restartPolicy"),
        api_server_default: None,
    },
    PodField {
        name: "securityContext.fsGroup",
        probe: Probe::Pointer("/spec/securityContext/fsGroup"),
        api_server_default: None,
    },
    PodField {
        name: "securityContext.fsGroupChangePolicy",
        probe: Probe::Pointer("/spec/securityContext/fsGroupChangePolicy"),
        api_server_default: None,
    },
    PodField {
        name: "karpenter.sh/do-not-disrupt annotation",
        probe: Probe::Pointer("/metadata/annotations/karpenter.sh~1do-not-disrupt"),
        api_server_default: None,
    },
    PodField {
        name: "restartable init sidecars",
        probe: Probe::Derived(init_sidecar_restart_policies),
        api_server_default: None,
    },
    PodField {
        name: "downward-API metadata.uid env ref",
        probe: Probe::Derived(worker_pod_uid_env_ref),
        api_server_default: None,
    },
    PodField {
        name: "propagated build-object label",
        probe: Probe::Pointer("/metadata/labels/djinn.io~1kueue-build-object"),
        api_server_default: None,
    },
];

/// The one Job-level field AC1 names. Kept separate because a Job is not a
/// PodTemplateSpec, and because `spec.suspend` — the field Kueue IS allowed to
/// change — is asserted on its own rather than diffed.
const AC1_JOB_FIELDS: &[PodField] = &[PodField {
    name: "backoffLimit",
    probe: Probe::Pointer("/spec/backoffLimit"),
    api_server_default: None,
}];

#[derive(Debug)]
struct FieldDiff {
    name: &'static str,
    expected: Value,
    actual: Value,
}

fn probe_value(field: &PodField, object: &Value) -> Value {
    match &field.probe {
        Probe::Pointer(pointer) => object.pointer(pointer).cloned().unwrap_or(Value::Null),
        Probe::Derived(derive) => (*derive)(object),
    }
}

/// A JSON pointer to one label, with the escaping RFC 6901 requires.
///
/// Worth a function because every label key in this repository contains a `/`,
/// and a pointer that forgets to escape it reads `Null` — on BOTH sides of the
/// diff, which is a silent pass rather than a failure.
fn label_pointer(label: &str) -> String {
    format!(
        "/metadata/labels/{}",
        label.replace('~', "~0").replace('/', "~1")
    )
}

/// The field-by-field diff AC1 asks for.
///
/// `submitted` is the object this process handed the API server; `observed` is
/// what came back off it. A whole-object comparison is not available and never
/// was — the API server defaults a dozen fields nobody is asserting about
/// (`terminationMessagePath`, `dnsPolicy`, the projected token volume) — so the
/// enumerated table is the contract, and
/// `guard_every_field_ac1_enumerates_is_probed` is what keeps the table from
/// quietly shrinking.
fn diff_fields(fields: &[PodField], submitted: &Value, observed: &Value) -> Vec<FieldDiff> {
    fields
        .iter()
        .filter_map(|field| {
            let mut expected = probe_value(field, submitted);
            if expected.is_null()
                && let Some(default) = field.api_server_default
            {
                expected =
                    serde_json::from_str(default).expect("declared API-server default is JSON");
            }
            let actual = probe_value(field, observed);
            (expected != actual).then(|| FieldDiff {
                name: field.name,
                expected,
                actual,
            })
        })
        .collect()
}

fn render_diff(diffs: &[FieldDiff]) -> String {
    diffs
        .iter()
        .map(|diff| {
            format!(
                "  {}: submitted {} -> observed {}",
                diff.name, diff.expected, diff.actual
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ===========================================================================
// The renderer's real output.
// ===========================================================================

/// The armed `KubernetesConfig` every Job below is rendered from.
///
/// `cgroup_launcher_mode: Disabled` + `task_run_cgroup_writable_enabled: false`
/// are forced, and the reason is measurable rather than stylistic: the harness
/// cluster deliberately has NO `djinn-cgroup-writable` RuntimeClass (that
/// absence is fbiy-B0's AC4), and the API server's RuntimeClass admission
/// plugin REJECTS a Pod that names a RuntimeClass which does not exist. An
/// armed-launcher render here would produce a Job whose Pod could never be
/// created, and every Pod assertion below would fail for a reason that has
/// nothing to do with Kueue. fbiy-C1 owns flipping both together once the kind
/// node can run the class.
fn armed_harness_config() -> KubernetesConfig {
    KubernetesConfig {
        namespace: NAMESPACE.into(),
        kueue_armed: true,
        kueue_local_queue_prefix: "djinn".into(),
        // The chart's rendered SA. See TASK_RUN_SERVICE_ACCOUNT.
        service_account: TASK_RUN_SERVICE_ACCOUNT.into(),
        cgroup_launcher_mode: CgroupLauncherMode::Disabled,
        task_run_cgroup_writable_enabled: false,
        ..KubernetesConfig::for_testing()
    }
}

/// Two backing services, so the rendered Pod carries exactly the two
/// restartable native sidecars AC1 names.
///
/// With the launcher disabled (see [`armed_harness_config`]) the launcher
/// sidecar is not rendered, so the two sidecars have to come from the service
/// path — which is the same `restartPolicy: Always` initContainer mechanism,
/// exercised through the same renderer.
fn harness_backing_services() -> Vec<BackingServiceSpec> {
    vec![
        BackingServiceSpec {
            service_type: "postgres".into(),
            image: "registry.example/postgres:harness".into(),
            port: 5432,
            env: vec![("POSTGRES_PASSWORD".into(), "harness".into())],
            cpu_request: "50m".into(),
            memory_request: "128Mi".into(),
            cpu_limit: "500m".into(),
            memory_limit: "512Mi".into(),
            conn_template: "postgres://harness@127.0.0.1:5432/harness".into(),
            conn_env_var: "TEST_POSTGRES_URL".into(),
        },
        BackingServiceSpec {
            service_type: "redis".into(),
            image: "registry.example/redis:harness".into(),
            port: 6379,
            env: Vec::new(),
            cpu_request: "50m".into(),
            memory_request: "64Mi".into(),
            cpu_limit: "250m".into(),
            memory_limit: "256Mi".into(),
            conn_template: "redis://127.0.0.1:6379".into(),
            conn_env_var: "TEST_REDIS_URL".into(),
        },
    ]
}

fn rendered_task_run_job() -> (Job, String) {
    let task_run_id = uuid::Uuid::now_v7();
    let job = build_task_run_job(
        &armed_harness_config(),
        &task_run_id,
        "harness-project",
        &format!("djinn-taskrun-{task_run_id}"),
        "registry.example/project:harness",
        &harness_backing_services(),
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

/// The PodTemplateSpec-shaped view of a Job manifest, which is what the field
/// table above reads.
fn template_of(job: &Value) -> Value {
    job.pointer("/spec/template")
        .cloned()
        .expect("a Job carries a pod template")
}

// ===========================================================================
// Hermetic guards — these run in the ordinary test lane, on every PR.
// ===========================================================================

/// AC1's premises. Every field the live diff compares must be something the
/// renderer ACTUALLY EMITS; otherwise the live comparison is `null == null` and
/// certifies nothing.
///
/// This is the guard that would have caught the whole class of failure fbiy
/// exists to refute, and it runs where the live tests do not.
#[test]
fn guard_the_renderer_emits_every_field_the_live_diff_compares() {
    let (job, _) = rendered_task_run_job();
    let manifest = job_as_json(&job);
    let template = template_of(&manifest);

    // Four of AC1's fields are absent from a correct render, and each absence
    // is itself the thing being protected — Kueue ADDING any of them is the
    // mutation this task is looking for:
    //
    //   * nodeSelector — `config.node_selector` is empty, so the renderer emits
    //     nothing. A ResourceFlavor with `nodeLabels` would land exactly here,
    //     which is why the live diff's teeth are proven by injecting one key
    //     (`live_the_field_by_field_diff_names_a_webhook_injected_node_selector`)
    //     rather than by this field's rendered value;
    //   * schedulingGates — the task-run renderer gates nothing; Kueue's
    //     Pod-based integration gates Pods, and its Job integration must not;
    //   * runtimeClassName / shareProcessNamespace — both hang off the cgroup
    //     launcher, which `armed_harness_config` disables (see its doc).
    let absent_by_design = [
        "nodeSelector",
        "schedulingGates",
        "runtimeClassName",
        "shareProcessNamespace",
    ];
    for field in AC1_POD_FIELDS {
        let value = probe_value(field, &template);
        if field.api_server_default.is_some() {
            assert!(
                value.is_null(),
                "{} declares an API-server default, so the renderer must leave it unset; got {value}",
                field.name,
            );
            continue;
        }
        if absent_by_design.contains(&field.name) {
            assert!(
                value.is_null(),
                "{} must be absent while the cgroup launcher is disabled; got {value}",
                field.name,
            );
            continue;
        }
        assert!(
            !value.is_null(),
            "the renderer no longer emits {} — the live field-by-field diff would compare null \
             against null and pass against nothing",
            field.name,
        );
    }

    // The values, not merely their presence. Each one is a fact AC1 names.
    assert_eq!(
        template.pointer("/spec/restartPolicy"),
        Some(&json!("Never"))
    );
    assert_eq!(
        template.pointer("/spec/automountServiceAccountToken"),
        Some(&json!(false)),
    );
    assert_eq!(
        template.pointer("/spec/securityContext/fsGroup"),
        Some(&json!(1000)),
    );
    assert_eq!(
        template.pointer("/spec/securityContext/fsGroupChangePolicy"),
        Some(&json!("OnRootMismatch")),
    );
    assert_eq!(
        template.pointer("/metadata/annotations/karpenter.sh~1do-not-disrupt"),
        Some(&json!("true")),
    );
    assert_eq!(manifest.pointer("/spec/backoffLimit"), Some(&json!(0)));
    assert_eq!(manifest.pointer("/spec/suspend"), Some(&json!(true)));
    assert_eq!(
        template.pointer(&label_pointer(LABEL_KUEUE_BUILD_OBJECT)),
        Some(&json!("true")),
    );
    assert_eq!(
        manifest.pointer(&label_pointer(LABEL_KUEUE_QUEUE_NAME)),
        Some(&json!(TASK_RUN_LOCAL_QUEUE)),
        "the armed renderer must target the chart's task-run LocalQueue, or Kueue never sees the \
         Job at all",
    );

    // AC1 says "both restartable init sidecars". There must be exactly two, and
    // both must be native sidecars.
    let sidecars = init_sidecar_restart_policies(&template);
    let sidecars = sidecars
        .as_array()
        .expect("the render emits initContainers");
    assert_eq!(
        sidecars.len(),
        2,
        "AC1 diffs BOTH restartable init sidecars; the render produced {sidecars:?}",
    );
    for sidecar in sidecars {
        assert_eq!(
            sidecar["restartPolicy"],
            json!("Always"),
            "a native sidecar is an initContainer with restartPolicy: Always; got {sidecar}",
        );
    }

    // The downward-API reference, spelled out. A Pod's uid does not exist when
    // the Job is built, so this reference is the only way the value can arrive.
    // `apiVersion` is the probe's normalisation of an API-server default, not
    // something the renderer emits — see `worker_pod_uid_env_ref`.
    assert_eq!(
        worker_pod_uid_env_ref(&template),
        json!({"fieldRef": {"apiVersion": "v1", "fieldPath": "metadata.uid"}}),
    );
    assert_eq!(
        template
            .pointer("/spec/containers/0/env")
            .and_then(Value::as_array)
            .and_then(|env| env
                .iter()
                .find(|var| var["name"] == "DJINN_TASK_RUN_POD_UID"))
            .and_then(|var| var.pointer("/valueFrom/fieldRef/apiVersion")),
        None,
        "the renderer must NOT set fieldRef.apiVersion; if it starts to, the probe's \
         normalisation stops being a normalisation and starts hiding a real value",
    );
}

/// The table must not quietly shrink. This is a literal transcription of AC1's
/// enumeration, held against the probe list.
#[test]
fn guard_every_field_ac1_enumerates_is_probed() {
    let enumerated = [
        "nodeSelector",
        "schedulingGates",
        "schedulerName",
        "runtimeClassName",
        "shareProcessNamespace",
        "automountServiceAccountToken",
        "restartPolicy",
        "securityContext.fsGroup",
        "securityContext.fsGroupChangePolicy",
        "karpenter.sh/do-not-disrupt annotation",
        "restartable init sidecars",
        "downward-API metadata.uid env ref",
        "propagated build-object label",
        // Job-level, diffed by AC1_JOB_FIELDS.
        "backoffLimit",
    ];
    let probed: Vec<&str> = AC1_POD_FIELDS
        .iter()
        .chain(AC1_JOB_FIELDS.iter())
        .map(|field| field.name)
        .collect();
    assert_eq!(
        probed, enumerated,
        "the probe table must cover exactly the fields AC1 enumerates, in order",
    );
}

/// A mistyped JSON pointer reads `Null` on both sides of the diff, so the
/// field silently stops being compared and the test stays green. The one
/// pointer that has a canonical spelling elsewhere in the codebase is checked
/// against it.
#[test]
fn guard_the_build_object_label_pointer_is_the_real_label() {
    let field = AC1_POD_FIELDS
        .iter()
        .find(|field| field.name == "propagated build-object label")
        .expect("AC1 enumerates the build-object label");
    let Probe::Pointer(pointer) = &field.probe else {
        panic!("the build-object label is read by pointer");
    };
    assert_eq!(
        *pointer,
        label_pointer(LABEL_KUEUE_BUILD_OBJECT),
        "the probe must point at the label the renderer actually stamps",
    );
}

/// The diff has to be able to REPORT a difference, not merely to compute one.
///
/// The live non-vacuity test asserts that a webhook-injected `nodeSelector` key
/// is named by the rendered diff. That assertion is only as good as
/// `render_diff`, and this exercises it with no cluster at all.
#[test]
fn guard_the_diff_names_an_injected_node_selector_key() {
    let (job, _) = rendered_task_run_job();
    let submitted = template_of(&job_as_json(&job));

    // What the API server hands back is the submitted template PLUS its
    // defaults. Simulating that here is what makes the single-entry assertion
    // below meaningful: a declared default must not itself read as a mutation,
    // or every live diff would carry a permanent false positive.
    let mut defaulted = submitted.clone();
    defaulted["spec"]["schedulerName"] = json!("default-scheduler");
    assert!(
        diff_fields(AC1_POD_FIELDS, &submitted, &defaulted).is_empty(),
        "an API-server default the table declares must not register as a difference: {:?}",
        diff_fields(AC1_POD_FIELDS, &submitted, &defaulted),
    );

    let mut mutated = defaulted.clone();
    mutated["spec"]["nodeSelector"] =
        json!({INJECTED_NODE_SELECTOR_KEY: INJECTED_NODE_SELECTOR_VALUE});

    let diffs = diff_fields(AC1_POD_FIELDS, &submitted, &mutated);
    assert_eq!(
        diffs.len(),
        1,
        "one injected key, one diff entry: {diffs:?}"
    );
    assert_eq!(diffs[0].name, "nodeSelector");
    let rendered = render_diff(&diffs);
    assert!(
        rendered.contains(INJECTED_NODE_SELECTOR_KEY),
        "the diff must NAME the injected key; rendered: {rendered}",
    );
}

/// AC3's premise, asserted rather than argued.
///
/// The chart renders `BestEffortFIFO`, not the `StrictFIFO` proposal 9oga
/// assumed, and deliberately: three kinds share one small queue and a
/// head-of-line Workload that cannot fit would block everything behind it
/// (`deploy/helm/djinn/templates/kueue-topology.yaml`,
/// `deploy/helm/djinn/tests/kueue-topology-render.sh`).
///
/// BestEffortFIFO differs from StrictFIFO in exactly one respect: it may skip a
/// head that does not fit and admit a lighter Workload behind it. That
/// difference is UNREACHABLE here, because every Workload this renderer
/// produces costs exactly one `pods` — `parallelism` and `completions` are both
/// unset, so the Job's single PodSet has count 1, and one unit is the smallest
/// unit the quota measures. A head can never be "too big for the remaining
/// quota while a follower fits", because there is no smaller follower. Under
/// that homogeneity the two strategies admit in the same order.
///
/// The moment this assertion fails — a renderer that sets `parallelism: 2`, or
/// a ClusterQueue that starts counting CPU — the equivalence argument is void
/// and AC3 must be re-derived rather than re-run.
#[test]
fn guard_every_rendered_workload_costs_exactly_one_pod() {
    let (job, _) = rendered_task_run_job();
    let manifest = job_as_json(&job);
    assert_eq!(
        manifest.pointer("/spec/parallelism"),
        None,
        "a rendered task-run Job must leave parallelism unset (defaulting to 1), or a Workload \
         could cost more than one pods and BestEffortFIFO could skip it for a lighter follower",
    );
    assert_eq!(
        manifest.pointer("/spec/completions"),
        None,
        "a rendered task-run Job must leave completions unset (defaulting to 1)",
    );
    assert_eq!(
        manifest.pointer("/spec/completionMode"),
        None,
        "an Indexed Job would carry a multi-pod PodSet",
    );
}

fn topology_template_text() -> String {
    std::fs::read_to_string(repo_root().join(TOPOLOGY_TEMPLATE))
        .expect("the Kueue topology template is readable")
}

/// The regression guard for the finding this task exists to have made.
///
/// A ClusterQueue with no `namespaceSelector` admits NOTHING — the CRD's own
/// doc string says "Defaults to null which is a nothing selector (no namespaces
/// eligible)". Rendering it back out is the single-line difference between an
/// armed install that dispatches and an armed install that captures every build
/// Job and runs none of them, with no error anywhere.
///
/// The live half proves the field WORKS; this proves it is still THERE, in the
/// lane that runs on every PR.
#[test]
fn guard_the_cluster_queue_still_declares_who_may_submit() {
    let template = topology_template_text();
    let cluster_queue = template
        .split("kind: ClusterQueue")
        .nth(1)
        .expect("the topology renders a ClusterQueue");
    assert!(
        cluster_queue.contains("namespaceSelector:"),
        "the ClusterQueue must declare a namespaceSelector. Without it Kueue admits nothing: \
         every Workload sits Pending with \"workload namespace doesn't match ClusterQueue \
         selector\" and no armed Job is ever unsuspended. Measured on a live armed cluster, \
         fbiy-B1, 2026-07-30.",
    );
}

/// The second regression guard for the second thing this task found.
///
/// Kueue refuses to assign a flavor when ANY resource a PodSet requests falls
/// outside the ClusterQueue's `resourceGroups`:
///
/// ```text
/// "couldn't assign flavors to pod set main: resource cpu unavailable in ClusterQueue"
/// ```
///
/// A `pods`-only ClusterQueue therefore admits NOTHING that asks for CPU — that
/// is, every real build Pod. The defect survived review because a synthetic Job
/// with no `resources:` block admits perfectly well, so only a Job from the real
/// renderer, against a real controller, could tell the two apart.
///
/// This guard holds both halves of that fact together: the renderer really does
/// request cpu and memory, and the chart really does cover them. Either one
/// alone is unfalsifiable.
#[test]
fn guard_the_cluster_queue_covers_the_resources_build_pods_request() {
    let (job, _) = rendered_task_run_job();
    let requests = job_as_json(&job)
        .pointer("/spec/template/spec/containers/0/resources/requests")
        .cloned()
        .expect("the worker container declares resource requests");
    assert!(
        requests.get("cpu").is_some(),
        "the worker requests cpu, so the ClusterQueue must cover it: {requests}",
    );
    assert!(
        requests.get("memory").is_some(),
        "the worker requests memory, so the ClusterQueue must cover it: {requests}",
    );

    let template = topology_template_text();
    let cluster_queue = template
        .split("kind: ClusterQueue")
        .nth(1)
        .expect("the topology renders a ClusterQueue");
    assert!(
        cluster_queue.contains(r#"coveredResources: ["pods", "cpu", "memory"]"#),
        "the ClusterQueue must cover every resource a build Pod requests. Covering only pods \
         leaves Kueue unable to assign a flavor and NOTHING is ever admitted — measured live, \
         fbiy-B1, 2026-07-30.",
    );
}

/// The chart renders BestEffortFIFO, and this file's AC3 argument depends on
/// knowing which one it is. Asserting it here means an edit to StrictFIFO
/// cannot land while this file still carries the homogeneity argument.
#[test]
fn guard_the_cluster_queue_is_best_effort_fifo() {
    let template = topology_template_text();
    assert!(
        template.contains("queueingStrategy: BestEffortFIFO"),
        "the chart must render BestEffortFIFO; AC3's ordering argument is stated in terms of it",
    );
    assert!(
        !template.contains("queueingStrategy: StrictFIFO"),
        "proposal 9oga asked for StrictFIFO and the chart deliberately does not render it \
         (three kinds share one small queue, and a long-lived warm Workload at the head would \
         block every task-run behind it). Do not change the chart to match the proposal — change \
         the proposal.",
    );
}

fn yaml_at(relative: &str) -> serde_yaml::Value {
    let path = repo_root().join(relative);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {relative}: {e}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("parse {relative}: {e}"))
}

fn fixture_build_pods() -> u64 {
    yaml_at(VALUES_FIXTURE)["kueue"]["buildPods"]
        .as_u64()
        .expect("kueue.buildPods is a number")
}

/// AC2 needs N admitted plus one queued, and AC3 needs two more queued behind a
/// full quota. A fixture quota of 1 would still satisfy "the (N+1)th is
/// suspended" while making the ordering test a single-Workload tautology.
#[test]
fn guard_the_fixture_quota_supports_the_ordering_test() {
    let build_pods = fixture_build_pods();
    assert!(
        build_pods >= 2,
        "the harness fixture's kueue.buildPods is {build_pods}; AC3 queues two Workloads behind a \
         full quota and needs at least two admitted slots for the release-one-at-a-time sequence \
         to distinguish an ordering from an accident",
    );
}

/// The ServiceAccount trap. A Pod naming a ServiceAccount that does not exist
/// is REJECTED at admission, and "no Pod appeared" is exactly what AC2 asserts
/// when the quota is exhausted. If these two drifted apart, AC2 would go green
/// while measuring a typo.
#[test]
fn guard_task_run_service_account_matches_the_chart() {
    let taskrun = yaml_at(CHART_VALUES)["serviceAccount"]["taskrun"]
        .as_str()
        .expect("the chart names a task-run ServiceAccount")
        .to_string();
    // `djinn.serviceAccountName.taskrun` is `printf "%s-%s" <fullname> <value>`,
    // and the harness installs release `djinn` of chart `djinn`, whose fullname
    // is `djinn`.
    assert_eq!(
        format!("djinn-{taskrun}"),
        TASK_RUN_SERVICE_ACCOUNT,
        "the live config's serviceAccountName must be the one the chart installs, or every Pod \
         in this file is rejected by ServiceAccount admission and every \"no Pod\" assertion \
         passes for the wrong reason",
    );
}

/// fbiy-B1 and fbiy-B2 run against two clusters at once. Divergence from the
/// setup script's defaults is asserted, not assumed — two harnesses sharing a
/// cluster name would have one deleting the other's cluster mid-run.
#[test]
fn guard_this_task_uses_names_of_its_own() {
    assert_ne!(HARNESS_CLUSTER, SETUP_SCRIPT_DEFAULT_CLUSTER);
    assert_ne!(HARNESS_REGISTRY, SETUP_SCRIPT_DEFAULT_REGISTRY);
    assert_ne!(HARNESS_REGISTRY_PORT, SETUP_SCRIPT_DEFAULT_REG_PORT);
    assert_eq!(HARNESS_CONTEXT, format!("kind-{HARNESS_CLUSTER}"));

    // And the script itself must accept them. `check` runs every production
    // -safety guard and then stops without creating anything, so this is a real
    // exercise of the refusals rather than a re-statement of them.
    let accepted = Command::new("bash")
        .arg(repo_root().join(SETUP_SCRIPT))
        .args([
            "check",
            "--cluster-name",
            HARNESS_CLUSTER,
            "--registry-name",
            HARNESS_REGISTRY,
            "--registry-port",
            HARNESS_REGISTRY_PORT,
            "--context",
            HARNESS_CONTEXT,
        ])
        .current_dir(repo_root())
        .output()
        .expect("setup-kueue-cluster.sh is executable");
    assert_eq!(
        exit_code(&accepted),
        0,
        "the setup script must accept this task's names; stderr: {}",
        stderr(&accepted),
    );
}

// ===========================================================================
// Live half — #[ignore] + DJINN_TEST_KUEUE_CLUSTER=1
// ===========================================================================

fn live_tests_enabled() -> bool {
    if env::var("DJINN_TEST_KUEUE_CLUSTER").is_err() {
        eprintln!("kueue_admission_conformance: DJINN_TEST_KUEUE_CLUSTER not set — skipping");
        return false;
    }
    for tool in ["kubectl", "helm", "docker", "openssl", "python3"] {
        if !which(tool) {
            eprintln!("kueue_admission_conformance: {tool} not found on PATH — skipping");
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

/// Every live test mutates one shared ClusterQueue and one shared namespace, so
/// they run one at a time. Serialising inside the binary rather than asking the
/// caller for `--test-threads=1` is deliberate: a budget that has to be
/// remembered is a budget that gets forgotten, and the failure mode is a
/// quota-accounting test reading another test's Jobs.
///
/// A poisoned lock is taken anyway — the next test's `reset_cluster` is what
/// makes a previous panic survivable, not an unpoisoned mutex.
fn serialise_live_tests() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The context every live call is pinned to, after two independent refusals of
/// anything else: the NAME must be the one this harness creates, and the
/// resolved API server must be loopback. kind always serves on loopback; no
/// managed control plane does, and all three contexts in a Djinn developer's
/// kubeconfig are EKS.
fn harness_context() -> String {
    let server = kubectl_raw(
        HARNESS_CONTEXT,
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
        "refusing to run against {server}: context {HARNESS_CONTEXT} does not resolve to a local \
         kind API server, so it is not a cluster this harness created",
    );
    HARNESS_CONTEXT.to_string()
}

/// A nonzero `kubectl` exit is never read as an empty result. `get workloads`
/// against a cluster with no Kueue CRDs exits nonzero, and treating that as
/// "zero Workloads" would make every negative assertion below pass against a
/// cluster that has no Kueue at all.
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

fn kubectl_best_effort(context: &str, args: &[&str]) {
    let _ = Command::new("kubectl")
        .arg("--context")
        .arg(context)
        .args(args)
        .output();
}

const TICK: Duration = Duration::from_millis(500);
/// How long a positive assertion waits before concluding something will never
/// appear. 60s.
const CAPTURE_TICKS: usize = 120;
/// The budget a NEGATIVE assertion waits before concluding absence. Named
/// rather than inlined, because "no Pod" is a statement about admission only if
/// the wait was long enough to have seen one — and the positive waits below
/// report how many ticks they actually needed.
const ABSENCE_TICKS: usize = 60;

fn poll<T>(ticks: usize, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..ticks {
        if let Some(found) = probe() {
            return Some(found);
        }
        std::thread::sleep(TICK);
    }
    probe()
}

fn get_job(context: &str, name: &str) -> Value {
    kubectl_json(context, &["-n", NAMESPACE, "get", "job", name])
}

fn job_is_unsuspended(context: &str, name: &str) -> bool {
    get_job(context, name).pointer("/spec/suspend") == Some(&json!(false))
}

/// The Pod the Job controller created for `job_name`, if any.
fn pod_of(context: &str, namespace: &str, job_name: &str) -> Option<Value> {
    let selector = format!("batch.kubernetes.io/job-name={job_name}");
    let list = kubectl_json(context, &["-n", namespace, "get", "pods", "-l", &selector]);
    list["items"]
        .as_array()
        .expect("a List has items")
        .first()
        .cloned()
}

fn workloads_owned_by(context: &str, namespace: &str, job_name: &str) -> Vec<Value> {
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
        .cloned()
        .collect()
}

fn sole_workload_of(context: &str, job_name: &str) -> Value {
    let owned = poll(CAPTURE_TICKS, || {
        let owned = workloads_owned_by(context, NAMESPACE, job_name);
        (!owned.is_empty()).then_some(owned)
    })
    .unwrap_or_default();
    assert_eq!(
        owned.len(),
        1,
        "exactly one Workload must own {job_name}, got {owned:?}",
    );
    owned.into_iter().next().expect("checked non-empty")
}

/// The ClusterQueue's admitted `pods` usage.
///
/// `status.flavorsUsage` is the field AC4 and AC5 name. It is a
/// `resource.Quantity`, which the API server round-trips as a STRING even
/// though the chart writes the quota as a bare number.
fn pods_usage(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let total = queue["status"]["flavorsUsage"][0]["resources"]
        .as_array()
        .unwrap_or_else(|| panic!("the ClusterQueue reports flavorsUsage: {}", queue["status"]))
        .iter()
        .find(|resource| resource["name"] == "pods")
        .map(|resource| resource["total"].clone())
        .expect("flavorsUsage covers pods");
    quantity_as_u64(&total)
}

fn nominal_quota(context: &str) -> u64 {
    let queue = kubectl_json(
        context,
        &["get", "clusterqueues.kueue.x-k8s.io", CLUSTER_QUEUE],
    );
    let quota = queue["spec"]["resourceGroups"][0]["flavors"][0]["resources"]
        .as_array()
        .expect("the flavor covers resources")
        .iter()
        .find(|resource| resource["name"] == "pods")
        .map(|resource| resource["nominalQuota"].clone())
        .expect("the ClusterQueue bounds the pods resource");
    quantity_as_u64(&quota)
}

fn quantity_as_u64(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or_else(|| panic!("not an integer quantity: {value}"))
}

fn await_usage(context: &str, expected: u64) -> u64 {
    poll(CAPTURE_TICKS, || {
        let usage = pods_usage(context);
        (usage == expected).then_some(usage)
    })
    .unwrap_or_else(|| pods_usage(context))
}

/// Recent events for a Job and its Pods, so a failure says WHY rather than
/// "expected 1, got 0".
fn diagnose(context: &str, job_name: &str) -> String {
    let events = Command::new("kubectl")
        .args([
            "--context",
            context,
            "-n",
            NAMESPACE,
            "get",
            "events",
            "--field-selector",
            &format!("involvedObject.name={job_name}"),
            "--sort-by=.lastTimestamp",
        ])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let workloads = workloads_owned_by(context, NAMESPACE, job_name);
    let conditions: Vec<Value> = workloads
        .iter()
        .map(|workload| workload["status"]["conditions"].clone())
        .collect();
    format!("job events:\n{events}\nworkload conditions: {conditions:?}")
}

/// Delete every task-run Job this file created and wait for the quota to drain.
///
/// Called at the START of each live test, not only at the end: a previous test
/// that panicked mid-way leaves Jobs holding quota, and a quota-accounting
/// assertion that inherits them fails for a reason with no connection to what
/// it is testing.
fn reset_cluster(context: &str) {
    kubectl_best_effort(
        context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "jobs",
            "-l",
            "djinn.app/component=task-run-worker",
            "--wait=true",
        ],
    );
    kubectl_best_effort(
        context,
        &[
            "delete",
            "mutatingwebhookconfiguration",
            WEBHOOK_CONFIG_NAME,
            "--ignore-not-found",
        ],
    );
    // Restore the fixture's quota if a previous run's non-vacuity bump survived.
    let fixture = fixture_build_pods();
    if nominal_quota(context) != fixture {
        helm_set_build_pods(context, fixture);
    }
    let drained = await_usage(context, 0);
    assert_eq!(
        drained, 0,
        "the ClusterQueue still reports {drained} pods in use after deleting every task-run Job; \
         a live test cannot start from an unknown quota",
    );
}

/// Re-run the REAL chart with a different `kueue.buildPods`.
///
/// AC2's non-vacuity is "set buildPods: N+1, re-run", so this goes through
/// `helm upgrade` of the same chart and the same values fixture rather than
/// patching the live ClusterQueue. A patched object would prove the CONTROLLER
/// honours a quota; only a re-render proves the CHART is what sets it.
fn helm_set_build_pods(context: &str, build_pods: u64) {
    let output = Command::new("helm")
        .args(["--kube-context", context, "upgrade", "--install", "djinn"])
        .arg(repo_root().join(CHART_DIR))
        .args(["--namespace", NAMESPACE, "--create-namespace", "--values"])
        .arg(repo_root().join(VALUES_FIXTURE))
        .args([
            "--set",
            "kueue.enabled=true",
            "--set",
            "kueue.armed=true",
            "--set",
            &format!("kueue.buildPods={build_pods}"),
        ])
        .current_dir(repo_root())
        .output()
        .expect("helm is on PATH");
    assert!(
        output.status.success(),
        "helm upgrade with kueue.buildPods={build_pods} failed: {}",
        stderr(&output),
    );
    let observed = poll(CAPTURE_TICKS, || {
        let quota = nominal_quota(context);
        (quota == build_pods).then_some(quota)
    })
    .unwrap_or_else(|| nominal_quota(context));
    assert_eq!(
        observed, build_pods,
        "the live ClusterQueue must carry the buildPods the chart was just rendered with",
    );
}

/// Submit a rendered task-run Job and return its name.
fn submit_task_run_job(context: &str, webhook_target: bool) -> String {
    let (job, name) = rendered_task_run_job();
    let mut manifest = job_as_json(&job);
    if webhook_target {
        manifest["spec"]["template"]["metadata"]["labels"]
            .as_object_mut()
            .expect("the renderer stamps template labels")
            .insert(WEBHOOK_TARGET_LABEL.into(), json!("true"));
    }
    kubectl_apply(context, NAMESPACE, &manifest);
    name
}

/// Wait for a Job to be admitted (unsuspended) AND for its Pod to exist.
fn await_admitted_pod(context: &str, job_name: &str) -> Value {
    poll(CAPTURE_TICKS, || pod_of(context, NAMESPACE, job_name)).unwrap_or_else(|| {
        panic!(
            "no Pod ever appeared for admitted Job {job_name}. {}",
            diagnose(context, job_name)
        )
    })
}

// ---------------------------------------------------------------------------
// AC1 — Kueue mutates only spec.suspend
// ---------------------------------------------------------------------------

/// AC1. The Job comes out of `build_task_run_job`, is serialised unmodified to
/// the API server, and the admitted Pod's spec is compared to the SUBMITTED
/// template field by field.
///
/// Three comparisons, because "only `spec.suspend`" is three claims:
///
/// 1. the Job the API server STORED still carries the template that was
///    submitted — this is where a ResourceFlavor's `nodeLabels` would land, and
///    it is the mutation Kueue is actually capable of making;
/// 2. the Job's `spec.suspend` went `true` -> `false` and its `backoffLimit`
///    did not move;
/// 3. the POD the kubelet was handed matches the submitted template across
///    every field AC1 enumerates.
///
/// Its non-vacuity is a separate test, because a diff that cannot report a
/// difference would pass all three.
#[test]
#[ignore]
fn live_kueue_mutates_only_spec_suspend() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let (job, job_name) = rendered_task_run_job();
    let submitted = job_as_json(&job);
    assert_eq!(
        submitted.pointer("/spec/suspend"),
        Some(&json!(true)),
        "the armed renderer must create the Job suspended so Kueue owns admission",
    );
    kubectl_apply(&context, NAMESPACE, &submitted);

    let admitted = poll(CAPTURE_TICKS, || {
        job_is_unsuspended(&context, &job_name).then(|| get_job(&context, &job_name))
    })
    .unwrap_or_else(|| {
        panic!(
            "Job {job_name} was never unsuspended. {}",
            diagnose(&context, &job_name)
        )
    });

    // (1) The stored Job's template is the submitted template.
    let template_diffs = diff_fields(
        AC1_POD_FIELDS,
        &template_of(&submitted),
        &template_of(&admitted),
    );
    assert!(
        template_diffs.is_empty(),
        "Kueue rewrote the Job's pod template:\n{}",
        render_diff(&template_diffs),
    );

    // (2) suspend flipped; nothing else at Job level moved.
    assert_eq!(
        admitted.pointer("/spec/suspend"),
        Some(&json!(false)),
        "admission is exactly the un-suspending of the Job",
    );
    let job_diffs = diff_fields(AC1_JOB_FIELDS, &submitted, &admitted);
    assert!(
        job_diffs.is_empty(),
        "Kueue changed a Job field other than spec.suspend:\n{}",
        render_diff(&job_diffs),
    );

    // (3) The Pod.
    let pod = await_admitted_pod(&context, &job_name);
    let pod_diffs = diff_fields(AC1_POD_FIELDS, &template_of(&submitted), &pod);
    assert!(
        pod_diffs.is_empty(),
        "the admitted Pod's spec diverges from the submitted template:\n{}",
        render_diff(&pod_diffs),
    );

    // The Pod is a real Pod that a real kubelet accepted, not an object the
    // field table happened to find nothing in.
    assert_eq!(
        pod.pointer("/spec/serviceAccountName"),
        Some(&json!(TASK_RUN_SERVICE_ACCOUNT)),
    );
    assert!(
        pod.pointer("/metadata/uid")
            .and_then(Value::as_str)
            .is_some(),
        "the API server assigned the Pod a uid",
    );

    reset_cluster(&context);
}

/// AC1's non-vacuity, and the only thing that makes the three comparisons above
/// mean anything: prove the diff can SEE a mutation.
///
/// A mutating admission webhook is registered for exactly one label selector,
/// injects exactly one `nodeSelector` key into the Pod, and the field-by-field
/// diff must come back with that one field and must NAME the key.
///
/// The webhook server runs on the host and is reached over the kind docker
/// bridge (`clientConfig.url`, which Kubernetes requires to be https). That
/// avoids pushing an image into the disposable registry for a forty-line
/// server, and it is torn down by [`MutatingWebhook`]'s `Drop` whether this
/// test passes or panics.
#[test]
#[ignore]
fn live_the_field_by_field_diff_names_a_webhook_injected_node_selector() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let webhook = MutatingWebhook::install(&context);

    let (job, job_name) = rendered_task_run_job();
    let mut submitted = job_as_json(&job);
    submitted["spec"]["template"]["metadata"]["labels"]
        .as_object_mut()
        .expect("the renderer stamps template labels")
        .insert(WEBHOOK_TARGET_LABEL.into(), json!("true"));
    kubectl_apply(&context, NAMESPACE, &submitted);

    let pod = poll(CAPTURE_TICKS, || pod_of(&context, NAMESPACE, &job_name)).unwrap_or_else(|| {
        panic!(
            "no Pod appeared for {job_name}. The webhook is registered failurePolicy: Fail, so an \
             unreachable webhook server rejects Pod creation outright — check that the API server \
             can reach {} on the kind bridge. {}",
            webhook.url,
            diagnose(&context, &job_name),
        )
    });

    let diffs = diff_fields(AC1_POD_FIELDS, &template_of(&submitted), &pod);
    let rendered = render_diff(&diffs);
    assert_eq!(
        diffs.len(),
        1,
        "exactly one field was injected; the diff reported:\n{rendered}",
    );
    assert_eq!(
        diffs[0].name, "nodeSelector",
        "the injected field is nodeSelector; the diff reported:\n{rendered}",
    );
    assert!(
        rendered.contains(INJECTED_NODE_SELECTOR_KEY),
        "the diff must NAME the injected key {INJECTED_NODE_SELECTOR_KEY}; reported:\n{rendered}",
    );

    drop(webhook);
    reset_cluster(&context);
}

// ---------------------------------------------------------------------------
// AC2 — the pods quota bounds admission by side effect
// ---------------------------------------------------------------------------

/// AC2. With `kueue.buildPods: N`, submit N+1 rendered task-run Jobs.
///
/// The evidence is a SIDE EFFECT, never a status string: N Jobs are unsuspended
/// and each has a Pod; the last Job still has `spec.suspend == true` and NO
/// Pod. Then one admitted Job is deleted and the queued one becomes
/// `suspend: false` with a Pod of its own.
///
/// The N Pods that DID appear are what make the (N+1)th Pod's absence a
/// statement about the quota: Pod creation demonstrably works in this
/// namespace, with this ServiceAccount, for this exact manifest.
///
/// Non-vacuity: the whole sequence is then re-run with the chart re-rendered at
/// `buildPods: N+1`, where the (N+1)th Job must admit immediately.
#[test]
#[ignore]
fn live_the_pods_quota_bounds_admission_and_a_release_admits_the_queued_job() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let quota = nominal_quota(&context);
    assert_eq!(
        quota,
        fixture_build_pods(),
        "the live ClusterQueue must carry the fixture's kueue.buildPods",
    );

    // N+1 Jobs, submitted oldest-first with a gap so "the last one" is a fact
    // about creation order rather than about a tie-break.
    let mut names = Vec::new();
    for _ in 0..=quota {
        names.push(submit_task_run_job(&context, false));
        std::thread::sleep(Duration::from_millis(1200));
    }
    let (admitted_names, queued_name) = names.split_at(quota as usize);
    let queued_name = queued_name
        .first()
        .expect("N+1 jobs were submitted")
        .clone();

    for name in admitted_names {
        let unsuspended = poll(CAPTURE_TICKS, || {
            job_is_unsuspended(&context, name).then_some(())
        });
        assert!(
            unsuspended.is_some(),
            "Job {name} is within the quota of {quota} and must be unsuspended. {}",
            diagnose(&context, name),
        );
        let pod = await_admitted_pod(&context, name);
        assert!(
            pod.pointer("/metadata/name").is_some(),
            "an admitted Job must have a Pod",
        );
    }

    assert_eq!(
        await_usage(&context, quota),
        quota,
        "the ClusterQueue must report the full quota in use",
    );

    // The (N+1)th. ABSENCE_TICKS is a real wait, not a glance: every positive
    // capture above landed well inside it.
    let no_pod = poll(ABSENCE_TICKS, || {
        pod_of(&context, NAMESPACE, &queued_name).map(|_| ())
    });
    assert!(
        no_pod.is_none(),
        "the (N+1)th Job must have no Pod while the quota is full; it has {:?}",
        pod_of(&context, NAMESPACE, &queued_name),
    );
    let queued = get_job(&context, &queued_name);
    assert_eq!(
        queued.pointer("/spec/suspend"),
        Some(&json!(true)),
        "the (N+1)th Job must EXIST and still be suspended. {}",
        diagnose(&context, &queued_name),
    );

    // Release one slot.
    let released = &admitted_names[0];
    kubectl_raw(
        &context,
        &["-n", NAMESPACE, "delete", "job", released, "--wait=true"],
    );

    let promoted = poll(CAPTURE_TICKS, || {
        job_is_unsuspended(&context, &queued_name).then_some(())
    });
    assert!(
        promoted.is_some(),
        "deleting an admitted Job must release a slot the queued Job takes. {}",
        diagnose(&context, &queued_name),
    );
    let promoted_pod = await_admitted_pod(&context, &queued_name);
    assert!(
        promoted_pod.pointer("/metadata/name").is_some(),
        "the promoted Job's Pod must appear",
    );

    reset_cluster(&context);

    // ---- Non-vacuity: the same (N+1)th Job admits immediately at N+1. ----
    helm_set_build_pods(&context, quota + 1);

    let mut wider = Vec::new();
    for _ in 0..=quota {
        wider.push(submit_task_run_job(&context, false));
        std::thread::sleep(Duration::from_millis(1200));
    }
    let last = wider.last().expect("N+1 jobs were submitted");
    let unsuspended = poll(CAPTURE_TICKS, || {
        job_is_unsuspended(&context, last).then_some(())
    });
    assert!(
        unsuspended.is_some(),
        "with kueue.buildPods={} the (N+1)th Job must admit immediately — otherwise the suspension \
         above was not the quota's doing. {}",
        quota + 1,
        diagnose(&context, last),
    );
    await_admitted_pod(&context, last);
    assert_eq!(
        await_usage(&context, quota + 1),
        quota + 1,
        "all N+1 Workloads must be in use at the wider quota",
    );

    reset_cluster(&context);
}

// ---------------------------------------------------------------------------
// AC3 — ordering under BestEffortFIFO
// ---------------------------------------------------------------------------

/// AC3. With the quota full and Jobs A then B queued behind it, one release
/// admits A — A's Pod appears, B stays suspended with no Pod — and the next
/// release admits B.
///
/// THE QUEUE IS `BestEffortFIFO`, NOT `StrictFIFO`.
/// `deploy/helm/djinn/templates/kueue-topology.yaml` renders it that way
/// deliberately: three kinds (task-run, warm, SCIP) share one small queue and a
/// long-lived warm Workload at the head would block every task-run behind it
/// under StrictFIFO. Proposal 9oga's head-of-line-blocking criterion is
/// therefore unprovable as written, and the chart is right, not the proposal.
///
/// WHY THE ORDERING PROPERTY STILL HOLDS. BestEffortFIFO differs from StrictFIFO
/// in exactly one way: it may skip a head that does not fit and admit a lighter
/// Workload behind it. Here every Workload costs exactly ONE `pods` — the
/// renderer leaves `parallelism` and `completions` unset, so each Job has a
/// single PodSet of count 1, which
/// `guard_every_rendered_workload_costs_exactly_one_pod` asserts hermetically.
/// A head can never be skipped for a lighter follower when no follower is
/// lighter. Under that homogeneity the two strategies admit in the same order,
/// which is why this test asserts FIFO admission against a BestEffortFIFO queue
/// without weakening the criterion.
///
/// A and B are submitted with a gap wider than the API server's one-second
/// `creationTimestamp` granularity, because that timestamp is the ordering key
/// and simultaneous creations are a documented tie, not a FIFO violation.
#[test]
#[ignore]
fn live_ordering_admits_the_older_workload_first() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let quota = nominal_quota(&context);

    // Fill the quota.
    let mut filler = Vec::new();
    for _ in 0..quota {
        filler.push(submit_task_run_job(&context, false));
        std::thread::sleep(Duration::from_millis(1200));
    }
    for name in &filler {
        assert!(
            poll(CAPTURE_TICKS, || job_is_unsuspended(&context, name)
                .then_some(()))
            .is_some(),
            "the filler Jobs must occupy the quota. {}",
            diagnose(&context, name),
        );
    }
    assert_eq!(await_usage(&context, quota), quota);

    // A, then B, two seconds apart.
    let a = submit_task_run_job(&context, false);
    std::thread::sleep(Duration::from_secs(2));
    let b = submit_task_run_job(&context, false);

    let a_created = workload_creation_timestamp(&context, &a);
    let b_created = workload_creation_timestamp(&context, &b);
    assert!(
        a_created < b_created,
        "A's Workload must be strictly older than B's for FIFO to be observable; A={a_created} \
         B={b_created}",
    );

    // Both queued.
    for name in [&a, &b] {
        assert_eq!(
            get_job(&context, name).pointer("/spec/suspend"),
            Some(&json!(true)),
            "{name} must be queued while the quota is full",
        );
    }

    // One release: A admits, B does not.
    kubectl_raw(
        &context,
        &["-n", NAMESPACE, "delete", "job", &filler[0], "--wait=true"],
    );
    assert!(
        poll(CAPTURE_TICKS, || job_is_unsuspended(&context, &a)
            .then_some(()))
        .is_some(),
        "the older Workload must take the freed slot. {}",
        diagnose(&context, &a),
    );
    await_admitted_pod(&context, &a);

    assert!(
        poll(ABSENCE_TICKS, || pod_of(&context, NAMESPACE, &b)
            .map(|_| ()))
        .is_none(),
        "B must have no Pod while only one slot was freed",
    );
    assert_eq!(
        get_job(&context, &b).pointer("/spec/suspend"),
        Some(&json!(true)),
        "B must still be suspended after a single release",
    );

    // The next release: B admits.
    kubectl_raw(
        &context,
        &["-n", NAMESPACE, "delete", "job", &a, "--wait=true"],
    );
    assert!(
        poll(CAPTURE_TICKS, || job_is_unsuspended(&context, &b)
            .then_some(()))
        .is_some(),
        "the second release must admit B. {}",
        diagnose(&context, &b),
    );
    await_admitted_pod(&context, &b);

    reset_cluster(&context);
}

fn workload_creation_timestamp(context: &str, job_name: &str) -> String {
    sole_workload_of(context, job_name)["metadata"]["creationTimestamp"]
        .as_str()
        .expect("a stored object has a creationTimestamp")
        .to_string()
}

// ---------------------------------------------------------------------------
// AC4 — direct Job deletion releases quota
// ---------------------------------------------------------------------------

/// AC4. Delete an admitted Job directly: its Workload must be garbage-collected
/// through the `ownerReferences` link Kueue put there, and the ClusterQueue's
/// `status.flavorsUsage` for `pods` must fall by exactly one.
///
/// Both halves matter. A Workload that survived its Job would hold quota
/// forever; a usage figure that did not move would mean the ClusterQueue was
/// accounting something other than what it admitted.
#[test]
#[ignore]
fn live_direct_job_deletion_collects_the_workload_and_releases_quota() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let job_name = submit_task_run_job(&context, false);
    assert!(
        poll(CAPTURE_TICKS, || job_is_unsuspended(&context, &job_name)
            .then_some(()))
        .is_some(),
        "the Job must be admitted before its deletion can release anything. {}",
        diagnose(&context, &job_name),
    );
    await_admitted_pod(&context, &job_name);

    let workload = sole_workload_of(&context, &job_name);
    let workload_name = workload["metadata"]["name"]
        .as_str()
        .expect("the Workload is named")
        .to_string();
    let owners = workload["metadata"]["ownerReferences"]
        .as_array()
        .expect("Kueue owns the Workload to the Job");
    assert!(
        owners
            .iter()
            .any(|owner| owner["kind"] == "Job" && owner["name"] == json!(job_name)),
        "the Workload's ownerReferences must name the Job — that link is what garbage-collects it: \
         {owners:?}",
    );

    let before = await_usage(&context, 1);
    assert_eq!(before, 1, "one admitted Workload is one pod of usage");

    kubectl_raw(
        &context,
        &["-n", NAMESPACE, "delete", "job", &job_name, "--wait=true"],
    );

    let collected = poll(CAPTURE_TICKS, || {
        workloads_owned_by(&context, NAMESPACE, &job_name)
            .is_empty()
            .then_some(())
    });
    assert!(
        collected.is_some(),
        "Workload {workload_name} outlived its owning Job and is still holding quota",
    );

    let after = await_usage(&context, before - 1);
    assert_eq!(
        after,
        before - 1,
        "deleting an admitted Job must drop pods usage by exactly one",
    );

    reset_cluster(&context);
}

// ---------------------------------------------------------------------------
// AC5 — Workload deletion is self-healing
// ---------------------------------------------------------------------------

/// AC5. Delete ONLY the Workload of a healthy admitted Job. Kueue must recreate
/// it — a genuinely new object, with a different `metadata.uid` — and usage
/// must return to what it was.
///
/// The uid comparison is the whole test. "A Workload named X exists" is true
/// both before and after and would pass against a delete that never happened;
/// only the uid distinguishes a recreated object from the original.
#[test]
#[ignore]
fn live_workload_deletion_is_self_healing() {
    if !live_tests_enabled() {
        return;
    }
    let _serial = serialise_live_tests();
    let context = harness_context();
    reset_cluster(&context);

    let job_name = submit_task_run_job(&context, false);
    assert!(
        poll(CAPTURE_TICKS, || job_is_unsuspended(&context, &job_name)
            .then_some(()))
        .is_some(),
        "the Job must be admitted and healthy before its Workload is deleted. {}",
        diagnose(&context, &job_name),
    );
    await_admitted_pod(&context, &job_name);

    let original = sole_workload_of(&context, &job_name);
    let original_name = original["metadata"]["name"]
        .as_str()
        .expect("the Workload is named")
        .to_string();
    let original_uid = original["metadata"]["uid"]
        .as_str()
        .expect("a stored object has a uid")
        .to_string();
    let usage_before = await_usage(&context, 1);
    assert_eq!(usage_before, 1);

    kubectl_raw(
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "workloads.kueue.x-k8s.io",
            &original_name,
            "--wait=false",
        ],
    );

    let recreated = poll(CAPTURE_TICKS, || {
        let owned = workloads_owned_by(&context, NAMESPACE, &job_name);
        owned
            .into_iter()
            .find(|workload| workload["metadata"]["uid"] != json!(original_uid))
    })
    .unwrap_or_else(|| {
        panic!(
            "Kueue never recreated a Workload for {job_name} after its Workload was deleted. {}",
            diagnose(&context, &job_name)
        )
    });

    assert_ne!(
        recreated["metadata"]["uid"],
        json!(original_uid),
        "a recreated Workload must be a NEW object, not the one that was deleted",
    );

    let usage_after = await_usage(&context, usage_before);
    assert_eq!(
        usage_after, usage_before,
        "usage must return to its prior value once Kueue has re-admitted the recreated Workload",
    );

    reset_cluster(&context);
}

// ===========================================================================
// The mutating webhook that gives AC1's diff its teeth.
// ===========================================================================

/// A self-signed HTTPS admission webhook, running on the host, registered
/// against the disposable cluster for exactly one Pod label.
///
/// It is a host process rather than an in-cluster Deployment because the
/// disposable node has no image that could serve one, and pushing one into the
/// throwaway registry to run forty lines of Python would be a second moving
/// part with its own failure modes. Kubernetes supports `clientConfig.url` for
/// exactly this; the API server reaches the host over the kind docker bridge.
struct MutatingWebhook {
    context: String,
    url: String,
    server: Child,
    work_dir: PathBuf,
}

/// Written to disk at test time rather than checked in: it is an input to a
/// process this test starts, in the same sense the JSON manifests above are,
/// and a second checked-in file would drift from the constants it depends on.
const WEBHOOK_SERVER_PY: &str = r#"
import base64, json, ssl, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

CERT, KEY, PORT, NS_KEY, NS_VALUE = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4], sys.argv[5]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get('Content-Length', 0))
        review = json.loads(self.rfile.read(length) or b'{}')
        patch = [{"op": "add", "path": "/spec/nodeSelector", "value": {NS_KEY: NS_VALUE}}]
        body = json.dumps({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "response": {
                # The API server rejects a response whose uid does not echo the
                # request's, so this is not decoration.
                "uid": review.get("request", {}).get("uid", ""),
                "allowed": True,
                "patchType": "JSONPatch",
                "patch": base64.b64encode(json.dumps(patch).encode()).decode(),
            },
        }).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(CERT, KEY)
server = HTTPServer(('0.0.0.0', PORT), Handler)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
"#;

impl MutatingWebhook {
    fn install(context: &str) -> Self {
        let work_dir = env::temp_dir().join(format!(
            "djinn-kueue-b1-webhook-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&work_dir).expect("create the webhook work dir");

        // The address the API server dials. kind puts its nodes on the `kind`
        // docker network, whose gateway IS this host.
        let gateway = kind_bridge_gateway();
        let cert = work_dir.join("cert.pem");
        let key = work_dir.join("key.pem");
        let openssl = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=djinn-kueue-b1-webhook",
                "-addext",
                &format!("subjectAltName=IP:{gateway}"),
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .expect("openssl is on PATH");
        assert!(
            openssl.status.success(),
            "generating the webhook certificate failed: {}",
            stderr(&openssl),
        );

        let script = work_dir.join("webhook.py");
        std::fs::write(&script, WEBHOOK_SERVER_PY).expect("write the webhook server");
        let log = work_dir.join("webhook.log");
        let mut server = Command::new("python3")
            .arg(&script)
            .arg(&cert)
            .arg(&key)
            .arg(WEBHOOK_PORT.to_string())
            .arg(INJECTED_NODE_SELECTOR_KEY)
            .arg(INJECTED_NODE_SELECTOR_VALUE)
            .stdout(Stdio::null())
            // To a FILE, not a pipe nobody reads. The first run of this test
            // failed with "connection refused" from the API server while the
            // readiness probe below was perfectly happy, and the reason was in
            // this stream.
            .stderr(std::fs::File::create(&log).expect("create the webhook log"))
            .spawn()
            .expect("python3 is on PATH");

        // Listening is a precondition, not a hope: the webhook is registered
        // failurePolicy: Fail, so a server that has not bound yet turns into
        // "Pod creation rejected" thirty seconds later with no obvious cause.
        //
        // The probe dials the GATEWAY address rather than loopback, and that is
        // not a detail. An unrelated `kubectl port-forward` holding
        // `127.0.0.1:<port>` satisfies a loopback probe perfectly while THIS
        // server is dead of EADDRINUSE — measured, on the first run — and the
        // API server, which dials the bridge address, then gets a connection
        // refused it cannot explain. Probing the address the API server will
        // actually use is the only probe that means anything.
        let target: SocketAddr = format!("{gateway}:{WEBHOOK_PORT}")
            .parse()
            .unwrap_or_else(|e| panic!("kind's gateway {gateway} is not an address: {e}"));
        let listening = poll(20, || {
            TcpStream::connect_timeout(&target, Duration::from_millis(250))
                .ok()
                .map(|_| ())
        });
        if listening.is_none() {
            let exited = server.try_wait().ok().flatten();
            let stderr_text = std::fs::read_to_string(&log).unwrap_or_default();
            let _ = server.kill();
            panic!(
                "the webhook server never accepted a connection on {target} (child exit: \
                 {exited:?}). Its stderr:\n{stderr_text}",
            );
        }

        let ca_bundle = Command::new("openssl")
            .args(["base64", "-A", "-in"])
            .arg(&cert)
            .output()
            .expect("openssl is on PATH");
        assert!(ca_bundle.status.success(), "base64-encoding the CA failed");
        let ca_bundle = String::from_utf8_lossy(&ca_bundle.stdout)
            .trim()
            .to_string();

        let url = format!("https://{gateway}:{WEBHOOK_PORT}/mutate");
        let configuration = json!({
            "apiVersion": "admissionregistration.k8s.io/v1",
            "kind": "MutatingWebhookConfiguration",
            "metadata": { "name": WEBHOOK_CONFIG_NAME },
            "webhooks": [{
                "name": "nodeselector.harness.djinn.io",
                "admissionReviewVersions": ["v1"],
                "sideEffects": "None",
                // Fail, not Ignore: an unreachable webhook must break the test
                // loudly rather than quietly produce an unmutated Pod that the
                // diff would then report as "no difference" — which is the
                // PASSING answer for every other test in this file.
                "failurePolicy": "Fail",
                "matchPolicy": "Equivalent",
                "timeoutSeconds": 10,
                "clientConfig": { "url": url, "caBundle": ca_bundle },
                "rules": [{
                    "operations": ["CREATE"],
                    "apiGroups": [""],
                    "apiVersions": ["v1"],
                    "resources": ["pods"],
                    "scope": "Namespaced",
                }],
                "namespaceSelector": { "matchLabels": { "kubernetes.io/metadata.name": NAMESPACE } },
                // The blast radius. Only the one Pod this test labels is
                // mutated, so Kueue's own controllers and every other test's
                // Pods are untouched while this is registered.
                "objectSelector": { "matchLabels": { WEBHOOK_TARGET_LABEL: "true" } },
            }],
        });
        kubectl_apply(context, NAMESPACE, &configuration);

        Self {
            context: context.to_string(),
            url,
            server,
            work_dir,
        }
    }
}

impl Drop for MutatingWebhook {
    fn drop(&mut self) {
        // Unregister FIRST. A registered webhook whose server is gone rejects
        // every labelled Pod creation in the namespace, and this runs on a
        // panic path too.
        kubectl_best_effort(
            &self.context,
            &[
                "delete",
                "mutatingwebhookconfiguration",
                WEBHOOK_CONFIG_NAME,
                "--ignore-not-found",
            ],
        );
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }
}

/// The IPv4 gateway of kind's docker network — the address a node container
/// reaches this host on.
fn kind_bridge_gateway() -> String {
    let output = Command::new("docker")
        .args([
            "network",
            "inspect",
            "kind",
            "-f",
            "{{range .IPAM.Config}}{{println .Gateway}}{{end}}",
        ])
        .output()
        .expect("docker is on PATH");
    assert!(
        output.status.success(),
        "docker network inspect kind failed: {}",
        stderr(&output),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.contains(':'))
        .unwrap_or_else(|| {
            panic!(
                "kind's docker network has no IPv4 gateway; the API server cannot reach the host"
            )
        })
        .to_string()
}
