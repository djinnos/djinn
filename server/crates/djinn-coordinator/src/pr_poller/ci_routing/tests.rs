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
    CiLaneEvidence, CiMergeGroupCorrelationError, capture_merge_group_evidence,
    capture_pr_head_evidence, correlate_merge_group_run, dequeue_identity,
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
    CheckRunsResponse::complete(runs.to_vec())
}

fn refs(runs: &[CheckRun]) -> Vec<&CheckRun> {
    runs.iter().collect()
}

/// A capture built from a **provably complete** enumeration.
///
/// Tests go through `prove_complete` for the same reason production does: it is
/// the only constructor, and routing a hand-named `FailedComplete` past it is
/// how a non-terminal run would acquire a Tier-1 provider authorization.
fn complete<'a>(blocking: &'a [&'a CheckRun]) -> CiCapture<'a> {
    CiCapture::prove_complete(CheckSetCompleteness::Complete, blocking)
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

    let decision = classify(&observe(&id, &id, complete(&blocking)));

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

    let decision = classify(&observe(&id, &id, complete(&blocking)));

    assert_eq!(decision.class(), CiClass::Inconclusive);
    assert_eq!(decision.action(), CiAction::RerunRun);
    assert!(decision.tier2_reason().is_none());
    let action = decision
        .provider_action()
        .expect("Tier 1 authorizes exactly one provider mutation");
    // The call target is readable only through the scope. `CiProviderAction`
    // itself no longer exposes `kind()`/`run_id()`, which is what stops a
    // sibling module from calling the provider without ever entering the scope.
    let scope = ProviderActionScope::new();
    let admitted = action
        .admit(&scope)
        .expect("an open scope admits the Tier-1 action");
    assert_eq!(admitted.kind(), CiProviderActionKind::RerunFailedJobs);
    assert_eq!(admitted.run_id(), 900);
}

/// A closed scope yields no call target through the accessor route.
///
/// `None` carries neither the action kind nor the run id, so the sibling-module
/// executor that used to be writable (`rerun_failed_jobs(owner, repo,
/// action.run_id() as u64)`, no scope, clean compile) no longer has anything to
/// pass.
///
/// Scoped deliberately: this asserts the accessor route is closed, **not** that
/// the scope cannot be bypassed at all. `CiEvidenceIdentity::run_id` is a public
/// field on the `djinn-db` type the executor already holds, so a determined
/// caller can still reconstruct the target. See [`CiProviderAction::admit`] for
/// why that gap is left open and what it means.
#[test]
fn a_refused_admission_yields_no_call_target_at_all() {
    let checks = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&checks);
    let id = pr_head_identity(900);
    let decision = classify(&observe(&id, &id, complete(&blocking)));
    let action = decision
        .provider_action()
        .expect("Tier 1 authorizes a call");

    let scope = ProviderActionScope::new();
    scope.close_admission();

    assert!(
        action.admit(&scope).is_none(),
        "a closed scope must not hand out a call target"
    );
    assert_eq!(scope.counts().admitted_total, 0);
    assert_eq!(scope.counts().refused_total, 1);
}

#[test]
fn fully_inconclusive_merge_group_run_re_enqueues() {
    let checks = [ran_then_cancelled("merge-group / integration", 7)];
    let blocking = refs(&checks);
    let id = merge_group_identity(
        7,
        "refs/heads/gh-readonly-queue/main/pr-41-abc@2026-08-06T09:00:00Z",
    );

    let decision = classify(&observe(&id, &id, complete(&blocking)));

    assert_eq!(decision.class(), CiClass::Inconclusive);
    assert_eq!(decision.action(), CiAction::Reenqueue);
    let scope = ProviderActionScope::new();
    assert_eq!(
        decision
            .provider_action()
            .and_then(|a| a.admit(&scope))
            .map(|a| a.kind()),
        Some(CiProviderActionKind::EnableAutoMerge)
    );
}

/// Everything a *remediation* route can produce is absent.
///
/// Broader than [`assert_no_effects`]: it also denies the durable row and the
/// Tier-1 charge, which is what the proposal's "creates no remediation state"
/// phrase actually means. Expressed as capability absence so a later branch
/// that grows one of these fails here rather than passing quietly.
#[track_caller]
fn assert_outside_remediation(decision: &CiRouteDecision) {
    assert_no_effects(decision);
    assert!(
        !decision.creates_route_row(),
        "a route row is remediation state: {decision:?}"
    );
    assert!(
        !decision.consumes_tier1_charge(),
        "no Tier-1 charge may be consumed: {decision:?}"
    );
    assert!(!decision.closes_route(), "{decision:?}");
}

/// Rev 49's PR-head complete-empty row.
///
/// The draft lane already has a no-CI path — `PrDraftCiAction::Proceed` after
/// the minimum-age guard — and this evidence must land on it rather than on any
/// remediation route. Wave 2 sent it to Tier 2 on the reasoning that "a red run
/// with nothing blocking is unexplained"; rev 49 supersedes that, because on a
/// repository with no CI configured *every* poll produces this shape, and Tier 2
/// there is a Lead session per poll for a repository that has no CI to fix.
#[test]
fn complete_empty_pr_head_proceeds_after_minimum_age() {
    let id = pr_head_identity(900);
    let decision = classify(&observe(&id, &id, complete(&[])));

    assert_eq!(decision.rationale(), CiRouteRationale::EmptyBlockingSet);
    assert_eq!(
        decision.complete_empty_route(),
        Some(CiCompleteEmptyRoute::PrHeadProceed),
        "the draft lane's compatibility path is Proceed-after-min-age, and the \
         guard itself stays where it already lives",
    );
    assert_outside_remediation(&decision);

    // The lane executor's own no-CI branch and this route agree on which action
    // to take. Asserting the mapping, not just the enum name, is what stops the
    // two from drifting.
    assert_eq!(
        crate::pr_poller::decide_pr_draft_ci_action(djinn_core::models::CiStatus::Unknown, false),
        crate::pr_poller::PrDraftCiAction::Proceed {
            needs_passing_persist: true
        },
    );
}

/// Rev 49's merge-group complete-empty row.
///
/// The review lane differs from the draft lane and the proposal is explicit
/// that they must not be collapsed. A `pr_review` PR has already cleared the
/// draft minimum-age guard, so there is nothing left to wait for; the existing
/// path persists `Passing` for the current head, and the merge gate — which
/// maps `Unknown` to `Hold` — is then free to progress. Recording nothing here
/// would wedge every no-CI repository in `pr_review` permanently.
#[test]
fn complete_empty_merge_group_records_passing_and_allows_gate() {
    let id = merge_group_identity(
        7,
        "refs/heads/gh-readonly-queue/main/pr-41-abc@2026-08-06T09:00:00Z",
    );
    let decision = classify(&observe(&id, &id, complete(&[])));

    assert_eq!(decision.rationale(), CiRouteRationale::EmptyBlockingSet);
    assert_eq!(
        decision.complete_empty_route(),
        Some(CiCompleteEmptyRoute::MergeGroupRecordPassing),
    );
    assert_outside_remediation(&decision);

    // `Passing` is what unblocks the existing gate; the no-CI-shaped `Unknown`
    // that a bare `record_ci_snapshot` would leave behind holds it forever.
    // That is the fact this route depends on, so it is asserted rather than
    // assumed.
    use crate::pr_poller::ci_helpers::{CiMergeGateVerdict, ci_merge_gate_verdict};
    use djinn_core::models::CiStatus;
    assert_eq!(
        ci_merge_gate_verdict(Some(&gate_snapshot(CiStatus::Passing)), HEAD),
        CiMergeGateVerdict::Allow
    );
    assert_eq!(
        ci_merge_gate_verdict(Some(&gate_snapshot(CiStatus::Unknown)), HEAD),
        CiMergeGateVerdict::Hold,
        "recording nothing leaves `Unknown`, which holds forever on a no-CI repo",
    );
}

/// A current-head snapshot carrying only the status under test.
fn gate_snapshot(ci_status: djinn_core::models::CiStatus) -> djinn_core::models::TaskPrCiSnapshot {
    djinn_core::models::TaskPrCiSnapshot {
        task_id: SUBJECT_A.to_owned(),
        pr_number: 41,
        head_sha: HEAD.to_owned(),
        ci_status,
        ..Default::default()
    }
}

/// The two lanes are not the same route, and the classifier is what keeps them
/// apart. A single collapsed "complete empty" answer would send the review lane
/// down the draft lane's minimum-age path — which it has already passed — or
/// the draft lane down a `Passing` persist it has not yet earned.
#[test]
fn the_two_complete_empty_lanes_do_not_collapse() {
    let pr_head = pr_head_identity(900);
    let merge_group = merge_group_identity(7, "refs/heads/gh-readonly-queue/main/pr-41-abc@t");

    let a = classify(&observe(&pr_head, &pr_head, complete(&[])));
    let b = classify(&observe(&merge_group, &merge_group, complete(&[])));

    assert_ne!(a.complete_empty_route(), b.complete_empty_route());
}

/// Complete-empty is still evidence about *a head*, and a superseded head
/// cannot drive a lane to green.
#[test]
fn stale_complete_empty_is_discarded_rather_than_proceeding() {
    let evidence = pr_head_identity(900);
    let current = CiEvidenceIdentity {
        pr_head_sha: OTHER_HEAD.to_owned(),
        ..pr_head_identity(900)
    };

    let decision = classify(&observe(&evidence, &current, complete(&[])));

    assert_eq!(decision.action(), CiAction::Discard);
    assert_eq!(
        decision.rationale(),
        CiRouteRationale::Stale(CiStaleField::PrHeadSha)
    );
    assert!(
        decision.complete_empty_route().is_none(),
        "a stale head must not reach either lane's no-CI fast path: {decision:?}"
    );
    assert_no_effects(&decision);
}

/// The seal, stated as a test.
///
/// A failed *first* page returns zero runs with `total_count: 0` — byte-for-byte
/// the shape of a repository with no CI. The only thing separating them is the
/// provider's verdict, and `prove_complete` is where that verdict is consulted.
/// If it were ever bypassed, an outage on page 1 would drive both lanes to
/// green.
#[test]
fn incomplete_enumeration_cannot_masquerade_as_complete_empty() {
    let id = pr_head_identity(900);

    for reason in [
        CheckSetIncompleteReason::PageFetchFailed,
        CheckSetIncompleteReason::MaxPagesTruncated,
        CheckSetIncompleteReason::ShortRead,
    ] {
        let capture = CiCapture::prove_complete(CheckSetCompleteness::Incomplete(reason), &[]);
        assert!(
            !capture.is_complete_empty(),
            "{reason:?} must not read as complete-empty"
        );

        let decision = classify(&observe(&id, &id, capture));
        assert!(
            decision.complete_empty_route().is_none(),
            "{reason:?} reached a lane no-CI fast path: {decision:?}"
        );
        assert!(
            !decision.consumes_tier1_charge(),
            "{reason:?}: {decision:?}"
        );
        assert!(
            decision.provider_action().is_none(),
            "{reason:?}: {decision:?}"
        );
        // Truncation is the one reason that earns a route row, because it is
        // the one that cannot be re-polled away. The rest spend nothing.
        if reason == CheckSetIncompleteReason::MaxPagesTruncated {
            assert!(decision.creates_route_row(), "{reason:?}: {decision:?}");
        } else {
            assert!(!decision.creates_route_row(), "{reason:?}: {decision:?}");
            assert_no_effects(&decision);
        }
    }
}

/// The seal's other half: a set of checks that have not finished cannot be
/// proven complete, whatever the caller believes.
///
/// This is the concrete hazard. `is_inconclusive` is "no member is causal", and
/// an `in_progress` check is not causal *yet* — so a hand-built complete capture
/// over running checks classifies as Tier 1 and authorizes `rerun_failed_jobs`
/// against a run that is still going.
#[test]
fn a_running_check_set_cannot_be_proven_complete() {
    let mut running = ran_then_cancelled("Quality Gate / test (1)", 900);
    running.status = "in_progress".to_owned();
    running.conclusion = None;
    let blocking = vec![&running];

    // The trap, demonstrated: were this set accepted as complete, the existing
    // Tier-1 predicate would say yes.
    assert!(crate::pr_poller::ci_triage::is_inconclusive(&blocking));

    let capture = CiCapture::prove_complete(CheckSetCompleteness::Complete, &blocking);
    let id = pr_head_identity(900);
    let decision = classify(&observe(&id, &id, capture));

    assert_eq!(decision.action(), CiAction::Hold);
    assert_eq!(
        decision.rationale(),
        CiRouteRationale::Pending(CiPendingReason::RequiredCheckNonTerminal),
    );
    assert!(
        decision.provider_action().is_none(),
        "a live run must never carry provider-mutation authority: {decision:?}"
    );
    assert!(!decision.creates_route_row(), "{decision:?}");
}

#[test]
fn a_passing_or_merged_observation_creates_no_route() {
    let id = pr_head_identity(900);
    for (capture, rationale) in [
        (CiCapture::passing(), CiRouteRationale::NewerPass),
        (CiCapture::merged(), CiRouteRationale::NewerMerge),
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
    for capture in [CiCapture::passing(), CiCapture::merged()] {
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
        let decision = classify(&observe(&id, &id, CiCapture::non_terminal(reason)));
        assert_eq!(decision.action(), CiAction::Hold, "{reason:?}");
        assert_eq!(decision.rationale(), CiRouteRationale::Pending(reason));
        // A hold reaches neither the provider NOR Lead: the snapshot is not
        // terminal, so there is nothing complete to adjudicate.
        assert_no_effects(&decision);
    }
}

/// Every reason the classifier can be handed, listed exhaustively.
///
/// The `match` is what makes it exhaustive: adding a variant to
/// [`CiIncompleteReason`] fails to compile here until it is placed on one side
/// of the partition, which is the only thing that stops a new reason from
/// silently inheriting whichever default the classifier happens to have.
const EVERY_INCOMPLETE_REASON: [CiIncompleteReason; 11] = [
    CiIncompleteReason::MissingStartTimestamp,
    CiIncompleteReason::MissingCompletionTimestamp,
    CiIncompleteReason::MalformedExecutionInterval,
    CiIncompleteReason::NonPositiveExecutionInterval,
    CiIncompleteReason::EnumerationPageFailed,
    CiIncompleteReason::CheckEnumerationUnavailable,
    CiIncompleteReason::PartialPagination,
    CiIncompleteReason::CheckApiError,
    CiIncompleteReason::LogApiError,
    CiIncompleteReason::AmbiguousMergeGroupCorrelation,
    CiIncompleteReason::MergeGroupCorrelationUnavailable,
];

/// Which side of the proposal's partition a reason belongs to, spelled out
/// independently of the implementation's `is_enumeration_failure` so the test
/// is a second opinion rather than a restatement.
fn expected_to_hold(reason: CiIncompleteReason) -> bool {
    match reason {
        // "Any failed page ... or collected count below reported
        // `total_count` → Hold without a route row." Both are transient
        // provider facts that a later poll can resolve for free.
        CiIncompleteReason::EnumerationPageFailed | CiIncompleteReason::PartialPagination => true,
        // `MAX_PAGES` truncation is deliberately NOT on the hold side, and the
        // divergence from the proposal's sentence is the point.
        //
        // A hold's premise is "a later poll turns this into an evidence bundle
        // for free". That is true of a failed page and of a short read. It is
        // false of truncation: the PR genuinely has more check runs than
        // `MAX_PAGES * PER_PAGE`, which is a property of the PR and not of the
        // moment, so every subsequent enumeration returns the identical
        // verdict. Held, such a PR's CI gate never resolves — no route row, no
        // adjudication, no board signal, forever.
        //
        // It is complete-but-unusable current evidence instead: a real
        // enumeration whose contents no automatic action can read. That is the
        // guarded Tier-2 row, and the head-scoped lease bounds it to one Lead
        // adjudication per PR head rather than one per poll.
        CiIncompleteReason::CheckEnumerationUnavailable => false,
        // "Complete snapshot with missing or malformed timestamps, check/log
        // API error after an immutable run is known, or ambiguous merge-group
        // correlation → Tier 2 after the current-identity guard."
        CiIncompleteReason::MissingStartTimestamp
        | CiIncompleteReason::MissingCompletionTimestamp
        | CiIncompleteReason::MalformedExecutionInterval
        | CiIncompleteReason::NonPositiveExecutionInterval
        | CiIncompleteReason::CheckApiError
        | CiIncompleteReason::LogApiError
        | CiIncompleteReason::AmbiguousMergeGroupCorrelation
        | CiIncompleteReason::MergeGroupCorrelationUnavailable
        | CiIncompleteReason::RunAttributionUnavailable => false,
    }
}

/// The partition, asserted end to end.
///
/// Both sides refuse the provider. They differ in what they are allowed to
/// *spend*: an enumeration failure spends nothing at all, while complete-but-
/// unusable evidence has a constructible immutable identity and therefore earns
/// one deduplicated Lead adjudication.
#[test]
fn every_incomplete_evidence_case_fails_closed_without_provider_mutation() {
    let id = pr_head_identity(900);
    for reason in EVERY_INCOMPLETE_REASON
        .into_iter()
        .chain([CiIncompleteReason::RunAttributionUnavailable])
    {
        let decision = classify(&observe(&id, &id, CiCapture::incomplete(reason)));
        assert_eq!(decision.class(), CiClass::Unknown, "{reason:?}");
        assert_eq!(
            decision.rationale(),
            CiRouteRationale::IncompleteEvidence(reason)
        );
        assert!(
            decision.provider_action().is_none(),
            "{reason:?} carried provider-mutation authority: {decision:?}",
        );

        if expected_to_hold(reason) {
            assert_eq!(decision.action(), CiAction::Hold, "{reason:?}");
            assert!(!decision.creates_route_row(), "{reason:?}: {decision:?}");
            assert_no_effects(&decision);
        } else {
            assert_lead_only(&decision, CiTier2Reason::EvidenceUnknown);
            assert!(decision.creates_route_row(), "{reason:?}: {decision:?}");
        }
    }
}

/// The completeness gate reaches the *existing* merge gate, not just the new
/// route — and it is not behind `ci_evidence_routing`.
///
/// # This test used to be a label
///
/// It asserted `CheckRunsResponse::incomplete(...).completeness.is_complete()`
/// is false — a fact about a constructor — and put the claim it cared about
/// ("the draft lane's no-CI branch and the review lane's `Passing` persist are
/// both gated on this") in an *assertion message*. Deleting the completeness
/// clause from either production branch left it green, because it never touched
/// either branch.
///
/// Both branches now call one predicate,
/// [`ci_helpers::empty_check_set_is_authoritatively_green`], and this drives
/// that predicate. Removing the `completeness` conjunct from it fails here, and
/// there is no second copy of the expression to drift.
#[test]
fn incomplete_check_set_never_records_passing() {
    use crate::pr_poller::ci_helpers::empty_check_set_is_authoritatively_green;

    // The failed-first-page shape: zero runs, `total_count: 0` — byte-identical
    // to a repository with no CI, which is the fast path to green.
    let failed_first_page =
        CheckRunsResponse::incomplete(0, Vec::new(), CheckSetIncompleteReason::PageFetchFailed);
    assert!(failed_first_page.check_runs.is_empty());
    assert!(
        !empty_check_set_is_authoritatively_green(&failed_first_page),
        "a failed first page must not reach either lane's no-CI fast path",
    );

    // Every other way an enumeration can fail to prove itself, with the same
    // empty shape. None of them may authorize green.
    for reason in [
        CheckSetIncompleteReason::PageFetchFailed,
        CheckSetIncompleteReason::MaxPagesTruncated,
        CheckSetIncompleteReason::ShortRead,
    ] {
        let empty_but_unproven = CheckRunsResponse::incomplete(0, Vec::new(), reason);
        assert!(
            !empty_check_set_is_authoritatively_green(&empty_but_unproven),
            "{reason:?} authorized the fast path to green",
        );
    }

    // The all-green-prefix shape: every fetched member passed, but the walk
    // stopped early. It is not empty, so the fast path never applies — and the
    // snapshot writer's own gate turns it into `Unknown` rather than `Passing`.
    let runs = [make_passing_check("Quality Gate / build")];
    let short_read =
        CheckRunsResponse::incomplete(9, runs.to_vec(), CheckSetIncompleteReason::ShortRead);
    assert!(
        short_read
            .check_runs
            .iter()
            .all(|cr| cr.conclusion.as_deref() == Some("success")),
    );
    assert!(!empty_check_set_is_authoritatively_green(&short_read));
    assert!(
        short_read.completeness.incomplete_reason().is_some(),
        "`record_ci_snapshot` returns Unknown on exactly this predicate",
    );

    // And the genuine no-CI shape, which must still be allowed through — the
    // gate has to be a filter, not a wall. A no-CI repository that stopped
    // going green would wedge every task in `pr_draft` forever.
    let genuinely_empty = CheckRunsResponse::complete(Vec::new());
    assert!(
        empty_check_set_is_authoritatively_green(&genuinely_empty),
        "an authoritatively complete empty enumeration is still green",
    );

    // A complete enumeration that is *not* empty never takes the fast path
    // either: it has checks to classify, and classification is the other path.
    assert!(!empty_check_set_is_authoritatively_green(
        &CheckRunsResponse::complete(runs.to_vec())
    ));
}

fn make_passing_check(name: &str) -> CheckRun {
    let mut cr = causal(name, 900);
    cr.conclusion = Some("success".to_owned());
    cr
}

// ---------------------------------------------------------------------------
// The provider-action scope
// ---------------------------------------------------------------------------

/// The lane executor's route to the provider runs through the scope, and a
/// closed scope has no route at all.
///
/// This is the "cancellation → action drain → lock release" order expressed
/// where it is enforceable in this crate. Leadership releases the coordinator
/// advisory lock only after the scope reports a graceful drain, and the scope
/// cannot report one while admission is open. So a Tier-1 decision that arrives
/// after admission closed has nothing to call the provider *with* — the call
/// target only exists on an admitted action.
#[test]
fn a_closed_action_scope_denies_the_tier_one_call_target() {
    let checks = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&checks);
    let id = pr_head_identity(900);
    let decision = classify(&observe(&id, &id, complete(&blocking)));

    let action = decision
        .provider_action()
        .expect("Tier 1 authorizes one provider mutation");

    let scope = ProviderActionScope::new();
    {
        let admitted = action
            .admit(&scope)
            .expect("an open scope admits the action");
        assert_eq!(admitted.kind(), CiProviderActionKind::RerunFailedJobs);
        assert_eq!(admitted.run_id(), 900);
        assert_eq!(
            scope.in_flight(),
            1,
            "an admitted action is visible to the drain",
        );
    }
    assert_eq!(scope.in_flight(), 0, "dropping the action leaves the scope");

    // Now the drain sequence's first step.
    scope.close_admission();
    assert!(
        action.admit(&scope).is_none(),
        "a route classified Tier 1 after admission closed must not reach the provider",
    );

    // And the decision itself is unchanged — closing admission suppresses the
    // *call*, it does not reclassify the evidence. The row this decision would
    // have opened simply never enters `calling`, which is what leaves the
    // charge unspent for the next leader.
    assert_eq!(decision.action(), CiAction::RerunRun);
}

/// The scope is the join between the two halves of the handoff proof, and
/// "empty" is not the signal leadership waits on.
#[tokio::test]
async fn the_drain_signal_is_the_stamp_not_the_emptiness() {
    let scope = ProviderActionScope::new();
    scope.close_admission();
    scope.wait_until_empty().await;

    assert!(
        !scope
            .wait_until_drained(std::time::Duration::from_millis(50))
            .await,
        "an empty but unstamped scope is not a graceful handoff — releasing the \
         advisory lock here would hand a new incarnation a proof it does not have",
    );

    scope.mark_drained();
    assert!(
        scope
            .wait_until_drained(std::time::Duration::from_secs(5))
            .await
    );
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
        let decision = classify(&observe(&evidence, &current, complete(&blocking)));
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
        complete(&blocking),
        complete(&[]),
        CiCapture::incomplete(CiIncompleteReason::CheckApiError),
        CiCapture::non_terminal(CiPendingReason::RunNonTerminal),
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
    classify(&observe(&id, &id, complete(&blocking)))
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

    let causal_route = classify(&observe(&evidence, &evidence, complete(&blocking)));
    let discarded = classify(&observe(&evidence, &stale_current, complete(&blocking)));
    let held = classify(&observe(
        &evidence,
        &evidence,
        CiCapture::non_terminal(CiPendingReason::RunNonTerminal),
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

/// Every way the enumeration can fail holds, and none of them creates
/// remediation state.
///
/// The verdict comes from the provider, not from re-deriving `collected <
/// total_count` here: the short-read row below is the only one that comparison
/// would catch, and the failed-page row is the one where it is actively wrong.
#[test]
fn incomplete_check_set_holds_without_route_or_session() {
    let runs = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&runs);
    let id = pr_head_identity(900);

    for (provider_reason, routing_reason, total, collected) in [
        (
            CheckSetIncompleteReason::PageFetchFailed,
            CiIncompleteReason::EnumerationPageFailed,
            0,
            0,
        ),
        (
            CheckSetIncompleteReason::MaxPagesTruncated,
            CiIncompleteReason::CheckEnumerationUnavailable,
            1,
            1,
        ),
        (
            CheckSetIncompleteReason::ShortRead,
            CiIncompleteReason::PartialPagination,
            7,
            1,
        ),
    ] {
        let response =
            CheckRunsResponse::incomplete(total, runs[..collected].to_vec(), provider_reason);

        match capture_pr_head_evidence(41, HEAD, &response, &blocking) {
            CiLaneEvidence::Incomplete(got) => assert_eq!(got, routing_reason),
            other => panic!("expected {routing_reason:?}, got {other:?}"),
        }

        let decision = classify(&observe(&id, &id, CiCapture::incomplete(routing_reason)));

        assert_eq!(
            decision.rationale(),
            CiRouteRationale::IncompleteEvidence(routing_reason),
        );
        // Whatever else it does, an incomplete enumeration never reaches the
        // provider and never charges a retry slot.
        assert!(decision.provider_action().is_none(), "{decision:?}");
        assert!(!decision.consumes_tier1_charge(), "{decision:?}");

        if expected_to_hold(routing_reason) {
            assert_eq!(
                decision.action(),
                CiAction::Hold,
                "{routing_reason:?} must hold, not adjudicate: {decision:?}",
            );
            assert!(
                !decision.creates_route_row(),
                "{routing_reason:?} created a route row: {decision:?}",
            );
            assert_no_effects(&decision);
        } else {
            // Truncation. It cannot be re-polled into completeness, so it takes
            // the guarded, deduplicated Tier-2 route instead of an endless hold.
            assert_lead_only(&decision, CiTier2Reason::EvidenceUnknown);
        }
    }
}

/// Repeated incomplete polls, including across a restart, stay in the same
/// negative space — and then a complete snapshot classifies normally.
///
/// The point is the *absence of an accumulator*. A retry counter or a synthetic
/// observation identity keyed on incomplete evidence is explicitly out of
/// scope, so nothing may change between poll 1 and poll 50: the decision is a
/// pure function of the capture, and the same capture must give the same route
/// forever.
#[test]
fn repeated_incomplete_polls_remain_side_effect_free() {
    let id = pr_head_identity(900);
    let incomplete = CiCapture::incomplete(CiIncompleteReason::EnumerationPageFailed);

    let first = classify(&observe(&id, &id, incomplete));
    for _ in 0..50 {
        let again = classify(&observe(&id, &id, incomplete));
        assert_eq!(
            again, first,
            "an incomplete poll accumulated state across polls",
        );
        assert!(!again.creates_route_row());
        assert!(!again.consumes_tier1_charge());
        assert_no_effects(&again);
    }

    // A restart changes nothing: there is no in-memory or durable counter for
    // the classifier to have lost or kept.
    assert_eq!(classify(&observe(&id, &id, incomplete)), first);

    // And once the provider recovers, the same identity classifies normally.
    let runs = [ran_then_cancelled("Quality Gate / test (1)", 900)];
    let blocking = refs(&runs);
    let recovered = classify(&observe(&id, &id, complete(&blocking)));
    assert_eq!(recovered.action(), CiAction::RerunRun);
    assert!(recovered.creates_route_row());
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
            super::blocking_evidence_completeness(&blocking),
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
        super::blocking_evidence_completeness(&blocking),
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
    assert_eq!(super::blocking_evidence_completeness(&blocking), None);

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
        Some(CiMergeGroupCorrelationError::Unusable(
            CiIncompleteReason::MergeGroupCorrelationUnavailable
        ))
    );
    // A neighbouring PR number must not correlate: the trailing dash in the
    // `pr-41-` marker is what keeps `pr-411-` out.
    assert_eq!(
        correlate_merge_group_run(
            41,
            &[run(1, "gh-readonly-queue/main/pr-411-abc", "failure")]
        )
        .err(),
        Some(CiMergeGroupCorrelationError::Unusable(
            CiIncompleteReason::MergeGroupCorrelationUnavailable
        ))
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
        Some(CiMergeGroupCorrelationError::Unusable(
            CiIncompleteReason::AmbiguousMergeGroupCorrelation
        ))
    );
}

/// A merge-group run that correlates perfectly but has not finished is the
/// proposal's **Hold** row, not Tier 2.
///
/// Wave 2 returned `MergeGroupCorrelationUnavailable` here — an
/// unknown-*evidence* reason, which the classifier fails closed to a guarded
/// Tier 2. That spends a route row and a Lead session on a run that is simply
/// still running, and which the very next poll would have classified for free.
/// The proposal's table says "run or any required check is pending/non-terminal
/// → Hold; wait for a terminal snapshot, do not classify".
#[test]
fn a_non_terminal_merge_group_run_holds_rather_than_asking_lead() {
    let running = djinn_provider::github_api::WorkflowRun {
        id: 9,
        workflow_id: None,
        name: None,
        path: None,
        head_branch: Some("gh-readonly-queue/main/pr-41-abc".to_owned()),
        head_sha: "9".repeat(40),
        status: Some("in_progress".to_owned()),
        conclusion: Some("failure".to_owned()),
    };

    let err = correlate_merge_group_run(41, std::slice::from_ref(&running))
        .expect_err("a non-terminal run cannot be correlated evidence");
    assert_eq!(
        err,
        CiMergeGroupCorrelationError::NotTerminal(CiPendingReason::RunNonTerminal),
    );

    // And the route it produces spends nothing.
    let id = merge_group_identity(9, "refs/heads/gh-readonly-queue/main/pr-41-abc@t");
    let evidence: CiLaneEvidence<'_> = err.into();
    let capture = evidence
        .lane_capture()
        .expect("a non-run lane evidence always yields a capture");
    let decision = classify(&observe(&id, &id, capture));

    assert_eq!(decision.action(), CiAction::Hold);
    assert_eq!(
        decision.rationale(),
        CiRouteRationale::Pending(CiPendingReason::RunNonTerminal),
    );
    assert!(!decision.creates_route_row(), "{decision:?}");
    assert_no_effects(&decision);
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
        lead_rejection: None,
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
