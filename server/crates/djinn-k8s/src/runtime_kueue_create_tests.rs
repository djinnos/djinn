//! Create-then-observe contract for the task-run dispatch path (task `37yq`).
//!
//! Everything here runs against [`FakeCluster`] — an in-process apiserver that
//! keeps *state*, not a canned response. That matters for every assertion in
//! this file:
//!
//! * a stateless mock cannot 409 the second create, so it cannot show that two
//!   dispatches of one task-run converge on one Job;
//! * a stateless mock cannot decline to materialise a Pod for a suspended Job,
//!   so it cannot show that `suspend: true` is what holds the Pod back rather
//!   than the renderer merely writing a key nobody reads.
//!
//! The fake models exactly two pieces of Kubernetes behaviour beyond storage:
//!
//! 1. the Job controller's rule that a Job creates Pods only while it is *not*
//!    suspended. That rule is the entire point of the Kueue cutover, so it is
//!    the one thing this file's tests refuse to take on trust;
//! 2. the Job controller's rule that a nonterminal Job whose Pod disappears
//!    gets a *replacement* Pod, with a fresh `metadata.uid`. That is what
//!    `runtime_pod_fence_tests` fences against — and it is honest to model here
//!    because it is Job-controller behaviour, not Kueue behaviour. It is not a
//!    substitute for the live-cluster proof.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};

use http::Response;
use kube::client::Body;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::*;
use crate::config::KubernetesConfig;
use crate::secret::task_run_resource_name;
use djinn_core::models::TaskRunTrigger;
use djinn_runtime::SupervisorFlow;

// ---------------------------------------------------------------------------
// The fake cluster
// ---------------------------------------------------------------------------

/// One recorded apiserver call, for assertions about what actually happened.
///
/// `body` is the request body exactly as the client serialised it. Delete
/// options ride in the DELETE body in the Kubernetes API, so this is the only
/// place `propagationPolicy` can be observed as something that was actually
/// *sent* rather than merely constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApiCall {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: Option<Value>,
}

#[derive(Default)]
pub(super) struct ClusterState {
    /// Job name -> stored Job object (as the apiserver would return it).
    pub(super) jobs: HashMap<String, Value>,
    /// Secret name -> stored Secret object.
    pub(super) secrets: HashMap<String, Value>,
    /// Pod objects the modelled Job controller has materialised, each with its
    /// own immutable `metadata.uid`.
    pub(super) pods: Vec<Value>,
    pub(super) calls: Vec<ApiCall>,
}

pub(super) struct FakeCluster {
    pub(super) state: StdMutex<ClusterState>,
    uid_seq: AtomicU64,
    pod_seq: AtomicU64,
    /// When set, every Job create fails with this `(code, reason)` instead of
    /// storing anything. Models a definitively-rejected create.
    job_create_failure: Option<(u16, &'static str)>,
}

impl FakeCluster {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(ClusterState::default()),
            uid_seq: AtomicU64::new(1),
            pod_seq: AtomicU64::new(1),
            job_create_failure: None,
        })
    }

    fn failing_job_create(code: u16, reason: &'static str) -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(ClusterState::default()),
            uid_seq: AtomicU64::new(1),
            pod_seq: AtomicU64::new(1),
            job_create_failure: Some((code, reason)),
        })
    }

    fn next_uid(&self) -> String {
        format!("uid-{}", self.uid_seq.fetch_add(1, Ordering::SeqCst))
    }

    pub(super) fn job_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.state.lock().unwrap().jobs.keys().cloned().collect();
        names.sort();
        names
    }

    pub(super) fn job(&self, name: &str) -> Option<Value> {
        self.state.lock().unwrap().jobs.get(name).cloned()
    }

    fn secret_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.state.lock().unwrap().secrets.keys().cloned().collect();
        names.sort();
        names
    }

    pub(super) fn pod_count(&self) -> usize {
        self.state.lock().unwrap().pods.len()
    }

    /// Every stored Pod's `metadata.uid`, in creation order.
    pub(super) fn pod_uids(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .pods
            .iter()
            .filter_map(|pod| {
                pod.pointer("/metadata/uid")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    pub(super) fn calls(&self) -> Vec<ApiCall> {
        self.state.lock().unwrap().calls.clone()
    }

    /// The Job controller, as far as these tests care:
    ///
    /// * a suspended Job has no Pod;
    /// * an unsuspended, *nonterminal* Job always has exactly one Pod — so if
    ///   its Pod is destroyed, the controller makes a NEW one, with a NEW uid.
    ///
    /// Kueue is what flips `suspend` on an admitted Workload, so
    /// [`Self::unsuspend`] below stands in for the admission this cutover hands
    /// over to it.
    fn reconcile_job_controller(&self, state: &mut ClusterState, job_name: &str) {
        let Some(job) = state.jobs.get(job_name).cloned() else {
            return;
        };
        let suspended = job
            .pointer("/spec/suspend")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let terminal = job
            .pointer("/status/succeeded")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
            || job
                .pointer("/status/failed")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0;
        if suspended || terminal || Self::owned_pod_index(state, job_name).is_some() {
            return;
        }
        let job_uid = job
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let labels = job
            .pointer("/spec/template/metadata/labels")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let pod_name = format!("{job_name}-{}", self.pod_seq.fetch_add(1, Ordering::SeqCst));
        state.pods.push(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": pod_name,
                "namespace": "djinn",
                "uid": self.next_uid(),
                "resourceVersion": "1",
                "labels": labels,
                "ownerReferences": [{
                    "apiVersion": "batch/v1",
                    "kind": "Job",
                    "name": job_name,
                    "uid": job_uid,
                    "controller": true,
                }],
            },
            "spec": {"containers": [{"name": "worker", "image": "registry/test:test"}]},
            "status": {"phase": "Running"},
        }));
    }

    fn owned_pod_index(state: &ClusterState, job_name: &str) -> Option<usize> {
        state.pods.iter().position(|pod| {
            pod.pointer("/metadata/ownerReferences/0/name")
                .and_then(Value::as_str)
                == Some(job_name)
        })
    }

    /// Stand in for Kueue admitting the Workload: clear `suspend` and let the
    /// modelled Job controller run again.
    pub(super) fn unsuspend(&self, job_name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.get_mut(job_name)
            && let Some(spec) = job.get_mut("spec").and_then(Value::as_object_mut)
        {
            spec.insert("suspend".into(), Value::Bool(false));
        }
        self.reconcile_job_controller(&mut state, job_name);
    }

    /// `kubectl delete pod --force --grace-period=0`: the Pod object vanishes
    /// immediately, and the Job controller — whose Job is still nonterminal —
    /// replaces it with a Pod carrying a FRESH uid.
    ///
    /// Returns `(destroyed_uid, replacement_uid)`.
    pub(super) fn force_delete_pod_of(&self, job_name: &str) -> (String, Option<String>) {
        let mut state = self.state.lock().unwrap();
        let index = Self::owned_pod_index(&state, job_name).expect("the Job has a Pod to destroy");
        let destroyed = state.pods.remove(index);
        let destroyed_uid = destroyed
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .expect("stored Pod has a uid")
            .to_string();
        self.reconcile_job_controller(&mut state, job_name);
        let replacement = Self::owned_pod_index(&state, job_name).and_then(|i| {
            state.pods[i]
                .pointer("/metadata/uid")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        (destroyed_uid, replacement)
    }

    /// TTL-GC of a finished Pod: the object disappears and nothing replaces it.
    pub(super) fn gc_pod_of(&self, job_name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = Self::owned_pod_index(&state, job_name) {
            state.pods.remove(index);
        }
    }

    /// TTL-GC of the Job object itself.
    pub(super) fn gc_job(&self, job_name: &str) {
        self.state.lock().unwrap().jobs.remove(job_name);
    }

    /// Drive the Job to its terminal `Failed` condition, as `backoffLimit: 0`
    /// does on a single Pod failure.
    pub(super) fn fail_job(&self, job_name: &str, reason: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.get_mut(job_name) {
            job["status"] = json!({
                "failed": 1,
                "conditions": [{"type": "Failed", "status": "True", "reason": reason}],
            });
        }
    }

    /// Drive the Job to its terminal `Complete` condition.
    pub(super) fn complete_job(&self, job_name: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.get_mut(job_name) {
            job["status"] = json!({
                "succeeded": 1,
                "conditions": [{"type": "Complete", "status": "True"}],
            });
        }
    }

    fn status_body(code: u16, reason: &str, message: &str) -> Value {
        json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "message": message,
            "reason": reason,
            "code": code,
        })
    }

    /// Filter stored Pods by a `k=v[,k=v]` label selector, exactly as the
    /// apiserver does for `GET .../pods?labelSelector=…`.
    fn select_pods(state: &ClusterState, selector: Option<&str>) -> Vec<Value> {
        let Some(selector) = selector.filter(|s| !s.is_empty()) else {
            return state.pods.clone();
        };
        state
            .pods
            .iter()
            .filter(|pod| {
                selector.split(',').all(|term| {
                    let Some((key, value)) = term.split_once('=') else {
                        return false;
                    };
                    pod.pointer("/metadata/labels")
                        .and_then(|labels| labels.get(key))
                        .and_then(Value::as_str)
                        == Some(value)
                })
            })
            .cloned()
            .collect()
    }

    async fn handle(self: Arc<Self>, request: http::Request<Body>) -> (u16, Value) {
        let method = request.method().clone();
        let path = request.uri().path().to_string();
        let query = parse_query(request.uri().query().unwrap_or_default());
        let body = axum_read_body(request).await;

        self.state.lock().unwrap().calls.push(ApiCall {
            method: method.to_string(),
            path: path.clone(),
            body: body.clone(),
        });

        let is_job = path.contains("/jobs");
        let is_secret = path.contains("/secrets");
        let is_pod = path.contains("/pods");
        // `.../jobs` on a create, `.../jobs/<name>` on a get/patch.
        let trailing = path.rsplit('/').next().unwrap_or_default().to_string();
        let named = !(trailing == "jobs" || trailing == "secrets" || trailing == "pods");

        if is_pod && method == "GET" && !named {
            let state = self.state.lock().unwrap();
            let items = Self::select_pods(&state, query.get("labelSelector").map(String::as_str));
            return (
                200,
                json!({
                    "apiVersion": "v1",
                    "kind": "PodList",
                    "metadata": {"resourceVersion": "1"},
                    "items": items,
                }),
            );
        }

        if is_job && method == "DELETE" && named {
            let mut state = self.state.lock().unwrap();
            let Some(job) = state.jobs.remove(&trailing) else {
                return (
                    404,
                    Self::status_body(
                        404,
                        "NotFound",
                        &format!("jobs.batch \"{trailing}\" not found"),
                    ),
                );
            };
            // Foreground/Background both cascade in the end state this fixture
            // models; Orphan deliberately does not, so a policy regression is
            // visible in the surviving Pods as well as in the recorded body.
            let orphan = body
                .as_ref()
                .and_then(|b| b.get("propagationPolicy"))
                .and_then(Value::as_str)
                == Some("Orphan");
            if !orphan {
                state.pods.retain(|pod| {
                    pod.pointer("/metadata/ownerReferences/0/name")
                        .and_then(Value::as_str)
                        != Some(trailing.as_str())
                });
            }
            return (200, job);
        }

        match (method.as_str(), is_job, is_secret, named) {
            ("POST", true, _, _) => {
                if let Some((code, reason)) = self.job_create_failure {
                    return (
                        code,
                        Self::status_body(code, reason, "injected Job create failure"),
                    );
                }
                let mut job: Value = body.expect("Job create carries a body");
                let name = job
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .expect("task-run Job is created with an explicit name")
                    .to_string();
                let mut state = self.state.lock().unwrap();
                if state.jobs.contains_key(&name) {
                    return (
                        409,
                        Self::status_body(
                            409,
                            "AlreadyExists",
                            &format!("jobs.batch \"{name}\" already exists"),
                        ),
                    );
                }
                job["metadata"]["uid"] = Value::String(self.next_uid());
                state.jobs.insert(name.clone(), job.clone());
                self.reconcile_job_controller(&mut state, &name);
                (201, job)
            }
            ("GET", true, _, true) => match self.state.lock().unwrap().jobs.get(&trailing) {
                Some(job) => (200, job.clone()),
                None => (
                    404,
                    Self::status_body(
                        404,
                        "NotFound",
                        &format!("jobs.batch \"{trailing}\" not found"),
                    ),
                ),
            },
            ("POST", _, true, _) => {
                let mut secret: Value = body.expect("Secret create carries a body");
                let name = secret
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .expect("task-run Secret is created with an explicit name")
                    .to_string();
                let mut state = self.state.lock().unwrap();
                if state.secrets.contains_key(&name) {
                    return (
                        409,
                        Self::status_body(
                            409,
                            "AlreadyExists",
                            &format!("secrets \"{name}\" already exists"),
                        ),
                    );
                }
                secret["metadata"]["uid"] = Value::String(self.next_uid());
                state.secrets.insert(name, secret.clone());
                (201, secret)
            }
            ("GET", _, true, true) => match self.state.lock().unwrap().secrets.get(&trailing) {
                Some(secret) => (200, secret.clone()),
                None => (
                    404,
                    Self::status_body(
                        404,
                        "NotFound",
                        &format!("secrets \"{trailing}\" not found"),
                    ),
                ),
            },
            ("PATCH", _, true, true) => {
                let mut state = self.state.lock().unwrap();
                match state.secrets.get_mut(&trailing) {
                    Some(secret) => {
                        if let Some(patch) = body
                            && let Some(owners) = patch.pointer("/metadata/ownerReferences")
                        {
                            secret["metadata"]["ownerReferences"] = owners.clone();
                        }
                        (200, secret.clone())
                    }
                    None => (
                        404,
                        Self::status_body(
                            404,
                            "NotFound",
                            &format!("secrets \"{trailing}\" not found"),
                        ),
                    ),
                }
            }
            ("DELETE", _, true, true) => {
                let mut state = self.state.lock().unwrap();
                match state.secrets.remove(&trailing) {
                    Some(secret) => (200, secret),
                    None => (
                        404,
                        Self::status_body(
                            404,
                            "NotFound",
                            &format!("secrets \"{trailing}\" not found"),
                        ),
                    ),
                }
            }
            _ => (
                404,
                Self::status_body(404, "NotFound", &format!("unhandled {method} {path}")),
            ),
        }
    }

    pub(super) fn client(self: &Arc<Self>) -> kube::Client {
        let cluster = Arc::clone(self);
        kube::Client::new(
            tower::service_fn(move |request: http::Request<Body>| {
                let cluster = Arc::clone(&cluster);
                async move {
                    let (code, body) = cluster.handle(request).await;
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(code)
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string().into_bytes()))
                            .expect("build fake apiserver response"),
                    )
                }
            }),
            "djinn",
        )
    }
}

/// Percent-decode a `application/x-www-form-urlencoded` query into its pairs.
///
/// Hand-rolled rather than pulled from a crate so the fixture has no dependency
/// the production crate does not already carry: the only values it ever sees are
/// the client's own `labelSelector`, whose `/` and `=` arrive percent-encoded.
fn parse_query(query: &str) -> HashMap<String, String> {
    fn decode(raw: &str) -> String {
        let bytes = raw.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                },
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                byte => {
                    out.push(byte);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

/// Drain a `kube::client::Body` into JSON (empty bodies become `None`).
async fn axum_read_body(request: http::Request<Body>) -> Option<Value> {
    use http_body_util::BodyExt;
    let bytes = request.into_body().collect().await.ok()?.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const TASK_RUN_ID: &str = "019f72b5-a92a-7501-8b41-b0ffe68cdda5";

fn kueue_spec() -> TaskRunSpec {
    TaskRunSpec {
        task_run_id: TASK_RUN_ID.into(),
        task_attempt_id: None,
        task_id: "task-kueue-cutover".into(),
        project_id: "owner-project-id".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "task/kueue-cutover".into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: Vec::new(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

fn runtime_on(cluster: &Arc<FakeCluster>, kueue_armed: bool, db: Database) -> KubernetesRuntime {
    let mut config = KubernetesConfig::for_testing();
    config.kueue_armed = kueue_armed;
    KubernetesRuntime {
        client: cluster.client(),
        config,
        registry: Arc::new(ConnectionRegistry::new()),
        db: Some(db),
        read_source_preparation: None,
        dispatch_image_override: Some("registry/test:test".into()),
        pending: Arc::new(Mutex::new(HashMap::new())),
    }
}

// ---------------------------------------------------------------------------
// AC1 — the Pod appears only when the Job is unsuspended
// ---------------------------------------------------------------------------

/// Armed, a task-run dispatch creates a Job that produces NO Pod, and
/// unsuspending it produces exactly one.
///
/// The 0 -> 1 transition across the unsuspend is the whole assertion. Asserting
/// only "a Job was created" passes for a Job with no `suspend` key at all, which
/// is precisely the regression this guards: under Kueue an unsuspended Job runs
/// immediately, outside the ClusterQueue's quota, and the cutover has silently
/// bought nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn armed_dispatch_creates_a_suspended_job_whose_pod_appears_only_on_unsuspend() {
    let cluster = FakeCluster::new();
    let db = Database::open_in_memory().expect("test database");
    let runtime = runtime_on(&cluster, true, db);

    runtime
        .prepare(&kueue_spec(), &ResolvedCredentials::default())
        .await
        .expect("armed dispatch prepares");

    let job_name = task_run_resource_name(&TASK_RUN_ID.parse().expect("task-run uuid"));
    assert_eq!(
        cluster.job_names(),
        vec![job_name.clone()],
        "exactly one task-run Job must exist"
    );
    assert_eq!(
        cluster
            .job(&job_name)
            .and_then(|job| job.pointer("/spec/suspend").and_then(Value::as_bool)),
        Some(true),
        "armed, the task-run Job is created suspended"
    );
    assert_eq!(
        cluster.pod_count(),
        0,
        "no Pod may exist while the Job is suspended — Kueue has not admitted it yet"
    );

    cluster.unsuspend(&job_name);

    assert_eq!(
        cluster.pod_count(),
        1,
        "unsuspending the Job must produce exactly one Pod"
    );
}

/// The same dispatch disarmed renders no `suspend` key at all, so the Pod exists
/// the moment the Job does. Without this branch the test above would also pass
/// for a fake that simply never makes Pods.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disarmed_dispatch_creates_an_unsuspended_job_that_pods_immediately() {
    let cluster = FakeCluster::new();
    let db = Database::open_in_memory().expect("test database");
    let runtime = runtime_on(&cluster, false, db);

    runtime
        .prepare(&kueue_spec(), &ResolvedCredentials::default())
        .await
        .expect("disarmed dispatch prepares");

    let job_name = task_run_resource_name(&TASK_RUN_ID.parse().expect("task-run uuid"));
    assert_eq!(
        cluster
            .job(&job_name)
            .and_then(|job| job.pointer("/spec/suspend").cloned()),
        None,
        "disarmed, the Job carries no suspend key — byte-identical to pre-cutover"
    );
    assert_eq!(
        cluster.pod_count(),
        1,
        "a disarmed Job pods immediately; nothing waits on Kueue"
    );
}

// ---------------------------------------------------------------------------
// AC2 — two dispatches converge on one Job
// ---------------------------------------------------------------------------

/// Two dispatches of the same task-run id produce exactly ONE Job, and the
/// loser reports the SAME Job UID as the winner.
///
/// The UID is read back off the Secret's OwnerReference, which is where the
/// dispatch path actually uses it. That makes the assertion impossible to
/// satisfy by silently no-opping: a loser that returned "no error" without
/// adopting would either write no ownerRef at all or write one naming a UID the
/// apiserver never issued, and the Secret would outlive its Job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_dispatches_of_one_task_run_converge_on_one_job_and_one_uid() {
    let cluster = FakeCluster::new();
    let job_name = task_run_resource_name(&TASK_RUN_ID.parse().expect("task-run uuid"));

    let db_first = Database::open_in_memory().expect("test database");
    let first = runtime_on(&cluster, true, db_first);
    first
        .prepare(&kueue_spec(), &ResolvedCredentials::default())
        .await
        .expect("first dispatch prepares");

    let winner_uid = cluster
        .job(&job_name)
        .and_then(|job| {
            job.pointer("/metadata/uid")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("winner Job has a UID");

    let db_second = Database::open_in_memory().expect("test database");
    let second = runtime_on(&cluster, true, db_second);
    second
        .prepare(&kueue_spec(), &ResolvedCredentials::default())
        .await
        .expect("the losing dispatch must adopt the existing Job, not fail");

    assert_eq!(
        cluster.job_names(),
        vec![job_name.clone()],
        "the second dispatch must not create a second Job"
    );

    let owner_uid = cluster
        .state
        .lock()
        .unwrap()
        .secrets
        .get(&job_name)
        .and_then(|secret| {
            secret
                .pointer("/metadata/ownerReferences/0/uid")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("the Secret carries an ownerReference naming its Job");
    assert_eq!(
        owner_uid, winner_uid,
        "the losing dispatch must report the winner's Job UID, not a fresh or absent one"
    );

    let gets: Vec<ApiCall> = cluster
        .calls()
        .into_iter()
        .filter(|call| call.method == "GET" && call.path.ends_with(&job_name))
        .collect();
    assert_eq!(
        gets.len(),
        1,
        "adoption must GET the existing Job exactly once; calls: {:?}",
        cluster.calls()
    );
}

// ---------------------------------------------------------------------------
// AC5 — a failed create leaves no orphan Secret
// ---------------------------------------------------------------------------

/// A Job create that fails for a reason that is NOT `AlreadyExists` still runs
/// the orphan-Secret cleanup, and leaves no Job behind.
///
/// Asserted on the fake's object store rather than on "a DELETE was issued":
/// the cleanup is a detached `tokio::spawn`, so a delete that raced the process
/// or targeted the wrong name would still show up as a call while the Secret
/// stayed. 403 is used deliberately — a definitive rejection, not the 409 the
/// adopt path is allowed to swallow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_create_deletes_the_orphan_secret_and_leaves_no_job() {
    let cluster = FakeCluster::failing_job_create(403, "Forbidden");
    let db = Database::open_in_memory().expect("test database");
    let runtime = runtime_on(&cluster, true, db);

    let error = runtime
        .prepare(&kueue_spec(), &ResolvedCredentials::default())
        .await
        .expect_err("a 403 on Job create must fail the dispatch");
    assert!(
        format!("{error}").contains("create job"),
        "the error must name the failed create; got {error}"
    );

    assert!(
        cluster.job_names().is_empty(),
        "a rejected create leaves no Job"
    );

    // The cleanup is spawned; give it a bounded window to land.
    for _ in 0..200 {
        if cluster.secret_names().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        cluster.secret_names().is_empty(),
        "the orphan Secret must be deleted after a failed Job create; still present: {:?}",
        cluster.secret_names()
    );
}
