use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{
    AttemptDraftPrResult, CloseAttemptDraftPrResult, CreateAttemptDraftPrParams,
    ExpectedAbsentRefResult, ExpectedOldShaRefUpdateResult, GitHubApiClient, PullRequest,
};

use super::seed_installation_token;

const REF: &str = "refs/heads/proposal/p1/a1";
const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn client(server: &MockServer) -> GitHubApiClient {
    GitHubApiClient::for_installation_with_base_url(seed_installation_token(), server.uri())
}

fn ref_response(sha: &str) -> serde_json::Value {
    serde_json::json!({"object": {"sha": sha}})
}

fn attempt_params() -> CreateAttemptDraftPrParams {
    CreateAttemptDraftPrParams {
        title: "Attempt p1".into(),
        body: "attempt body".into(),
        head: "proposal/p1/a1".into(),
        expected_head_sha: NEW.into(),
    }
}

fn pr(number: u64, sha: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number, "title": "Attempt p1", "state": "open", "merged": false,
        "html_url": format!("https://example.test/pull/{number}"),
        "head": {"ref": "proposal/p1/a1", "sha": sha},
        "base": {"ref": "main", "sha": OLD}, "auto_merge": null,
        "node_id": format!("PR_{number}"), "draft": true
    })
}

async fn mount_attempt_list(server: &MockServer, response: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "djinnos:proposal/p1/a1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_absent_ref_creates_then_adopts_exactly_on_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/git/refs"))
        .and(body_json(serde_json::json!({"ref": REF, "sha": OLD})))
        .respond_with(ResponseTemplate::new(201))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/git/refs"))
        .respond_with(ResponseTemplate::new(422).set_body_string("already exists"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/djinnos/server/git/ref/refs/heads/proposal/p1/a1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ref_response(OLD)))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    assert!(matches!(
        client
            .create_ref_expected_absent("djinnos", "server", REF, OLD)
            .await,
        ExpectedAbsentRefResult::Created
    ));
    assert!(
        matches!(client.create_ref_expected_absent("djinnos", "server", REF, OLD).await, ExpectedAbsentRefResult::AdoptedExact { sha } if sha == OLD)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_absent_ref_reports_different_existing_sha_as_identity_mismatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/git/refs"))
        .respond_with(ResponseTemplate::new(422).set_body_string("already exists"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/djinnos/server/git/ref/refs/heads/proposal/p1/a1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ref_response(NEW)))
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).create_ref_expected_absent("djinnos", "server", REF, OLD).await,
        ExpectedAbsentRefResult::BranchIdentityMismatch { observed_sha } if observed_sha == NEW)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_old_ref_update_has_non_force_success_stale_and_provider_failure_contracts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/djinnos/server/git/ref/refs/heads/proposal/p1/a1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ref_response(OLD)))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(
            "/repos/djinnos/server/git/refs/refs/heads/proposal/p1/a1",
        ))
        .and(body_json(serde_json::json!({"sha": NEW, "force": false})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).update_ref_expected_old_sha("djinnos", "server", REF, OLD, NEW).await,
        ExpectedOldShaRefUpdateResult::Updated { sha } if sha == NEW)
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/djinnos/server/git/ref/refs/heads/proposal/p1/a1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ref_response(NEW)))
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).update_ref_expected_old_sha("djinnos", "server", REF, OLD, NEW).await,
        ExpectedOldShaRefUpdateResult::StaleObservedHead { observed_sha: Some(sha) } if sha == NEW)
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/repos/djinnos/server/git/ref/refs/heads/proposal/p1/a1",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ref_response(OLD)))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path(
            "/repos/djinnos/server/git/refs/refs/heads/proposal/p1/a1",
        ))
        .and(body_json(serde_json::json!({"sha": NEW, "force": false})))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).update_ref_expected_old_sha("djinnos", "server", REF, OLD, NEW).await,
        ExpectedOldShaRefUpdateResult::ProviderFailure(error) if error.status == Some(reqwest::StatusCode::FORBIDDEN))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_draft_pr_creates_or_adopts_only_one_exact_candidate() {
    let server = MockServer::start().await;
    mount_attempt_list(&server, serde_json::json!([])).await;
    Mock::given(method("POST")).and(path("/repos/djinnos/server/pulls"))
        .and(body_json(serde_json::json!({"title":"Attempt p1","body":"attempt body","head":"proposal/p1/a1","base":"main","maintainer_can_modify":false,"draft":true})))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr(7, NEW))).mount(&server).await;
    assert!(
        matches!(client(&server).create_or_adopt_attempt_draft_pr("djinnos", "server", attempt_params()).await,
        AttemptDraftPrResult::Created(created) if created.number == 7)
    );
    let server = MockServer::start().await;
    mount_attempt_list(&server, serde_json::json!([pr(8, NEW)])).await;
    assert!(
        matches!(client(&server).create_or_adopt_attempt_draft_pr("djinnos", "server", attempt_params()).await,
        AttemptDraftPrResult::AdoptedExact(adopted) if adopted.number == 8)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_draft_pr_race_adopts_one_exact_candidate_and_rejects_wrong_identity() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_attempt_list(&server, serde_json::json!([pr(9, NEW)])).await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_string("already exists"))
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).create_or_adopt_attempt_draft_pr("djinnos", "server", attempt_params()).await,
        AttemptDraftPrResult::AdoptedExact(adopted) if adopted.number == 9)
    );

    let server = MockServer::start().await;
    mount_attempt_list(&server, serde_json::json!([pr(10, OLD)])).await;
    assert!(
        matches!(client(&server).create_or_adopt_attempt_draft_pr("djinnos", "server", attempt_params()).await,
        AttemptDraftPrResult::ProposalPrIdentityMismatch { candidates } if candidates.len() == 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attempt_draft_pr_race_rejects_multiple_candidates() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_attempt_list(&server, serde_json::json!([pr(9, NEW), pr(10, NEW)])).await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_string("already exists"))
        .mount(&server)
        .await;
    assert!(
        matches!(client(&server).create_or_adopt_attempt_draft_pr("djinnos", "server", attempt_params()).await,
        AttemptDraftPrResult::ProposalPrIdentityMismatch { candidates } if candidates.len() == 2)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_attempt_pr_comments_with_stop_reason_then_closes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/djinnos/server/issues/12/comments"))
        .and(body_json(
            serde_json::json!({"body":"build_attempt_stopped: replaced"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let mut closed = pr(12, NEW);
    closed["state"] = serde_json::json!("closed");
    Mock::given(method("PATCH"))
        .and(path("/repos/djinnos/server/pulls/12"))
        .and(body_json(serde_json::json!({"state":"closed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(closed))
        .expect(1)
        .mount(&server)
        .await;
    let attempt: PullRequest = serde_json::from_value(pr(12, NEW)).unwrap();
    assert!(
        matches!(client(&server).close_attempt_draft_pr("djinnos", "server", &attempt, "replaced").await,
        CloseAttemptDraftPrResult::Closed(closed) if closed.number == 12)
    );
}
