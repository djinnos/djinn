//! Wire-contract and confirmation-source tests for the limits-only
//! `pods/resize` client (`b62u`, proposal `3i92`).
//!
//! Every test here drives the **public** surface of `djinn_k8s::pod_resize`
//! against JSON Pod fixtures through a recording fake apiserver. No cluster is
//! involved and none is needed: the properties under test are the shape of the
//! emitted PATCH body and the choice of field consulted for confirmation.
//!
//! The fixtures are adversarial on purpose. Each one is built so that a
//! specific *wrong* implementation would pass:
//!
//! | fixture | wrong implementation it catches |
//! |---|---|
//! | [`misleading_container_status_pod`] | confirming from `status.containerStatuses` |
//! | [`duplicate_launcher_spec_pod`] / [`launcher_not_first_pod`] | resolving the launcher by index `[0]` |
//! | [`resize_pending_pod`] | ignoring `status.conditions` |
//! | [`spec_ahead_of_status_pod`] | confirming from the mutable `spec` |
//! | [`FakeApi::with_flattering_patch_response`] | confirming from the PATCH response |

use std::sync::Mutex;

use async_trait::async_trait;
use djinn_k8s::pod_resize::{
    CpuLimit, LauncherIdentitySite, NotConfirmed, PodResizeApi, PodResizeClient, PodResizeError,
    build_resize_patch, confirm_launcher_cpu, declared_launcher_cpu_limit, locate_launcher_spec,
    locate_launcher_status,
};
use k8s_openapi::api::core::v1::Pod;
use serde_json::{Value, json};

const LAUNCHER: &str = "cgroup-launcher";
const BIRTH: &str = "250m";
const CEILING: &str = "4000m";

fn birth() -> CpuLimit {
    CpuLimit::parse(BIRTH).expect("birth limit parses")
}

fn ceiling() -> CpuLimit {
    CpuLimit::parse(CEILING).expect("ceiling parses")
}

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

fn container(name: &str, cpu: &str) -> Value {
    json!({
        "name": name,
        "image": "example.invalid/project:latest",
        "resources": { "limits": { "cpu": cpu }, "requests": { "cpu": "100m" } }
    })
}

fn status(name: &str, cpu: Option<&str>) -> Value {
    let mut entry = json!({
        "name": name,
        "image": "example.invalid/project:latest",
        "imageID": "example.invalid/project@sha256:deadbeef",
        "ready": true,
        "restartCount": 0,
    });
    if let Some(cpu) = cpu {
        entry["resources"] = json!({ "limits": { "cpu": cpu }, "requests": { "cpu": "100m" } });
    }
    entry
}

/// Assemble a task-run-shaped Pod: one `worker` regular container, the
/// launcher rendered as a **restartable init container** (native sidecar).
fn pod(
    init_specs: Vec<Value>,
    init_statuses: Vec<Value>,
    container_statuses: Vec<Value>,
    conditions: Vec<&str>,
) -> Pod {
    let value = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": { "name": "taskrun-abc", "namespace": "djinn", "uid": "11111111-2222-3333-4444-555555555555" },
        "spec": {
            "containers": [container("worker", "2")],
            "initContainers": init_specs,
        },
        "status": {
            "phase": "Running",
            "initContainerStatuses": init_statuses,
            "containerStatuses": container_statuses,
            "conditions": conditions
                .into_iter()
                .map(|t| json!({ "type": t, "status": "True" }))
                .collect::<Vec<_>>(),
        }
    });
    serde_json::from_value(value).expect("fixture is a valid Pod")
}

/// Nominal Pod: launcher declared at the ceiling, status agreeing with `cpu`.
fn healthy_pod(status_cpu: &str) -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING)],
        vec![status(LAUNCHER, Some(status_cpu))],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// **AC2 fixture.** `status.containerStatuses` carries an entry named
/// `cgroup-launcher` whose CPU limit *already equals the target*, while the
/// real confirmation source — `status.initContainerStatuses` — is still stale
/// at the ceiling.
///
/// No such regular-container entry exists in production; it is planted here so
/// that an implementation reading the wrong array reports success. If this
/// fixture ever confirms, confirmation is reading `status.containerStatuses`.
fn misleading_container_status_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING)],
        vec![status(LAUNCHER, Some(CEILING))],
        vec![
            status("worker", Some("2")),
            status(LAUNCHER, Some(BIRTH)), // the lie
        ],
        vec![],
    )
}

/// **AC3 fixture.** Two launcher entries in `spec.initContainers`. Index `[0]`
/// is a well-formed launcher, so an implementation that resolves by position
/// instead of by name proceeds happily.
fn duplicate_launcher_spec_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING), container(LAUNCHER, "1")],
        vec![status(LAUNCHER, Some(CEILING))],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// **AC3 fixture.** Two launcher entries in `status.initContainerStatuses`,
/// the first of which already reports the birth target — so an index-`[0]`
/// confirmation would report success against an ambiguous status.
fn duplicate_launcher_status_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING)],
        vec![
            status(LAUNCHER, Some(BIRTH)),
            status(LAUNCHER, Some(CEILING)),
        ],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// **AC3 fixture.** No launcher at all — a Pod rendered without the sidecar.
fn absent_launcher_pod() -> Pod {
    pod(
        vec![container("other-sidecar", "1")],
        vec![status("other-sidecar", Some("1"))],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// **AC3 fixture.** Exactly one launcher, but it is not at index `[0]`, and the
/// entry that *is* at `[0]` already reports the birth target. An index-`[0]`
/// implementation patches and confirms the wrong container.
fn launcher_not_first_pod() -> Pod {
    pod(
        vec![
            container("other-sidecar", BIRTH),
            container(LAUNCHER, CEILING),
        ],
        vec![
            status("other-sidecar", Some(BIRTH)),
            status(LAUNCHER, Some(CEILING)),
        ],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// **AC4 fixture.** The launcher's init-container status *already equals the
/// target* — everything a status-only check looks at agrees — but a
/// `PodResizePending` condition says the kubelet has not actuated the resize.
/// An implementation that ignores `status.conditions` confirms.
fn resize_pending_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING)],
        vec![status(LAUNCHER, Some(BIRTH))],
        vec![status("worker", Some("2"))],
        vec!["Ready", "PodResizePending"],
    )
}

/// The mutable `spec` already shows the target (the apiserver records the
/// request immediately) while the status lags. Confirming from `spec` passes.
fn spec_ahead_of_status_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, BIRTH)],
        vec![status(LAUNCHER, Some(CEILING))],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

/// Launcher init-container status exists but reports no `resources` block at
/// all — the pre-actuation shape.
fn status_without_resources_pod() -> Pod {
    pod(
        vec![container(LAUNCHER, CEILING)],
        vec![status(LAUNCHER, None)],
        vec![status("worker", Some("2"))],
        vec![],
    )
}

// ---------------------------------------------------------------------------
// Recording fake apiserver
// ---------------------------------------------------------------------------

struct FakeApi {
    /// Successive GET responses. The last one is repeated once exhausted.
    gets: Mutex<Vec<Pod>>,
    get_calls: Mutex<usize>,
    /// Pod echoed back by the PATCH call.
    patch_response: Pod,
    /// Every PATCH body actually sent, in order.
    patches: Mutex<Vec<Value>>,
}

impl FakeApi {
    fn new(gets: Vec<Pod>) -> Self {
        let patch_response = gets.first().cloned().expect("at least one GET fixture");
        Self {
            gets: Mutex::new(gets),
            get_calls: Mutex::new(0),
            patch_response,
            patches: Mutex::new(Vec::new()),
        }
    }

    /// Make the PATCH *response* already report success, so an implementation
    /// that treats the PATCH response as confirmation passes.
    fn with_flattering_patch_response(mut self, pod: Pod) -> Self {
        self.patch_response = pod;
        self
    }

    fn patch_bodies(&self) -> Vec<Value> {
        self.patches.lock().expect("patches lock").clone()
    }

    fn patch_count(&self) -> usize {
        self.patches.lock().expect("patches lock").len()
    }
}

#[async_trait]
impl PodResizeApi for FakeApi {
    async fn get_pod(&self, _name: &str) -> Result<Pod, PodResizeError> {
        let gets = self.gets.lock().expect("gets lock");
        let mut calls = self.get_calls.lock().expect("get_calls lock");
        let idx = (*calls).min(gets.len() - 1);
        *calls += 1;
        Ok(gets[idx].clone())
    }

    async fn patch_resize(&self, _name: &str, body: &Value) -> Result<Pod, PodResizeError> {
        self.patches
            .lock()
            .expect("patches lock")
            .push(body.clone());
        Ok(self.patch_response.clone())
    }
}

// ---------------------------------------------------------------------------
// AC1 — the PATCH body's key set is exactly {name, resources.limits.cpu}
// ---------------------------------------------------------------------------

/// Executable statement of AC1's shape rule.
///
/// Returned as a `Result` rather than asserted inline so the tests below can
/// prove it is *live*: the same checker is run against deliberately mutated
/// bodies and must reject each one. A checker that cannot fail proves nothing.
fn assert_limits_only_shape(body: &Value) -> Result<(), String> {
    fn keys(v: &Value, path: &str) -> Result<Vec<String>, String> {
        v.as_object()
            .ok_or_else(|| format!("{path} is not an object"))
            .map(|m| m.keys().cloned().collect())
    }
    fn require(actual: &[String], want: &[&str], path: &str) -> Result<(), String> {
        let mut a: Vec<&str> = actual.iter().map(String::as_str).collect();
        a.sort_unstable();
        let mut w = want.to_vec();
        w.sort_unstable();
        if a == w {
            Ok(())
        } else {
            Err(format!("{path}: key set {a:?} != expected {w:?}"))
        }
    }

    require(&keys(body, "$")?, &["spec"], "$")?;
    let spec = &body["spec"];
    require(&keys(spec, "$.spec")?, &["initContainers"], "$.spec")?;

    let arr = spec["initContainers"]
        .as_array()
        .ok_or("$.spec.initContainers is not an array")?;
    if arr.len() != 1 {
        return Err(format!(
            "$.spec.initContainers has {} entries, expected exactly 1",
            arr.len()
        ));
    }

    let entry = &arr[0];
    require(
        &keys(entry, "$.spec.initContainers[0]")?,
        &["name", "resources"],
        "$.spec.initContainers[0]",
    )?;
    if entry["name"] != json!(LAUNCHER) {
        return Err(format!(
            "$.spec.initContainers[0].name is {}, expected \"{LAUNCHER}\"",
            entry["name"]
        ));
    }

    let resources = &entry["resources"];
    require(
        &keys(resources, "$..resources")?,
        &["limits"],
        "$..resources",
    )?;
    let limits = &resources["limits"];
    require(&keys(limits, "$..limits")?, &["cpu"], "$..limits")?;
    if !limits["cpu"].is_string() {
        return Err("$..limits.cpu is not a string".to_string());
    }
    Ok(())
}

#[test]
fn ac1_patch_body_is_byte_exact() {
    let body = build_resize_patch(birth());
    assert_eq!(
        serde_json::to_string(&body).expect("serializes"),
        r#"{"spec":{"initContainers":[{"name":"cgroup-launcher","resources":{"limits":{"cpu":"250m"}}}]}}"#
    );
}

#[test]
fn ac1_patch_body_key_set_is_exactly_name_and_limits_cpu() {
    assert_limits_only_shape(&build_resize_patch(birth())).expect("emitted body satisfies AC1");
    assert_limits_only_shape(&build_resize_patch(ceiling())).expect("emitted body satisfies AC1");
}

/// AC1 non-vacuity: the shape checker rejects every way of saying more than
/// `limits.cpu`. Each mutation below is applied to the real emitted body.
#[test]
fn ac1_shape_check_rejects_every_extra_field() {
    /// One named way of saying more than `limits.cpu`, applied to the emitted body.
    type BodyMutation = (&'static str, fn(&mut Value));

    let mutations: Vec<BodyMutation> = vec![
        ("requests key added", |b| {
            b["spec"]["initContainers"][0]["resources"]["requests"] = json!({ "cpu": "250m" });
        }),
        ("second limits key added", |b| {
            b["spec"]["initContainers"][0]["resources"]["limits"]["memory"] = json!("1Gi");
        }),
        ("second init-container entry added", |b| {
            let extra = json!({ "name": "other", "resources": { "limits": { "cpu": "1" } } });
            b["spec"]["initContainers"]
                .as_array_mut()
                .expect("array")
                .push(extra);
        }),
        ("spec.containers added", |b| {
            b["spec"]["containers"] = json!([{ "name": LAUNCHER }]);
        }),
        ("extra field on the entry", |b| {
            b["spec"]["initContainers"][0]["image"] = json!("example.invalid/x:1");
        }),
        ("name dropped (positional targeting)", |b| {
            b["spec"]["initContainers"][0]
                .as_object_mut()
                .expect("object")
                .remove("name");
        }),
    ];

    for (label, mutate) in mutations {
        let mut body = build_resize_patch(birth());
        mutate(&mut body);
        let err = assert_limits_only_shape(&body)
            .expect_err(&format!("mutation `{label}` must be rejected"));
        assert!(!err.is_empty(), "rejection for `{label}` must explain why");
    }
}

/// The emitted value survives canonicalisation: whether the caller spells the
/// target `4000m` or `4`, the same byte-stable body is produced.
#[test]
fn ac1_emitted_value_is_canonical_regardless_of_caller_spelling() {
    assert_eq!(
        build_resize_patch(CpuLimit::parse("4000m").unwrap()),
        build_resize_patch(CpuLimit::parse("4").unwrap())
    );
}

// ---------------------------------------------------------------------------
// AC2 — confirmation reads only status.initContainerStatuses
// ---------------------------------------------------------------------------

/// **The criterion that matters.** A Pod whose
/// `status.containerStatuses[cgroup-launcher].resources.limits.cpu` already
/// equals the target while `status.initContainerStatuses` is stale must NOT
/// confirm.
#[test]
fn ac2_matching_regular_container_status_does_not_confirm() {
    let pod = misleading_container_status_pod();

    // Precondition: the fixture really is misleading — the wrong array agrees.
    let planted = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .expect("containerStatuses present")
        .iter()
        .find(|c| c.name == LAUNCHER)
        .expect("planted launcher entry in containerStatuses");
    assert_eq!(
        planted
            .resources
            .as_ref()
            .and_then(|r| r.limits.as_ref())
            .and_then(|l| l.get("cpu"))
            .map(|q| q.0.as_str()),
        Some(BIRTH),
        "fixture must plant a MATCHING regular-container status, else it proves nothing"
    );

    let err = confirm_launcher_cpu(&pod, birth()).expect_err("must not confirm");
    assert_eq!(
        err,
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: 4000,
            target_millis: 250,
        })
    );
}

#[tokio::test]
async fn ac2_full_cycle_refuses_the_misleading_pod() {
    let api = FakeApi::new(vec![misleading_container_status_pod()]);
    let client = PodResizeClient::new(api);
    let err = client
        .resize_launcher_cpu("taskrun-abc", birth())
        .expect_err_async()
        .await;
    assert!(
        matches!(err, PodResizeError::NotConfirmed(_)),
        "got {err:?}"
    );
    assert_eq!(
        client.api().patch_count(),
        1,
        "identity was fine, so the PATCH is expected — only confirmation must fail"
    );
}

/// The mutable `spec` is not confirmation either.
#[test]
fn ac2_spec_agreement_does_not_confirm() {
    let pod = spec_ahead_of_status_pod();
    assert_eq!(
        declared_launcher_cpu_limit(&pod).expect("spec limit readable"),
        birth(),
        "fixture must have the SPEC already at target"
    );
    let err = confirm_launcher_cpu(&pod, birth()).expect_err("spec is not confirmation");
    assert_eq!(
        err,
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: 4000,
            target_millis: 250,
        })
    );
}

/// The PATCH response is not confirmation: here the response body already
/// reports the target, but the fresh GET does not.
#[tokio::test]
async fn ac2_patch_response_is_not_confirmation() {
    let api =
        FakeApi::new(vec![healthy_pod(CEILING)]).with_flattering_patch_response(healthy_pod(BIRTH));
    let client = PodResizeClient::new(api);
    let err = client
        .resize_launcher_cpu("taskrun-abc", birth())
        .expect_err_async()
        .await;
    assert_eq!(
        err,
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: 4000,
            target_millis: 250,
        })
    );
}

/// A launcher status with no `resources` block at all is not a match.
#[test]
fn ac2_absent_status_limit_does_not_confirm() {
    assert_eq!(
        confirm_launcher_cpu(&status_without_resources_pod(), birth())
            .expect_err("absent limit is not a match"),
        PodResizeError::NotConfirmed(NotConfirmed::StatusLimitAbsent)
    );
}

/// Confirmation compares millicores, so apiserver canonicalisation of
/// `4000m` to `4` still confirms.
#[test]
fn ac2_confirmation_survives_quantity_canonicalisation() {
    confirm_launcher_cpu(&healthy_pod("4"), ceiling()).expect("4 == 4000m");
    confirm_launcher_cpu(&healthy_pod("0.25"), birth()).expect("0.25 == 250m");
}

// ---------------------------------------------------------------------------
// AC3 — zero or duplicate launcher entries: typed error, no PATCH
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac3_ambiguous_identity_yields_typed_error_and_no_patch() {
    let cases: Vec<(&str, Pod, LauncherIdentitySite, usize)> = vec![
        (
            "zero in spec",
            absent_launcher_pod(),
            LauncherIdentitySite::SpecInitContainers,
            0,
        ),
        (
            "duplicate in spec",
            duplicate_launcher_spec_pod(),
            LauncherIdentitySite::SpecInitContainers,
            2,
        ),
        (
            "duplicate in status",
            duplicate_launcher_status_pod(),
            LauncherIdentitySite::StatusInitContainerStatuses,
            2,
        ),
        (
            "zero in status",
            pod(
                vec![container(LAUNCHER, CEILING)],
                vec![status("other-sidecar", Some("1"))],
                vec![status("worker", Some("2"))],
                vec![],
            ),
            LauncherIdentitySite::StatusInitContainerStatuses,
            0,
        ),
    ];

    for (label, fixture, site, found) in cases {
        let client = PodResizeClient::new(FakeApi::new(vec![fixture]));
        let err = client
            .resize_launcher_cpu("taskrun-abc", birth())
            .expect_err_async()
            .await;
        assert_eq!(
            err,
            PodResizeError::LauncherIdentityAmbiguous { site, found },
            "case `{label}`"
        );
        assert_eq!(
            client.api().patch_count(),
            0,
            "case `{label}`: an ambiguous Pod must never be PATCHed"
        );
    }
}

/// AC3 non-vacuity: the duplicate fixtures are built so a positional `[0]`
/// lookup would succeed. Asserting that here means the test above cannot pass
/// by accident on a fixture that is ambiguous only in some harmless way.
#[test]
fn ac3_duplicate_fixtures_would_satisfy_a_positional_implementation() {
    let dup_spec = duplicate_launcher_spec_pod();
    let entries = dup_spec
        .spec
        .as_ref()
        .and_then(|s| s.init_containers.as_ref())
        .expect("initContainers");
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].name, LAUNCHER,
        "index [0] must look like a valid launcher to a positional implementation"
    );

    let dup_status = duplicate_launcher_status_pod();
    let statuses = dup_status
        .status
        .as_ref()
        .and_then(|s| s.init_container_statuses.as_ref())
        .expect("initContainerStatuses");
    assert_eq!(statuses.len(), 2);
    assert_eq!(
        statuses[0]
            .resources
            .as_ref()
            .and_then(|r| r.limits.as_ref())
            .and_then(|l| l.get("cpu"))
            .map(|q| q.0.as_str()),
        Some(BIRTH),
        "index [0] must already report the target, so a positional confirm would succeed"
    );
}

/// AC3 non-vacuity, the other direction: a *unique* launcher that simply is not
/// first must still resolve correctly. A positional implementation resolves the
/// neighbouring sidecar — which, in this fixture, already reports the target.
#[test]
fn ac3_launcher_is_located_by_name_not_position() {
    let pod = launcher_not_first_pod();

    let spec = locate_launcher_spec(&pod).expect("unique launcher resolves");
    assert_eq!(spec.name, LAUNCHER);
    assert_eq!(
        declared_launcher_cpu_limit(&pod).expect("declared limit"),
        ceiling(),
        "must read the launcher's ceiling, not the decoy's birth limit"
    );

    let status = locate_launcher_status(&pod).expect("unique launcher status resolves");
    assert_eq!(status.name, LAUNCHER);

    // The decoy at index [0] already reports the target: a positional
    // confirmation would return Ok here.
    assert_eq!(
        confirm_launcher_cpu(&pod, birth()).expect_err("must read the launcher, not index [0]"),
        PodResizeError::NotConfirmed(NotConfirmed::StatusStale {
            observed_millis: 4000,
            target_millis: 250,
        })
    );
}

/// `spec.containers` is never a fallback, even when it holds a launcher-named
/// entry.
#[test]
fn ac3_regular_container_array_is_never_a_fallback() {
    let mut pod = absent_launcher_pod();
    pod.spec
        .as_mut()
        .expect("spec")
        .containers
        .push(serde_json::from_value(container(LAUNCHER, CEILING)).expect("container"));

    assert_eq!(
        locate_launcher_spec(&pod).expect_err("must not fall back to spec.containers"),
        PodResizeError::LauncherIdentityAmbiguous {
            site: LauncherIdentitySite::SpecInitContainers,
            found: 0,
        }
    );
}

// ---------------------------------------------------------------------------
// AC4 — a PodResizePending condition means never confirmed
// ---------------------------------------------------------------------------

#[test]
fn ac4_resize_pending_condition_blocks_confirmation() {
    let pod = resize_pending_pod();

    // Non-vacuity precondition: with conditions ignored, this fixture confirms.
    // That is exactly what the `PodResizePending` check has to override.
    let launcher = locate_launcher_status(&pod).expect("unique launcher status");
    assert_eq!(
        launcher
            .resources
            .as_ref()
            .and_then(|r| r.limits.as_ref())
            .and_then(|l| l.get("cpu"))
            .map(|q| q.0.as_str()),
        Some(BIRTH),
        "fixture's init-container status must already MATCH the target, else the \
         conditions check is not what makes this fail"
    );

    assert_eq!(
        confirm_launcher_cpu(&pod, birth()).expect_err("PodResizePending must block"),
        PodResizeError::NotConfirmed(NotConfirmed::ResizePending)
    );
}

#[tokio::test]
async fn ac4_resize_pending_blocks_the_full_cycle() {
    let client = PodResizeClient::new(FakeApi::new(vec![
        healthy_pod(CEILING),
        resize_pending_pod(),
    ]));
    let err = client
        .resize_launcher_cpu("taskrun-abc", birth())
        .expect_err_async()
        .await;
    assert_eq!(
        err,
        PodResizeError::NotConfirmed(NotConfirmed::ResizePending)
    );
}

/// Removing the condition from the very same fixture confirms — proving the
/// condition, and nothing else about the fixture, is what blocks.
#[test]
fn ac4_same_fixture_without_the_condition_confirms() {
    let mut pod = resize_pending_pod();
    pod.status
        .as_mut()
        .expect("status")
        .conditions
        .as_mut()
        .expect("conditions")
        .retain(|c| c.type_ != "PodResizePending");

    confirm_launcher_cpu(&pod, birth())
        .expect("with the condition gone, the identical fixture confirms");
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn confirmed_resize_emits_exactly_one_limits_only_patch() {
    let client = PodResizeClient::new(FakeApi::new(vec![healthy_pod(CEILING), healthy_pod(BIRTH)]));
    client
        .resize_launcher_cpu("taskrun-abc", birth())
        .await
        .expect("status-confirmed resize");

    let bodies = client.api().patch_bodies();
    assert_eq!(bodies.len(), 1, "exactly one PATCH per resize");
    assert_limits_only_shape(&bodies[0]).expect("the wire body satisfies AC1");
    assert_eq!(bodies[0], build_resize_patch(birth()));
}

// ---------------------------------------------------------------------------
// Small helper: `expect_err` for futures, so the tests above read linearly.
// ---------------------------------------------------------------------------

trait ExpectErrAsync<T> {
    async fn expect_err_async(self) -> PodResizeError;
}

impl<F, T> ExpectErrAsync<T> for F
where
    F: std::future::Future<Output = Result<T, PodResizeError>>,
    T: std::fmt::Debug,
{
    async fn expect_err_async(self) -> PodResizeError {
        match self.await {
            Ok(v) => panic!("expected an error, got Ok({v:?})"),
            Err(e) => e,
        }
    }
}
