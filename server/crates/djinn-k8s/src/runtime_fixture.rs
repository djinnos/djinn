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

/// Scripted capacity-controller cluster: one 12-core Node, a complete set of
/// protected Pods totalling 4200m, and an owned ClusterQueue.
#[must_use]
pub fn capacity_controller_cluster(
    namespace: &str,
    binding_resource: &str,
) -> (kube::Client, RecordedApiserver) {
    capacity_controller_cluster_with_pods(namespace, binding_resource, CapacityPods::Complete)
}

/// Old-chart queue shape with historical sentinel-looking non-binding quotas.
#[must_use]
pub fn capacity_controller_legacy_sentinel_cluster(
    namespace: &str,
    binding_resource: &str,
) -> (kube::Client, RecordedApiserver) {
    capacity_controller_cluster_fixture(
        namespace,
        binding_resource,
        CapacityPods::Complete,
        false,
        true,
    )
}

#[derive(Clone, Copy)]
pub enum CapacityPods {
    Complete,
    Empty,
    ReadFailure,
}

#[must_use]
pub fn capacity_controller_cluster_with_pods(
    namespace: &str,
    binding_resource: &str,
    pod_mode: CapacityPods,
) -> (kube::Client, RecordedApiserver) {
    capacity_controller_cluster_fixture(namespace, binding_resource, pod_mode, false, false)
}

/// Two exclusively owned flavors, plus an eligible but unmatched Node.
#[must_use]
pub fn capacity_controller_multi_flavor_cluster(
    namespace: &str,
    binding_resource: &str,
) -> (kube::Client, RecordedApiserver) {
    capacity_controller_cluster_fixture(
        namespace,
        binding_resource,
        CapacityPods::Complete,
        true,
        false,
    )
}

/// Scripted NodePool-list responses used by the explicit Karpenter source test.
#[derive(Clone, Copy)]
pub enum NodePoolFixture {
    Valid,
    Missing,
    Malformed,
    Incomplete,
    Negative,
    Overflow,
    NotFound,
    Forbidden,
}

/// Recorded NodePool source fixture with stale cpu, memory, and pods quotas.
#[must_use]
pub fn capacity_controller_nodepool_cluster(
    namespace: &str,
    fixture: NodePoolFixture,
) -> (
    kube::Client,
    RecordedApiserver,
    Arc<Mutex<serde_json::Value>>,
) {
    use http::Response;
    use http_body_util::BodyExt as _;
    use kube::client::Body;
    use tower::service_fn;

    let recorder = RecordedApiserver::new();
    let captured = recorder.clone();
    let live = Arc::new(Mutex::new(nodepool_queue(fixture)));
    let captured_live = live.clone();
    let client = kube::Client::new(
        service_fn(move |request: http::Request<Body>| {
            let captured = captured.clone();
            let captured_live = captured_live.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_string();
                let body = request.into_body().collect().await.unwrap().to_bytes();
                captured.0.lock().unwrap().push(RecordedRequest {
                    method: method.clone(),
                    path: path.clone(),
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
                let (status, payload) = if method == "PATCH" {
                    let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let mut live = captured_live.lock().unwrap();
                    for op in patch.as_array().unwrap() {
                        if op["op"] == "replace" {
                            *live.pointer_mut(op["path"].as_str().unwrap()).unwrap() =
                                op["value"].clone();
                        }
                    }
                    (200, live.clone())
                } else if path == "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue" {
                    (200, captured_live.lock().unwrap().clone())
                } else if path == "/apis/karpenter.sh/v1/nodepools" {
                    let limits = match fixture {
                        NodePoolFixture::Valid => {
                            serde_json::json!({"cpu":"12","memory":"16Gi","pods":"42"})
                        }
                        NodePoolFixture::Missing => serde_json::json!({}),
                        NodePoolFixture::Malformed => {
                            serde_json::json!({"cpu":"bogus","memory":"16Gi","pods":"42"})
                        }
                        NodePoolFixture::Incomplete => {
                            serde_json::json!({"cpu":"12","memory":"16Gi"})
                        }
                        NodePoolFixture::Negative => {
                            serde_json::json!({"cpu":"-1","memory":"16Gi","pods":"42"})
                        }
                        NodePoolFixture::Overflow => {
                            serde_json::json!({"cpu":"9223372036854775807","memory":"16Gi","pods":"42"})
                        }
                        NodePoolFixture::NotFound | NodePoolFixture::Forbidden => {
                            serde_json::json!({})
                        }
                    };
                    let status = match fixture {
                        NodePoolFixture::NotFound => 404,
                        NodePoolFixture::Forbidden => 403,
                        _ => 200,
                    };
                    let payload = if status == 200 {
                        serde_json::json!({"apiVersion":"karpenter.sh/v1","kind":"NodePoolList","metadata":{"resourceVersion":"1"},"items":[{"apiVersion":"karpenter.sh/v1","kind":"NodePool","metadata":{"name":"dedicated","labels":{"cohort":"wrong"}},"spec":{"template":{"metadata":{"labels":{"cohort":"right"}}},"limits":limits}}]})
                    } else {
                        serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":if status == 404 {"NotFound"} else {"Forbidden"},"code":status})
                    };
                    (status, payload)
                } else {
                    (
                        404,
                        serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}),
                    )
                };
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                        .unwrap(),
                )
            }
        }),
        namespace.to_owned(),
    );
    (client, recorder, live)
}

fn nodepool_queue(_fixture: NodePoolFixture) -> serde_json::Value {
    serde_json::json!({"apiVersion":"kueue.x-k8s.io/v1beta1","kind":"ClusterQueue","metadata":{"name":"djinn-kueue","resourceVersion":"nodepool-rv","labels":{"djinn.io/quota-owner":"derived-capacity"}},"spec":{"resourceGroups":[{"flavors":[{"name":"pool","resources":[{"name":"pods","nominalQuota":"1"},{"name":"cpu","nominalQuota":"1m"},{"name":"memory","nominalQuota":"1"}]}]}]}})
}

fn capacity_controller_cluster_fixture(
    namespace: &str,
    binding_resource: &str,
    pod_mode: CapacityPods,
    multi_flavor: bool,
    sentinel_quotas: bool,
) -> (kube::Client, RecordedApiserver) {
    use http::Response;
    use http_body_util::BodyExt as _;
    use kube::client::Body;
    use tower::service_fn;

    let recorder = RecordedApiserver::new();
    let captured = recorder.clone();
    let binding = binding_resource.to_owned();
    let client = kube::Client::new(
        service_fn(move |request: http::Request<Body>| {
            let captured = captured.clone();
            let binding = binding.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_string();
                let uri_parameters = request
                    .uri()
                    .path_and_query()
                    .and_then(|value| value.as_str().split_once('?').map(|(_, tail)| tail))
                    .unwrap_or_default()
                    .to_string();
                let body = request.into_body().collect().await.unwrap().to_bytes();
                captured.0.lock().unwrap().push(RecordedRequest {
                    method: method.clone(),
                    path: path.clone(),
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
                let payload = if method == "PATCH" {
                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":403})
                } else if path == "/api/v1/nodes" && uri_parameters.contains("bad") {
                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Invalid","code":422})
                } else if path == "/api/v1/nodes" {
                    let items = if multi_flavor {
                        vec![
                            serde_json::json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker-a","labels":{"djinn.io/capacity-pool":"a","djinn.io/eligible":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":{"cpu":"12","memory":"48Gi","pods":"110"}}}),
                            serde_json::json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker-b","labels":{"djinn.io/capacity-pool":"b","djinn.io/eligible":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":{"cpu":"8","memory":"32Gi","pods":"70"}}}),
                            serde_json::json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker-unmatched","labels":{"djinn.io/capacity-pool":"other","djinn.io/eligible":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":{"cpu":"100","memory":"400Gi","pods":"500"}}}),
                        ]
                    } else {
                        vec![
                            serde_json::json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker-1","labels":{"kubernetes.io/hostname":"worker-1"}},"status":{"conditions":[{"type":"Ready","status":"True"}],"allocatable":{"cpu":"12","memory":"48Gi","pods":"110"}}}),
                        ]
                    };
                    serde_json::json!({"apiVersion":"v1","kind":"NodeList","metadata":{"resourceVersion":"1"},"items":items})
                } else if path == "/api/v1/pods" {
                    if matches!(pod_mode, CapacityPods::ReadFailure) {
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"InternalError","code":500})
                    } else {
                        let requests = if matches!(pod_mode, CapacityPods::Empty) {
                            Vec::new()
                        } else {
                            [1000, 1000, 1000, 700, 500, 90_000].into_iter().enumerate().filter(|(index, _)| multi_flavor || *index < 5).map(|(index, cpu)| {
                                let node = if multi_flavor && index == 5 { "worker-unmatched" } else if multi_flavor && index < 3 { "worker-a" } else if multi_flavor { "worker-b" } else { "worker-1" };
                                serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":format!("protected-{index}-{cpu}"),"labels":{"djinn.io/capacity-reserved":"true"}},"spec":{"nodeName":node,"containers":[{"name":"main","resources":{"requests":{"cpu":format!("{cpu}m"),"memory":"1Mi"}}}]}})
                            }).collect()
                        };
                        serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"1"},"items":requests})
                    }
                } else if path == "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue" {
                    let flavors = if multi_flavor {
                        vec![
                            serde_json::json!({"name":"a","resources":[{"name":"pods","nominalQuota":"3"},{"name":"cpu","nominalQuota":"3000m"},{"name":"memory","nominalQuota":"100Gi"}]}),
                            serde_json::json!({"name":"b","resources":[{"name":"pods","nominalQuota":"3"},{"name":"cpu","nominalQuota":"3000m"},{"name":"memory","nominalQuota":"100Gi"}]}),
                        ]
                    } else {
                        vec![
                            serde_json::json!({"name":"default","resources":[{"name":"pods","nominalQuota":if sentinel_quotas {"10k"} else {"3"}},{"name":"cpu","nominalQuota":if sentinel_quotas {"10000"} else {"3000m"}},{"name":"memory","nominalQuota":if sentinel_quotas {"100Ti"} else {"100Gi"}}]}),
                        ]
                    };
                    serde_json::json!({"apiVersion":"kueue.x-k8s.io/v1beta1","kind":"ClusterQueue","metadata":{"name":"djinn-kueue","resourceVersion":"42","labels":{"djinn.io/quota-owner":"derived-capacity"},"annotations":{"djinn.io/binding-resource":binding}},"spec":{"resourceGroups":[{"flavors":flavors}]}})
                } else if path == "/apis/karpenter.sh/v1/nodepools" {
                    serde_json::json!({"apiVersion":"karpenter.sh/v1","kind":"NodePoolList","metadata":{"resourceVersion":"1"},"items":[]})
                } else {
                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404})
                };
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(if method == "PATCH" {
                            403
                        } else if (path == "/api/v1/nodes" && uri_parameters.contains("bad"))
                            || (path == "/api/v1/pods"
                                && matches!(pod_mode, CapacityPods::ReadFailure))
                        {
                            422
                        } else if path == "/api/v1/nodes"
                            || path == "/api/v1/pods"
                            || path == "/apis/kueue.x-k8s.io/v1beta1/clusterqueues/djinn-kueue"
                            || path == "/apis/karpenter.sh/v1/nodepools"
                        {
                            200
                        } else {
                            404
                        })
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                        .unwrap(),
                )
            }
        }),
        namespace.to_owned(),
    );
    (client, recorder)
}
