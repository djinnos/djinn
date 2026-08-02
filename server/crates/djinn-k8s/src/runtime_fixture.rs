//! A [`KubernetesRuntime`] bound to an in-process apiserver that records every
//! request it is given.
//!
//! # Why this exists
//!
//! `ResizeRollout::production` composes `KubernetesTaskRunPodPlane`, which is
//! built from a `KubernetesRuntime`. Every *other* `KubernetesRuntime`
//! constructor resolves through `kube::Client::try_default()` — the ambient
//! kubeconfig — so a test that wanted to drive the production composition either
//! touched whatever cluster the developer's context happens to point at, or
//! stopped short of the production composition and drove a stand-in instead.
//! That second option is how this epic shipped eight pieces of merged, green,
//! unreachable work.
//!
//! [`KubernetesRuntime::from_client`] is the seam that makes neither necessary,
//! and this module supplies the client. The transport is a `tower` service, so
//! the Kubernetes half of the stack under test — `Api::namespaced`, the URL the
//! lister builds, the deserialization of the response — is entirely real.
//!
//! # What the recorder is FOR
//!
//! "No Pod was created" is a claim about the wire, not about an intent field. A
//! test asserts it by reading [`RecordedApiserver::mutations`] and finding it
//! empty: every non-`GET` this fixture observes is recorded with its method and
//! path *before* the fixture refuses it, so a driver that tried to create a Pod
//! and got a `403` is still visible as an attempt.

use std::sync::{Arc, Mutex};

use djinn_supervisor::ConnectionRegistry;

use crate::config::KubernetesConfig;
use crate::runtime::KubernetesRuntime;

/// One request the in-process apiserver observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    /// `GET`, `POST`, `PATCH`, `DELETE`, …
    pub method: String,
    /// The request path, without the query string.
    pub path: String,
    /// The request body, as UTF-8 (lossy).
    pub body: String,
}

/// Everything the in-process apiserver was asked to do, in issue order.
#[derive(Clone)]
pub struct RecordedApiserver(Arc<Mutex<Vec<RecordedRequest>>>);

impl RecordedApiserver {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    /// Every request, in issue order.
    #[must_use]
    pub fn all(&self) -> Vec<RecordedRequest> {
        self.0.lock().expect("recorder poisoned").clone()
    }

    /// Every request that was not a read.
    ///
    /// A cutover that refuses must leave this empty: it is the assertion that
    /// nothing was created, patched or deleted, made against the wire rather
    /// than against a counter the code under test increments itself.
    #[must_use]
    pub fn mutations(&self) -> Vec<RecordedRequest> {
        self.all()
            .into_iter()
            .filter(|request| request.method != "GET")
            .collect()
    }

    /// Every attempt to CREATE a workload — a `POST` to a Pod or Job
    /// collection.
    #[must_use]
    pub fn workload_creations(&self) -> Vec<RecordedRequest> {
        self.all()
            .into_iter()
            .filter(|request| {
                request.method == "POST"
                    && (request.path.ends_with("/pods") || request.path.ends_with("/jobs"))
            })
            .collect()
    }
}

impl Default for RecordedApiserver {
    fn default() -> Self {
        Self::new()
    }
}

/// A runtime over an apiserver that holds **no** task-run Jobs and **no** Pods.
///
/// Reads are answered with genuinely empty typed lists, so
/// `KubernetesRuntime::list_taskrun_jobs` and the resize surface's Pod reads
/// both succeed and both return nothing. Writes are recorded and then refused
/// with `403`, because nothing in a cutover is allowed to create a workload and
/// a fixture that quietly accepted one would hide exactly that.
#[must_use]
pub fn empty_task_run_cluster(namespace: &str) -> (Arc<KubernetesRuntime>, RecordedApiserver) {
    let mut config = KubernetesConfig::from_env();
    config.namespace = namespace.to_owned();
    let recorder = RecordedApiserver(Arc::new(Mutex::new(Vec::new())));
    let client = recording_client(&recorder, namespace);
    (
        Arc::new(KubernetesRuntime::from_client(
            client,
            config,
            Arc::new(ConnectionRegistry::new()),
        )),
        recorder,
    )
}

/// Build a client whose every request lands in `recorder`.
pub fn recording_client(recorder: &RecordedApiserver, namespace: &str) -> kube::Client {
    use http::Response;
    use http_body_util::BodyExt as _;
    use kube::client::Body;
    use tower::service_fn;

    let captured = recorder.clone();
    kube::Client::new(
        service_fn(move |request: http::Request<Body>| {
            let captured = captured.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_string();
                let body = request
                    .into_body()
                    .collect()
                    .await
                    .expect("collect kube request")
                    .to_bytes();
                captured
                    .0
                    .lock()
                    .expect("recorder poisoned")
                    .push(RecordedRequest {
                        method: method.clone(),
                        path: path.clone(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });

                let (status, payload) = if method != "GET" {
                    (
                        403,
                        serde_json::json!({
                            "kind": "Status",
                            "apiVersion": "v1",
                            "status": "Failure",
                            "message": format!("{method} {path} is forbidden in this fixture"),
                            "reason": "Forbidden",
                            "code": 403,
                        }),
                    )
                } else if path.contains("/jobs") {
                    (
                        200,
                        serde_json::json!({
                            "apiVersion": "batch/v1",
                            "kind": "JobList",
                            "metadata": { "resourceVersion": "1" },
                            "items": [],
                        }),
                    )
                } else if path.contains("/pods") {
                    (
                        200,
                        serde_json::json!({
                            "apiVersion": "v1",
                            "kind": "PodList",
                            "metadata": { "resourceVersion": "1" },
                            "items": [],
                        }),
                    )
                } else {
                    (
                        404,
                        serde_json::json!({
                            "kind": "Status",
                            "apiVersion": "v1",
                            "status": "Failure",
                            "message": format!("{path} is not served by this fixture"),
                            "reason": "NotFound",
                            "code": 404,
                        }),
                    )
                };

                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&payload).expect("fixture payload serializes"),
                        ))
                        .expect("fixture response builds"),
                )
            }
        }),
        namespace.to_owned(),
    )
}
