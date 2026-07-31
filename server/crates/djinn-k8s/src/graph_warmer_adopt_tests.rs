//! Create-then-observe for the shared warm/SCIP dispatcher (task `37yq`).
//!
//! [`KubeClientDispatcher`] is the ONE production create site for both the
//! graph-warm Job and the standalone SCIP-index Job (`scip_schedule.rs` and
//! `graph_warmer.rs` both dispatch through the `WarmJobDispatcher` trait), so
//! its 409 handling is two of the cutover's three create sites at once. Both
//! Job names are deterministic in their work identity, which is what makes
//! adoption the correct response rather than a guess.
//!
//! These run against `tower_test::mock`, the same seam `runtime.rs` uses for
//! its apiserver-shaped tests.

use super::*;
use http::Response;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::client::Body;
use tower_test::mock::{Handle, Mock};

type Req = http::Request<Body>;
type Resp = http::Response<Body>;

const JOB_NAME: &str = "djinn-warm-proj-g1-0123456789abcdef";

fn named_job() -> Job {
    Job {
        metadata: ObjectMeta {
            name: Some(JOB_NAME.to_string()),
            namespace: Some("djinn".to_string()),
            ..ObjectMeta::default()
        },
        ..Job::default()
    }
}

fn json_response(code: u16, body: serde_json::Value) -> Resp {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string().into_bytes()))
        .expect("build response")
}

fn already_exists_status() -> serde_json::Value {
    serde_json::json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": format!("jobs.batch \"{JOB_NAME}\" already exists"),
        "reason": "AlreadyExists",
        "code": 409,
    })
}

/// A 409 on create is followed by a GET of the SAME name, and the dispatcher
/// returns that object's name instead of an error.
///
/// The assertion on the GET's request path is what makes this non-vacuous: a
/// dispatcher that swallowed the 409 and returned the name it had locally would
/// satisfy "returns Ok" while never confirming the object exists — and would
/// report success for a name some other work identity owns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conflicting_create_adopts_the_existing_job_by_get() {
    let (mock_service, mut handle): (Mock<Req, Resp>, Handle<Req, Resp>) = tower_test::mock::pair();
    let dispatcher = KubeClientDispatcher::new(kube::Client::new(mock_service, "djinn"));

    let server = tokio::spawn(async move {
        let (create, send) = handle.next_request().await.expect("create request");
        assert_eq!(create.method(), http::Method::POST);
        send.send_response(json_response(409, already_exists_status()));

        let (get, send) = handle.next_request().await.expect("adopting GET request");
        assert_eq!(get.method(), http::Method::GET, "adoption must GET");
        assert!(
            get.uri().path().ends_with(JOB_NAME),
            "adoption must GET the conflicting name, got {}",
            get.uri().path()
        );
        send.send_response(json_response(
            200,
            serde_json::json!({
                "apiVersion": "batch/v1",
                "kind": "Job",
                "metadata": {"name": JOB_NAME, "namespace": "djinn", "uid": "existing-uid"},
            }),
        ));
    });

    let dispatched = dispatcher.dispatch("djinn", named_job()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("mock apiserver completes")
        .expect("mock apiserver task does not panic");

    assert_eq!(
        dispatched,
        Ok(JOB_NAME.to_string()),
        "a 409 must adopt the existing Job, not fail the dispatch"
    );
}

/// A NON-409 rejection still fails. The contract is "AlreadyExists is
/// adoptable", not "errors are ignored" — a 403 that silently returned a name
/// would make the warmer believe a Job exists that never will, and nothing
/// would ever re-dispatch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbidden_create_is_not_adopted() {
    let (mock_service, mut handle): (Mock<Req, Resp>, Handle<Req, Resp>) = tower_test::mock::pair();
    let dispatcher = KubeClientDispatcher::new(kube::Client::new(mock_service, "djinn"));

    let server = tokio::spawn(async move {
        let (create, send) = handle.next_request().await.expect("create request");
        assert_eq!(create.method(), http::Method::POST);
        send.send_response(json_response(
            403,
            serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "metadata": {},
                "status": "Failure",
                "message": "forbidden: User cannot create jobs.batch",
                "reason": "Forbidden",
                "code": 403,
            }),
        ));
        // No second request may arrive; dropping the handle proves it.
    });

    let dispatched = dispatcher.dispatch("djinn", named_job()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .expect("mock apiserver completes")
        .expect("mock apiserver task does not panic");

    assert!(
        dispatched.is_err(),
        "a 403 must surface as a dispatch failure, got {dispatched:?}"
    );
}

/// The 409 classifier reads the structured `reason`, not the message. The
/// apiserver also answers 409 for an optimistic-concurrency `Conflict` on
/// update, and adopting on that would swallow a genuine write conflict.
#[test]
fn only_already_exists_counts_as_a_conflict_to_adopt() {
    let response = |code: u16, reason: &str| {
        kube::Error::Api(kube::core::ErrorResponse {
            status: "Failure".into(),
            message: "…".into(),
            reason: reason.into(),
            code,
        })
    };

    assert!(api_error_is_already_exists(&response(409, "AlreadyExists")));
    assert!(
        !api_error_is_already_exists(&response(409, "Conflict")),
        "a resourceVersion conflict is not an existing object to adopt"
    );
    assert!(!api_error_is_already_exists(&response(403, "Forbidden")));
    assert!(!api_error_is_already_exists(&response(404, "NotFound")));
}
