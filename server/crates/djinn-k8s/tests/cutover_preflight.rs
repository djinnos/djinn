//! Acceptance contract for the `3i92` cutover preflight (task `zpen`).
//!
//! # The rule this suite is built around
//!
//! Seven changes shipped in this repository in two days that were merged,
//! green, and unreachable in production. So every case below was written by
//! first answering: **what stays green if the validator body does nothing?**
//! The answer has to be "nothing", and that is enforced three ways:
//!
//! 1. Every case drives the PRODUCTION [`cutover_preflight::run`] — the same
//!    function `bin/cutover-preflight.rs` calls. There is no test-local
//!    reimplementation of any rule.
//! 2. Every positive case is a LIVE `helm template` render of
//!    `deploy/helm/djinn` with the repo's stock `values.yaml`, produced at test
//!    time. No checked-in YAML is the input to anything.
//! 3. Every negative case is that live render with exactly ONE field mutated,
//!    and [`each_negative_fixture_differs_from_the_live_render_in_exactly_one_path`]
//!    proves the mutation is the only difference — so a rejection can never be
//!    attributed to fixture drift.
//!
//! # Why almost everything here is `#[ignore]`d
//!
//! The suite needs `helm` (for the live render) and a real Postgres (for the
//! drain fence). The ordinary sharded test lane has Postgres but not helm, so
//! the render-bearing proofs are `#[ignore]`d and executed by the dedicated
//! `cutover-preflight` quality-gate job, which installs helm v3.12.3 and runs
//! them with `--ignored`.
//!
//! An `#[ignore]` that nothing runs is a silent skip, which is the exact class
//! of defect this task exists to remove. Two always-on guards close it:
//! [`the_cutover_preflight_lane_is_wired_and_cannot_silently_skip`] reads
//! `.github/workflows/quality-gate.yml` back and fails if the job, its
//! `--ignored` invocation or its declared proof count disappears, and the lane
//! itself fails if fewer than `DJINN_CUTOVER_EXPECTED_PROOFS` proofs execute.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_core::events::EventBus;
use djinn_db::repositories::build_pod_permit::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildPodPermitState,
    BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult,
    TransitionBuildPodResizeLifecycleResult,
};
use djinn_db::{
    BuildPodPermitRepository, CreateTaskRunParams, Database, LegacyDigestInventory,
    PreProtocolDigest, ProjectRepository, TaskRepository, TaskRunRepository,
};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::cutover_preflight::{
    BirthObservation, CatalogDeclaration, CatalogImage, CutoverPreflightInput, DefectClass,
    DrainFenceObservation, RESIZE_RULE_API_GROUP, RESIZE_RULE_RESOURCE, RESIZE_RULE_VERB,
    TOKEN_AUDIENCE, catalog_verdict, observe_drain_fence, run,
};
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::{
    LAUNCHER_CONTAINER_NAME, LAUNCHER_CPU_REQUEST_MILLICORES, apply_launcher_authority_protocol,
};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use serde::Deserialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Repository / live render
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root")
}

/// The LIVE render of the shipped chart, with the repo's stock `values.yaml`.
///
/// Rendered once per process and cloned per case, so twenty proofs cost one
/// `helm template`. `--is-upgrade` matches `deploy/preflight/render-gate.sh`:
/// `.Release.IsInstall` is the chart's only install-vs-upgrade branch and it
/// gates a bootstrap secret that has nothing to do with the cutover.
fn live_render() -> &'static [Value] {
    static RENDER: OnceLock<Vec<Value>> = OnceLock::new();
    RENDER.get_or_init(|| {
        let chart = repo_root().join("deploy/helm/djinn");
        let output = Command::new("helm")
            .arg("template")
            .arg("djinn-cutover-preflight")
            .arg(&chart)
            .arg("--is-upgrade")
            .output()
            .expect(
                "`helm` must be on PATH: this suite's whole claim is that its fixtures are a live \
                 render of the shipped chart, so a missing helm is a failure and never a skip",
            );
        assert!(
            output.status.success(),
            "helm template failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("helm output is utf-8");
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&stdout)
            .filter_map(|document| Value::deserialize(document).ok())
            .filter(|document| document.is_object())
            .collect();
        assert!(
            documents.len() > 5,
            "the live render produced {} documents; that is not the shipped chart",
            documents.len()
        );
        documents
    })
}

// ---------------------------------------------------------------------------
// Rendered task-run Job (the Rust surface Helm never sees)
// ---------------------------------------------------------------------------

/// The dispatch-ready task-run Job, rendered by the same pair dispatch uses.
///
/// `apply_launcher_authority_protocol` is where the `resize-v2` ceiling is
/// written, so calling it here — rather than hand-assembling a sidecar — means
/// the positive ceiling case is the artifact production actually creates.
fn task_run_job(mode: LauncherAuthorityProtocol) -> Job {
    let config = KubernetesConfig::for_testing();
    let mut job = build_task_run_job(
        &config,
        &uuid::Uuid::nil(),
        "cutover-preflight",
        "cutover-preflight-secret",
        "registry.example/cutover:preflight",
        &[],
        None,
        false,
        None,
    );
    apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, mode)
        .expect("the stock render resolves a ceiling for both protocols");
    job
}

/// A task-run Job with no launcher sidecar at all — what `cgroupLauncher.mode:
/// disabled` (the chart's stock value) produces.
fn task_run_job_without_launcher() -> Job {
    let mut config = KubernetesConfig::for_testing();
    config.cgroup_launcher_mode = djinn_k8s::launcher::CgroupLauncherMode::Disabled;
    build_task_run_job(
        &config,
        &uuid::Uuid::nil(),
        "cutover-preflight",
        "cutover-preflight-secret",
        "registry.example/cutover:preflight",
        &[],
        None,
        false,
        None,
    )
}

fn sidecar_mut(job: &mut Job) -> &mut k8s_openapi::api::core::v1::Container {
    job.spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
        .and_then(|spec| spec.init_containers.as_mut())
        .and_then(|list| {
            list.iter_mut()
                .find(|container| container.name == LAUNCHER_CONTAINER_NAME)
        })
        .expect("the armed render carries a cgroup-launcher sidecar")
}

fn set_ceiling(job: &mut Job, cpu: Option<&str>) {
    let limits = sidecar_mut(job)
        .resources
        .as_mut()
        .and_then(|resources| resources.limits.as_mut())
        .expect("the sidecar renders a limits map (memory, at minimum)");
    match cpu {
        Some(value) => {
            limits.insert(
                "cpu".to_string(),
                k8s_openapi::apimachinery::pkg::api::resource::Quantity(value.to_string()),
            );
        }
        None => {
            limits.remove("cpu");
        }
    }
}

// ---------------------------------------------------------------------------
// Input assembly
// ---------------------------------------------------------------------------

/// A clean cutover: the live render, the armed Job for `mode`, an empty
/// catalog, no live births, and a drained fence.
struct Fixture {
    manifests: Vec<Value>,
    job: Job,
    mode: LauncherAuthorityProtocol,
    catalog: Vec<CatalogImage>,
    inventory: LegacyDigestInventory,
    births: Vec<BirthObservation>,
    drain: DrainFenceObservation,
}

impl Fixture {
    fn clean(mode: LauncherAuthorityProtocol) -> Self {
        Self {
            manifests: live_render().to_vec(),
            job: task_run_job(mode),
            mode,
            catalog: Vec::new(),
            inventory: LegacyDigestInventory::Unconfigured,
            births: Vec::new(),
            drain: DrainFenceObservation::observed(Vec::new(), Vec::new()),
        }
    }

    fn verdict(&self) -> Result<Vec<DefectClass>, BTreeSet<DefectClass>> {
        let input = CutoverPreflightInput {
            manifests: &self.manifests,
            task_run_job: &self.job,
            authority_mode: self.mode,
            catalog: &self.catalog,
            legacy_digest_inventory: &self.inventory,
            births: &self.births,
            drain: &self.drain,
        };
        run(&input)
            .map(|report| report.evaluated().to_vec())
            .map_err(|blocked| blocked.classes())
    }

    /// Assert the cutover is refused for exactly `class`, and that the message
    /// mentions `needle`. Both halves matter: an exit status alone cannot tell
    /// a missing RBAC rule apart from a broken fixture.
    #[track_caller]
    fn expect_blocked(&self, class: DefectClass, needle: &str) {
        let input = CutoverPreflightInput {
            manifests: &self.manifests,
            task_run_job: &self.job,
            authority_mode: self.mode,
            catalog: &self.catalog,
            legacy_digest_inventory: &self.inventory,
            births: &self.births,
            drain: &self.drain,
        };
        let blocked = run(&input).expect_err("the preflight must refuse this fixture");
        assert_eq!(
            blocked.classes(),
            BTreeSet::from([class]),
            "expected exactly the {class} class, got: {blocked}"
        );
        assert!(
            blocked.to_string().contains(needle),
            "the refusal must name {needle:?}, got: {blocked}"
        );
    }

    #[track_caller]
    fn expect_clean(&self) {
        match self.verdict() {
            Ok(evaluated) => assert_eq!(
                evaluated,
                DefectClass::ALL.to_vec(),
                "a clean verdict must state that every class was evaluated"
            ),
            Err(classes) => panic!("expected a clean cutover, blocked on {classes:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Render mutations, and the guard that they are single-field
// ---------------------------------------------------------------------------

/// What a negative fixture is allowed to change relative to the live render.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Intent {
    /// One fingerprint entry removed, and nothing added. The `pods/resize` rule
    /// is gone.
    Removed(String),
    /// Exactly one rule replaced by exactly one other, both under this prefix:
    /// RBAC rules are fingerprinted by CONTENT, so editing one element of one
    /// rule removes the old canonical form and adds the new one, and touches
    /// nothing else in the render.
    RuleRewritten(String),
    /// Entries added, all beneath this fingerprint prefix, and nothing else
    /// changed or removed anywhere.
    AddedUnder(String),
}


/// Locate the Role rule that carries the `pods/resize` triple.
fn resize_rule_position(manifests: &[Value]) -> (usize, usize) {
    for (document_index, document) in manifests.iter().enumerate() {
        if document.get("kind").and_then(Value::as_str) != Some("Role") {
            continue;
        }
        let Some(rules) = document.get("rules").and_then(Value::as_array) else {
            continue;
        };
        for (rule_index, rule) in rules.iter().enumerate() {
            if list_contains(rule, "resources", RESIZE_RULE_RESOURCE) {
                return (document_index, rule_index);
            }
        }
    }
    panic!("the live render carries no Role rule naming {RESIZE_RULE_RESOURCE}");
}

fn list_contains(rule: &Value, field: &str, wanted: &str) -> bool {
    rule.get(field)
        .and_then(Value::as_array)
        .is_some_and(|list| list.iter().any(|item| item.as_str() == Some(wanted)))
}

fn with_resize_rule_deleted(manifests: &mut Vec<Value>) {
    let (document, rule) = resize_rule_position(manifests);
    manifests[document]["rules"]
        .as_array_mut()
        .expect("rules array")
        .remove(rule);
}

/// Replace one element of one field of the `pods/resize` rule.
fn with_resize_rule_field(manifests: &mut [Value], field: &str, from: &str, to: &str) {
    let (document, rule) = resize_rule_position(manifests);
    let list = manifests[document]["rules"][rule][field]
        .as_array_mut()
        .expect("rule field is a list");
    let position = list
        .iter()
        .position(|item| item.as_str() == Some(from))
        .unwrap_or_else(|| panic!("the resize rule's {field} does not contain {from:?}"));
    list[position] = Value::String(to.to_string());
}

/// The name of the ServiceAccount task-run Pods run as, read back off the
/// render by its component label rather than hard-coded.
fn taskrun_service_account(manifests: &[Value]) -> String {
    manifests
        .iter()
        .find(|document| {
            document.get("kind").and_then(Value::as_str) == Some("ServiceAccount")
                && document
                    .pointer("/metadata/labels/app.kubernetes.io~1component")
                    .and_then(Value::as_str)
                    == Some("taskrun")
        })
        .and_then(|document| document.pointer("/metadata/name"))
        .and_then(Value::as_str)
        .expect("the live render carries a taskrun-component ServiceAccount")
        .to_string()
}

fn with_taskrun_rolebinding(manifests: &mut Vec<Value>) -> String {
    let account = taskrun_service_account(manifests);
    let name = "djinn-cutover-preflight-taskrun-escalation";
    manifests.push(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": { "name": name, "namespace": "djinn" },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "Role",
            "name": "djinn-cutover-preflight-controller",
        },
        "subjects": [{ "kind": "ServiceAccount", "name": account, "namespace": "djinn" }],
    }));
    format!("RoleBinding/{name}")
}

/// Flatten a render into comparable leaf entries.
///
/// RBAC rules are fingerprinted by their canonical CONTENT rather than by array
/// index, so deleting one rule removes exactly one entry instead of shifting
/// every rule after it — which would make "differs in exactly one path"
/// unassertable for the deletion fixture.
fn fingerprint(manifests: &[Value]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for document in manifests {
        let kind = document.get("kind").and_then(Value::as_str).unwrap_or("?");
        let name = document
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let root = format!("{kind}/{name}");
        for (key, value) in document.as_object().into_iter().flatten() {
            if key == "rules" {
                for rule in value.as_array().into_iter().flatten() {
                    out.insert(
                        format!("{root}/rules[{}]", canonical(rule)),
                        "present".to_string(),
                    );
                }
            } else {
                flatten(value, &format!("{root}/{key}"), &mut out);
            }
        }
    }
    out
}

fn canonical(value: &Value) -> String {
    serde_json::to_string(value).expect("json is serializable")
}

fn flatten(value: &Value, path: &str, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                flatten(child, &format!("{path}/{key}"), out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                flatten(child, &format!("{path}/{index}"), out);
            }
        }
        scalar => {
            out.insert(path.to_string(), canonical(scalar));
        }
    }
}

/// Assert `mutated` differs from the live render in exactly `intent`.
#[track_caller]
fn assert_single_mutation(mutated: &[Value], intent: &Intent) {
    let base = fingerprint(live_render());
    let after = fingerprint(mutated);
    let changed: Vec<&String> = base
        .keys()
        .filter(|key| after.get(*key).is_some_and(|value| value != &base[*key]))
        .collect();
    let removed: Vec<&String> = base.keys().filter(|key| !after.contains_key(*key)).collect();
    let added: Vec<&String> = after.keys().filter(|key| !base.contains_key(*key)).collect();

    match intent {
        Intent::Removed(path) => {
            assert!(changed.is_empty(), "changed paths: {changed:?}");
            assert_eq!(added, Vec::<&String>::new(), "added paths");
            assert_eq!(removed.len(), 1, "removed paths: {removed:?}");
            assert!(
                removed[0].starts_with(path),
                "removed {:?}, wanted something under {path:?}",
                removed[0]
            );
        }
        Intent::RuleRewritten(path) => {
            assert!(changed.is_empty(), "changed paths: {changed:?}");
            assert_eq!(removed.len(), 1, "removed paths: {removed:?}");
            assert_eq!(added.len(), 1, "added paths: {added:?}");
            assert!(
                removed[0].starts_with(path) && added[0].starts_with(path),
                "the rewrite left {path:?}: removed {:?}, added {:?}",
                removed[0],
                added[0]
            );
        }
        Intent::AddedUnder(prefix) => {
            assert!(changed.is_empty(), "changed paths: {changed:?}");
            assert!(removed.is_empty(), "removed paths: {removed:?}");
            assert!(!added.is_empty(), "the fixture added nothing");
            for path in &added {
                assert!(
                    path.starts_with(prefix),
                    "added {path:?} outside the intended prefix {prefix:?}"
                );
            }
        }
    }
}

/// Every render-derived negative fixture, with the single path it is allowed to
/// touch. Built once and reused, so the guard below and the behavioural proofs
/// judge the same bytes.
fn negative_fixtures() -> Vec<(&'static str, Vec<Value>, Intent)> {
    let (document, _) = resize_rule_position(live_render());
    let role = live_render()[document]
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .expect("the resize Role is named")
        .to_string();

    let mut deleted = live_render().to_vec();
    with_resize_rule_deleted(&mut deleted);

    let mut verb = live_render().to_vec();
    with_resize_rule_field(&mut verb, "verbs", RESIZE_RULE_VERB, "get");
    let mut group = live_render().to_vec();
    with_resize_rule_field(&mut group, "apiGroups", RESIZE_RULE_API_GROUP, "apps");
    let mut resource = live_render().to_vec();
    with_resize_rule_field(&mut resource, "resources", RESIZE_RULE_RESOURCE, "pods");

    let mut bound = live_render().to_vec();
    let binding = with_taskrun_rolebinding(&mut bound);

    // A field mutation rewrites the rule's canonical fingerprint wholesale, so
    // the guard sees one rule leave and one arrive under the Role's rules —
    // which is precisely "one rule, and nothing else, is different".
    let rules_prefix = format!("Role/{role}/rules[");
    vec![
        (
            "pods/resize rule deleted",
            deleted,
            Intent::Removed(rules_prefix.clone()),
        ),
        (
            "verb patch -> get",
            verb,
            Intent::RuleRewritten(rules_prefix.clone()),
        ),
        (
            "apiGroup \"\" -> apps",
            group,
            Intent::RuleRewritten(rules_prefix.clone()),
        ),
        (
            "resource pods/resize -> pods",
            resource,
            Intent::RuleRewritten(rules_prefix),
        ),
        (
            "RoleBinding naming the taskrun ServiceAccount",
            bound,
            Intent::AddedUnder(binding),
        ),
    ]
}

// ===========================================================================
// AC1 / AC2 — the fixtures are live, and the mutations are surgical
// ===========================================================================

/// **AC2.** Each negative fixture differs from the LIVE render in exactly the
/// intended path and nowhere else.
///
/// Non-vacuity: point `live_render` at a checked-in file and this test goes red
/// — a static baseline cannot express the mutated-path diff of a render it was
/// not derived from, and the `helm template` assertion inside `live_render`
/// fires first.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn each_negative_fixture_differs_from_the_live_render_in_exactly_one_path() {
    let fixtures = negative_fixtures();
    assert_eq!(fixtures.len(), 5, "the fixture roster shrank");
    for (label, manifests, intent) in &fixtures {
        assert_ne!(
            fingerprint(manifests),
            fingerprint(live_render()),
            "{label}: the fixture is identical to the live render"
        );
        assert_single_mutation(manifests, intent);
    }
}

/// **AC1.** The stock render, with a drained fence and an empty catalog, is a
/// clean cutover under BOTH protocols — and the report states that all six
/// classes were evaluated, so "no defects" cannot be a report from a validator
/// that checked nothing.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn the_live_render_is_a_clean_cutover_under_both_protocols() {
    for mode in LauncherAuthorityProtocol::ALL {
        Fixture::clean(mode).expect_clean();
    }
}

// ===========================================================================
// AC3 — `pods/resize` RBAC, as the exact triple
// ===========================================================================

/// **AC3.** Deleting the rule, and each of the three independent single-element
/// mutations, is refused and the message names `pods/resize`.
///
/// The verb case is why this asserts the triple rather than "a rule mentioning
/// pods": with `get` in place of `patch` the rule still names `pods/resize`, so
/// a lenient matcher stays green while every lift in production returns 403.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn each_pods_resize_rbac_mutation_is_refused_and_names_the_subresource() {
    for (label, manifests, _) in negative_fixtures()
        .into_iter()
        .filter(|(label, _, _)| *label != "RoleBinding naming the taskrun ServiceAccount")
    {
        let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
        fixture.manifests = manifests;
        println!("rbac mutation: {label}");
        fixture.expect_blocked(DefectClass::PodsResizeRbac, RESIZE_RULE_RESOURCE);
    }
}

/// **AC3.** Promoting the grant cluster-wide is refused too. RBAC cannot name
/// the per-run Pods this authorises, so the namespaced scope is the only bound
/// there is.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn a_cluster_wide_pods_resize_grant_is_refused() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    fixture.manifests.push(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRole",
        "metadata": { "name": "djinn-cutover-preflight-resize-everywhere" },
        "rules": [{
            "apiGroups": [RESIZE_RULE_API_GROUP],
            "resources": [RESIZE_RULE_RESOURCE],
            "verbs": [RESIZE_RULE_VERB],
        }],
    }));
    fixture.expect_blocked(DefectClass::PodsResizeRbac, "cluster-wide");
}

// ===========================================================================
// AC4 / AC5 — the protocol-conditional ceiling, compared in millicores
// ===========================================================================

/// **AC4, both directions.** An absent launcher CPU ceiling FAILS under
/// `resize-v2` and PASSES under `leaf-v1`.
///
/// The `leaf-v1` arm is a REQUIRED PASS, not an accident: a launcher CPU limit
/// under `leaf-v1` is an ancestor clamp over every invocation leaf. Task 7deu
/// measured a leaf set to 4 cores burning 0.25 of one, with the leaf's own
/// `nr_throttled` reading 0 because the throttling happened at the parent.
///
/// Non-vacuity: make the ceiling check unconditional and this test goes red on
/// the `leaf-v1` half.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn an_absent_launcher_ceiling_fails_under_resize_v2_and_passes_under_leaf_v1() {
    let mut refused = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    set_ceiling(&mut refused.job, None);
    refused.expect_blocked(DefectClass::LauncherCpuCeiling, "ONLY ceiling");

    let mut required_pass = Fixture::clean(LauncherAuthorityProtocol::LeafV1);
    set_ceiling(&mut required_pass.job, None);
    required_pass.expect_clean();
}

/// **AC4.** The mirror image: a PRESENT ceiling under `leaf-v1` is the 7deu
/// ancestor clamp and is refused, so the conditionality cannot be satisfied by
/// simply never checking `leaf-v1` at all.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn a_present_launcher_ceiling_under_leaf_v1_is_refused_as_an_ancestor_clamp() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::LeafV1);
    set_ceiling(&mut fixture.job, Some("4000m"));
    fixture.expect_blocked(DefectClass::LauncherCpuCeiling, "ANCESTOR CLAMP");
}

/// **AC4.** A ceiling below the sidecar's own 50m request is refused: the
/// apiserver rejects a container whose limit is under its request, so this is a
/// Pod that never admits rather than a build that merely runs slowly.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn a_ceiling_below_the_launcher_cpu_request_is_refused_under_resize_v2() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    set_ceiling(
        &mut fixture.job,
        Some(&format!("{}m", LAUNCHER_CPU_REQUEST_MILLICORES - 1)),
    );
    fixture.expect_blocked(DefectClass::LauncherCpuCeiling, "below the sidecar's own");
}

/// **AC4.** Flipping to `resize-v2` with no launcher sidecar at all — the
/// chart's stock `cgroupLauncher.mode: disabled` — is refused, because the
/// sidecar IS the resize target. Under `leaf-v1` that same render is the
/// pre-cutover status quo and passes.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn resize_v2_without_a_launcher_sidecar_is_refused_but_leaf_v1_is_not() {
    let mut refused = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    refused.job = task_run_job_without_launcher();
    refused.expect_blocked(DefectClass::LauncherCpuCeiling, "no `spec.initContainers");

    let mut allowed = Fixture::clean(LauncherAuthorityProtocol::LeafV1);
    allowed.job = task_run_job_without_launcher();
    allowed.expect_clean();
}

/// **AC5.** The ceiling `4` and the lease `4000` millicores are the same
/// quantity, and the birth target `4000m` confirms against a status reporting
/// `4` — because the apiserver canonicalises `4000m` to `4` and this
/// repository's own stock worker `cpu_limit` is the bare string `"4"`.
///
/// Non-vacuity: replace either millicore parse with string equality and this
/// test goes red on both halves.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn a_ceiling_of_4_equals_an_expectation_of_4000_millicores() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    // The apiserver's canonical form of the 4000m the renderer wrote.
    set_ceiling(&mut fixture.job, Some("4"));
    fixture.births = vec![BirthObservation {
        pod: birth_pod(&[("4", true)], &[]),
        target_cpu: "4000m".to_string(),
    }];
    fixture.expect_clean();
}

/// **AC4, second mutation.** Source-level gate: the ceiling branch is taken on
/// `LauncherAuthorityProtocol::launcher_owns_leaf_quota()`, never on a protocol
/// string.
///
/// This has to be a source assertion because the two spellings are
/// behaviourally identical TODAY — `as_wire() == "leaf-v1"` and
/// `launcher_owns_leaf_quota()` agree on both current variants. They stop
/// agreeing the moment a third protocol lands, and by then the string form has
/// silently classified it as "not leaf-v1, therefore clamp it", which is the
/// 7deu regression arriving through the resize-v2 door.
#[test]
fn the_ceiling_branch_is_gated_on_the_predicate_not_a_protocol_string() {
    let source = std::fs::read_to_string(
        repo_root().join("server/crates/djinn-k8s/src/cutover_preflight.rs"),
    )
    .expect("the validator source is readable");
    let arm = source
        .split_once("fn check_launcher_ceiling")
        .expect("the ceiling arm exists")
        .1
        .split_once("\nfn pod_spec")
        .expect("the ceiling arm ends before pod_spec")
        .0;
    assert!(
        arm.contains("mode.launcher_owns_leaf_quota()"),
        "the ceiling arm must branch on the predicate"
    );
    for spelling in ["as_wire()", "\"leaf-v1\"", "\"resize-v2\"", "to_string()"] {
        assert!(
            !arm.contains(spelling),
            "the ceiling arm compares a protocol STRING ({spelling}); the wire spelling is not \
             the property that decides whether a container limit is a ceiling or a clamp"
        );
    }
}

// ===========================================================================
// AC6 — birth confirmation comes ONLY from status.initContainerStatuses
// ===========================================================================

/// Build a Pod whose init-container statuses are `init` and whose regular
/// container statuses are `regular`; each entry is `(cpu, is_launcher)`.
fn birth_pod(init: &[(&str, bool)], regular: &[(&str, bool)]) -> Pod {
    let entry = |(cpu, is_launcher): &(&str, bool)| {
        json!({
            "name": if *is_launcher { LAUNCHER_CONTAINER_NAME } else { "worker" },
            "ready": true,
            "restartCount": 0,
            "image": "registry.example/cutover:preflight",
            "imageID": "",
            "resources": { "limits": { "cpu": cpu } },
        })
    };
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "djinn-task-run-birth", "namespace": "djinn" },
        "status": {
            "initContainerStatuses": init.iter().map(entry).collect::<Vec<_>>(),
            "containerStatuses": regular.iter().map(entry).collect::<Vec<_>>(),
        },
    }))
    .expect("a Pod fixture deserializes")
}

/// **AC6.** A Pod carrying a MISLEADING matching `status.containerStatuses`
/// entry named `cgroup-launcher`, with no init-container status, is refused.
///
/// Non-vacuity: change the lookup to `status.containerStatuses` and this
/// fixture goes green. A happy-path-only assertion would survive that mutation,
/// which is exactly why this fixture exists.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn a_misleading_container_status_never_confirms_a_birth_downsize() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    fixture.births = vec![BirthObservation {
        pod: birth_pod(&[], &[("4000m", true)]),
        target_cpu: "4000m".to_string(),
    }];
    fixture.expect_blocked(DefectClass::BirthConfirmation, "initContainerStatuses");
}

/// **AC6.** Missing entry, duplicate entry, a stale value, and a value that
/// differs only in canonical form are each covered — the last one PASSES.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn birth_confirmation_covers_missing_duplicate_stale_and_canonical_forms() {
    // Missing: an init-container status array with no launcher entry.
    let mut missing = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    missing.births = vec![BirthObservation {
        pod: birth_pod(&[("4000m", false)], &[]),
        target_cpu: "4000m".to_string(),
    }];
    missing.expect_blocked(DefectClass::BirthConfirmation, "ambiguous");

    // Duplicate: two launcher entries. Resolving this by index would silently
    // confirm from whichever happened to be first.
    let mut duplicate = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    duplicate.births = vec![BirthObservation {
        pod: birth_pod(&[("4000m", true), ("250m", true)], &[]),
        target_cpu: "4000m".to_string(),
    }];
    duplicate.expect_blocked(DefectClass::BirthConfirmation, "found 2");

    // Stale: the kubelet has not actuated the downsize yet.
    let mut stale = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    stale.births = vec![BirthObservation {
        pod: birth_pod(&[("4000m", true)], &[]),
        target_cpu: "250m".to_string(),
    }];
    stale.expect_blocked(DefectClass::BirthConfirmation, "reports 4000m");

    // Canonical form: `250m` observed against a `0.25` target is the SAME
    // quantity and must confirm.
    let mut canonical_form = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    canonical_form.births = vec![BirthObservation {
        pod: birth_pod(&[("250m", true)], &[("4000m", false)]),
        target_cpu: "0.25".to_string(),
    }];
    canonical_form.expect_clean();
}

// ===========================================================================
// AC7 — catalog protocol vs authority mode, as a derived truth table
// ===========================================================================

/// A digest the inventory vouches for, and one it does not. Both canonical:
/// `sha256:` plus 64 lowercase hex, which is the only shape
/// `PreProtocolDigest::parse` accepts.
const INVENTORIED: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const UNINVENTORIED: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// A real signed-inventory value carrying exactly [`INVENTORIED`].
fn inventory() -> LegacyDigestInventory {
    LegacyDigestInventory::verified(
        "zpen-cutover-preflight",
        "2026-07-31T00:00:00Z",
        [PreProtocolDigest::parse(INVENTORIED).expect("a canonical digest")],
    )
}

/// The expected verdict for each cell, as a literal table.
///
/// Deliberately NOT a call back into `catalog_verdict`: a table that asks the
/// implementation what it thinks is not a table. The row comes from an
/// exhaustive match, so adding a `CatalogDeclaration` or a
/// `LauncherAuthorityProtocol` variant fails to COMPILE here rather than
/// silently skipping a cell.
const fn expected_cell(declaration: CatalogDeclaration, mode: LauncherAuthorityProtocol) -> bool {
    let row = match declaration {
        // Dispatchable under leaf-v1 only: the artifact predates the handshake,
        // so whatever launcher it contains writes leaf-v1 cpu.max.
        CatalogDeclaration::NoHandshakeAllowlistedDigest => [true, false],
        // Never: nothing vouches for these bytes.
        CatalogDeclaration::NoHandshakeUnknownDigest => [false, false],
        CatalogDeclaration::DeclaredLeafV1 => [true, false],
        CatalogDeclaration::DeclaredResizeV2 => [false, true],
    };
    match mode {
        LauncherAuthorityProtocol::LeafV1 => row[0],
        LauncherAuthorityProtocol::ResizeV2 => row[1],
    }
}

/// **AC7.** The full cross product of both enums, derived from `ALL` rather
/// than hand-listed, driven through the production `catalog_verdict` — which
/// delegates to `djinn_db::launcher_compatibility::decide_admission`.
///
/// Non-vacuity: drop the mode from the comparison and the
/// (`no-handshake-allowlisted-digest`, `resize-v2`) cell flips to dispatchable,
/// which this table says must be refused.
#[test]
fn the_catalog_truth_table_is_the_full_cross_product_of_both_enums() {
    let inventory = inventory();
    let mut visited = 0usize;
    for declaration in CatalogDeclaration::ALL {
        let image = declaration.sample_image(INVENTORIED, UNINVENTORIED);
        for mode in LauncherAuthorityProtocol::ALL {
            visited += 1;
            let dispatchable = catalog_verdict(&image, mode, &inventory).is_ok();
            assert_eq!(
                dispatchable,
                expected_cell(declaration, mode),
                "cell ({declaration}, {mode}) for {image:?}"
            );
        }
    }
    assert_eq!(
        visited,
        CatalogDeclaration::ALL.len() * LauncherAuthorityProtocol::ALL.len(),
        "the matrix must be the cross product, not a sample of it"
    );
    // The cell the "drop the mode" mutation would flip, stated on its own so a
    // failure names it rather than an index.
    let legacy =
        CatalogDeclaration::NoHandshakeAllowlistedDigest.sample_image(INVENTORIED, UNINVENTORIED);
    assert!(catalog_verdict(&legacy, LauncherAuthorityProtocol::LeafV1, &inventory).is_ok());
    assert!(
        catalog_verdict(&legacy, LauncherAuthorityProtocol::ResizeV2, &inventory).is_err(),
        "a legacy artifact's launcher writes leaf-v1 cpu.max; the signed inventory says nothing \
         about resize-v2"
    );
}

/// **AC7, second mutation.** A mutable tag is never an allowlisted digest.
///
/// `PreProtocolDigest::parse` requires `sha256:` plus 64 lowercase hex, so a
/// tag cannot enter the inventory in the first place and a tag recorded as an
/// image's digest is `MalformedDigest` — not a near-miss, a refusal.
#[test]
fn a_mutable_tag_is_never_an_allowlisted_digest() {
    assert!(
        PreProtocolDigest::parse("v1.2.3").is_err(),
        "a tag must not parse as a pre-protocol digest"
    );
    assert!(
        PreProtocolDigest::parse("registry.example/base:latest").is_err(),
        "an image reference must not parse as a pre-protocol digest"
    );
    // Uppercase hex is not the canonical form either: two spellings of one
    // artifact would make inventory membership ambiguous.
    assert!(PreProtocolDigest::parse(&INVENTORIED.to_uppercase()).is_err());

    let tagged = CatalogImage {
        pull_ref: "registry.example/base:v1.2.3".to_string(),
        declared: None,
        digest: Some("v1.2.3".to_string()),
    };
    let refusal = catalog_verdict(&tagged, LauncherAuthorityProtocol::LeafV1, &inventory())
        .expect_err("a tag must never satisfy the signed-digest inventory");
    assert!(
        refusal.contains("immutable manifest digest"),
        "the refusal must say why: {refusal}"
    );
}

/// **AC7.** Membership is required once an inventory is configured: an
/// uninventoried digest is refused even under the mode it was built for.
#[test]
fn an_uninventoried_legacy_digest_is_refused_even_under_leaf_v1() {
    let unknown =
        CatalogDeclaration::NoHandshakeUnknownDigest.sample_image(INVENTORIED, UNINVENTORIED);
    assert!(
        catalog_verdict(&unknown, LauncherAuthorityProtocol::LeafV1, &inventory()).is_err(),
        "membership in the signed inventory is required once one is configured"
    );
}

/// **AC7.** The class reaches the report: a dispatch-eligible legacy image
/// blocks a `resize-v2` cutover through the production `run`, and the same
/// image under the mode it was built for does not.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn an_allowlisted_legacy_digest_blocks_a_resize_v2_cutover() {
    let image =
        CatalogDeclaration::NoHandshakeAllowlistedDigest.sample_image(INVENTORIED, UNINVENTORIED);

    let mut blocked = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    blocked.inventory = inventory();
    blocked.catalog = vec![image.clone()];
    blocked.expect_blocked(
        DefectClass::CatalogProtocol,
        "may not run under authority mode",
    );

    let mut allowed = Fixture::clean(LauncherAuthorityProtocol::LeafV1);
    allowed.inventory = inventory();
    allowed.catalog = vec![image];
    allowed.expect_clean();
}

// ===========================================================================
// AC8 — the drain fence, against REAL Postgres
// ===========================================================================

/// Create the FK chain a `build_pod_permits` row needs, through the production
/// repositories. No raw SQL leaves `djinn-db`.
async fn seed_task_run(database: &Database, task_run_id: &str) {
    database.ensure_initialized().await.expect("migrations");
    let project = ProjectRepository::new(database.clone(), EventBus::noop())
        .create(&format!("zpen-{task_run_id}"), "djinnos", "zpen")
        .await
        .expect("seed project");
    let task = TaskRepository::new(database.clone(), EventBus::noop())
        .create_fixture_in_project(
            &project.id,
            None,
            "zpen drain fence",
            "drain fence fixture",
            "drain fence fixture",
            "feature",
            1,
            "",
            None,
            None,
        )
        .await
        .expect("seed task");
    TaskRunRepository::new(database.clone())
        .create(CreateTaskRunParams {
            id: task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed task run");
}

/// Walk one permit to `target` through the real lifecycle: acquire, bind a Job
/// UID, capture the resize identity (which lands on `birth_confirmed`), then
/// apply the fenced transitions the state machine allows.
async fn seed_permit_in_state(
    database: &Database,
    task_run_id: &str,
    target: BuildPodPermitState,
) {
    seed_task_run(database, task_run_id).await;
    let repo = BuildPodPermitRepository::new(database.clone());
    let AcquireBuildPodPermitResult::Acquired { row, .. } = repo.acquire(task_run_id, 8).await
    else {
        panic!("the permit pool must admit the fixture");
    };
    let BindBuildPodPermitResult::Bound(row) = repo
        .bind_or_refresh_job_uid(task_run_id, &row.permit_id, row.fencing_token, "job-uid")
        .await
        .expect("bind job uid")
    else {
        panic!("the fixture permit must bind a Job UID");
    };
    let identity = BuildPodResizeIdentity {
        pod_namespace: "djinn".to_string(),
        pod_name: format!("djinn-task-run-{task_run_id}"),
        pod_uid: format!("pod-uid-{task_run_id}"),
        launcher_container_name: LAUNCHER_CONTAINER_NAME.to_string(),
        launcher_container_id: "containerd://zpen".to_string(),
        image_digest: INVENTORIED.to_string(),
        observed_launcher_protocol: LauncherAuthorityProtocol::ResizeV2.as_wire().to_string(),
        effective_launcher_protocol: LauncherAuthorityProtocol::ResizeV2.as_wire().to_string(),
        admitted_cpu_millicores: 4000,
    };
    let CaptureBuildPodResizeIdentityResult::Captured(_) = repo
        .capture_resize_identity(task_run_id, &row.permit_id, row.fencing_token, &identity)
        .await
        .expect("capture resize identity")
    else {
        panic!("the fixture permit must capture its resize identity");
    };

    // `capture_resize_identity` lands on `birth_confirmed`; walk from there.
    let walk: &[BuildPodPermitState] = match target {
        BuildPodPermitState::BirthConfirmed => &[],
        BuildPodPermitState::LiftApplying => &[BuildPodPermitState::LiftApplying],
        BuildPodPermitState::Lifted => &[
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
        ],
        BuildPodPermitState::DropRequired => &[
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
            BuildPodPermitState::DropRequired,
        ],
        BuildPodPermitState::DropApplying => &[
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
            BuildPodPermitState::DropRequired,
            BuildPodPermitState::DropApplying,
        ],
        BuildPodPermitState::Quarantined => &[BuildPodPermitState::Quarantined],
        other => panic!("{other:?} is not a nonterminal resize state"),
    };
    let mut current = BuildPodPermitState::BirthConfirmed;
    for next in walk {
        let outcome = repo
            .transition_resize_lifecycle(
                task_run_id,
                &row.permit_id,
                row.fencing_token,
                &identity.pod_uid,
                current,
                *next,
            )
            .await
            .expect("transition");
        let TransitionBuildPodResizeLifecycleResult::Transitioned(_) = outcome else {
            panic!("{current:?} -> {next:?} must be a legal transition, got {outcome:?}");
        };
        current = *next;
    }
    assert_eq!(current, target, "the walk must land on the requested state");
}

/// Every state migration 164 treats as a nonterminal resize/lease state.
///
/// Seeded INDEPENDENTLY, one case each: narrowing the production predicate by a
/// single state leaves that state's row undetected, and only a per-state case
/// turns red when it does.
const NONTERMINAL_STATES: [BuildPodPermitState; 6] = [
    BuildPodPermitState::BirthConfirmed,
    BuildPodPermitState::LiftApplying,
    BuildPodPermitState::Lifted,
    BuildPodPermitState::DropRequired,
    BuildPodPermitState::DropApplying,
    BuildPodPermitState::Quarantined,
];

/// **AC8.** Seeding exactly one nonterminal row — in each nonterminal state in
/// turn — makes the preflight refuse and name the row count. Zero rows passes.
///
/// Real Postgres, the production `list_nonterminal_resize` query and its
/// `build_pod_permits_resize_nonterminal_idx` partial index. A fake repository
/// could not hold the property under test, which is that the PREDICATE selects
/// these rows.
///
/// Non-vacuity: drop any one state from `NONTERMINAL_RESIZE_STATES` in
/// `djinn-db` and this test goes red on that state's case.
#[tokio::test]
#[ignore = "needs helm + postgres; run by the cutover-preflight quality-gate lane"]
async fn one_nonterminal_resize_row_in_any_state_blocks_the_cutover() {
    for (index, state) in NONTERMINAL_STATES.iter().enumerate() {
        let database = Database::ephemeral().await.expect("real postgres");
        let task_run_id = format!("zpen-run-{index}");
        seed_permit_in_state(&database, &task_run_id, *state).await;
        let permits = BuildPodPermitRepository::new(database.clone());

        let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
        fixture.drain = observe_drain_fence(&permits, Vec::new()).await;
        println!("drain fence state under test: {state:?}");
        fixture.expect_blocked(
            DefectClass::DrainFence,
            "1 nonterminal resize/lease row(s) are still in flight",
        );
    }
}

/// **AC8.** A drained ledger passes. The negative control is a permit that is
/// live for capacity but owes no resize work (`job_created`): it must NOT block
/// the fence, or "drained" would be unreachable in any real deployment.
#[tokio::test]
#[ignore = "needs helm + postgres; run by the cutover-preflight quality-gate lane"]
async fn a_drained_resize_ledger_passes_the_fence() {
    let database = Database::ephemeral().await.expect("real postgres");
    seed_task_run(&database, "zpen-run-drained").await;
    let permits = BuildPodPermitRepository::new(database.clone());
    let AcquireBuildPodPermitResult::Acquired { row, .. } =
        permits.acquire("zpen-run-drained", 8).await
    else {
        panic!("the permit pool must admit the fixture");
    };
    permits
        .bind_or_refresh_job_uid(
            "zpen-run-drained",
            &row.permit_id,
            row.fencing_token,
            "job-uid",
        )
        .await
        .expect("bind job uid");

    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    fixture.drain = observe_drain_fence(&permits, Vec::new()).await;
    fixture.expect_clean();
}

/// **AC8.** A fence that could not be READ is a defect, not an empty fence.
/// This is the asymmetry the whole cutover turns on: "the database was
/// unreachable" and "nothing is in flight" must never resolve the same way.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn an_unobservable_drain_fence_is_a_defect() {
    let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    fixture.drain = DrainFenceObservation::unobservable("connection refused");
    fixture.expect_blocked(DefectClass::DrainFence, "could not be read");

    let mut live = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    live.drain = DrainFenceObservation::observed(Vec::new(), vec!["djinn-task-run-abc".to_string()]);
    live.expect_blocked(DefectClass::DrainFence, "live task-run Pod(s)");
}

// SOURCE-GATE BOUNDARY: everything below necessarily names the banned words.

/// **AC8, source gate.** The drain-fence proofs construct a REAL Postgres
/// database and no `Fake`/`Mock`/`Stub` repository appears anywhere above.
///
/// `Database::ephemeral()` in this repository is the template-cloned real
/// Postgres harness — a per-test `CREATE DATABASE ... TEMPLATE` clone — not an
/// in-memory stand-in despite its sibling's name.
#[test]
fn the_drain_fence_proofs_use_a_real_pg_pool_and_no_fake_repository() {
    let source = std::fs::read_to_string(
        repo_root().join("server/crates/djinn-k8s/tests/cutover_preflight.rs"),
    )
    .expect("this test file is readable");
    // Split off this test and its doc comment, which necessarily name the
    // banned words.
    let body = source
        .split_once("// SOURCE-GATE BOUNDARY")
        .expect("the source-gate boundary marker exists")
        .0;
    for required in [
        "Database::ephemeral()",
        "observe_drain_fence(&permits",
        "BuildPodPermitRepository::new",
    ] {
        assert!(
            body.contains(required),
            "the drain-fence proofs must go through {required}"
        );
    }
    for banned in ["Fake", "Mock", "Stub"] {
        assert!(
            !body.contains(banned),
            "a {banned}* repository cannot hold the property under test: that the production \
             predicate and its partial index actually select nonterminal rows"
        );
    }
}

// ===========================================================================
// AC9 — the credential boundary, across BOTH surfaces
// ===========================================================================

/// **AC9.** Three independent mutations, each refused, each asserted on the
/// RENDERED artifact rather than on a comment or a constant.
///
/// Two of them live in Rust (`job.rs`) and one in Helm, which is the entire
/// reason this check reads both: `automountServiceAccountToken` and the token
/// audience never appear in a chart, and the ServiceAccount's RoleBindings
/// never appear in a Job.
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn each_credential_boundary_mutation_is_refused() {
    for automount in [Some(true), None] {
        let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
        fixture
            .job
            .spec
            .as_mut()
            .and_then(|spec| spec.template.spec.as_mut())
            .expect("pod spec")
            .automount_service_account_token = automount;
        fixture.expect_blocked(DefectClass::CredentialBoundary, "automountServiceAccountToken");
    }

    for audience in [Some("kubernetes.default.svc"), None] {
        let mut fixture = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
        let spec = fixture
            .job
            .spec
            .as_mut()
            .and_then(|spec| spec.template.spec.as_mut())
            .expect("pod spec");
        let mut touched = 0usize;
        for volume in spec.volumes.iter_mut().flatten() {
            for source in volume
                .projected
                .iter_mut()
                .flat_map(|projected| projected.sources.iter_mut().flatten())
            {
                if let Some(token) = source.service_account_token.as_mut() {
                    token.audience = audience.map(str::to_string);
                    touched += 1;
                }
            }
        }
        assert_eq!(touched, 1, "the stock render projects exactly one token");
        fixture.expect_blocked(DefectClass::CredentialBoundary, "audience");
    }

    let mut bound = Fixture::clean(LauncherAuthorityProtocol::ResizeV2);
    with_taskrun_rolebinding(&mut bound.manifests);
    bound.expect_blocked(DefectClass::CredentialBoundary, "must hold no RBAC at all");
}

/// **AC9.** The stock render's audience is exactly `djinn`, asserted on the
/// artifact — so the positive half is not "we did not look".
#[test]
#[ignore = "needs helm; run by the cutover-preflight quality-gate lane"]
fn the_stock_task_run_pod_projects_exactly_the_djinn_audience() {
    let job = task_run_job(LauncherAuthorityProtocol::ResizeV2);
    let spec = job
        .spec
        .as_ref()
        .and_then(|spec| spec.template.spec.as_ref())
        .expect("pod spec");
    assert_eq!(spec.automount_service_account_token, Some(false));
    let audiences: Vec<Option<String>> = spec
        .volumes
        .iter()
        .flatten()
        .flat_map(|volume| volume.projected.iter())
        .flat_map(|projected| projected.sources.iter().flatten())
        .filter_map(|source| source.service_account_token.as_ref())
        .map(|token| token.audience.clone())
        .collect();
    assert_eq!(audiences, vec![Some(TOKEN_AUDIENCE.to_string())]);
}

// ===========================================================================
// AC10 — the lane exists, and these proofs cannot silently skip
// ===========================================================================

/// **AC10.** The dedicated quality-gate job exists, invokes the deploy-time
/// driver's contract suite, runs THIS file's `#[ignore]`d proofs with
/// `--ignored`, and declares how many must execute.
///
/// Always-on by design: it is the guard that makes every `#[ignore]` above
/// safe. A preflight that exists but is not invoked by any lane satisfies
/// nothing, and an ignored proof nobody runs is the same defect wearing a
/// different hat.
#[test]
fn the_cutover_preflight_lane_is_wired_and_cannot_silently_skip() {
    let workflow =
        std::fs::read_to_string(repo_root().join(".github/workflows/quality-gate.yml"))
            .expect("quality-gate.yml is readable");
    let job = workflow
        .split_once("\n  cutover-preflight:\n")
        .expect("the cutover-preflight job must exist in quality-gate.yml")
        .1;
    let job = job.split("\n  launcher-kernel-boundary:").next().unwrap_or(job);

    for required in [
        "deploy/preflight/tests/cutover-preflight.sh",
        "--test cutover_preflight",
        "--run-ignored all",
        "DJINN_CUTOVER_EXPECTED_PROOFS",
        "azure/setup-helm@v4",
        "postgres:16",
        // Without a database URL the driver reports the drain fence
        // UNOBSERVABLE, and the suite's clean case is never a genuine exit 0.
        "DJINN_DATABASE_URL:",
    ] {
        assert!(
            job.contains(required),
            "the cutover-preflight lane must carry {required:?}; without it these proofs are \
             merged, green and never executed"
        );
    }

    // A lane that runs is not a lane that BLOCKS. `deploy/preflight/tests/
    // cutover-preflight.sh`'s exit code is only a contract if the aggregating
    // `quality-gate` job fails on it.
    let aggregator = workflow
        .split_once("\n  quality-gate:\n")
        .expect("the aggregating quality-gate job exists")
        .1;
    assert!(
        aggregator.contains("      - cutover-preflight\n"),
        "the aggregating quality-gate job must `needs: cutover-preflight`"
    );
    assert!(
        aggregator.contains("check cutover-preflight \"$CUTOVER_PREFLIGHT\""),
        "the aggregating quality-gate job must fail closed on the cutover-preflight lane"
    );

    let declared: usize = workflow
        .split_once("DJINN_CUTOVER_EXPECTED_PROOFS: \"")
        .expect("the declared proof count")
        .1
        .split_once('"')
        .expect("the declared proof count is quoted")
        .0
        .parse()
        .expect("the declared proof count is a number");
    let actual = std::fs::read_to_string(
        repo_root().join("server/crates/djinn-k8s/tests/cutover_preflight.rs"),
    )
    .expect("this file is readable")
    .matches("\n#[ignore")
    .count();
    assert_eq!(
        declared, actual,
        "this file declares {actual} ignored proofs but the lane expects {declared}; a lane that \
         runs fewer proofs than exist is a silent skip"
    );
}

/// **AC1.** `run` is the production entry point, and the driver binary calls
/// exactly it — no second rule to drift from.
#[test]
fn the_deploy_driver_calls_the_production_run() {
    let driver = std::fs::read_to_string(
        repo_root().join("server/crates/djinn-k8s/src/bin/cutover-preflight.rs"),
    )
    .expect("the driver source is readable");
    assert!(
        driver.contains("djinn_k8s::cutover_preflight::"),
        "the driver must import the production validator"
    );
    assert!(
        driver.contains("run(&input)"),
        "the driver must call the production `run`"
    );
    assert_eq!(
        DefectClass::ALL.len(),
        6,
        "six independent defect classes, one per epic requirement"
    );
}
