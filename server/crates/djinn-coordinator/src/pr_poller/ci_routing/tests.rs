//! Acceptance tests for the `nafu` wave-2 classifier and route-decision
//! contract.
//!
//! # What these tests assert, and why it is not a label
//!
//! "Asserts no provider mutation" is easy to fake: a test that checks a
//! returned enum name stays green if the branch it names later grows a
//! provider call. So the assertion here is structural instead.
//! [`CiProviderAction`] has no public constructor, [`CiRouteDecision`] is the
//! only thing that can hold one, and `provider_action()` is the only accessor.
//! A wave-3 lane executor that cannot obtain that value cannot call GitHub.
//! `assert_no_effects` therefore asserts the *absence of the capability*, not
//! the absence of an observed call.
//!
//! The same reasoning covers the Tier-2 lease (`tier2_reason()` is the only
//! authorization to open one) and the two board accessors.

use super::*;
use crate::pr_poller::ci_snapshot::evidence::{
    self, CiLaneEvidence, capture_merge_group_evidence, capture_pr_head_evidence,
    correlate_merge_group_run, dequeue_identity,
};
use djinn_db::CiActionPhase;
use djinn_provider::github_api::{CheckRun, CheckRunOutput, CheckRunsResponse, DequeueEvent};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const T0: &str = "2026-08-06T10:00:00Z";
const T1: &str = "2026-08-06T10:05:00Z";

const SUBJECT_A: &str = "019fcc00-0000-7000-8000-00000000000a";
const SUBJECT_B: &str = "019fcc00-0000-7000-8000-00000000000b";

fn subject() -> CiRouteSubject {
    CiRouteSubject::task(SUBJECT_A)
}

fn other_subject() -> CiRouteSubject {
    CiRouteSubject::task(SUBJECT_B)
}

fn pr_head_identity(run_id: i64) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: 41,
        pr_head_sha: HEAD.to_owned(),
        run_id,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

fn merge_group_identity(run_id: i64, dequeue: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: 41,
        pr_head_sha: HEAD.to_owned(),
        run_id,
        run_head_sha: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
        dequeue_id: Some(dequeue.to_owned()),
    }
}

/// A check run that executed and reached its own verdict — `ci_triage`'s
/// `Causal` class.
fn causal(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        id: 1,
        run_id: Some(run_id),
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("failure".to_owned()),
        html_url: format!("https://github.com/djinnos/djinn/actions/runs/{run_id}/job/9"),
        started_at: Some(T0.to_owned()),
        completed_at: Some(T1.to_owned()),
        output: None,
    }
}

/// A lane that executed and was killed by a run-level cancel —
/// `RanThenCancelled`.
fn ran_then_cancelled(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        conclusion: Some("cancelled".to_owned()),
        ..causal(name, run_id)
    }
}

/// A `needs:`-sealed aggregator: cancelled, no annotations, and a non-positive
/// execution interval. `ci_triage` calls this `NeverExecuted`, and it is the
/// canonical member of an inconclusive run.
fn never_executed(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        conclusion: Some("cancelled".to_owned()),
        started_at: Some(T1.to_owned()),
        completed_at: Some(T0.to_owned()),
        ..causal(name, run_id)
    }
}

fn with_annotations(mut cr: CheckRun, count: u64) -> CheckRun {
    cr.output = Some(CheckRunOutput {
        title: None,
        summary: None,
        annotations_count: Some(count),
    });
    cr
}

fn checks_response(runs: &[CheckRun]) -> CheckRunsResponse {
    CheckRunsResponse {
        total_count: runs.len() as u32,
        check_runs: runs.to_vec(),
    }
}

fn refs(runs: &[CheckRun]) -> Vec<&CheckRun> {
    runs.iter().collect()
}

fn observe<'a>(
    evidence: &'a CiEvidenceIdentity,
    current: &'a CiEvidenceIdentity,
    capture: CiCapture<'a>,
) -> CiObservation<'a> {
    CiObservation {
        evidence,
        observed_current: current,
        capture,
    }
}

/// Every authorization a decision can carry is absent.
///
/// This is the "no provider mutation, no Tier-2 lease, no board mutation, no
/// worker dispatch" assertion the acceptance matrix demands, expressed as the
/// absence of the four capabilities rather than as an unobserved side effect.
#[track_caller]
fn assert_no_effects(decision: &CiRouteDecision) {
    assert!(
        decision.provider_action().is_none(),
        "route carried provider-mutation authority: {decision:?}"
    );
    assert!(
        decision.tier2_reason().is_none(),
        "route carried Tier-2 lease authority: {decision:?}"
    );
    assert!(!decision.opens_tier2_lease(), "{decision:?}");
    assert!(!decision.authorizes_board_transition(), "{decision:?}");
    assert!(!decision.authorizes_worker_dispatch(), "{decision:?}");
}

/// A Tier-2 route: adjudication is authorized, provider mutation is not.
#[track_caller]
fn assert_lead_only(decision: &CiRouteDecision, reason: CiTier2Reason) {
    assert_eq!(decision.action(), CiAction::AskLead, "{decision:?}");
    assert_eq!(decision.tier2_reason(), Some(reason), "{decision:?}");
    assert!(
        decision.provider_action().is_none(),
        "Tier 2 must never carry provider-mutation authority: {decision:?}"
    );
    assert!(!decision.authorizes_board_transition(), "{decision:?}");
    assert!(!decision.authorizes_worker_dispatch(), "{decision:?}");
}

// ---------------------------------------------------------------------------
// The closed classification table
// ---------------------------------------------------------------------------

#[test]
fn one_causal_check_vetoes_automation_for_the_whole_run() {
    // A causal failure alongside cancelled and never-executed siblings — the
    // exact fan a run-level fail-fast produces. One causal check is enough.
    let checks = [
        causal("Quality Gate / build", 900),
        ran_then_cancelled("Quality Gate / test (1)", 900),
        never_executed("Publish Nextest Timing", 900),
    ];
    let blocking = refs(&checks);
    let id = pr_head_identity(900);

    let decision = classify(&observe(
        &id,
        &id,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ));

    assert_eq!(decision.class(), CiClass::CausalFailure);
    assert_eq!(decision.rationale(), CiRouteRationale::CausalFailure);
    assert_lead_only(&decision, CiTier2Reason::CausalFailure);
}

#[test]
fn fully_inconclusive_pr_head_run_is_tier_one_rerun() {
    let checks = [
        ran_then_cancelled("Quality Gate / test (1)", 900),
        never_executed("Publish Nextest Timing", 900),
    ];
    let blocking = refs(&checks);
    // Sanity: the Tier-1 predicate is the existing one, not a local copy.
    assert!(crate::pr_poller::ci_triage::is_inconclusive(&blocking));
    let id = pr_head_identity(900);

    let decision = classify(&observe(
        &id,
        &id,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ));

    assert_eq!(decision.class(), CiClass::Inconclusive);
    assert_eq!(decision.action(), CiAction::RerunRun);
    assert!(decision.tier2_reason().is_none());
    let action = decision
        .provider_action()
        .expect("Tier 1 authorizes exactly one provider mutation");
    assert_eq!(action.kind(), CiProviderActionKind::RerunFailedJobs);
    assert_eq!(action.run_id(), 900);
}

#[test]
fn fully_inconclusive_merge_group_run_re_enqueues() {
    let checks = [ran_then_cancelled("merge-group / integration", 7)];
    let blocking = refs(&checks);
    let id = merge_group_identity(
        7,
        "refs/heads/gh-readonly-queue/main/pr-41-abc@2026-08-06T09:00:00Z",
    );

    let decision = classify(&observe(
        &id,
        &id,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ));

    assert_eq!(decision.class(), CiClass::Inconclusive);
    assert_eq!(decision.action(), CiAction::Reenqueue);
    assert_eq!(
        decision.provider_action().map(CiProviderAction::kind),
        Some(CiProviderActionKind::EnableAutoMerge)
    );
}

#[test]
fn empty_blocking_set_on_a_failed_run_is_not_proof_of_transience() {
    // Absence of a blocking check on a run that concluded RED is unexplained,
    // not transient. It must never reach the Tier-1 provider action.
    let id = pr_head_identity(900);
    let decision = classify(&observe(
        &id,
        &id,
        CiCapture::FailedComplete { blocking: &[] },
    ));

    assert_eq!(decision.class(), CiClass::Unknown);
    assert_eq!(decision.rationale(), CiRouteRationale::EmptyBlockingSet);
    assert_lead_only(&decision, CiTier2Reason::EvidenceUnknown);
}

#[test]
fn a_passing_or_merged_observation_creates_no_route() {
    let id = pr_head_identity(900);
    for (capture, rationale) in [
        (CiCapture::Passing, CiRouteRationale::NewerPass),
        (CiCapture::Merged, CiRouteRationale::NewerMerge),
    ] {
        let decision = classify(&observe(&id, &id, capture));
        assert_eq!(decision.rationale(), rationale);
        assert!(decision.closes_route(), "{decision:?}");
        assert_no_effects(&decision);
    }
}

#[test]
fn a_newer_pass_or_merge_outranks_stale_and_causal_evidence() {
    // The passing/merged rows are authoritative over every other row: they
    // suppress older pending adjudications rather than being suppressed.
    let evidence = pr_head_identity(900);
    let current = pr_head_identity(901);
    for capture in [CiCapture::Passing, CiCapture::Merged] {
        let decision = classify(&observe(&evidence, &current, capture));
        assert!(decision.closes_route(), "{decision:?}");
        assert_no_effects(&decision);
    }
}

// ---------------------------------------------------------------------------
// The closed unknown / incomplete set
// ---------------------------------------------------------------------------

#[test]
fn a_non_terminal_run_or_check_holds_and_spends_nothing() {
    let id = pr_head_identity(900);
    for reason in [
        CiPendingReason::RunNonTerminal,
        CiPendingReason::RequiredCheckNonTerminal,
    ] {
        let decision = classify(&observe(&id, &id, CiCapture::NonTerminal(reason)));
        assert_eq!(decision.action(), CiAction::Hold, "{reason:?}");
        assert_eq!(decision.rationale(), CiRouteRationale::Pending(reason));
        // A hold reaches neither the provider NOR Lead: the snapshot is not
        // terminal, so there is nothing complete to adjudicate.
        assert_no_effects(&decision);
    }
}

/// The proposal's closed unknown/incomplete set, verbatim, minus the four
/// stale-identity cases (which are the discard test below) and the two pending
/// cases (which hold). Every one fails closed: Lead may adjudicate, the
/// provider may not be touched.
const CLOSED_INCOMPLETE_SET: [CiIncompleteReason; 10] = [
    CiIncompleteReason::MissingStartTimestamp,
    CiIncompleteReason::MissingCompletionTimestamp,
    CiIncompleteReason::MalformedExecutionInterval,
    CiIncompleteReason::NonPositiveExecutionInterval,
    CiIncompleteReason::CheckEnumerationUnavailable,
    CiIncompleteReason::PartialPagination,
    CiIncompleteReason::CheckApiError,
    CiIncompleteReason::LogApiError,
    CiIncompleteReason::AmbiguousMergeGroupCorrelation,
    CiIncompleteReason::MergeGroupCorrelationUnavailable,
];

#[test]
fn every_incomplete_evidence_case_fails_closed_without_provider_mutation() {
    let id = pr_head_identity(900);
    for reason in CLOSED_INCOMPLETE_SET {
        let decision = classify(&observe(&id, &id, CiCapture::Incomplete(reason)));
        assert_eq!(decision.class(), CiClass::Unknown, "{reason:?}");
        assert_eq!(
            decision.rationale(),
            CiRouteRationale::IncompleteEvidence(reason)
        );
        assert_lead_only(&decision, CiTier2Reason::EvidenceUnknown);
    }
    // Plus the one the proposal does not name but this repository produces:
    // a blocking check with no attributable Actions run.
    let decision = classify(&observe(
        &id,
        &id,
        CiCapture::Incomplete(CiIncompleteReason::RunAttributionUnavailable),
    ));
    assert_lead_only(&decision, CiTier2Reason::EvidenceUnknown);
}

// ---------------------------------------------------------------------------
// Stale identity
// ---------------------------------------------------------------------------

fn stale_variants() -> Vec<(CiStaleField, CiEvidenceIdentity, CiEvidenceIdentity)> {
    let pr_head = pr_head_identity(900);
    let mg = merge_group_identity(7, "grp@t0");
    vec![
        (CiStaleField::Lane, pr_head.clone(), {
            let mut o = pr_head.clone();
            o.lane = CiLane::MergeGroup;
            o
        }),
        (CiStaleField::PrNumber, pr_head.clone(), {
            let mut o = pr_head.clone();
            o.pr_number = 42;
            o
        }),
        (CiStaleField::PrHeadSha, pr_head.clone(), {
            let mut o = pr_head.clone();
            o.pr_head_sha = OTHER_HEAD.to_owned();
            o
        }),
        (CiStaleField::RunId, pr_head.clone(), {
            let mut o = pr_head.clone();
            o.run_id = 901;
            o
        }),
        (CiStaleField::RunHeadSha, pr_head.clone(), {
            let mut o = pr_head.clone();
            o.run_head_sha = OTHER_HEAD.to_owned();
            o
        }),
        (CiStaleField::DequeueId, mg.clone(), {
            let mut o = mg.clone();
            o.dequeue_id = Some("grp@t1".to_owned());
            o
        }),
    ]
}

#[test]
fn every_stale_identity_discards_without_provider_lease_board_or_worker() {
    let checks = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&checks);

    for (field, evidence, current) in stale_variants() {
        // Deliberately paired with INCONCLUSIVE evidence, the one class that
        // would otherwise authorize a provider call. If the guard were
        // evaluated after the Tier-1 branch, this is the case that would leak.
        let decision = classify(&observe(
            &evidence,
            &current,
            CiCapture::FailedComplete {
                blocking: &blocking,
            },
        ));
        assert_eq!(decision.action(), CiAction::Discard, "{field:?}");
        assert_eq!(decision.rationale(), CiRouteRationale::Stale(field));
        assert_no_effects(&decision);
        // The class its evidence actually carried is still recorded, so the
        // suppression report can say what was thrown away.
        assert_eq!(decision.class(), CiClass::Inconclusive, "{field:?}");
    }
}

#[test]
fn a_stale_route_stays_discarded_for_every_capture() {
    let evidence = pr_head_identity(900);
    let current = pr_head_identity(901);
    let checks = [causal("Quality Gate / build", 900)];
    let blocking = refs(&checks);

    let captures = [
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
        CiCapture::FailedComplete { blocking: &[] },
        CiCapture::Incomplete(CiIncompleteReason::CheckApiError),
        CiCapture::NonTerminal(CiPendingReason::RunNonTerminal),
    ];
    for capture in captures {
        let decision = classify(&observe(&evidence, &current, capture));
        assert_eq!(decision.action(), CiAction::Discard, "{capture:?}");
        assert_no_effects(&decision);
    }
}

#[test]
fn stale_field_names_the_first_divergence_deterministically() {
    let evidence = pr_head_identity(900);
    let mut current = evidence.clone();
    current.pr_head_sha = OTHER_HEAD.to_owned();
    current.run_id = 901;
    // Two fields diverge; the earlier one in identity order is named, always.
    assert_eq!(
        stale_field(&evidence, &current),
        Some(CiStaleField::PrHeadSha)
    );
    assert_eq!(stale_field(&evidence, &evidence), None);
}

// ---------------------------------------------------------------------------
// Monotonic budgets
// ---------------------------------------------------------------------------

fn counts(signature: i64, head: i64) -> CiBudgetCounts {
    CiBudgetCounts { signature, head }
}

fn tier1_decision() -> CiRouteDecision {
    let checks = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&checks);
    let id = pr_head_identity(900);
    classify(&observe(
        &id,
        &id,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ))
}

#[test]
fn budget_thresholds_come_from_the_repository_layer() {
    assert_eq!(budget_ceilings(), (2, 4));
}

#[test]
fn tier_one_survives_counts_below_both_ceilings() {
    for (sig, head) in [(0, 0), (1, 1), (0, 3), (1, 3)] {
        let decision = apply_budget(tier1_decision(), counts(sig, head));
        assert!(
            decision.provider_action().is_some(),
            "signature={sig} head={head} must remain Tier 1"
        );
        assert_eq!(decision.action(), CiAction::RerunRun);
    }
}

#[test]
fn either_ceiling_routes_tier_one_to_lead_through_retry_exhaustion() {
    // Signature ceiling reached with head budget to spare, and head ceiling
    // reached with signature budget to spare. Either alone is enough.
    for (sig, head) in [(2, 0), (0, 4), (2, 4), (3, 5)] {
        let decision = apply_budget(tier1_decision(), counts(sig, head));
        assert_eq!(decision.rationale(), CiRouteRationale::BudgetExhausted);
        assert_lead_only(&decision, CiTier2Reason::RetryExhausted);
        // The class is retained: the evidence was still inconclusive, it just
        // can no longer be paid for.
        assert_eq!(decision.class(), CiClass::Inconclusive);
    }
}

#[test]
fn budget_never_changes_a_route_that_was_not_tier_one() {
    let checks = [causal("Quality Gate / build", 900)];
    let blocking = refs(&checks);
    let evidence = pr_head_identity(900);
    let stale_current = pr_head_identity(901);

    let causal_route = classify(&observe(
        &evidence,
        &evidence,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ));
    let discarded = classify(&observe(
        &evidence,
        &stale_current,
        CiCapture::FailedComplete {
            blocking: &blocking,
        },
    ));
    let held = classify(&observe(
        &evidence,
        &evidence,
        CiCapture::NonTerminal(CiPendingReason::RunNonTerminal),
    ));

    for spent in [counts(0, 0), counts(2, 4)] {
        assert_eq!(apply_budget(causal_route.clone(), spent), causal_route);
        // The important one: an exhausted budget must not give an obsolete
        // route a Tier-2 lease it was never entitled to.
        assert_eq!(apply_budget(discarded.clone(), spent), discarded);
        assert_no_effects(&apply_budget(discarded.clone(), spent));
        assert_eq!(apply_budget(held.clone(), spent), held);
        assert_no_effects(&apply_budget(held.clone(), spent));
    }
}

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

#[test]
fn every_key_is_scoped_to_its_subject() {
    // The proposal's `CiEvidenceId` has no repository field and `pr_number` is
    // unique only within one repo. Without a subject scope these four keys
    // would collide across projects: a duplicate-suppressed provider call, two
    // projects sharing one retry budget, and one project's Lead adjudication
    // blocking another's.
    let id = pr_head_identity(900);
    let (a, b) = (subject(), other_subject());

    assert_ne!(
        provider_action_key(&a, &id, CiAction::RerunRun),
        provider_action_key(&b, &id, CiAction::RerunRun)
    );
    assert_ne!(
        retry_budget_key(&a, &id, "fp"),
        retry_budget_key(&b, &id, "fp")
    );
    assert_ne!(head_budget_key(&a, 41, HEAD), head_budget_key(&b, 41, HEAD));
    assert_ne!(tier2_lease_key(&a, 41, HEAD), tier2_lease_key(&b, 41, HEAD));
}

#[test]
fn field_boundaries_cannot_be_shifted_to_forge_a_collision() {
    // Adjacent fields that concatenate identically must not hash identically.
    // Length prefixing is what stops that; a separator character alone cannot,
    // because separators occur in SHAs, refs, and fingerprints.
    let s = subject();
    let mut ab = pr_head_identity(900);
    ab.pr_head_sha = "ab".to_owned();
    ab.run_head_sha = "c".to_owned();
    let mut a = pr_head_identity(900);
    a.pr_head_sha = "a".to_owned();
    a.run_head_sha = "bc".to_owned();
    assert_ne!(
        provider_action_key(&s, &ab, CiAction::RerunRun),
        provider_action_key(&s, &a, CiAction::RerunRun)
    );

    // And the same across the subject boundary.
    let id = pr_head_identity(900);
    assert_ne!(
        provider_action_key(&CiRouteSubject::task("ab"), &id, CiAction::RerunRun),
        provider_action_key(&CiRouteSubject::task("a"), &id, CiAction::RerunRun)
    );
}

#[test]
fn the_action_key_separates_runs_while_the_budget_key_unites_them() {
    let s = subject();
    let first = pr_head_identity(900);
    let second = pr_head_identity(901);

    // A distinct later run gets its own call episode...
    assert_ne!(
        provider_action_key(&s, &first, CiAction::RerunRun),
        provider_action_key(&s, &second, CiAction::RerunRun)
    );
    // ...but shares one budget, because the budget key excludes the run id.
    assert_eq!(
        retry_budget_key(&s, &first, "fp"),
        retry_budget_key(&s, &second, "fp")
    );
    // And the same evidence with a different action is a different key.
    assert_ne!(
        provider_action_key(&s, &first, CiAction::RerunRun),
        provider_action_key(&s, &first, CiAction::AskLead)
    );
}

#[test]
fn the_dequeue_id_separates_action_keys_but_not_budgets() {
    let s = subject();
    let first = merge_group_identity(7, "grp@t0");
    let second = merge_group_identity(7, "grp@t1");
    assert_ne!(
        provider_action_key(&s, &first, CiAction::Reenqueue),
        provider_action_key(&s, &second, CiAction::Reenqueue)
    );
    assert_eq!(
        retry_budget_key(&s, &first, "fp"),
        retry_budget_key(&s, &second, "fp")
    );
    // An absent dequeue id is a different fact from an empty one.
    let mut empty = first.clone();
    empty.dequeue_id = Some(String::new());
    let mut absent = first.clone();
    absent.dequeue_id = None;
    assert_ne!(
        provider_action_key(&s, &empty, CiAction::Reenqueue),
        provider_action_key(&s, &absent, CiAction::Reenqueue)
    );
}

#[test]
fn the_head_budget_is_shared_across_lanes_and_the_signature_budget_is_not() {
    let s = subject();
    let head_lane = pr_head_identity(900);
    let queue_lane = merge_group_identity(7, "grp@t0");
    assert_eq!(head_lane.pr_head_sha, queue_lane.pr_head_sha);

    assert_eq!(
        head_budget_key(&s, head_lane.pr_number, &head_lane.pr_head_sha),
        head_budget_key(&s, queue_lane.pr_number, &queue_lane.pr_head_sha),
        "four charged actions per PR head are shared ACROSS both lanes"
    );
    assert_ne!(
        retry_budget_key(&s, &head_lane, "fp"),
        retry_budget_key(&s, &queue_lane, "fp"),
        "two charged actions per signature are per-lane"
    );
}

#[test]
fn the_tier2_lease_key_is_head_scoped_not_lane_scoped() {
    // Lane-scoped leases would permit two concurrent Lead adjudications for
    // one PR head — the `pr_head` route and the `merge_group` route — which
    // defeats the head-level hold the retry-storm safeguard is built on.
    let s = subject();
    let head_lane = pr_head_identity(900);
    let queue_lane = merge_group_identity(7, "grp@t0");
    let other_run = pr_head_identity(901);

    let key = tier2_lease_key(&s, 41, HEAD);
    for id in [&head_lane, &queue_lane, &other_run] {
        assert_eq!(
            tier2_lease_key(&s, id.pr_number, &id.pr_head_sha),
            key,
            "lane, run id, and dequeue id must not scope the lease"
        );
    }
    // A different head is a genuinely different hold.
    assert_ne!(tier2_lease_key(&s, 41, OTHER_HEAD), key);
    // And the head budget and the lease are separate key spaces even though
    // their field vectors match, so one can never be read as the other.
    assert_ne!(head_budget_key(&s, 41, HEAD), key);
}

#[test]
fn a_changed_pr_head_starts_every_key_fresh() {
    let s = subject();
    let old = pr_head_identity(900);
    let mut new = old.clone();
    new.pr_head_sha = OTHER_HEAD.to_owned();
    new.run_head_sha = OTHER_HEAD.to_owned();

    assert_ne!(
        provider_action_key(&s, &old, CiAction::RerunRun),
        provider_action_key(&s, &new, CiAction::RerunRun)
    );
    assert_ne!(
        retry_budget_key(&s, &old, "fp"),
        retry_budget_key(&s, &new, "fp")
    );
    assert_ne!(
        head_budget_key(&s, 41, &old.pr_head_sha),
        head_budget_key(&s, 41, &new.pr_head_sha)
    );
}

#[test]
fn every_derived_key_fits_the_durable_column() {
    let s = subject();
    let id = merge_group_identity(
        i64::MAX,
        "refs/heads/gh-readonly-queue/main/pr-41-0123456789abcdef@2026-08-06T10:00:00Z",
    );
    let long_name = "a very long check name ".repeat(20);
    let checks = [causal(&long_name, 900)];
    let keys = [
        provider_action_key(&s, &id, CiAction::Reenqueue),
        retry_budget_key(
            &s,
            &id,
            &transient_fingerprint(CiLane::MergeGroup, &refs(&checks)),
        ),
        head_budget_key(&s, id.pr_number, &id.pr_head_sha),
        tier2_lease_key(&s, id.pr_number, &id.pr_head_sha),
        transient_fingerprint(CiLane::PrHead, &refs(&checks)),
    ];
    for key in keys {
        assert!(key.len() <= 128, "{} chars: {key}", key.len());
    }
}

#[test]
fn the_transient_fingerprint_is_order_independent_and_evidence_sensitive() {
    let a = ran_then_cancelled("Quality Gate / test (1)", 900);
    let b = never_executed("Publish Nextest Timing", 900);

    let forward = vec![&a, &b];
    let reverse = vec![&b, &a];
    assert_eq!(
        transient_fingerprint(CiLane::PrHead, &forward),
        transient_fingerprint(CiLane::PrHead, &reverse),
        "provider check ordering must not split one signature into two budgets"
    );

    // The lane participates.
    assert_ne!(
        transient_fingerprint(CiLane::PrHead, &forward),
        transient_fingerprint(CiLane::MergeGroup, &forward)
    );

    // Same names and conclusions, different ci_triage evidence class: the
    // aggregator now carries annotations, so it counts as executed. That is a
    // different signature and starts a different budget.
    let b_executed = with_annotations(b.clone(), 3);
    let changed = vec![&a, &b_executed];
    assert_ne!(
        transient_fingerprint(CiLane::PrHead, &forward),
        transient_fingerprint(CiLane::PrHead, &changed)
    );
}

// ---------------------------------------------------------------------------
// Evidence capture: complete terminal evidence identity
// ---------------------------------------------------------------------------

#[test]
fn a_short_check_enumeration_is_a_proven_partial_read() {
    let runs = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&runs);
    let mut response = checks_response(&runs);
    response.total_count = 7;

    match capture_pr_head_evidence(41, HEAD, &response, &blocking) {
        CiLaneEvidence::Incomplete(CiIncompleteReason::PartialPagination) => {}
        other => panic!("expected partial pagination, got {other:?}"),
    }
}

#[test]
fn a_non_terminal_required_check_holds_before_any_completeness_verdict() {
    let mut pending = ran_then_cancelled("Quality Gate / test (1)", 900);
    pending.status = "in_progress".to_owned();
    pending.completed_at = None;
    let runs = [pending];
    let blocking = refs(&runs);

    // The missing `completed_at` would read as incomplete evidence if
    // terminality were not checked first; a running check is a hold, not an
    // unknown, because it resolves itself on the next poll.
    match capture_pr_head_evidence(41, HEAD, &checks_response(&runs), &blocking) {
        CiLaneEvidence::NonTerminal(CiPendingReason::RequiredCheckNonTerminal) => {}
        other => panic!("expected a hold, got {other:?}"),
    }
}

#[test]
fn timestamp_completeness_is_checked_in_a_fixed_order() {
    let base = ran_then_cancelled("Quality Gate / test (1)", 900);

    let mut no_start = base.clone();
    no_start.started_at = None;
    let mut no_end = base.clone();
    no_end.completed_at = None;
    let mut malformed = base.clone();
    malformed.completed_at = Some("2026-08-06T10:05:00+00:00".to_owned());

    for (check, expected) in [
        (no_start, CiIncompleteReason::MissingStartTimestamp),
        (no_end, CiIncompleteReason::MissingCompletionTimestamp),
        (malformed, CiIncompleteReason::MalformedExecutionInterval),
    ] {
        let runs = [check];
        let blocking = refs(&runs);
        assert_eq!(
            evidence::blocking_evidence_completeness(&blocking),
            Some(expected)
        );
    }
}

#[test]
fn a_hard_failure_that_claims_it_never_ran_is_contradictory_not_transient() {
    // `failure` is a verdict about the lane's own work; a non-positive
    // interval says the lane never dispatched work. Both cannot be true, and
    // `ci_triage` resolves the pair toward NeverExecuted — which would make
    // this run look inconclusive and earn an automatic retry. That is the
    // proposal's "false transient" risk, so the contradiction fails closed.
    let mut contradictory = causal("Quality Gate / build", 900);
    contradictory.started_at = Some(T1.to_owned());
    contradictory.completed_at = Some(T0.to_owned());
    let runs = [contradictory];
    let blocking = refs(&runs);

    assert_eq!(
        crate::pr_poller::ci_triage::check_evidence(blocking[0]),
        crate::pr_poller::ci_triage::CheckEvidence::NeverExecuted,
        "ci_triage's own ranking is unchanged"
    );
    assert!(crate::pr_poller::ci_triage::is_inconclusive(&blocking));
    assert_eq!(
        evidence::blocking_evidence_completeness(&blocking),
        Some(CiIncompleteReason::NonPositiveExecutionInterval),
        "routing layers a fail-closed guard on top rather than editing ci_triage"
    );

    match capture_pr_head_evidence(41, HEAD, &checks_response(&runs), &blocking) {
        CiLaneEvidence::Incomplete(CiIncompleteReason::NonPositiveExecutionInterval) => {}
        other => panic!("expected a fail-closed capture, got {other:?}"),
    }
}

#[test]
fn a_plain_never_executed_aggregator_stays_tier_one_eligible() {
    // The counterpart of the test above: a `cancelled` lane with a
    // non-positive interval is exactly what `ci_triage` calls NeverExecuted
    // and exactly what makes a run inconclusive. If this were also treated as
    // incomplete, Tier 1 could never fire for the incident the proposal cites.
    let runs = [
        ran_then_cancelled("Quality Gate / test (1)", 900),
        never_executed("Publish Nextest Timing", 900),
    ];
    let blocking = refs(&runs);
    assert_eq!(evidence::blocking_evidence_completeness(&blocking), None);

    let captured = capture_pr_head_evidence(41, HEAD, &checks_response(&runs), &blocking);
    let CiLaneEvidence::Runs(routes) = captured else {
        panic!("expected one complete run, got {captured:?}");
    };
    assert_eq!(routes.len(), 1);
    let decision = classify(&observe(
        &routes[0].identity,
        &routes[0].identity,
        routes[0].capture(),
    ));
    assert_eq!(decision.action(), CiAction::RerunRun);
}

#[test]
fn each_distinct_actions_run_becomes_its_own_evidence_identity() {
    // The immutable identity names ONE run and `rerun_failed_jobs` acts on
    // ONE run, so a head with two failing workflow runs is two routes with
    // two action keys — which is what lets a genuinely new second run get its
    // own call episode instead of colliding with the first.
    let runs = [
        ran_then_cancelled("CI / test", 900),
        ran_then_cancelled("Release / build", 901),
        never_executed("CI / aggregate", 900),
    ];
    let blocking = refs(&runs);

    let CiLaneEvidence::Runs(routes) =
        capture_pr_head_evidence(41, HEAD, &checks_response(&runs), &blocking)
    else {
        panic!("expected complete per-run evidence");
    };
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].identity.run_id, 900);
    assert_eq!(routes[0].blocking.len(), 2);
    assert_eq!(routes[1].identity.run_id, 901);
    assert_eq!(routes[1].blocking.len(), 1);

    let s = subject();
    assert_ne!(
        provider_action_key(&s, &routes[0].identity, CiAction::RerunRun),
        provider_action_key(&s, &routes[1].identity, CiAction::RerunRun)
    );
    // Both nevertheless charge one shared head budget.
    assert_eq!(
        head_budget_key(&s, 41, &routes[0].identity.pr_head_sha),
        head_budget_key(&s, 41, &routes[1].identity.pr_head_sha)
    );
}

#[test]
fn an_unattributable_blocking_check_fails_the_whole_lane_closed() {
    // A required check that belongs to no Actions run cannot be rerun, and it
    // may be the causal one. Dropping it would let a run that reached no
    // verdict look inconclusive, so the lane refuses to claim completeness.
    let mut external = ran_then_cancelled("vercel/preview", 0);
    external.run_id = None;
    external.html_url = "https://vercel.com/deployments/xyz".to_owned();
    let runs = [ran_then_cancelled("CI / test", 900), external];
    let blocking = refs(&runs);

    match capture_pr_head_evidence(41, HEAD, &checks_response(&runs), &blocking) {
        CiLaneEvidence::Incomplete(CiIncompleteReason::RunAttributionUnavailable) => {}
        other => panic!("expected a fail-closed capture, got {other:?}"),
    }
}

#[test]
fn merge_group_correlation_is_unavailable_or_ambiguous_rather_than_guessed() {
    let run = |id: u64, branch: &str, conclusion: &str| djinn_provider::github_api::WorkflowRun {
        id,
        workflow_id: None,
        name: None,
        path: None,
        head_branch: Some(branch.to_owned()),
        head_sha: format!("{id:040}"),
        status: Some("completed".to_owned()),
        conclusion: Some(conclusion.to_owned()),
    };

    // Nothing correlates.
    assert_eq!(
        correlate_merge_group_run(41, &[run(1, "main", "failure")]).err(),
        Some(CiIncompleteReason::MergeGroupCorrelationUnavailable)
    );
    // A neighbouring PR number must not correlate: the trailing dash in the
    // `pr-41-` marker is what keeps `pr-411-` out.
    assert_eq!(
        correlate_merge_group_run(
            41,
            &[run(1, "gh-readonly-queue/main/pr-411-abc", "failure")]
        )
        .err(),
        Some(CiIncompleteReason::MergeGroupCorrelationUnavailable)
    );
    // Exactly one correlates.
    let single = [run(9, "gh-readonly-queue/main/pr-41-abc", "failure")];
    assert_eq!(correlate_merge_group_run(41, &single).map(|r| r.id), Ok(9));
    // Two correlate: the legacy enrichment path takes the newest; the route
    // refuses, because it cannot say which run this dequeue refers to.
    let ambiguous = [
        run(9, "gh-readonly-queue/main/pr-41-abc", "failure"),
        run(10, "gh-readonly-queue/main/pr-41-def", "failure"),
    ];
    assert_eq!(
        correlate_merge_group_run(41, &ambiguous).err(),
        Some(CiIncompleteReason::AmbiguousMergeGroupCorrelation)
    );
}

#[test]
fn a_dequeue_without_a_ref_and_timestamp_cannot_be_identified() {
    let full = DequeueEvent {
        reason: Some("CHECKS_FAILED".to_owned()),
        merge_group_ref: Some("refs/heads/gh-readonly-queue/main/pr-41-abc".to_owned()),
        created_at: Some(T0.to_owned()),
        before_commit_sha: None,
    };
    assert_eq!(
        dequeue_identity(&full).as_deref(),
        Some("refs/heads/gh-readonly-queue/main/pr-41-abc@2026-08-06T10:00:00Z")
    );

    for missing in [
        DequeueEvent {
            created_at: None,
            ..full.clone()
        },
        DequeueEvent {
            merge_group_ref: None,
            ..full.clone()
        },
    ] {
        assert!(dequeue_identity(&missing).is_none());
    }

    // Two dequeues of the same merge group at different times are different
    // evidence, which is what makes a stale dequeue detectable.
    let later = DequeueEvent {
        created_at: Some(T1.to_owned()),
        ..full.clone()
    };
    assert_ne!(dequeue_identity(&full), dequeue_identity(&later));
}

#[test]
fn merge_group_capture_names_the_run_head_not_the_pr_head() {
    let run = djinn_provider::github_api::WorkflowRun {
        id: 9,
        workflow_id: None,
        name: None,
        path: None,
        head_branch: Some("gh-readonly-queue/main/pr-41-abc".to_owned()),
        head_sha: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
        status: Some("completed".to_owned()),
        conclusion: Some("failure".to_owned()),
    };
    let runs = [ran_then_cancelled("merge-group / integration", 9)];
    let blocking = refs(&runs);

    let CiLaneEvidence::Runs(routes) =
        capture_merge_group_evidence(41, HEAD, &run, "grp@t0", &checks_response(&runs), &blocking)
    else {
        panic!("expected complete merge-group evidence");
    };
    assert_eq!(routes.len(), 1);
    let id = &routes[0].identity;
    assert_eq!(id.lane, CiLane::MergeGroup);
    assert_eq!(id.pr_head_sha, HEAD);
    assert_ne!(
        id.run_head_sha, id.pr_head_sha,
        "the merge-group run runs against a synthetic queue commit"
    );
    assert_eq!(id.dequeue_id.as_deref(), Some("grp@t0"));
}

// ---------------------------------------------------------------------------
// Routing the repository layer's outcomes
// ---------------------------------------------------------------------------

#[test]
fn an_exhausted_reservation_whose_head_lease_is_taken_is_neither_resumed_nor_stranded() {
    // `recover_reserved` can return RetryExhausted with NO lease id: another
    // row already holds the head's Tier-2 lease. It stamps `retry_exhausted_at`
    // and leaves the row in phase `reserved`. Treating that as a resumption
    // would call the provider on a row that was never charged; treating it as
    // a completed escalation would report a Lead adjudication that does not
    // exist.
    let attempt = |phase| dummy_attempt(phase);
    let exhausted_with_lease = CiReservedRecovery::RetryExhausted {
        attempt: Box::new(attempt(CiActionPhase::Reserved)),
        counts: counts(2, 4),
        tier2_lease_id: Some("lease-1".to_owned()),
    };
    let exhausted_without_lease = CiReservedRecovery::RetryExhausted {
        attempt: Box::new(attempt(CiActionPhase::Reserved)),
        counts: counts(2, 4),
        tier2_lease_id: None,
    };

    assert_eq!(
        route_reserved_recovery(&exhausted_with_lease),
        CiReservedRecoveryRoute::Tier2Exhausted
    );
    assert_eq!(
        route_reserved_recovery(&exhausted_without_lease),
        CiReservedRecoveryRoute::HeldByHeadLease
    );
    assert_ne!(
        route_reserved_recovery(&exhausted_without_lease),
        CiReservedRecoveryRoute::ResumeProviderCall
    );

    assert_eq!(
        route_reserved_recovery(&CiReservedRecovery::Resumed {
            attempt: Box::new(attempt(CiActionPhase::Calling)),
            counts: counts(1, 1),
        }),
        CiReservedRecoveryRoute::ResumeProviderCall
    );
    assert_eq!(
        route_reserved_recovery(&CiReservedRecovery::SupersededPreCall(Box::new(attempt(
            CiActionPhase::Terminal
        )))),
        CiReservedRecoveryRoute::Superseded
    );
    for inert in [
        CiReservedRecovery::NotFound,
        CiReservedRecovery::NotEligible(Box::new(attempt(CiActionPhase::Reserved))),
    ] {
        assert_eq!(
            route_reserved_recovery(&inert),
            CiReservedRecoveryRoute::Inert
        );
    }
}

#[test]
fn a_route_that_already_adjudicated_may_never_adjudicate_again() {
    // "Route ONCE to Tier 2" is once-ever, not once-concurrently. A resolved
    // lease must not be reopened by a later sweep, or the row's
    // `tier2_resolution` flips back to open carrying a stale Lead result.
    let mut attempt = dummy_attempt(CiActionPhase::Terminal);
    assert_eq!(tier2_admission(&attempt), CiTier2Admission::Admit);
    assert!(
        may_open_tier2(&attempt),
        "a fresh route may adjudicate once"
    );

    attempt.tier2_lease_state = Some(djinn_db::CiTier2LeaseState::Open);
    assert_eq!(
        tier2_admission(&attempt),
        CiTier2Admission::AlreadyAdjudicated
    );

    attempt.tier2_lease_state = Some(djinn_db::CiTier2LeaseState::Resolved);
    assert_eq!(
        tier2_admission(&attempt),
        CiTier2Admission::AlreadyAdjudicated,
        "a resolved lease still counts: the trip has been used"
    );
}

#[test]
fn a_row_with_a_provider_call_in_flight_is_refused_before_the_lease() {
    // The `calling` refusal outranks the once-ever one: a row that is calling
    // and has never adjudicated must still be refused, because the reason is
    // the in-flight provider future, not the lease history.
    let mut calling = dummy_attempt(CiActionPhase::Calling);
    calling.owner_incarnation_id = Some("inc-1".to_owned());
    calling.calling_at = Some(T1.to_owned());
    assert_eq!(
        tier2_admission(&calling),
        CiTier2Admission::ProviderCallInFlight
    );
    assert!(!may_open_tier2(&calling));

    // Reserved and terminal rows are admitted; only `calling` is not.
    for phase in [CiActionPhase::Reserved, CiActionPhase::Terminal] {
        assert_eq!(
            tier2_admission(&dummy_attempt(phase)),
            CiTier2Admission::Admit,
            "{phase:?}"
        );
    }
}

#[test]
fn provider_finalization_uses_only_owner_fenced_outcomes() {
    // `finalize_calling` accepts exactly these three and fences the write to
    // the owning incarnation; `terminalize` accepts anything on any
    // non-terminal row with no owner check, including a live `calling` row.
    // Keeping the mapping inside this set is what keeps the fenced path
    // available for every provider result.
    assert_eq!(
        provider_finalization_outcome(CiLane::PrHead, true),
        CiRouteOutcome::Retriggered
    );
    assert_eq!(
        provider_finalization_outcome(CiLane::MergeGroup, true),
        CiRouteOutcome::Reenqueued
    );
    for lane in [CiLane::PrHead, CiLane::MergeGroup] {
        assert_eq!(
            provider_finalization_outcome(lane, false),
            CiRouteOutcome::ActionFailed
        );
    }
}

#[test]
fn each_supersession_stage_has_exactly_one_terminal_outcome() {
    let mapped = [
        (CiRouteStage::PreCall, CiRouteOutcome::SupersededPreCall),
        (CiRouteStage::AfterCall, CiRouteOutcome::SupersededAfterCall),
        (
            CiRouteStage::BeforeLead,
            CiRouteOutcome::SupersededBeforeLead,
        ),
        (
            CiRouteStage::BeforeApply,
            CiRouteOutcome::SupersededBeforeApply,
        ),
    ];
    for (stage, outcome) in mapped {
        assert_eq!(supersession_outcome(stage), outcome);
    }
}

#[test]
fn a_reopen_after_a_provider_failure_is_still_a_reopen() {
    // `terminal_outcome` is write-once and is claimed by whichever close
    // happened FIRST. For a route whose provider call errored, that is
    // `action_failed`, committed before Lead ever ran; the Lead result then
    // lands on `tier2_resolution`. A report that reads `terminal_outcome`
    // alone counts this as a provider failure and drops the reopen entirely.
    let mut attempt = dummy_attempt(CiActionPhase::Terminal);
    attempt.terminal_outcome = Some(CiRouteOutcome::ActionFailed);
    attempt.tier2_resolution = Some(CiRouteOutcome::RepairReopened);

    assert_eq!(
        attempt.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened)
    );
    assert!(is_reopen(&attempt));
    assert!(!is_park(&attempt));

    // Same shape after an unknown outcome, and for a park.
    attempt.terminal_outcome = Some(CiRouteOutcome::OutcomeUnknown);
    attempt.tier2_resolution = Some(CiRouteOutcome::Parked);
    assert!(is_park(&attempt));
    assert!(!is_reopen(&attempt));

    // And a route whose only outcome is the terminal one still reads.
    attempt.tier2_resolution = None;
    attempt.terminal_outcome = Some(CiRouteOutcome::DiagnosticReopened);
    assert!(is_reopen(&attempt));
    // A route with neither has no effective outcome at all.
    attempt.terminal_outcome = None;
    assert_eq!(attempt.adjudicated_outcome(), None);
    assert!(!is_reopen(&attempt));
    assert!(!is_park(&attempt));
}

fn dummy_attempt(phase: CiActionPhase) -> CiRouteAttempt {
    CiRouteAttempt {
        subject: subject(),
        provider_action_key: "pa:test".to_owned(),
        identity: pr_head_identity(900),
        task_id: Some(SUBJECT_A.to_owned()),
        origin_state: djinn_db::CiOriginState::PrDraft,
        class: CiClass::Inconclusive,
        action: CiAction::RerunRun,
        transient_fingerprint: "fp".to_owned(),
        retry_budget_key: "sig:test".to_owned(),
        head_budget_key: "head:test".to_owned(),
        action_phase: phase,
        terminal_outcome: None,
        reserved_at: T0.to_owned(),
        calling_at: None,
        terminalized_at: None,
        owner_incarnation_id: None,
        pre_call_resumptions: 0,
        charged_signature_count: None,
        charged_head_count: None,
        retry_exhausted_at: None,
        tier2_lease_id: None,
        tier2_lease_key: None,
        tier2_lease_state: None,
        tier2_lease_reason: None,
        tier2_leased_at: None,
        tier2_resolution: None,
        lead_session_id: None,
        reopen_mode: None,
        diagnostic_reason: None,
        park_justification: None,
        provider_error: None,
        superseded_by_evidence: None,
    }
}

// ---------------------------------------------------------------------------
// The keys against the real durable constraints
// ---------------------------------------------------------------------------
//
// The tests below are the only ones here that touch Postgres. Everything above
// proves the derived key STRINGS differ; these prove the difference is the one
// the unique constraints actually enforce, which is the claim that matters. A
// derivation that looked distinct but landed on one column value would pass
// every assertion above and still collide.

use djinn_db::{
    CiOriginState, CiReserveOutcome, CiRouteAttemptRepository, CiRouteReservation,
    CiTier2LeaseOutcome, Database,
};

struct Fixture {
    _db: Database,
    repository: CiRouteAttemptRepository,
    subjects: Vec<CiRouteSubject>,
}

/// Seeds `n` tasks in one project, each of which is its own route subject.
async fn route_fixture(n: usize) -> Fixture {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project =
        djinn_db::test_support::make_project(&db, std::path::Path::new("ci-routing")).await;
    let mut subjects = Vec::new();
    for _ in 0..n {
        let task_id = djinn_db::test_support::seed_task_row(
            &db,
            djinn_db::test_support::UsageTestTaskSeed {
                project_id: &project.id,
                status: "pr_draft",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await;
        subjects.push(CiRouteSubject::task(task_id));
    }
    let repository = CiRouteAttemptRepository::new(db.clone());
    Fixture {
        _db: db,
        repository,
        subjects,
    }
}

fn reservation(subject: &CiRouteSubject, identity: &CiEvidenceIdentity) -> CiRouteReservation {
    CiRouteReservation {
        subject: subject.clone(),
        provider_action_key: provider_action_key(subject, identity, CiAction::RerunRun),
        identity: identity.clone(),
        origin_state: CiOriginState::PrDraft,
        class: CiClass::Inconclusive,
        action: CiAction::RerunRun,
        transient_fingerprint: "fp".to_owned(),
        retry_budget_key: retry_budget_key(subject, identity, "fp"),
        head_budget_key: head_budget_key(subject, identity.pr_number, &identity.pr_head_sha),
    }
}

#[tokio::test]
async fn two_subjects_sharing_a_pr_number_get_two_independent_routes() {
    let f = route_fixture(2).await;
    let (a_subject, b_subject) = (&f.subjects[0], &f.subjects[1]);
    let identity = pr_head_identity(900);

    // Byte-identical evidence identities. Only the subject differs — which is
    // the field the proposal's `CiEvidenceId` does not have, and `pr_number`
    // is unique only within one repository.
    let first = reservation(a_subject, &identity);
    let second = reservation(b_subject, &identity);

    let a = f.repository.reserve(&first).await.expect("reserve a");
    let b = f.repository.reserve(&second).await.expect("reserve b");

    assert!(
        matches!(a, CiReserveOutcome::Reserved(_)),
        "first reservation: {a:?}"
    );
    assert!(
        matches!(b, CiReserveOutcome::Reserved(_)),
        "a second subject must not be suppressed as a duplicate poll: {b:?}"
    );

    // Assert the rows the mechanism WROTE, not the returned labels.
    for (subject, reservation) in [(a_subject, &first), (b_subject, &second)] {
        assert!(
            f.repository
                .get(subject, &reservation.provider_action_key)
                .await
                .expect("read back")
                .is_some()
        );
    }

    // And each subject charges its own budget.
    f.repository
        .charge_and_begin_calling(a_subject, &first.provider_action_key, "inc-1", &identity)
        .await
        .expect("charge a");
    assert_eq!(
        f.repository
            .budget_counts(a_subject, &first.retry_budget_key, &first.head_budget_key)
            .await
            .expect("counts a"),
        counts(1, 1)
    );
    assert_eq!(
        f.repository
            .budget_counts(b_subject, &second.retry_budget_key, &second.head_budget_key)
            .await
            .expect("counts b"),
        counts(0, 0),
        "one subject's retry must never be charged to another's budget"
    );
}

#[tokio::test]
async fn one_subjects_pr_head_holds_one_tier2_lease_across_both_lanes() {
    let f = route_fixture(1).await;
    let s = &f.subjects[0];
    let head_route = pr_head_identity(900);
    let queue_route = merge_group_identity(7, "grp@t0");
    let lease_key = tier2_lease_key(s, 41, HEAD);

    for identity in [&head_route, &queue_route] {
        f.repository
            .reserve(&reservation(s, identity))
            .await
            .expect("reserve");
    }

    let opened = f
        .repository
        .open_tier2_lease(
            s,
            &provider_action_key(s, &head_route, CiAction::RerunRun),
            &head_route,
            &lease_key,
            CiTier2Reason::CausalFailure,
        )
        .await
        .expect("open the head's lease");
    assert!(matches!(opened, CiTier2LeaseOutcome::Opened { .. }));

    // The merge-group route for the SAME PR head derives the SAME lease key,
    // so the partial unique index refuses it a second concurrent Lead
    // adjudication. Under lane scope both would open, which is the retry storm
    // the head-level hold exists to prevent.
    let contended = f
        .repository
        .open_tier2_lease(
            s,
            &provider_action_key(s, &queue_route, CiAction::RerunRun),
            &queue_route,
            &tier2_lease_key(s, 41, HEAD),
            CiTier2Reason::CausalFailure,
        )
        .await
        .expect("second lane");
    assert!(
        matches!(contended, CiTier2LeaseOutcome::KeyHeldElsewhere(_)),
        "a second lane opened a concurrent adjudication for one head: {contended:?}"
    );

    // A genuinely newer head is a different hold and may open.
    let mut newer = pr_head_identity(902);
    newer.pr_head_sha = OTHER_HEAD.to_owned();
    newer.run_head_sha = OTHER_HEAD.to_owned();
    f.repository
        .reserve(&reservation(s, &newer))
        .await
        .expect("reserve newer");
    let newer_lease = f
        .repository
        .open_tier2_lease(
            s,
            &provider_action_key(s, &newer, CiAction::RerunRun),
            &newer,
            &tier2_lease_key(s, 41, OTHER_HEAD),
            CiTier2Reason::CausalFailure,
        )
        .await
        .expect("newer head");
    assert!(matches!(newer_lease, CiTier2LeaseOutcome::Opened { .. }));
}

#[tokio::test]
async fn the_head_budget_a_route_derives_is_charged_once_per_lane_up_to_four() {
    // The head ceiling is shared ACROSS lanes, so the derivation must make two
    // different lanes land on ONE counter row. Four charges fill it and the
    // fifth is refused — proving the key, the ceiling, and the sharing at once.
    let f = route_fixture(1).await;
    let s = &f.subjects[0];

    let identities = [
        pr_head_identity(900),
        pr_head_identity(901),
        merge_group_identity(7, "grp@t0"),
        merge_group_identity(8, "grp@t1"),
        pr_head_identity(902),
    ];

    let mut charged = 0;
    for identity in &identities {
        // A distinct transient fingerprint per route, so only the HEAD ceiling
        // can be the thing that stops the fifth.
        let mut input = reservation(s, identity);
        let fingerprint = format!("fp-{}-{}", identity.run_id, identity.lane.as_str());
        input.transient_fingerprint = fingerprint.clone();
        input.retry_budget_key = retry_budget_key(s, identity, &fingerprint);

        match f.repository.reserve(&input).await.expect("reserve") {
            CiReserveOutcome::Reserved(_) => {}
            CiReserveOutcome::BudgetExhausted(counts) => {
                assert_eq!(counts.head, CI_HEAD_BUDGET_LIMIT);
                continue;
            }
            other => panic!("unexpected reservation outcome: {other:?}"),
        }
        if let djinn_db::CiChargeOutcome::Charged { .. } = f
            .repository
            .charge_and_begin_calling(s, &input.provider_action_key, "inc-1", identity)
            .await
            .expect("charge")
        {
            charged += 1;
        }
    }

    assert_eq!(
        charged, CI_HEAD_BUDGET_LIMIT,
        "the shared head ceiling must stop the fifth route on the same head"
    );
    let last = &identities[4];
    assert_eq!(
        f.repository
            .budget_counts(
                s,
                &retry_budget_key(s, last, "fp-902-pr_head"),
                &head_budget_key(s, last.pr_number, &last.pr_head_sha)
            )
            .await
            .expect("counts"),
        counts(0, CI_HEAD_BUDGET_LIMIT),
        "the refused route charged nothing, and the head counter is exactly at its ceiling"
    );
}

#[tokio::test]
async fn the_tier2_guard_keeps_a_live_calling_row_from_being_superseded() {
    // The hazard this guard exists for, driven end to end.
    //
    // `open_tier2_lease`'s supersession branch terminalizes through a
    // compare-and-set guarded only on `action_phase <> 'terminal'`, which
    // `calling` satisfies. Handing it a `calling` row plus a superseding
    // observed identity therefore flips the row to
    // `terminal / superseded_before_lead` while the owner's provider future is
    // still in flight, and the owner's `finalize_calling` — fenced on
    // `action_phase = 'calling'` — then returns `false`, silently discarding a
    // provider result that really was produced.
    //
    // Consulting `tier2_admission` first is what prevents that. If the guard
    // ever degrades to "always admit", this test fails on the phase assertion.
    let f = route_fixture(1).await;
    let s = &f.subjects[0];
    let identity = pr_head_identity(900);
    let input = reservation(s, &identity);

    f.repository.reserve(&input).await.expect("reserve");
    f.repository
        .charge_and_begin_calling(s, &input.provider_action_key, "owner-1", &identity)
        .await
        .expect("charge");

    let calling = f
        .repository
        .get(s, &input.provider_action_key)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(calling.action_phase, CiActionPhase::Calling);

    // The head moved while the provider call was in flight.
    let mut superseding = identity.clone();
    superseding.pr_head_sha = OTHER_HEAD.to_owned();

    match tier2_admission(&calling) {
        CiTier2Admission::ProviderCallInFlight => { /* refused, as it must be */ }
        admitted => {
            f.repository
                .open_tier2_lease(
                    s,
                    &input.provider_action_key,
                    &superseding,
                    &tier2_lease_key(s, 41, HEAD),
                    CiTier2Reason::CausalFailure,
                )
                .await
                .expect("lease");
            panic!("a live calling row was admitted to Tier 2: {admitted:?}");
        }
    }

    // The row is untouched and the owner can still record its result.
    let after = f
        .repository
        .get(s, &input.provider_action_key)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(after.action_phase, CiActionPhase::Calling);
    assert_eq!(after.owner_incarnation_id.as_deref(), Some("owner-1"));
    assert!(
        f.repository
            .finalize_calling(
                s,
                &input.provider_action_key,
                "owner-1",
                provider_finalization_outcome(CiLane::PrHead, true),
                None,
            )
            .await
            .expect("finalize"),
        "the owner's provider result must not be discardable by a Tier-2 sweep"
    );
    let finalized = f
        .repository
        .get(s, &input.provider_action_key)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        finalized.terminal_outcome,
        Some(CiRouteOutcome::Retriggered)
    );
}

#[tokio::test]
async fn the_head_level_hold_is_per_subject_and_this_records_it() {
    // The proposal states the retry-storm safeguard at PR-head level. The
    // durable scope is `(subject_kind, subject_id, tier2_lease_key)`, so what
    // is actually enforced is per subject PER head. Two subjects observing one
    // PR head each open a lease.
    //
    // Unreachable today — one PR head belongs to one task — and asserted here
    // so the gap is a measured fact rather than a worry, and so a future change
    // that makes it global fails loudly instead of silently changing behaviour.
    let f = route_fixture(2).await;
    let (a, b) = (&f.subjects[0], &f.subjects[1]);
    let identity = pr_head_identity(900);

    assert_ne!(
        tier2_lease_key(a, 41, HEAD),
        tier2_lease_key(b, 41, HEAD),
        "each subject derives its own lease key for one PR head"
    );

    for subject in [a, b] {
        f.repository
            .reserve(&reservation(subject, &identity))
            .await
            .expect("reserve");
        let opened = f
            .repository
            .open_tier2_lease(
                subject,
                &provider_action_key(subject, &identity, CiAction::RerunRun),
                &identity,
                &tier2_lease_key(subject, 41, HEAD),
                CiTier2Reason::CausalFailure,
            )
            .await
            .expect("lease");
        assert!(
            matches!(opened, CiTier2LeaseOutcome::Opened { .. }),
            "the hold is per subject, so the second subject also opens: {opened:?}"
        );
    }
}
