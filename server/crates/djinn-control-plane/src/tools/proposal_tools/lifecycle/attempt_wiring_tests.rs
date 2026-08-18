//! Regressions for the production attempt-lifecycle call sites (`ct0a`).
//!
//! NAMED FAILING MUTATIONS.
//! (a) Delete `self.start_proposal_build_attempt(&proposal)` from
//!     `proposal_graduate`: nothing else in the tree reserves a build attempt,
//!     so `graduation_reserves_the_attempt_branch_and_opens_one_draft_attempt_pr`
//!     finds no `proposal_build_attempts` row and no forge request.
//! (b) Delete `self.stop_proposal_build_attempt(proposal, reason)` from
//!     `abort_proposal_build`: nothing else closes an attempt PR or writes a
//!     retirement tag, so
//!     `abort_closes_the_attempt_pr_and_retires_the_branch_as_a_tag` fails on
//!     the recorded requests and on the retained attempt's lifecycle.
//! (c) Drop the epoch probe in `proposal_attempt_lifecycle`: the disabled
//!     deployment starts issuing forge requests during graduation and
//!     `a_disabled_epoch_keeps_graduation_off_the_forge_entirely` fails.

use super::attempt_wiring::set_attempt_client_base_url_for_test;
use crate::server::DjinnMcpServer;
use crate::state::stubs::test_mcp_state;
use djinn_core::events::EventBus;
use djinn_core::models::{DirectDeliveryParkReason, ProposalBuildAttemptLifecycle};
use djinn_db::{
    Database, ProjectRepository, ProposalBuildAttemptRepository, ProposalCreateInput,
    ProposalRepository, UserRepository,
    test_support::{
        activate_direct_delivery_epoch_for_test, persist_project_github_installation_for_test,
        proposal_build_attempt_ids_for_test,
    },
};
use djinn_provider::github_app::installations::prime_cache_for_tests;
use std::sync::atomic::{AtomicI64, Ordering};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex, query_param},
};

/// The attempt-client base URL is process-global, so these tests take turns.
static ATTEMPT_CLIENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static NEXT_GITHUB_ID: AtomicI64 = AtomicI64::new(9_610_000_000);

const MAIN_SHA: &str = "1111111111111111111111111111111111111111";
/// A freshly created attempt branch points at the exact `main` head it forked
/// from; nothing has appended to it yet.
const HEAD_SHA: &str = MAIN_SHA;
const OWNER: &str = "acme";
const REPO: &str = "widget";
const INSTALLATION_ID: u64 = 8_242;

/// A well-formed body that passes every deterministic readiness check.
fn ready_body() -> &'static str {
    r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A
- Deliver B

## File map
```file-map
    src/main.rs
    src/lib.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#
}

struct Fixture {
    server: DjinnMcpServer,
    db: Database,
    user_id: String,
    proposal_id: String,
    proposal_short_id: String,
}

/// An approved, readiness-passing proposal whose primary target project
/// carries a real GitHub App installation identity.
async fn approved_proposal(slug: &str) -> Fixture {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let github_id = NEXT_GITHUB_ID.fetch_add(1, Ordering::Relaxed);
    let users = UserRepository::new(db.clone());
    let user = users
        .upsert_from_github(
            github_id,
            &format!("attempt-wiring-{github_id}"),
            None,
            None,
        )
        .await
        .unwrap();
    users.set_role(&user.id, "engineer").await.unwrap();

    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create(slug, "test", slug)
        .await
        .unwrap();
    persist_project_github_installation_for_test(&db, &project.id, OWNER, REPO, INSTALLATION_ID)
        .await;
    prime_cache_for_tests(INSTALLATION_ID, "ghs_attempt_wiring_fixture");

    let proposals = ProposalRepository::new(db.clone(), EventBus::noop());
    let proposal = djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user.id.clone()), async {
            proposals
                .create(ProposalCreateInput {
                    title: "Attempt Wiring",
                    body: ready_body(),
                    acceptance_criteria: Some(r#"[{"criterion":"API returns 200","met":false}]"#),
                    status: None,
                    body_format: None,
                })
                .await
        })
        .await
        .unwrap();
    proposals
        .add_target(&proposal.id, &project.id, "primary")
        .await
        .unwrap();
    proposals
        .set_status(&proposal.id, "approved")
        .await
        .unwrap();

    Fixture {
        server: DjinnMcpServer::new(test_mcp_state(db.clone())),
        db,
        user_id: user.id,
        proposal_id: proposal.id,
        proposal_short_id: proposal.short_id,
    }
}

/// Every forge response `ProposalAttemptLifecycle::start` needs.
async fn mount_start(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/ref/heads/main")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":MAIN_SHA}})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/repos/{OWNER}/{REPO}/git/ref/heads/proposal/"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":HEAD_SHA}})),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/refs")))
        .respond_with(ResponseTemplate::new(201))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
}

fn attempt_pr(number: u64, branch: &str, state: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": "attempt",
        "state": state,
        "merged": false,
        "html_url": format!("https://github.test/{OWNER}/{REPO}/pull/{number}"),
        "head": {"ref": branch, "sha": HEAD_SHA},
        "base": {"ref": "main", "sha": MAIN_SHA},
        "auto_merge": null,
        "node_id": format!("PR_{number}"),
        "draft": true,
    })
}

/// The attempt branch is only known after the reservation, so the created PR
/// echoes whatever head the caller asked for.
async fn mount_attempt_pr_create(server: &MockServer, number: u64) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let head = body["head"].as_str().unwrap().to_owned();
            ResponseTemplate::new(201).set_body_json(attempt_pr(number, &head, "open"))
        })
        .mount(server)
        .await;
}

async fn graduate(fixture: &Fixture) -> serde_json::Value {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(fixture.user_id.clone()), async {
            fixture
                .server
                .dispatch_tool(
                    "proposal_graduate",
                    serde_json::json!({ "id": fixture.proposal_id }),
                )
                .await
        })
        .await
        .unwrap()
}

async fn abort(fixture: &Fixture, reason: &str) -> serde_json::Value {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(fixture.user_id.clone()), async {
            fixture
                .server
                .dispatch_tool(
                    "proposal_stop_build",
                    serde_json::json!({
                        "id": fixture.proposal_id,
                        "mode": "abort",
                        "reason": reason,
                    }),
                )
                .await
        })
        .await
        .unwrap()
}

async fn posted_refs(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| {
            request.method == wiremock::http::Method::POST
                && request.url.path() == format!("/repos/{OWNER}/{REPO}/git/refs")
        })
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            format!("{} {}", body["ref"].as_str().unwrap(), body["sha"])
        })
        .collect()
}

/// Every distinct forge operation the run reached, as `METHOD path`. Comparing
/// this against a closed list is what proves an absence: no task-PR merge,
/// auto-merge, approval, signoff or queue call can hide in an unlisted path.
async fn observed_operations(server: &MockServer) -> Vec<String> {
    let mut operations: Vec<String> = server
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .map(|request| format!("{} {}", request.method, request.url.path()))
        .collect();
    operations.sort();
    operations.dedup();
    operations
}

/// AC1 / AC3 / AC5 / AC6: graduation reserves the attempt branch from the exact
/// observed `main` SHA and opens exactly one draft attempt PR based on `main`,
/// and reaches no other forge operation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graduation_reserves_the_attempt_branch_and_opens_one_draft_attempt_pr() {
    let _serialized = ATTEMPT_CLIENT_TEST_LOCK.lock().await;
    let fixture = approved_proposal("svc-attempt-start").await;
    activate_direct_delivery_epoch_for_test(&fixture.db).await;
    let forge = MockServer::start().await;
    mount_start(&forge).await;
    mount_attempt_pr_create(&forge, 41).await;
    set_attempt_client_base_url_for_test(Some(forge.uri()));

    let response = graduate(&fixture).await;
    set_attempt_client_base_url_for_test(None);
    assert!(
        response["error"].is_null(),
        "graduation must succeed: {response:?}"
    );

    let attempt = ProposalBuildAttemptRepository::new(fixture.db.clone())
        .active_attempt(&fixture.proposal_id)
        .await
        .expect("read the active attempt")
        .expect("graduation must leave one active build attempt");
    assert_eq!(attempt.lifecycle, ProposalBuildAttemptLifecycle::Active);
    assert_eq!(
        attempt.base_sha, MAIN_SHA,
        "the attempt must fork from the exact observed main SHA"
    );
    assert_eq!(attempt.branch_head_sha.as_deref(), Some(HEAD_SHA));
    assert_eq!(attempt.proposal_pr_number, Some(41));
    assert_eq!(
        attempt.branch_name,
        format!(
            "proposal/{}/{}",
            fixture.proposal_short_id, attempt.short_id
        )
    );
    assert!(attempt.park_reason.is_none());

    assert_eq!(
        posted_refs(&forge).await,
        vec![format!("refs/heads/{} \"{MAIN_SHA}\"", attempt.branch_name)],
        "exactly one expected-absent ref create, at the observed main SHA"
    );
    let opened: Vec<serde_json::Value> = forge
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| {
            request.method == wiremock::http::Method::POST
                && request.url.path() == format!("/repos/{OWNER}/{REPO}/pulls")
        })
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(opened.len(), 1, "exactly one draft attempt PR: {opened:?}");
    assert_eq!(opened[0]["head"], attempt.branch_name);
    assert_eq!(opened[0]["base"], "main");
    assert_eq!(opened[0]["draft"], true);

    // Closed set: graduation is attempt-scoped end to end. Anything else — a
    // task-PR merge, auto-merge, approval, signoff or queue call — would appear
    // here as an extra row.
    assert_eq!(
        observed_operations(&forge).await,
        vec![
            format!("GET /repos/{OWNER}/{REPO}/git/ref/heads/main"),
            format!(
                "GET /repos/{OWNER}/{REPO}/git/ref/heads/{}",
                attempt.branch_name
            ),
            format!("GET /repos/{OWNER}/{REPO}/pulls"),
            format!("POST /repos/{OWNER}/{REPO}/git/refs"),
            format!("POST /repos/{OWNER}/{REPO}/pulls"),
        ]
    );
}

/// AC2: abort closes the unmerged draft attempt PR with `build_attempt_stopped`,
/// retires the branch as an immutable tag, and a re-graduation reserves a
/// distinct branch and PR identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_closes_the_attempt_pr_and_retires_the_branch_as_a_tag() {
    let _serialized = ATTEMPT_CLIENT_TEST_LOCK.lock().await;
    let fixture = approved_proposal("svc-attempt-stop").await;
    activate_direct_delivery_epoch_for_test(&fixture.db).await;
    let attempts = ProposalBuildAttemptRepository::new(fixture.db.clone());

    let first_forge = MockServer::start().await;
    mount_start(&first_forge).await;
    mount_attempt_pr_create(&first_forge, 51).await;
    set_attempt_client_base_url_for_test(Some(first_forge.uri()));
    assert!(graduate(&fixture).await["error"].is_null());
    let first = attempts
        .active_attempt(&fixture.proposal_id)
        .await
        .unwrap()
        .expect("first attempt");

    let stop_forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls/51")))
        .respond_with(ResponseTemplate::new(200).set_body_json(attempt_pr(
            51,
            &first.branch_name,
            "open",
        )))
        .mount(&stop_forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/issues/51/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&stop_forge)
        .await;
    Mock::given(method("PATCH"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls/51")))
        .respond_with(ResponseTemplate::new(200).set_body_json(attempt_pr(
            51,
            &first.branch_name,
            "closed",
        )))
        .expect(1)
        .mount(&stop_forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/refs")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&stop_forge)
        .await;
    set_attempt_client_base_url_for_test(Some(stop_forge.uri()));

    let aborted = abort(&fixture, "superseded").await;
    assert!(
        aborted["error"].is_null(),
        "abort must succeed: {aborted:?}"
    );

    // The attempt branch is retained as an immutable tag at its exact head.
    assert_eq!(
        posted_refs(&stop_forge).await,
        vec![format!(
            "refs/tags/{}/retired \"{HEAD_SHA}\"",
            first.branch_name
        )],
    );
    let comments: Vec<String> = stop_forge
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| {
            request.method == wiremock::http::Method::POST
                && request.url.path() == format!("/repos/{OWNER}/{REPO}/issues/51/comments")
        })
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["body"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(comments, vec!["build_attempt_stopped: superseded"]);
    // Closed set: the stop reads, comments on, closes and tags exactly one
    // attempt PR. No merge, auto-merge, approval, signoff or queue call.
    assert_eq!(
        observed_operations(&stop_forge).await,
        vec![
            // `get_pull_request` reads the head's checks alongside the PR.
            format!("GET /repos/{OWNER}/{REPO}/commits/{HEAD_SHA}/check-runs"),
            format!("GET /repos/{OWNER}/{REPO}/pulls/51"),
            format!("PATCH /repos/{OWNER}/{REPO}/pulls/51"),
            format!("POST /repos/{OWNER}/{REPO}/git/refs"),
            format!("POST /repos/{OWNER}/{REPO}/issues/51/comments"),
        ]
    );
    let retained = attempts
        .get(&first.id)
        .await
        .unwrap()
        .expect("the retired attempt is retained");
    assert_eq!(retained.lifecycle, ProposalBuildAttemptLifecycle::Retired);
    assert!(
        attempts
            .active_attempt(&fixture.proposal_id)
            .await
            .unwrap()
            .is_none()
    );

    // Re-graduation gets distinct branch and PR identities.
    let second_forge = MockServer::start().await;
    mount_start(&second_forge).await;
    mount_attempt_pr_create(&second_forge, 52).await;
    set_attempt_client_base_url_for_test(Some(second_forge.uri()));
    assert!(graduate(&fixture).await["error"].is_null());
    set_attempt_client_base_url_for_test(None);
    let second = attempts
        .active_attempt(&fixture.proposal_id)
        .await
        .unwrap()
        .expect("second attempt");
    assert_ne!(first.branch_name, second.branch_name);
    assert_ne!(first.proposal_pr_number, second.proposal_pr_number);
    assert_eq!(second.proposal_pr_number, Some(52));
}

/// AC1: an attempt branch that already exists at a foreign commit is never
/// adopted — graduation parks the attempt as `branch_identity_mismatch` and
/// never opens a PR against someone else's history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_existing_attempt_branch_parks_branch_identity_mismatch() {
    const FOREIGN_SHA: &str = "9999999999999999999999999999999999999999";
    let _serialized = ATTEMPT_CLIENT_TEST_LOCK.lock().await;
    let fixture = approved_proposal("svc-attempt-branch-mismatch").await;
    activate_direct_delivery_epoch_for_test(&fixture.db).await;
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/ref/heads/main")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":MAIN_SHA}})),
        )
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/refs")))
        .respond_with(
            ResponseTemplate::new(422).set_body_string(r#"{"message":"Reference already exists"}"#),
        )
        .mount(&forge)
        .await;
    // The ref that already exists points somewhere this attempt never chose.
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/repos/{OWNER}/{REPO}/git/ref/refs/heads/proposal/"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":FOREIGN_SHA}})),
        )
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&forge)
        .await;
    set_attempt_client_base_url_for_test(Some(forge.uri()));

    let response = graduate(&fixture).await;
    set_attempt_client_base_url_for_test(None);
    let error = response
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        error.contains("branch_identity_mismatch"),
        "graduation must refuse a foreign attempt branch: {response:?}"
    );
    let attempts = ProposalBuildAttemptRepository::new(fixture.db.clone());
    let reserved = proposal_build_attempt_ids_for_test(&fixture.db, &fixture.proposal_id).await;
    assert_eq!(reserved.len(), 1, "one reserved attempt: {reserved:?}");
    let parked = attempts
        .get(&reserved[0])
        .await
        .unwrap()
        .expect("the reserved attempt is retained");
    assert_eq!(
        parked.park_reason,
        Some(DirectDeliveryParkReason::BranchIdentityMismatch)
    );
    assert_eq!(parked.lifecycle, ProposalBuildAttemptLifecycle::Reserved);
    assert!(
        parked.branch_head_sha.is_none(),
        "a foreign head is never installed as the attempt identity"
    );
}

/// AC5: a draft attempt PR whose head is not the exact attempt head parks the
/// attempt as `proposal_pr_identity_mismatch` and refuses graduation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_attempt_pr_parks_proposal_pr_identity_mismatch() {
    let _serialized = ATTEMPT_CLIENT_TEST_LOCK.lock().await;
    let fixture = approved_proposal("svc-attempt-mismatch").await;
    activate_direct_delivery_epoch_for_test(&fixture.db).await;
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/ref/heads/main")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":MAIN_SHA}})),
        )
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/repos/{OWNER}/{REPO}/git/ref/heads/proposal/"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object":{"sha":HEAD_SHA}})),
        )
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/git/refs")))
        .respond_with(ResponseTemplate::new(201))
        .mount(&forge)
        .await;
    // An open PR on the attempt head, but at a different commit.
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .and(query_param("state", "open"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "number": 77,
                "title": "stray",
                "state": "open",
                "merged": false,
                "html_url": "https://github.test/stray/77",
                "head": {"ref": "someone-elses", "sha": "3333333333333333333333333333333333333333"},
                "base": {"ref": "main", "sha": MAIN_SHA},
                "auto_merge": null,
                "node_id": "PR_77",
                "draft": true,
            }])),
        )
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&forge)
        .await;
    set_attempt_client_base_url_for_test(Some(forge.uri()));

    let response = graduate(&fixture).await;
    set_attempt_client_base_url_for_test(None);
    let error = response
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        error.contains("proposal_pr_identity_mismatch"),
        "graduation must refuse a foreign attempt PR: {response:?}"
    );
    // The proposal never advances and the reserved attempt carries the park.
    let stored = ProposalRepository::new(fixture.db.clone(), EventBus::noop())
        .get(&fixture.proposal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, "approved");
    assert!(stored.build_breakdown_task_id.is_none());
    let attempts = ProposalBuildAttemptRepository::new(fixture.db.clone());
    assert!(
        attempts
            .active_attempt(&fixture.proposal_id)
            .await
            .unwrap()
            .is_none(),
        "a parked attempt is never activated"
    );
    let reserved = proposal_build_attempt_ids_for_test(&fixture.db, &fixture.proposal_id).await;
    assert_eq!(reserved.len(), 1, "one reserved attempt: {reserved:?}");
    let parked = attempts
        .get(&reserved[0])
        .await
        .unwrap()
        .expect("the reserved attempt is retained");
    assert_eq!(
        parked.park_reason,
        Some(DirectDeliveryParkReason::ProposalPrIdentityMismatch),
        "the mismatch must be persisted on the attempt, not only reported"
    );
    assert_eq!(parked.lifecycle, ProposalBuildAttemptLifecycle::Reserved);
    assert!(parked.proposal_pr_number.is_none());
}

/// AC6: with the shipped default epoch state, graduation and abort behave
/// exactly as before and never touch the forge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_epoch_keeps_graduation_off_the_forge_entirely() {
    let _serialized = ATTEMPT_CLIENT_TEST_LOCK.lock().await;
    let fixture = approved_proposal("svc-attempt-disabled").await;
    let forge = MockServer::start().await;
    set_attempt_client_base_url_for_test(Some(forge.uri()));

    assert!(graduate(&fixture).await["error"].is_null());
    let proposals = ProposalRepository::new(fixture.db.clone(), EventBus::noop());
    let graduated = proposals.get(&fixture.proposal_id).await.unwrap().unwrap();
    assert_eq!(graduated.status, "building");
    assert!(
        graduated.build_breakdown_task_id.is_some(),
        "graduation still does its own job with the epoch disabled"
    );
    let aborted = abort(&fixture, "not needed").await;
    set_attempt_client_base_url_for_test(None);
    assert!(
        aborted["error"].is_null(),
        "abort must succeed: {aborted:?}"
    );

    assert!(
        forge
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "a disabled epoch must not reach the forge from graduation or abort"
    );
    assert!(
        ProposalBuildAttemptRepository::new(fixture.db.clone())
            .active_attempt(&fixture.proposal_id)
            .await
            .unwrap()
            .is_none(),
        "a disabled epoch must not reserve a build attempt"
    );
    let stored = proposals.get(&fixture.proposal_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "approved", "abort reverts to approved");
}

/// AC4: every attempt-lifecycle repository and provider primitive this proposal
/// built has a production caller. The audit reads the production halves of the
/// real sources, so moving a call into a `#[cfg(test)]` module reddens it.
#[test]
fn every_attempt_lifecycle_primitive_has_a_production_caller() {
    const LIFECYCLE: &str = include_str!("../../../proposal_attempt_lifecycle.rs");
    const WIRING: &str = include_str!("attempt_wiring.rs");
    const GRADUATE: &str = include_str!("../lifecycle.rs");

    /// Everything before the file's first `#[cfg(test)]` item.
    fn production(source: &'static str) -> &'static str {
        source.split("#[cfg(test)]").next().unwrap()
    }

    for callee in [
        ".reserve(&ReserveProposalBuildAttemptInput",
        ".activate(&ActivateProposalBuildAttemptInput",
        ".acquire_lease(&AcquireProposalBuildAttemptLeaseInput",
        ".create_ref_expected_absent(",
        ".observe_exact_ref(",
        ".create_or_adopt_attempt_draft_pr(",
        ".close_attempt_draft_pr(",
    ] {
        assert!(
            production(LIFECYCLE).contains(callee),
            "{callee} has no production caller in proposal_attempt_lifecycle.rs"
        );
    }
    for callee in [
        "ProposalAttemptLifecycle::new(",
        ".start(StartAttemptInput",
        ".stop(active, reason)",
        ".active_attempt(",
    ] {
        assert!(
            production(WIRING).contains(callee),
            "{callee} has no production caller in attempt_wiring.rs"
        );
    }
    for callee in [
        "self.start_proposal_build_attempt(&proposal)",
        "self.stop_proposal_build_attempt(proposal, reason)",
    ] {
        assert!(
            production(GRADUATE).contains(callee),
            "{callee} is not called from the proposal lifecycle tools"
        );
    }
}
