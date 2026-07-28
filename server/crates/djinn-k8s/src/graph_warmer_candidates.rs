//! Immutable-UID inventory and control primitives for lease-gated warm Jobs.
//!
//! This module deliberately contains Kubernetes mechanics only.  The durable
//! lease state machine decides *when* to inventory, bind, or reclaim; these
//! types retain every observed candidate and make it impossible to turn a
//! reusable object name into either authorization or termination evidence.

use std::collections::BTreeMap;

use async_trait::async_trait;
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{ConfigMap, Pod},
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, ListParams, PostParams, Preconditions};

use crate::graph_warmer_identity::LeasedWarmJobIdentity;
use crate::warm_job::{
    ANNOTATION_FENCING_TOKEN, ANNOTATION_GRAPH_REVISION, ANNOTATION_WARM_REQUEST_ID,
    GATE_AUTHORIZATION_KEY, LABEL_WARM, warm_gate_config_map_name,
};

/// Kubernetes kind of a warm candidate.  Jobs and Pods are deliberately kept
/// separate: the selected, gate-authorized object is a Pod, while every Job
/// and Pod still has to be retained for later reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WarmCandidateKind {
    Job,
    Pod,
}

fn list_observation(
    result: &Result<Vec<WarmCandidateObject>, String>,
) -> WarmCandidateListObservation {
    match result {
        Ok(_) => WarmCandidateListObservation::Observed,
        Err(error) => WarmCandidateListObservation::ApiError(error.clone()),
    }
}

fn inventory_observation(
    jobs: &WarmCandidateListObservation,
    pods: &WarmCandidateListObservation,
) -> WarmInventoryObservation {
    match (jobs, pods) {
        (WarmCandidateListObservation::ApiError(error), _) => {
            WarmInventoryObservation::ApiError(error.clone())
        }
        (_, WarmCandidateListObservation::ApiError(error)) => {
            WarmInventoryObservation::ApiError(error.clone())
        }
        _ => WarmInventoryObservation::Observed,
    }
}

/// Whether Kubernetes has declared one observed object finished.
///
/// This is deliberately two-valued and fail-safe: everything a probe cannot
/// positively classify as finished is [`Self::Live`], because the only thing
/// this answer is used for is releasing a build slot, and releasing a slot
/// whose workload is still running is the failure mode the object-absence rule
/// existed to prevent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WarmObjectLifecycle {
    /// The object exists and is not provably finished — running, pending,
    /// unreadable status, or a status shape this code does not understand.
    #[default]
    Live,
    /// A Job carrying a `Complete` or `Failed` condition, or a Pod in the
    /// `Succeeded` / `Failed` phase. Both are Kubernetes' own terminal
    /// declarations, not an inference from counters: `status.failed > 0` is NOT
    /// terminal in general (a Job with retries left keeps running), which is
    /// why the conditions are read instead.
    Terminal,
}

/// Data obtained from Kubernetes before identity validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidateObject {
    pub kind: WarmCandidateKind,
    pub name: String,
    pub uid: Option<String>,
    pub annotations: BTreeMap<String, String>,
    /// Kubernetes' own terminal declaration for this object. Defaults to
    /// [`WarmObjectLifecycle::Live`] for anything not positively finished.
    pub lifecycle: WarmObjectLifecycle,
}

/// Result of checking the three durable annotations against the persisted
/// leased identity. A malformed candidate stays in the inventory; it never
/// becomes absence merely because it cannot be selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmAnnotationValidation {
    Matching,
    Mismatch {
        key: &'static str,
        expected: String,
        found: Option<String>,
    },
}

/// Candidate plus its immutable UID and identity-validation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidate {
    pub kind: WarmCandidateKind,
    pub name: String,
    pub uid: Option<String>,
    pub annotation_validation: WarmAnnotationValidation,
    /// Carried through from the observation. See [`WarmObjectLifecycle`].
    pub lifecycle: WarmObjectLifecycle,
}

/// Classification for candidates of one Kubernetes kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmCandidateSetState {
    Zero,
    One,
    Duplicate,
    /// A candidate was found but cannot be safely treated as a selectable
    /// object. This includes missing UIDs and annotation mismatches.
    Unresolved,
}

/// All observed candidates of one kind, never collapsed even when duplicate
/// or malformed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidateSet {
    pub state: WarmCandidateSetState,
    pub candidates: Vec<WarmCandidate>,
}

impl Default for WarmCandidateSet {
    fn default() -> Self {
        Self {
            state: WarmCandidateSetState::Zero,
            candidates: Vec::new(),
        }
    }
}

/// Complete observation for one stable request. An API failure is explicitly
/// different from a zero-candidate inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmInventoryObservation {
    Observed,
    ApiError(String),
}

/// Outcome of listing a single Kubernetes kind. Keeping these separately
/// ensures a successful Job observation is retained when, for example, the
/// independently fallible Pod list times out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarmCandidateListObservation {
    Observed,
    ApiError(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidateInventory {
    /// Aggregate status for consumers which only need to know whether every
    /// list succeeded. Per-kind status and all successfully observed objects
    /// remain available below for conservative reclamation.
    pub observation: WarmInventoryObservation,
    pub jobs_observation: WarmCandidateListObservation,
    pub jobs: WarmCandidateSet,
    pub pods_observation: WarmCandidateListObservation,
    pub pods: WarmCandidateSet,
}

impl WarmCandidateInventory {
    /// Return exactly one matching, UID-bearing Pod, otherwise `None`. Callers
    /// must leave the gate closed for zero, duplicate, malformed, and failed
    /// observations.
    pub fn selected_pod(&self) -> Option<&WarmCandidate> {
        (self.observation == WarmInventoryObservation::Observed
            && self.pods.state == WarmCandidateSetState::One)
            .then(|| self.pods.candidates.first())
            .flatten()
    }

    /// Whether this request's workload has provably finished: Kubernetes has
    /// declared every observed Job terminal and no observed Pod is still live.
    ///
    /// # Why this is safe to release a build slot on
    ///
    /// The rule it replaces was "both object lists are empty", which is a proof
    /// about the API server's garbage collector, not about the workload: a
    /// `Complete` warm Job holds one of only three slots until
    /// `ttlSecondsAfterFinished` fires. Observed live as `occupancy=3 cap=3`
    /// with two running task-runs.
    ///
    /// Three conditions, all required, and each one closes a hole the others
    /// leave open:
    ///
    /// * `observation == Observed` — an `ApiError` on EITHER list makes the
    ///   answer unknown, and unknown is never proof. A degraded API server
    ///   therefore keeps every slot occupied, exactly like the absence rule.
    /// * at least one Job candidate, and every Job candidate terminal. A Job's
    ///   `Complete`/`Failed` condition is the Job controller's own statement
    ///   that it will create no further Pods, so nothing can start after this
    ///   observation. (The warm Job is `backoffLimit: 0`, `restartPolicy:
    ///   Never`, one completion — a single Pod, no retries.)
    /// * every observed Pod candidate terminal. This is the direct statement of
    ///   "nothing is still running", and it is what makes the release safe
    ///   independently of any assumption about the Job controller: a `Running`
    ///   or `Pending` Pod — including one still terminating — keeps the slot.
    ///
    /// A zero-Job inventory deliberately answers `false`: that population is
    /// the object-absence branch, which releases for its own reason.
    pub fn workload_finished(&self) -> bool {
        self.observation == WarmInventoryObservation::Observed
            && !self.jobs.candidates.is_empty()
            && self
                .jobs
                .candidates
                .iter()
                .chain(self.pods.candidates.iter())
                .all(|candidate| candidate.lifecycle == WarmObjectLifecycle::Terminal)
    }
}

/// Outcome of attempting to write a gate authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateObservation {
    Opened,
    /// The name now denotes another immutable object, so no authorization was
    /// written. This is never retry-success or absence evidence.
    RejectedUid,
    Unresolved(String),
}

/// Result of a UID-preconditioned deletion request. `ConfirmedDelete` means
/// Kubernetes accepted deletion of the specified UID, not that name
/// disappearance proves termination; later reclamation still needs UID-based
/// evidence for every retained candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupObservation {
    ConfirmedDelete,
    Unresolved(String),
}

/// Fakeable Kubernetes operation seam. Test fakes receive the exact UID used
/// for gate activation and the UID-preconditioned delete operation.
#[async_trait]
pub trait WarmCandidateClient: Send + Sync {
    /// Return all warm-labelled Jobs visible to this namespace.
    /// Filtering to one request is deliberately done by the pure inventory
    /// logic, so mismatched annotations and name reuse remain observable.
    async fn list_warm_jobs(&self) -> Result<Vec<WarmCandidateObject>, String>;

    /// Return all warm-labelled Pods visible to this namespace. This is a
    /// separate operation because a failure here must not discard Job evidence
    /// returned by [`Self::list_warm_jobs`].
    async fn list_warm_pods(&self) -> Result<Vec<WarmCandidateObject>, String>;

    async fn open_gate(
        &self,
        job_name: &str,
        pod_name: &str,
        pod_uid: &str,
        identity: &LeasedWarmJobIdentity,
    ) -> GateObservation;

    async fn delete_uid(&self, candidate: &WarmCandidate) -> CleanupObservation;
}

/// Kubernetes implementation of [`WarmCandidateClient`].
pub struct KubeWarmCandidateClient {
    client: kube::Client,
    namespace: String,
}

impl KubeWarmCandidateClient {
    pub fn new(client: kube::Client, namespace: impl Into<String>) -> Self {
        Self {
            client,
            namespace: namespace.into(),
        }
    }
}

fn candidate_object(
    kind: WarmCandidateKind,
    name: Option<String>,
    uid: Option<String>,
    annotations: Option<BTreeMap<String, String>>,
    lifecycle: WarmObjectLifecycle,
) -> Option<WarmCandidateObject> {
    Some(WarmCandidateObject {
        kind,
        name: name?,
        uid,
        annotations: annotations.unwrap_or_default(),
        lifecycle,
    })
}

/// Kubernetes' terminal declaration for a Job.
///
/// Read from `status.conditions`, never from the `succeeded`/`failed` counters:
/// a counter says how many Pods have finished, and for a Job with retries left
/// `failed > 0` is entirely compatible with a Pod that is still running. The
/// `Complete` / `Failed` conditions are the Job controller's statement that it
/// is done creating Pods, which is the only thing a slot release may rely on.
fn job_lifecycle(job: &Job) -> WarmObjectLifecycle {
    let terminal = job.status.as_ref().is_some_and(|status| {
        status.conditions.iter().flatten().any(|condition| {
            matches!(condition.type_.as_str(), "Complete" | "Failed") && condition.status == "True"
        })
    });
    if terminal {
        WarmObjectLifecycle::Terminal
    } else {
        WarmObjectLifecycle::Live
    }
}

/// Kubernetes' terminal declaration for a Pod: the two terminal phases. Every
/// other phase — including an absent or unrecognised one — is [`Live`].
///
/// [`Live`]: WarmObjectLifecycle::Live
fn pod_lifecycle(pod: &Pod) -> WarmObjectLifecycle {
    let terminal = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| matches!(phase, "Succeeded" | "Failed"));
    if terminal {
        WarmObjectLifecycle::Terminal
    } else {
        WarmObjectLifecycle::Live
    }
}

fn api_error(error: kube::Error) -> String {
    error.to_string()
}

#[async_trait]
impl WarmCandidateClient for KubeWarmCandidateClient {
    async fn list_warm_jobs(&self) -> Result<Vec<WarmCandidateObject>, String> {
        let params = ListParams::default().labels(&format!("{LABEL_WARM}=true"));
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let jobs = jobs.list(&params).await.map_err(api_error)?;
        Ok(jobs
            .items
            .into_iter()
            .filter_map(|job| {
                let lifecycle = job_lifecycle(&job);
                candidate_object(
                    WarmCandidateKind::Job,
                    job.metadata.name,
                    job.metadata.uid,
                    job.metadata.annotations,
                    lifecycle,
                )
            })
            .collect())
    }

    async fn list_warm_pods(&self) -> Result<Vec<WarmCandidateObject>, String> {
        let params = ListParams::default().labels(&format!("{LABEL_WARM}=true"));
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods = pods.list(&params).await.map_err(api_error)?;
        Ok(pods
            .items
            .into_iter()
            .filter_map(|pod| {
                let lifecycle = pod_lifecycle(&pod);
                candidate_object(
                    WarmCandidateKind::Pod,
                    pod.metadata.name,
                    pod.metadata.uid,
                    pod.metadata.annotations,
                    lifecycle,
                )
            })
            .collect())
    }

    async fn open_gate(
        &self,
        job_name: &str,
        pod_name: &str,
        pod_uid: &str,
        identity: &LeasedWarmJobIdentity,
    ) -> GateObservation {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = match pods.get_opt(pod_name).await {
            Ok(Some(pod)) if pod.metadata.uid.as_deref() == Some(pod_uid) => pod,
            Ok(_) => return GateObservation::RejectedUid,
            Err(error) => return GateObservation::Unresolved(api_error(error)),
        };
        let annotations = pod.metadata.annotations.unwrap_or_default();
        if validate_annotations(&annotations, identity) != WarmAnnotationValidation::Matching {
            return GateObservation::Unresolved("selected Pod annotations changed".into());
        }

        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = warm_gate_config_map_name(job_name);
        let map = ConfigMap {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([(
                GATE_AUTHORIZATION_KEY.into(),
                format!("{pod_uid}:{}", identity.fencing_token),
            )])),
            ..ConfigMap::default()
        };
        match maps.get_opt(&name).await {
            Ok(Some(existing)) => {
                let mut replacement = map;
                replacement.metadata.resource_version = existing.metadata.resource_version;
                maps.replace(&name, &PostParams::default(), &replacement)
                    .await
                    .map(|_| GateObservation::Opened)
                    .unwrap_or_else(|error| GateObservation::Unresolved(api_error(error)))
            }
            Ok(None) => maps
                .create(&PostParams::default(), &map)
                .await
                .map(|_| GateObservation::Opened)
                .unwrap_or_else(|error| GateObservation::Unresolved(api_error(error))),
            Err(error) => GateObservation::Unresolved(api_error(error)),
        }
    }

    async fn delete_uid(&self, candidate: &WarmCandidate) -> CleanupObservation {
        let Some(uid) = candidate.uid.as_ref() else {
            return CleanupObservation::Unresolved("candidate has no immutable UID".into());
        };
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.clone()),
                ..Preconditions::default()
            }),
            ..DeleteParams::default()
        };
        match candidate.kind {
            WarmCandidateKind::Job => Api::<Job>::namespaced(self.client.clone(), &self.namespace)
                .delete(&candidate.name, &params)
                .await
                .map(|_| CleanupObservation::ConfirmedDelete)
                .unwrap_or_else(|error| CleanupObservation::Unresolved(api_error(error))),
            WarmCandidateKind::Pod => Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
                .delete(&candidate.name, &params)
                .await
                .map(|_| CleanupObservation::ConfirmedDelete)
                .unwrap_or_else(|error| CleanupObservation::Unresolved(api_error(error))),
        }
    }
}

/// Pure inventory/control facade used by the future lease state machine.
pub struct WarmCandidateControl<C> {
    client: C,
}

impl<C> WarmCandidateControl<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

impl<C: WarmCandidateClient> WarmCandidateControl<C> {
    pub async fn inventory(&self, identity: &LeasedWarmJobIdentity) -> WarmCandidateInventory {
        let jobs_result = self.client.list_warm_jobs().await;
        let pods_result = self.client.list_warm_pods().await;
        let jobs_observation = list_observation(&jobs_result);
        let pods_observation = list_observation(&pods_result);
        let observation = inventory_observation(&jobs_observation, &pods_observation);
        let candidates = jobs_result
            .unwrap_or_default()
            .into_iter()
            .chain(pods_result.unwrap_or_default())
            .filter(|object| {
                object.annotations.get(ANNOTATION_WARM_REQUEST_ID)
                    == Some(&identity.warm_request_id)
                    || object.name == identity.object_name
            })
            .map(|object| WarmCandidate {
                kind: object.kind,
                name: object.name,
                uid: object.uid,
                annotation_validation: validate_annotations(&object.annotations, identity),
                lifecycle: object.lifecycle,
            })
            .collect::<Vec<_>>();
        let jobs = candidate_set(
            candidates
                .iter()
                .filter(|c| c.kind == WarmCandidateKind::Job)
                .cloned()
                .collect(),
        );
        let pods = candidate_set(
            candidates
                .into_iter()
                .filter(|c| c.kind == WarmCandidateKind::Pod)
                .collect(),
        );
        WarmCandidateInventory {
            observation,
            jobs_observation,
            jobs,
            pods_observation,
            pods,
        }
    }

    /// Open only the exact Pod returned by a successful [`Self::inventory`].
    /// A missing or changed UID cannot reach the client operation.
    pub async fn open_selected_pod_gate(
        &self,
        identity: &LeasedWarmJobIdentity,
        inventory: &WarmCandidateInventory,
    ) -> GateObservation {
        let Some(pod) = inventory.selected_pod() else {
            return GateObservation::Unresolved("no uniquely validated Pod candidate".into());
        };
        let Some(uid) = pod.uid.as_deref() else {
            return GateObservation::Unresolved("selected Pod has no immutable UID".into());
        };
        self.client
            .open_gate(&identity.object_name, &pod.name, uid, identity)
            .await
    }

    /// Delete one observed candidate with its immutable UID precondition.
    pub async fn delete_candidate(&self, candidate: &WarmCandidate) -> CleanupObservation {
        self.client.delete_uid(candidate).await
    }
}

fn validate_annotations(
    annotations: &BTreeMap<String, String>,
    identity: &LeasedWarmJobIdentity,
) -> WarmAnnotationValidation {
    let fencing_token = identity.fencing_token.to_string();
    for (key, expected) in [
        (
            ANNOTATION_WARM_REQUEST_ID,
            identity.warm_request_id.as_str(),
        ),
        (ANNOTATION_GRAPH_REVISION, identity.graph_revision.as_str()),
        (ANNOTATION_FENCING_TOKEN, fencing_token.as_str()),
    ] {
        if annotations.get(key).map(String::as_str) != Some(expected) {
            return WarmAnnotationValidation::Mismatch {
                key,
                expected: expected.into(),
                found: annotations.get(key).cloned(),
            };
        }
    }
    WarmAnnotationValidation::Matching
}

fn candidate_set(candidates: Vec<WarmCandidate>) -> WarmCandidateSet {
    let state = if candidates.is_empty() {
        WarmCandidateSetState::Zero
    } else if candidates.iter().any(|candidate| {
        candidate.uid.is_none()
            || candidate.annotation_validation != WarmAnnotationValidation::Matching
    }) {
        WarmCandidateSetState::Unresolved
    } else if candidates.len() == 1 {
        WarmCandidateSetState::One
    } else {
        WarmCandidateSetState::Duplicate
    };
    WarmCandidateSet { state, candidates }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        jobs: Result<Vec<WarmCandidateObject>, String>,
        pods: Result<Vec<WarmCandidateObject>, String>,
        gates: Mutex<Vec<(String, String, String)>>,
        deletes: Mutex<Vec<(String, Option<String>)>>,
        live_uids: Mutex<BTreeMap<String, String>>,
        gate_result: GateObservation,
        delete_result: CleanupObservation,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                jobs: Ok(Vec::new()),
                pods: Ok(Vec::new()),
                gates: Mutex::new(Vec::new()),
                deletes: Mutex::new(Vec::new()),
                live_uids: Mutex::new(BTreeMap::new()),
                gate_result: GateObservation::Unresolved("not configured".into()),
                delete_result: CleanupObservation::Unresolved("not configured".into()),
            }
        }
    }
    #[async_trait]
    impl WarmCandidateClient for Fake {
        async fn list_warm_jobs(&self) -> Result<Vec<WarmCandidateObject>, String> {
            self.jobs.clone()
        }
        async fn list_warm_pods(&self) -> Result<Vec<WarmCandidateObject>, String> {
            self.pods.clone()
        }
        async fn open_gate(
            &self,
            job: &str,
            pod: &str,
            uid: &str,
            _: &LeasedWarmJobIdentity,
        ) -> GateObservation {
            self.gates
                .lock()
                .unwrap()
                .push((job.into(), pod.into(), uid.into()));
            if self
                .live_uids
                .lock()
                .unwrap()
                .get(pod)
                .is_some_and(|live_uid| live_uid != uid)
            {
                return GateObservation::RejectedUid;
            }
            self.gate_result.clone()
        }
        async fn delete_uid(&self, candidate: &WarmCandidate) -> CleanupObservation {
            self.deletes
                .lock()
                .unwrap()
                .push((candidate.name.clone(), candidate.uid.clone()));
            if self
                .live_uids
                .lock()
                .unwrap()
                .get(&candidate.name)
                .is_some_and(|live_uid| Some(live_uid.as_str()) != candidate.uid.as_deref())
            {
                return CleanupObservation::Unresolved("UID precondition rejected".into());
            }
            self.delete_result.clone()
        }
    }
    fn identity() -> LeasedWarmJobIdentity {
        LeasedWarmJobIdentity::new("project", "request", "revision", 7)
    }

    /// The classification a build-slot release depends on, read off the API
    /// objects Kubernetes actually returns.
    ///
    /// Without this the reconciler's release test could pass forever against a
    /// mapping that answers `Live` for every real Job — the fix would be inert
    /// in production and every test would still be green.
    #[test]
    fn kubernetes_status_decides_the_lifecycle() {
        let job = |status: serde_json::Value| -> Job {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": "warm"},
                "status": status,
            }))
            .expect("job fixture")
        };
        assert_eq!(
            job_lifecycle(&job(
                serde_json::json!({"conditions": [{"type": "Complete", "status": "True"}]})
            )),
            WarmObjectLifecycle::Terminal,
            "a Complete Job is finished and its slot must come back"
        );
        assert_eq!(
            job_lifecycle(&job(
                serde_json::json!({"conditions": [{"type": "Failed", "status": "True"}]})
            )),
            WarmObjectLifecycle::Terminal
        );
        assert_eq!(
            job_lifecycle(&job(serde_json::json!({"active": 1}))),
            WarmObjectLifecycle::Live
        );
        assert_eq!(
            job_lifecycle(&job(serde_json::json!({}))),
            WarmObjectLifecycle::Live
        );
        // A condition that is present but NOT true says the opposite of what a
        // type-only match would read.
        assert_eq!(
            job_lifecycle(&job(
                serde_json::json!({"conditions": [{"type": "Complete", "status": "False"}]})
            )),
            WarmObjectLifecycle::Live
        );
        // The counter trap: `failed > 0` is not terminal on its own, because a
        // Job with retries left keeps running.
        assert_eq!(
            job_lifecycle(&job(serde_json::json!({"failed": 1, "active": 1}))),
            WarmObjectLifecycle::Live,
            "a failure counter is not a terminal declaration"
        );
        assert_eq!(
            job_lifecycle(
                &serde_json::from_value(serde_json::json!({"metadata": {"name": "warm"}}))
                    .expect("job fixture")
            ),
            WarmObjectLifecycle::Live,
            "an unreported status is unknown, and unknown never releases"
        );

        let pod = |phase: &str| -> Pod {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": "warm-pod"},
                "status": {"phase": phase},
            }))
            .expect("pod fixture")
        };
        assert_eq!(
            pod_lifecycle(&pod("Succeeded")),
            WarmObjectLifecycle::Terminal
        );
        assert_eq!(pod_lifecycle(&pod("Failed")), WarmObjectLifecycle::Terminal);
        assert_eq!(pod_lifecycle(&pod("Running")), WarmObjectLifecycle::Live);
        assert_eq!(pod_lifecycle(&pod("Pending")), WarmObjectLifecycle::Live);
        assert_eq!(
            pod_lifecycle(
                &serde_json::from_value(serde_json::json!({"metadata": {"name": "warm-pod"}}))
                    .expect("pod fixture")
            ),
            WarmObjectLifecycle::Live
        );
    }

    /// The slot-release predicate itself: every unknown answers "keep the slot".
    #[test]
    fn only_a_finished_and_fully_observed_workload_releases() {
        let candidate = |kind, lifecycle| WarmCandidate {
            kind,
            name: "object".into(),
            uid: Some("uid".into()),
            annotation_validation: WarmAnnotationValidation::Matching,
            lifecycle,
        };
        let inventory = |observation: WarmInventoryObservation,
                         jobs: Vec<WarmCandidate>,
                         pods: Vec<WarmCandidate>| WarmCandidateInventory {
            observation,
            jobs_observation: WarmCandidateListObservation::Observed,
            jobs: candidate_set(jobs),
            pods_observation: WarmCandidateListObservation::Observed,
            pods: candidate_set(pods),
        };
        let terminal_job = candidate(WarmCandidateKind::Job, WarmObjectLifecycle::Terminal);
        let live_job = candidate(WarmCandidateKind::Job, WarmObjectLifecycle::Live);
        let terminal_pod = candidate(WarmCandidateKind::Pod, WarmObjectLifecycle::Terminal);
        let live_pod = candidate(WarmCandidateKind::Pod, WarmObjectLifecycle::Live);

        assert!(
            inventory(
                WarmInventoryObservation::Observed,
                vec![terminal_job.clone()],
                vec![terminal_pod.clone()]
            )
            .workload_finished()
        );
        assert!(
            inventory(
                WarmInventoryObservation::Observed,
                vec![terminal_job.clone()],
                vec![]
            )
            .workload_finished(),
            "a finished Job whose Pod is already gone is finished"
        );
        assert!(
            !inventory(
                WarmInventoryObservation::Observed,
                vec![terminal_job.clone()],
                vec![live_pod]
            )
            .workload_finished(),
            "a running Pod keeps the slot however finished its Job claims to be"
        );
        assert!(
            !inventory(
                WarmInventoryObservation::Observed,
                vec![live_job],
                vec![terminal_pod.clone()]
            )
            .workload_finished()
        );
        assert!(
            !inventory(
                WarmInventoryObservation::ApiError("apiserver unavailable".into()),
                vec![terminal_job],
                vec![terminal_pod]
            )
            .workload_finished(),
            "an unusable observation is never proof"
        );
        assert!(
            !inventory(WarmInventoryObservation::Observed, vec![], vec![]).workload_finished(),
            "an empty inventory is the object-absence branch, not this one"
        );
    }
    fn object(kind: WarmCandidateKind, name: &str, uid: Option<&str>) -> WarmCandidateObject {
        let id = identity();
        WarmCandidateObject {
            kind,
            name: name.into(),
            uid: uid.map(str::to_owned),
            annotations: BTreeMap::from([
                (ANNOTATION_WARM_REQUEST_ID.into(), id.warm_request_id),
                (ANNOTATION_GRAPH_REVISION.into(), id.graph_revision),
                (
                    ANNOTATION_FENCING_TOKEN.into(),
                    id.fencing_token.to_string(),
                ),
            ]),
            lifecycle: WarmObjectLifecycle::Live,
        }
    }
    #[tokio::test]
    async fn zero_one_and_duplicate_pods_remain_distinct() {
        let id = identity();
        let zero = WarmCandidateControl::new(Fake {
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(zero.pods.state, WarmCandidateSetState::Zero);
        let one = WarmCandidateControl::new(Fake {
            pods: Ok(vec![object(WarmCandidateKind::Pod, "pod", Some("p1"))]),
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(one.pods.state, WarmCandidateSetState::One);
        let duplicate = WarmCandidateControl::new(Fake {
            pods: Ok(vec![
                object(WarmCandidateKind::Pod, "pod-a", Some("p1")),
                object(WarmCandidateKind::Pod, "pod-b", Some("p2")),
            ]),
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(duplicate.pods.state, WarmCandidateSetState::Duplicate);
        assert_eq!(duplicate.pods.candidates.len(), 2);
    }
    #[tokio::test]
    async fn mismatch_and_missing_uid_are_unresolved_and_retained() {
        let id = identity();
        let mut mismatch = object(WarmCandidateKind::Pod, &id.object_name, Some("p1"));
        mismatch
            .annotations
            .insert(ANNOTATION_GRAPH_REVISION.into(), "other".into());
        let inventory = WarmCandidateControl::new(Fake {
            jobs: Ok(vec![object(WarmCandidateKind::Job, "job", None)]),
            pods: Ok(vec![mismatch]),
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(inventory.pods.state, WarmCandidateSetState::Unresolved);
        assert_eq!(inventory.jobs.state, WarmCandidateSetState::Unresolved);
        assert_eq!(inventory.pods.candidates.len(), 1);
        assert_eq!(inventory.jobs.candidates[0].uid, None);
    }
    #[tokio::test]
    async fn api_error_is_not_zero_candidates() {
        let inventory = WarmCandidateControl::new(Fake {
            pods: Err("offline".into()),
            ..Default::default()
        })
        .inventory(&identity())
        .await;
        assert_eq!(
            inventory.observation,
            WarmInventoryObservation::ApiError("offline".into())
        );
        assert_eq!(inventory.pods.state, WarmCandidateSetState::Zero);
    }
    #[tokio::test]
    async fn pod_list_error_retains_successful_job_evidence() {
        let inventory = WarmCandidateControl::new(Fake {
            jobs: Ok(vec![object(WarmCandidateKind::Job, "job", Some("job-1"))]),
            pods: Err("pod timeout".into()),
            ..Default::default()
        })
        .inventory(&identity())
        .await;
        assert_eq!(
            inventory.observation,
            WarmInventoryObservation::ApiError("pod timeout".into())
        );
        assert_eq!(
            inventory.jobs_observation,
            WarmCandidateListObservation::Observed
        );
        assert_eq!(
            inventory.pods_observation,
            WarmCandidateListObservation::ApiError("pod timeout".into())
        );
        assert_eq!(inventory.jobs.candidates[0].uid.as_deref(), Some("job-1"));
    }
    #[tokio::test]
    async fn gate_and_delete_are_fenced_to_selected_uid() {
        let id = identity();
        let fake = Fake {
            pods: Ok(vec![object(
                WarmCandidateKind::Pod,
                "same-name",
                Some("old-uid"),
            )]),
            gate_result: GateObservation::Opened,
            delete_result: CleanupObservation::ConfirmedDelete,
            ..Default::default()
        };
        let control = WarmCandidateControl::new(fake);
        let inventory = control.inventory(&id).await;
        assert_eq!(
            control.open_selected_pod_gate(&id, &inventory).await,
            GateObservation::Opened
        );
        assert_eq!(
            control
                .delete_candidate(inventory.selected_pod().unwrap())
                .await,
            CleanupObservation::ConfirmedDelete
        );
        assert_eq!(control.client.gates.lock().unwrap()[0].2, "old-uid");
        assert_eq!(
            control.client.deletes.lock().unwrap()[0],
            ("same-name".into(), Some("old-uid".into()))
        );
    }
    #[tokio::test]
    async fn same_name_different_uid_is_not_activated_or_delete_evidence() {
        let id = identity();
        let fake = Fake {
            pods: Ok(vec![object(
                WarmCandidateKind::Pod,
                "same-name",
                Some("old-uid"),
            )]),
            live_uids: Mutex::new(BTreeMap::from([("same-name".into(), "new-uid".into())])),
            gate_result: GateObservation::Opened,
            delete_result: CleanupObservation::ConfirmedDelete,
            ..Default::default()
        };
        let control = WarmCandidateControl::new(fake);
        let inventory = control.inventory(&id).await;
        assert_eq!(inventory.pods.state, WarmCandidateSetState::One);
        assert_eq!(
            control.open_selected_pod_gate(&id, &inventory).await,
            GateObservation::RejectedUid
        );
        assert_eq!(control.client.gates.lock().unwrap()[0].2, "old-uid");
        assert_eq!(
            control
                .delete_candidate(inventory.selected_pod().unwrap())
                .await,
            CleanupObservation::Unresolved("UID precondition rejected".into())
        );
        assert_eq!(
            control.client.deletes.lock().unwrap()[0],
            ("same-name".into(), Some("old-uid".into()))
        );
    }
    #[tokio::test]
    async fn uncertain_delete_stays_unresolved() {
        let candidate = WarmCandidate {
            kind: WarmCandidateKind::Pod,
            name: "pod".into(),
            uid: Some("uid".into()),
            annotation_validation: WarmAnnotationValidation::Matching,
            lifecycle: WarmObjectLifecycle::Live,
        };
        let control = WarmCandidateControl::new(Fake {
            delete_result: CleanupObservation::Unresolved("timeout".into()),
            ..Default::default()
        });
        assert_eq!(
            control.delete_candidate(&candidate).await,
            CleanupObservation::Unresolved("timeout".into())
        );
    }
}
