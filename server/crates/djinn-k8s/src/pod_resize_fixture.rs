//! A stored task-run Pod, drivable with no cluster.
//!
//! # Why this lives in `djinn-k8s` and not in the test that uses it
//!
//! `deny.toml` bans a direct `k8s-openapi` dependency outside this crate and
//! `djinn-image-controller`, and that ban is not bureaucracy — it is the
//! capability boundary that keeps Kubernetes types from leaking into crates that
//! have no business constructing them. A test in `djinn-server` that needs a
//! `Pod` to drive the resize bootstrap against therefore does not get to build
//! one; it asks this module for one.
//!
//! # What is real in here
//!
//! Only the transport is fake. The observation goes through
//! [`crate::runtime::observe_launcher_sidecar`], the resize goes through
//! [`crate::pod_resize::PodResizeClient`], and confirmation goes through
//! [`crate::pod_resize::confirm_launcher_cpu`] — so the "unique in BOTH
//! `spec.initContainers` and `status.initContainerStatuses`" rule, the
//! never-fall-back-to-`containerStatuses` rule, and the millicore comparison are
//! all the production ones. A caller cannot accidentally test a re-implementation
//! of them, because there isn't one.
//!
//! The PATCH is applied with **strategic-merge semantics** for `initContainers`,
//! whose `patchMergeKey` is `name`: merge into the entry with the matching name,
//! leave every other entry and every other field alone. That is what makes the
//! "requests and QoS are untouched" and "the other init containers survive"
//! assertions mean anything — an RFC 7386 merge would replace the whole array.

use std::sync::{Arc, Mutex};

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use serde_json::{Value, json};

use crate::pod_resize::{CpuLimit, PodResizeApi, PodResizeClient, PodResizeError};
use crate::runtime::{LauncherObservationError, ObservedLauncherSidecar, observe_launcher_sidecar};

/// Whether the fixture kubelet actuates an accepted resize into
/// `status.initContainerStatuses`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actuation {
    /// The kubelet applies the accepted limit to the init-container status, as a
    /// healthy node does.
    Actuates,
    /// The apiserver accepts the PATCH and the status never moves — the shape a
    /// node under pressure produces, and the one that must never be read as
    /// confirmation.
    NeverActuates,
}

/// An apiserver verdict (or transport failure) the fixture returns instead of
/// performing the call.
///
/// It exists so `403`, `422` and "no response at all" can be driven as the
/// distinct things they are. The lift classifies on the numeric status carried
/// by [`PodResizeError::Api`], so a fault injected here reaches the state
/// machine through the same field a real `kube::Error::Api` would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiFault {
    /// The apiserver's HTTP status, or `None` for a transport failure/timeout.
    pub status: Option<u16>,
    /// The rendered message.
    pub message: String,
}

impl ApiFault {
    /// `403 Forbidden` — the shape a missing `pods/resize` RBAC rule produces.
    #[must_use]
    pub fn forbidden() -> Self {
        Self {
            status: Some(403),
            message: "pods \"taskrun\" is forbidden: User cannot patch resource \
                      \"pods/resize\" in API group \"\""
                .to_owned(),
        }
    }

    /// `422 Unprocessable Entity` — the apiserver refusing this resize.
    #[must_use]
    pub fn unprocessable() -> Self {
        Self {
            status: Some(422),
            message: "Pod \"taskrun\" is invalid: spec.initContainers[0].resources.limits: \
                      Forbidden: resource limits cannot be removed"
                .to_owned(),
        }
    }

    /// A transport timeout: no HTTP response, so no status.
    #[must_use]
    pub fn timeout() -> Self {
        Self {
            status: None,
            message: "error trying to connect: operation timed out".to_owned(),
        }
    }

    fn into_error(self, op: &'static str) -> PodResizeError {
        PodResizeError::Api {
            op,
            message: self.message,
            status: self.status,
        }
    }
}

struct ClusterState {
    pod: Option<Pod>,
    actuation: Actuation,
    resize_patches: usize,
    deletes: Vec<(String, String)>,
    /// Fault returned by every `get_pod`, when set.
    get_fault: Option<ApiFault>,
    /// Fault returned by every `patch_resize`, when set. The patch counter is
    /// **not** incremented for a faulted call: the fixture models a request the
    /// apiserver rejected, and counting it as an applied PATCH would make the
    /// "zero PATCH bodies above the ceiling" counters lie.
    patch_fault: Option<ApiFault>,
    /// The raw `cpu` quantity carried by every accepted PATCH body, in order.
    patched_cpu: Vec<String>,
}

/// One stored task-run Pod, plus a record of everything done to it.
#[derive(Clone)]
pub struct StoredTaskRunPod {
    state: Arc<Mutex<ClusterState>>,
}

impl StoredTaskRunPod {
    fn new(pod: Pod) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClusterState {
                pod: Some(pod),
                actuation: Actuation::Actuates,
                resize_patches: 0,
                deletes: Vec::new(),
                get_fault: None,
                patch_fault: None,
                patched_cpu: Vec::new(),
            })),
        }
    }

    /// A healthy admitted `resize-v2` Pod: the launcher is uniquely nameable in
    /// both arrays, has started (so it carries a container ID), and declares
    /// `ceiling` in both spec and status. A second init container stands beside
    /// it so a whole-array patch is detectable.
    #[must_use]
    pub fn resize_v2(pod_uid: &str, ceiling: &str) -> Self {
        Self::new(build_pod(
            pod_uid,
            json!([
                init_container("preflight", None, None),
                init_container("cgroup-launcher", Some(ceiling), Some("resize-v2")),
            ]),
            json!([
                init_status("preflight", None, Some("containerd://preflight")),
                init_status(
                    "cgroup-launcher",
                    Some(ceiling),
                    Some("containerd://launcher")
                ),
            ]),
        ))
    }

    /// Admitted and nameable in both arrays, but the kubelet has not started the
    /// launcher: no `containerID`, so there is nothing to fence a write-once
    /// identity with.
    #[must_use]
    pub fn resize_v2_launcher_not_started(pod_uid: &str, ceiling: &str) -> Self {
        Self::new(build_pod(
            pod_uid,
            json!([init_container(
                "cgroup-launcher",
                Some(ceiling),
                Some("resize-v2")
            )]),
            json!([init_status("cgroup-launcher", Some(ceiling), None)]),
        ))
    }

    /// A `leaf-v1` Pod. It carries **no** launcher CPU limit, because
    /// `resolve_launcher_cpu_ceiling` returns `Ok(None)` for `leaf-v1` — a limit
    /// there is an ancestor clamp over every invocation leaf.
    #[must_use]
    pub fn leaf_v1(pod_uid: &str) -> Self {
        Self::new(build_pod(
            pod_uid,
            json!([init_container("cgroup-launcher", None, Some("leaf-v1"))]),
            json!([init_status(
                "cgroup-launcher",
                None,
                Some("containerd://launcher")
            )]),
        ))
    }

    /// Model a node that accepts the resize request and never actuates it.
    #[must_use]
    pub fn never_actuating(self) -> Self {
        self.state.lock().expect("cluster").actuation = Actuation::NeverActuates;
        self
    }

    /// Stop actuating accepted resizes from here on.
    ///
    /// The `&self` twin of [`Self::never_actuating`], for the fault-injection
    /// order the real lifecycle forces: a Pod must be admitted and
    /// birth-downsized on a *healthy* node before the fault the lift then meets
    /// can be installed.
    pub fn stop_actuating(&self) {
        self.state.lock().expect("cluster").actuation = Actuation::NeverActuates;
    }

    /// Forget every PATCH counted so far.
    ///
    /// Used to separate the birth downsize — which is `0ppk-1b`'s write, not the
    /// lift's — from the lift's own PATCHes, so a "zero PATCHes" assertion means
    /// "the lift issued none" rather than "nothing has ever happened".
    pub fn reset_patch_counter(&self) {
        let mut state = self.state.lock().expect("cluster");
        state.resize_patches = 0;
        state.patched_cpu.clear();
    }

    /// Add a `PodResizePending` condition. Its presence alone means no observed
    /// limit is authoritative — the fixture does not set a `status` field on it,
    /// because [`crate::pod_resize::has_resize_pending_condition`] deliberately
    /// does not consult one.
    pub fn add_resize_pending(&self) {
        self.mutate(|pod| {
            if let Some(status) = pod.status.as_mut() {
                status.conditions.get_or_insert_with(Default::default).push(
                    serde_json::from_value(json!({
                        "type": crate::pod_resize::POD_RESIZE_PENDING_CONDITION,
                        "status": "True",
                    }))
                    .expect("condition fixture deserializes"),
                );
            }
        });
    }

    /// Plant a **regular**-container status named `cgroup-launcher` carrying
    /// `cpu`.
    ///
    /// This is the false-confirmation trap in its sharpest form: the launcher is
    /// a native sidecar, so `status.containerStatuses` can never legitimately
    /// hold an entry by that name — but a confirmation that read the wrong array
    /// would find a *matching* limit there and report success while the
    /// launcher's own init-container status was still stale.
    pub fn add_misleading_regular_launcher_status(&self, cpu: &str) {
        let entry = json!({
            "name": crate::launcher::LAUNCHER_CONTAINER_NAME,
            "ready": true,
            "restartCount": 0,
            "image": "registry.example/launcher@sha256:feed",
            "imageID": "registry.example/launcher@sha256:feed",
            "containerID": "containerd://impostor",
            "resources": { "limits": { "cpu": cpu } },
        });
        self.mutate(|pod| {
            if let Some(status) = pod.status.as_mut() {
                status
                    .container_statuses
                    .get_or_insert_with(Default::default)
                    .push(serde_json::from_value(entry.clone()).expect("status fixture"));
            }
        });
    }

    /// Strip `resources` from the launcher's init-container status, leaving the
    /// spec untouched: the launcher is nameable in both arrays but reports no
    /// CPU limit at all.
    pub fn clear_launcher_status_limit(&self) {
        self.mutate(|pod| {
            for status in launcher_statuses(pod) {
                status.resources = None;
            }
        });
    }

    /// Replace the stored Pod's `metadata.uid`, keeping its NAME.
    ///
    /// This is a Pod deleted and recreated under the same name — the exact
    /// object `PodResizeClient::resize_launcher_cpu`, whose only argument is a
    /// name, cannot tell apart from the original.
    pub fn recreate_under_same_name(&self, new_uid: &str) {
        self.mutate(|pod| pod.metadata.uid = Some(new_uid.to_owned()));
    }

    /// Restart the launcher sidecar: a new `containerID` and a bumped
    /// `restartCount`.
    pub fn restart_launcher(&self, new_container_id: &str) {
        self.mutate(|pod| {
            for status in launcher_statuses(pod) {
                status.container_id = Some(new_container_id.to_owned());
                status.restart_count += 1;
            }
        });
    }

    /// Rewrite the launcher's declared `DJINN_LAUNCHER_AUTHORITY_PROTOCOL`.
    pub fn set_observed_protocol(&self, protocol: &str) {
        self.mutate(|pod| {
            for container in pod
                .spec
                .as_mut()
                .and_then(|spec| spec.init_containers.as_mut())
                .into_iter()
                .flatten()
                .filter(|c| c.name == crate::launcher::LAUNCHER_CONTAINER_NAME)
            {
                for entry in container
                    .env
                    .get_or_insert_with(Default::default)
                    .iter_mut()
                    .filter(|e| e.name == crate::launcher::AUTHORITY_PROTOCOL_ENV)
                {
                    entry.value = Some(protocol.to_owned());
                }
            }
        });
    }

    /// Every `get_pod` fails with `fault` until cleared.
    pub fn fail_gets(&self, fault: ApiFault) {
        self.state.lock().expect("cluster").get_fault = Some(fault);
    }

    /// Every `patch_resize` fails with `fault`, without mutating the Pod and
    /// without incrementing the PATCH counter.
    pub fn fail_patches(&self, fault: ApiFault) {
        self.state.lock().expect("cluster").patch_fault = Some(fault);
    }

    /// The launcher's live `containerID`, for restart assertions.
    #[must_use]
    pub fn launcher_container_id(&self) -> Option<String> {
        let pod = self.pod()?;
        crate::pod_resize::locate_launcher_status(&pod)
            .ok()?
            .container_id
            .clone()
    }

    /// The launcher's `restartCount`. A limits-only resize must never move it.
    #[must_use]
    pub fn launcher_restart_count(&self) -> Option<i32> {
        let pod = self.pod()?;
        Some(
            crate::pod_resize::locate_launcher_status(&pod)
                .ok()?
                .restart_count,
        )
    }

    fn mutate(&self, apply: impl FnOnce(&mut Pod)) {
        let mut state = self.state.lock().expect("cluster");
        if let Some(pod) = state.pod.as_mut() {
            apply(pod);
        }
    }

    fn pod(&self) -> Option<Pod> {
        self.state.lock().expect("cluster").pod.clone()
    }

    /// Fresh observation, through the production flattening function.
    ///
    /// # Errors
    ///
    /// Whatever [`observe_launcher_sidecar`] raises for the stored Pod.
    pub fn observe_launcher(
        &self,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        // A GET fault applies to the observation too. The whole point of the
        // fault is that the apiserver did not answer; a fixture that served the
        // stored Pod anyway would let a caller "observe" a cluster it cannot
        // reach.
        if let Some(fault) = self.state.lock().expect("cluster").get_fault.clone() {
            return Err(LauncherObservationError::Api(fault.message));
        }
        match self.pod() {
            None => Ok(None),
            Some(pod) => observe_launcher_sidecar(&pod).map(Some),
        }
    }

    /// One limits-only resize, through the production client and the production
    /// confirmation rule.
    ///
    /// # Errors
    ///
    /// Whatever [`PodResizeClient::resize_launcher_cpu`] raises.
    pub async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        PodResizeClient::new(FixtureApi(self.clone()))
            .resize_launcher_cpu(pod_name, CpuLimit::from_millis(target_millicores))
            .await
    }

    /// UID-fenced destruction of exactly the stored Pod.
    ///
    /// # Errors
    ///
    /// A rendered precondition failure when `pod_uid` is not the stored Pod's.
    /// An unfenced delete would destroy a replacement Pod some other actor
    /// legitimately owns, so the fixture refuses one exactly as the real path
    /// does.
    pub fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        let mut state = self.state.lock().expect("cluster");
        let stored = state
            .pod
            .as_ref()
            .and_then(|pod| pod.metadata.uid.clone())
            .unwrap_or_default();
        if stored != pod_uid {
            return Err(format!(
                "uid precondition failed: stored `{stored}`, fenced `{pod_uid}`"
            ));
        }
        state
            .deletes
            .push((task_run_id.to_owned(), pod_uid.to_owned()));
        state.pod = None;
        Ok(())
    }

    /// How many `pods/resize` PATCHes have been issued.
    #[must_use]
    pub fn resize_patches(&self) -> usize {
        self.state.lock().expect("cluster").resize_patches
    }

    /// The CPU value carried by every accepted PATCH **body**, in millicores.
    ///
    /// This is what makes "zero PATCH bodies above the ceiling" a measurement of
    /// what was *sent to the apiserver* rather than of a number the caller
    /// restated. The values are parsed back through
    /// [`crate::pod_resize::CpuLimit`], so the canonicalisation the apiserver
    /// applies (`2000m` → `2`) cannot make an above-ceiling body read as
    /// below-ceiling.
    #[must_use]
    pub fn patched_cpu_millicores(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("cluster")
            .patched_cpu
            .iter()
            .map(|raw| {
                CpuLimit::parse(raw)
                    .expect("every emitted patch body carries a parseable quantity")
                    .millis()
            })
            .collect()
    }

    /// Every UID-fenced delete, as `(task_run_id, pod_uid)`.
    #[must_use]
    pub fn deletes(&self) -> Vec<(String, String)> {
        self.state.lock().expect("cluster").deletes.clone()
    }

    /// The CPU limit the launcher's **init-container status** reports.
    ///
    /// This is the only field that can confirm a resize, which is why it is the
    /// only one exposed for a birth assertion. A caller that wants to know what
    /// was *stored* rather than what the launcher *holds* is asking the wrong
    /// question — that is the "stored but never adopted" failure.
    #[must_use]
    pub fn launcher_status_cpu(&self) -> Option<String> {
        let pod = self.pod()?;
        crate::pod_resize::locate_launcher_status(&pod)
            .ok()?
            .resources
            .as_ref()?
            .limits
            .as_ref()?
            .get("cpu")
            .map(|quantity| quantity.0.clone())
    }

    /// The launcher's declared CPU **request**. A limits-only resize must leave
    /// it byte-identical.
    #[must_use]
    pub fn launcher_spec_cpu_request(&self) -> Option<String> {
        let pod = self.pod()?;
        crate::pod_resize::locate_launcher_spec(&pod)
            .ok()?
            .resources
            .as_ref()?
            .requests
            .as_ref()?
            .get("cpu")
            .map(|quantity| quantity.0.clone())
    }

    /// How many entries `spec.initContainers` holds. An RFC 7386 merge patch
    /// drops every entry the body did not name; a strategic one does not.
    #[must_use]
    pub fn init_container_count(&self) -> usize {
        self.pod()
            .and_then(|pod| pod.spec)
            .and_then(|spec| spec.init_containers)
            .map_or(0, |list| list.len())
    }

    /// `status.qosClass`. A limits-only resize must leave it byte-identical.
    #[must_use]
    pub fn qos_class(&self) -> Option<String> {
        self.pod()
            .and_then(|pod| pod.status)
            .and_then(|status| status.qos_class)
    }
}

struct FixtureApi(StoredTaskRunPod);

/// Every launcher entry in `status.initContainerStatuses`, for mutation.
fn launcher_statuses(
    pod: &mut Pod,
) -> impl Iterator<Item = &mut k8s_openapi::api::core::v1::ContainerStatus> {
    pod.status
        .as_mut()
        .and_then(|status| status.init_container_statuses.as_mut())
        .into_iter()
        .flatten()
        .filter(|status| status.name == crate::launcher::LAUNCHER_CONTAINER_NAME)
}

#[async_trait::async_trait]
impl PodResizeApi for FixtureApi {
    async fn get_pod(&self, name: &str) -> Result<Pod, PodResizeError> {
        if let Some(fault) = self.0.state.lock().expect("cluster").get_fault.clone() {
            return Err(fault.into_error("get"));
        }
        self.0.pod().ok_or_else(|| PodResizeError::Api {
            op: "get",
            message: format!("pod {name} not found"),
            status: Some(404),
        })
    }

    async fn patch_resize(&self, name: &str, body: &Value) -> Result<Pod, PodResizeError> {
        let mut state = self.0.state.lock().expect("cluster");
        if let Some(fault) = state.patch_fault.clone() {
            return Err(fault.into_error("patch"));
        }
        state.resize_patches += 1;
        let actuation = state.actuation;
        let target = body["spec"]["initContainers"][0]["name"]
            .as_str()
            .expect("the patch names its target")
            .to_owned();
        let cpu = body["spec"]["initContainers"][0]["resources"]["limits"]["cpu"]
            .as_str()
            .expect("the patch carries a cpu limit")
            .to_owned();
        state.patched_cpu.push(cpu.clone());
        let Some(pod) = state.pod.as_mut() else {
            return Err(PodResizeError::Api {
                op: "patch",
                message: format!("pod {name} not found"),
                status: Some(404),
            });
        };
        let quantity = Quantity(cpu);
        for container in pod
            .spec
            .as_mut()
            .and_then(|spec| spec.init_containers.as_mut())
            .into_iter()
            .flatten()
            .filter(|container| container.name == target)
        {
            container
                .resources
                .get_or_insert_with(Default::default)
                .limits
                .get_or_insert_with(Default::default)
                .insert("cpu".to_owned(), quantity.clone());
        }
        if actuation == Actuation::Actuates {
            for status in pod
                .status
                .as_mut()
                .and_then(|status| status.init_container_statuses.as_mut())
                .into_iter()
                .flatten()
                .filter(|status| status.name == target)
            {
                status
                    .resources
                    .get_or_insert_with(Default::default)
                    .limits
                    .get_or_insert_with(Default::default)
                    .insert("cpu".to_owned(), quantity.clone());
            }
        }
        Ok(pod.clone())
    }
}

fn init_container(name: &str, cpu_limit: Option<&str>, protocol: Option<&str>) -> Value {
    let mut resources = json!({ "requests": { "cpu": "100m", "memory": "128Mi" } });
    if let Some(cpu) = cpu_limit {
        resources["limits"] = json!({ "cpu": cpu });
    }
    let env = protocol.map_or_else(
        || json!([]),
        |protocol| {
            json!([{
                "name": crate::launcher::AUTHORITY_PROTOCOL_ENV,
                "value": protocol,
            }])
        },
    );
    json!({
        "name": name,
        "image": "registry.example/launcher@sha256:feed",
        "restartPolicy": "Always",
        "env": env,
        "resources": resources,
    })
}

fn init_status(name: &str, cpu_limit: Option<&str>, container_id: Option<&str>) -> Value {
    let mut status = json!({
        "name": name,
        "ready": true,
        "restartCount": 0,
        "image": "registry.example/launcher@sha256:feed",
        "imageID": "registry.example/launcher@sha256:feed",
    });
    if let Some(id) = container_id {
        status["containerID"] = json!(id);
    }
    if let Some(cpu) = cpu_limit {
        status["resources"] = json!({
            "requests": { "cpu": "100m", "memory": "128Mi" },
            "limits": { "cpu": cpu },
        });
    }
    status
}

/// A stored Pod whose *worker* container also declares `cpu: 4`.
///
/// That entry is the trap: a confirmation that read `status.containerStatuses`
/// would find a coincidentally matching limit there and report success while the
/// launcher was never resized at all.
fn build_pod(pod_uid: &str, init_containers: Value, init_container_statuses: Value) -> Pod {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": format!("taskrun-{pod_uid}"),
            "namespace": "djinn",
            "uid": pod_uid,
        },
        "spec": {
            "initContainers": init_containers,
            "containers": [{
                "name": "worker",
                "image": "registry.example/worker@sha256:beef",
                "resources": {
                    "requests": { "cpu": "1", "memory": "2Gi" },
                    "limits": { "cpu": "4", "memory": "4Gi" },
                },
            }],
        },
        "status": {
            "phase": "Running",
            "qosClass": "Burstable",
            "conditions": [],
            "initContainerStatuses": init_container_statuses,
            "containerStatuses": [{
                "name": "worker",
                "ready": true,
                "restartCount": 0,
                "image": "registry.example/worker@sha256:beef",
                "imageID": "registry.example/worker@sha256:beef",
                "containerID": "containerd://worker",
            }],
        },
    }))
    .expect("pod fixture deserializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_healthy_fixture_confirms_and_leaves_the_rest_of_the_pod_alone() {
        let cluster = StoredTaskRunPod::resize_v2("pod-uid", "4");
        assert_eq!(cluster.launcher_status_cpu().as_deref(), Some("4"));
        cluster
            .resize_launcher_cpu("taskrun-pod-uid", 250)
            .await
            .expect("a healthy fixture confirms");
        assert_eq!(cluster.launcher_status_cpu().as_deref(), Some("250m"));
        assert_eq!(cluster.init_container_count(), 2, "strategic, not merge");
        assert_eq!(cluster.launcher_spec_cpu_request().as_deref(), Some("100m"));
        assert_eq!(cluster.qos_class().as_deref(), Some("Burstable"));
    }

    #[tokio::test]
    async fn an_unactuated_resize_is_not_confirmed_even_though_the_worker_status_matches() {
        // `spec.containers[worker]` declares `cpu: 4` and its status is present.
        // Nothing here may be read as confirmation of the LAUNCHER's limit.
        let cluster = StoredTaskRunPod::resize_v2("pod-uid", "4").never_actuating();
        let error = cluster
            .resize_launcher_cpu("taskrun-pod-uid", 250)
            .await
            .expect_err("an unactuated resize must not confirm");
        assert!(matches!(error, PodResizeError::NotConfirmed(_)), "{error}");
        assert_eq!(cluster.resize_patches(), 1, "the PATCH was still issued");
    }

    #[test]
    fn an_unfenced_delete_is_refused() {
        let cluster = StoredTaskRunPod::resize_v2("pod-uid", "4");
        assert!(cluster.uid_fenced_delete("run", "some-other-uid").is_err());
        assert_eq!(cluster.deletes(), Vec::new());
        assert!(cluster.uid_fenced_delete("run", "pod-uid").is_ok());
        assert_eq!(
            cluster.deletes(),
            vec![("run".to_owned(), "pod-uid".to_owned())]
        );
    }
}
