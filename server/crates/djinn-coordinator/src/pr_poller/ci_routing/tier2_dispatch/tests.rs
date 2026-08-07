//! Acceptance fixtures for the Tier-2 Lead dispatch (proposal `nafu`, wave 5).
//!
//! # What these prove that nothing else could
//!
//! Waves 3b and 4 each held one end of a wire that was not connected. The
//! executor opened a Tier-2 lease and returned; the supervisor's
//! `read_arbiter_directive` looked for a `ci_route` block that **no code in the
//! workspace wrote**. So the validator's every production call answered
//! `NoRoute`, and a route that reached Tier 2 left the task in
//! `pr_draft`/`pr_review` with the legacy remediation path suppressed and no
//! remedy in its place.
//!
//! Every fixture here drives the real `CoordinatorActor` method against a real
//! ephemeral database and then reads the row back, so "the block is written" is
//! a durable fact rather than a call that happened.
//!
//! The strongest of them is
//! [`the_written_block_is_the_one_the_supervisor_parses`], which round-trips the
//! coordinator's output through the *supervisor's own required-key list*. Two
//! sides of one JSON contract in separate crates is exactly the shape that
//! drifts silently, and a mismatch there is not a compile error — it is a
//! `Malformed` at 3am that applies nothing forever.

use djinn_db::{
    CiEvidenceIdentity, CiLane, CiOriginState, CiRouteSubject, CiTier2Reason, Database,
};

use super::*;
use crate::pr_poller::ci_routing::executor::CiTier2Handoff;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PR: i64 = 4242;

struct Harness {
    actor: crate::actor::CoordinatorActor,
    db: Database,
    task_id: String,
}

async fn harness(status: &str) -> Harness {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project =
        djinn_db::test_support::make_project(&db, std::path::Path::new("ci-tier2-dispatch")).await;
    let task_id = djinn_db::test_support::seed_task_row(
        &db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project.id,
            status,
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    Harness {
        actor: crate::actor::actor_with_test_db(db.clone()),
        db,
        task_id,
    }
}

impl Harness {
    async fn task(&self) -> djinn_core::models::Task {
        djinn_db::TaskRepository::new(
            self.db.clone(),
            djinn_db::test_support::event_bus_for(&tokio::sync::broadcast::channel(4).0),
        )
        .get(&self.task_id)
        .await
        .expect("task read")
        .expect("task exists")
    }

    async fn directive(&self) -> Option<serde_json::Value> {
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(self.db.clone())
            .get_latest_for_task(&self.task_id)
            .await
            .expect("arbitration read")
            .and_then(|record| record.directive)
    }

    fn handoff(&self, origin: CiOriginState, lane: CiLane) -> CiTier2Handoff {
        CiTier2Handoff {
            subject: CiRouteSubject::task(&self.task_id),
            provider_action_key: "pak-1".to_owned(),
            tier2_lease_id: "lease-1".to_owned(),
            identity: CiEvidenceIdentity {
                lane,
                pr_number: PR,
                pr_head_sha: HEAD.to_owned(),
                run_id: Some(90210),
                run_head_sha: HEAD.to_owned(),
                dequeue_id: (lane == CiLane::MergeGroup).then(|| "dq-1".to_owned()),
            },
            origin_state: origin,
            reason: CiTier2Reason::CausalFailure,
            evidence_references: vec!["90210".to_owned(), HEAD.to_owned()],
            repository_commands: vec!["cargo test -p djinn-db".to_owned()],
        }
    }
}

/// The required keys the supervisor's `read_arbiter_directive` refuses a block
/// without.
///
/// Duplicated here **on purpose**, and kept in one list so the duplication is
/// visible: `djinn-agent` does not depend on `djinn-coordinator`, so the two
/// halves of this JSON contract cannot share a type. A key added to the reader
/// and forgotten here is caught by
/// [`the_written_block_is_the_one_the_supervisor_parses`]; the reverse is caught
/// by the agent's `every_missing_required_key_is_malformed_not_legacy`.
const SUPERVISOR_REQUIRED_KEYS: &[&str] = &[
    "lane",
    "origin_state",
    "tier2_reason",
    "subject_kind",
    "subject_id",
    "provider_action_key",
    "tier2_lease_id",
    "pr_number",
    "pr_head_sha",
    "run_id",
    "run_head_sha",
    "evidence_references",
];

/// The PR-head lane: the block lands, the lease is bound, and the task enters
/// the Lead lane.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_head_route_dispatches_lead_with_a_complete_block() {
    let h = harness("pr_draft").await;
    let handoff = h.handoff(CiOriginState::PrDraft, CiLane::PrHead);
    let task = h.task().await;

    let dispatch = h
        .actor
        .dispatch_ci_tier2_lead(&task, &handoff, Some("https://example/pr/1"))
        .await;
    assert_eq!(dispatch, CiTier2Dispatch::Dispatched);

    // The board moved into the only lane a Lead session can run from.
    assert_eq!(
        djinn_db::test_support::task_status_for_test(&h.db, &h.task_id).await,
        "needs_lead_intervention",
    );

    let directive = h.directive().await.expect("a directive was written");
    let route = directive
        .get("ci_route")
        .expect("the block the supervisor reads");
    assert_eq!(route["lane"], "pr_head");
    assert_eq!(route["origin_state"], "pr_draft");
    assert_eq!(route["provider_action_key"], "pak-1");
    assert_eq!(route["tier2_lease_id"], "lease-1");
    assert_eq!(route["run_id"], 90210);
    assert!(
        route["evidence_references"]
            .as_array()
            .is_some_and(|refs| !refs.is_empty()),
        "an empty bundle grounds nothing and the reader refuses the block"
    );
    assert_eq!(
        directive["terminal_disposition_required"], false,
        "a CI route is not a terminal-disposition arbitration"
    );
}

/// The merge-group lane carries its dequeue identity, which is part of the
/// evidence identity the guard compares.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_group_route_carries_its_dequeue_identity() {
    let h = harness("pr_review").await;
    let handoff = h.handoff(CiOriginState::PrReview, CiLane::MergeGroup);
    let task = h.task().await;
    assert_eq!(
        h.actor.dispatch_ci_tier2_lead(&task, &handoff, None).await,
        CiTier2Dispatch::Dispatched
    );
    let directive = h.directive().await.expect("directive");
    assert_eq!(directive["ci_route"]["lane"], "merge_group");
    assert_eq!(directive["ci_route"]["origin_state"], "pr_review");
    assert_eq!(directive["ci_route"]["dequeue_id"], "dq-1");
}

/// The block the coordinator writes must satisfy **every** key the supervisor
/// requires.
///
/// Without this, the two halves drift into a `Malformed` that applies nothing
/// and fails no test — the exact failure mode wave 5 exists to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_written_block_is_the_one_the_supervisor_parses() {
    let h = harness("pr_draft").await;
    let task = h.task().await;
    h.actor
        .dispatch_ci_tier2_lead(
            &task,
            &h.handoff(CiOriginState::PrDraft, CiLane::PrHead),
            None,
        )
        .await;
    let directive = h.directive().await.expect("directive");
    let route = directive["ci_route"]
        .as_object()
        .expect("ci_route is an object");
    for key in SUPERVISOR_REQUIRED_KEYS {
        let value = route
            .get(*key)
            .unwrap_or_else(|| panic!("the supervisor requires `{key}` and it was not written"));
        assert!(!value.is_null(), "`{key}` is required and must not be null");
        if let Some(text) = value.as_str() {
            assert!(!text.trim().is_empty(), "`{key}` must not be blank");
        }
    }
}

/// One Lead adjudication per hold cycle. A second dispatch for the same task
/// finds the row unconsumed and refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_dispatch_for_one_hold_cycle_is_refused() {
    let h = harness("pr_draft").await;
    let handoff = h.handoff(CiOriginState::PrDraft, CiLane::PrHead);
    let task = h.task().await;
    assert_eq!(
        h.actor.dispatch_ci_tier2_lead(&task, &handoff, None).await,
        CiTier2Dispatch::Dispatched
    );
    // A second handoff for a *different* route on the same task must not open a
    // second adjudication.
    let mut second = handoff.clone();
    second.provider_action_key = "pak-2".to_owned();
    second.tier2_lease_id = "lease-2".to_owned();
    let refreshed = h.task().await;
    assert_eq!(
        h.actor
            .dispatch_ci_tier2_lead(&refreshed, &second, None)
            .await,
        CiTier2Dispatch::AlreadyInFlight,
        "the head-level hold admits one adjudication, not one per route"
    );
    // And the block still names the FIRST route, so the guard resolves the
    // lease the Lead session was actually dispatched under.
    let directive = h.directive().await.expect("directive");
    assert_eq!(directive["ci_route"]["provider_action_key"], "pak-1");
}

/// Lane and origin must agree, or the reopen would fire from the wrong board
/// state.
#[test]
fn lane_and_origin_agree_for_both_routes_and_disagree_for_the_crossed_ones() {
    assert!(lane_agrees_with_origin(
        CiLane::PrHead,
        CiOriginState::PrDraft
    ));
    assert!(lane_agrees_with_origin(
        CiLane::MergeGroup,
        CiOriginState::PrReview
    ));
    assert!(!lane_agrees_with_origin(
        CiLane::PrHead,
        CiOriginState::PrReview
    ));
    assert!(!lane_agrees_with_origin(
        CiLane::MergeGroup,
        CiOriginState::PrDraft
    ));
}

/// Every Tier-2 reason is adjudicable. Closed match, so a sixth reason fails to
/// compile here rather than silently never dispatching.
#[test]
fn every_tier_two_reason_can_reach_a_lead_dispatch() {
    for reason in [
        CiTier2Reason::CausalFailure,
        CiTier2Reason::EvidenceUnknown,
        CiTier2Reason::ProviderActionFailed,
        CiTier2Reason::OutcomeUnknown,
        CiTier2Reason::RetryExhausted,
    ] {
        assert!(
            dispatchable_reason(reason),
            "{reason:?} must be dispatchable"
        );
    }
}

// ---------------------------------------------------------------------------
// The repair corpus
// ---------------------------------------------------------------------------

/// A handoff's `repository_commands` must reach the block verbatim.
///
/// The supervisor compares a repair's `verification_command` against this list
/// by whitespace-normalized **exact equality**, so a corpus that is reordered,
/// re-quoted, or truncated on the way through is a corpus that matches nothing
/// — and every repair silently becomes a diagnosis.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repair_corpus_reaches_the_block_verbatim() {
    let h = harness("pr_draft").await;
    let mut handoff = h.handoff(CiOriginState::PrDraft, CiLane::PrHead);
    handoff.repository_commands = vec![
        "cargo test -p djinn-db ci_route_attempt".to_owned(),
        "cargo clippy --workspace --all-targets".to_owned(),
    ];
    let task = h.task().await;
    h.actor.dispatch_ci_tier2_lead(&task, &handoff, None).await;

    let directive = h.directive().await.expect("directive");
    let commands: Vec<String> = directive["ci_route"]["repository_commands"]
        .as_array()
        .expect("repository_commands is an array")
        .iter()
        .map(|value| value.as_str().expect("a command string").to_owned())
        .collect();
    assert_eq!(
        commands, handoff.repository_commands,
        "the corpus is compared by exact equality; any rewrite makes it match nothing"
    );
}

/// An empty corpus is legal and is written as an empty array, not omitted.
///
/// A check that could not be reproduced yields no command. That must still
/// produce a parseable block — `repository_commands` is optional to the reader
/// precisely because this case is real — and every repair on the route then
/// degrades to a diagnosis rather than being accepted with an invented command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreproducible_check_yields_an_empty_corpus_not_a_broken_block() {
    let h = harness("pr_draft").await;
    let mut handoff = h.handoff(CiOriginState::PrDraft, CiLane::PrHead);
    handoff.repository_commands.clear();
    let task = h.task().await;
    assert_eq!(
        h.actor.dispatch_ci_tier2_lead(&task, &handoff, None).await,
        CiTier2Dispatch::Dispatched
    );
    let directive = h.directive().await.expect("directive");
    assert_eq!(
        directive["ci_route"]["repository_commands"],
        serde_json::json!([]),
        "an empty corpus is a real outcome and must not omit the key"
    );
    // Every other required key is still present, so the block still parses.
    for key in SUPERVISOR_REQUIRED_KEYS {
        assert!(
            directive["ci_route"].get(*key).is_some(),
            "`{key}` must survive an empty corpus"
        );
    }
}
