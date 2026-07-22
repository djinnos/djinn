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

/// Data obtained from Kubernetes before identity validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidateObject {
    pub kind: WarmCandidateKind,
    pub name: String,
    pub uid: Option<String>,
    pub annotations: BTreeMap<String, String>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarmCandidateInventory {
    pub observation: WarmInventoryObservation,
    pub jobs: WarmCandidateSet,
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
    /// Return all warm-labelled Jobs and Pods visible to this namespace.
    /// Filtering to one request is deliberately done by the pure inventory
    /// logic, so mismatched annotations and name reuse remain observable.
    async fn list_warm_objects(&self) -> Result<Vec<WarmCandidateObject>, String>;

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
) -> Option<WarmCandidateObject> {
    Some(WarmCandidateObject {
        kind,
        name: name?,
        uid,
        annotations: annotations.unwrap_or_default(),
    })
}

fn api_error(error: kube::Error) -> String {
    error.to_string()
}

#[async_trait]
impl WarmCandidateClient for KubeWarmCandidateClient {
    async fn list_warm_objects(&self) -> Result<Vec<WarmCandidateObject>, String> {
        let params = ListParams::default().labels(&format!("{LABEL_WARM}=true"));
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let jobs = jobs.list(&params).await.map_err(api_error)?;
        let pods = pods.list(&params).await.map_err(api_error)?;
        Ok(jobs
            .items
            .into_iter()
            .filter_map(|job| {
                candidate_object(
                    WarmCandidateKind::Job,
                    job.metadata.name,
                    job.metadata.uid,
                    job.metadata.annotations,
                )
            })
            .chain(pods.items.into_iter().filter_map(|pod| {
                candidate_object(
                    WarmCandidateKind::Pod,
                    pod.metadata.name,
                    pod.metadata.uid,
                    pod.metadata.annotations,
                )
            }))
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
        let objects = match self.client.list_warm_objects().await {
            Ok(objects) => objects,
            Err(error) => {
                return WarmCandidateInventory {
                    observation: WarmInventoryObservation::ApiError(error),
                    jobs: WarmCandidateSet::default(),
                    pods: WarmCandidateSet::default(),
                };
            }
        };
        let candidates = objects
            .into_iter()
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
            observation: WarmInventoryObservation::Observed,
            jobs,
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
        objects: Result<Vec<WarmCandidateObject>, String>,
        gates: Mutex<Vec<(String, String, String)>>,
        deletes: Mutex<Vec<(String, Option<String>)>>,
        gate_result: GateObservation,
        delete_result: CleanupObservation,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                objects: Ok(Vec::new()),
                gates: Mutex::new(Vec::new()),
                deletes: Mutex::new(Vec::new()),
                gate_result: GateObservation::Unresolved("not configured".into()),
                delete_result: CleanupObservation::Unresolved("not configured".into()),
            }
        }
    }
    #[async_trait]
    impl WarmCandidateClient for Fake {
        async fn list_warm_objects(&self) -> Result<Vec<WarmCandidateObject>, String> {
            self.objects.clone()
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
            self.gate_result.clone()
        }
        async fn delete_uid(&self, candidate: &WarmCandidate) -> CleanupObservation {
            self.deletes
                .lock()
                .unwrap()
                .push((candidate.name.clone(), candidate.uid.clone()));
            self.delete_result.clone()
        }
    }
    fn identity() -> LeasedWarmJobIdentity {
        LeasedWarmJobIdentity::new("project", "request", "revision", 7)
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
        }
    }
    #[tokio::test]
    async fn zero_one_and_duplicate_pods_remain_distinct() {
        let id = identity();
        let zero = WarmCandidateControl::new(Fake {
            objects: Ok(vec![]),
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(zero.pods.state, WarmCandidateSetState::Zero);
        let one = WarmCandidateControl::new(Fake {
            objects: Ok(vec![object(WarmCandidateKind::Pod, "pod", Some("p1"))]),
            ..Default::default()
        })
        .inventory(&id)
        .await;
        assert_eq!(one.pods.state, WarmCandidateSetState::One);
        let duplicate = WarmCandidateControl::new(Fake {
            objects: Ok(vec![
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
            objects: Ok(vec![mismatch, object(WarmCandidateKind::Job, "job", None)]),
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
            objects: Err("offline".into()),
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
    async fn gate_and_delete_are_fenced_to_selected_uid() {
        let id = identity();
        let fake = Fake {
            objects: Ok(vec![object(
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
            objects: Ok(vec![
                object(WarmCandidateKind::Pod, "same-name", Some("old-uid")),
                object(WarmCandidateKind::Pod, "same-name", Some("new-uid")),
            ]),
            gate_result: GateObservation::Opened,
            delete_result: CleanupObservation::ConfirmedDelete,
            ..Default::default()
        };
        let control = WarmCandidateControl::new(fake);
        let inventory = control.inventory(&id).await;
        assert_eq!(inventory.pods.state, WarmCandidateSetState::Duplicate);
        assert!(matches!(
            control.open_selected_pod_gate(&id, &inventory).await,
            GateObservation::Unresolved(_)
        ));
        assert!(control.client.gates.lock().unwrap().is_empty());
        // A caller must retain both UIDs; the control layer does not convert a
        // reused name into a deletion/termination fact.
        assert_eq!(
            inventory
                .pods
                .candidates
                .iter()
                .map(|c| c.uid.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("old-uid"), Some("new-uid")]
        );
    }
    #[tokio::test]
    async fn uncertain_delete_stays_unresolved() {
        let candidate = WarmCandidate {
            kind: WarmCandidateKind::Pod,
            name: "pod".into(),
            uid: Some("uid".into()),
            annotation_validation: WarmAnnotationValidation::Matching,
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
