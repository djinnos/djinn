//! The atomic apply guard, against a real route table (proposal `nafu`,
//! wave 5).
//!
//! # Why these are not the sibling `tests.rs`
//!
//! Wave 4's fixtures pass a `CiGuardOutcome` in by hand. That proves
//! [`board_effect`] collapses correctly *given* an answer, and it is worth
//! having — but it cannot prove the answer is ever produced, and in wave 4 it
//! never was: `resolve_tier2_lease` had zero production callers workspace-wide
//! and the whole application half was `#[allow(dead_code)]`.
//!
//! Every fixture here goes through [`apply_under_guard`] against an ephemeral
//! Postgres carrying a real reserved route with a real open lease, so the
//! guard's verdict comes from the repository's compare-and-set rather than from
//! the test. That is the difference between "the guard works when it says no"
//! and "the guard says no when it should".
//!
//! # The mutation these are written to survive
//!
//! Make the guard always pass — replace `CiGuardOutcome::from_resolve(applied)`
//! with `CiGuardOutcome::Current`, or make the repository's identity comparison
//! a tautology — and [`a_moved_head_applies_nothing`],
//! [`a_merged_pr_applies_nothing`] and
//! [`a_resolved_lease_cannot_be_resolved_twice`] must all fail. They assert on
//! the **durable row and the counted effects**, not on the name of the returned
//! variant, so a mutation that merely renames the outcome does not slip past.
//!
//! The other half of the same contract is the guard that could not be
//! *evaluated*. `apply_under_guard` maps a repository `Err` to "applied
//! nothing", and until [`a_repository_error_applies_nothing`] existed, flipping
//! that arm to "applied" left the whole crate green: the three fixtures above
//! all reach a repository that answers, so none of them exercises the branch
//! where it cannot.

use djinn_db::{
    CiAction, CiActionPhase, CiClass, CiEvidenceIdentity, CiLane, CiOriginState, CiReserveOutcome,
    CiRouteAttempt, CiRouteAttemptRepository, CiRouteOutcome, CiRouteReservation, CiRouteSubject,
    CiTier2LeaseOutcome, CiTier2Reason, Database,
};

use super::*;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MOVED_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PR: i64 = 4242;
const RUN: i64 = 90210;
const KEY: &str = "pak-guard-fixture";
const RUN_REF: &str = "90210";

struct Guarded {
    db: Database,
    routes: CiRouteAttemptRepository,
    subject: CiRouteSubject,
    lease_id: String,
    identity: CiEvidenceIdentity,
}

fn identity(head: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: head.to_owned(),
        run_id: RUN,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

/// A reserved route with an open Tier-2 lease, exactly as the coordinator
/// leaves it before dispatching Lead.
async fn guarded() -> Guarded {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project =
        djinn_db::test_support::make_project(&db, std::path::Path::new("ci-apply-guard")).await;
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
    let subject = CiRouteSubject::task(task_id);
    let routes = CiRouteAttemptRepository::new(db.clone());
    let identity = identity(HEAD);

    let reserved = routes
        .reserve(&CiRouteReservation {
            subject: subject.clone(),
            provider_action_key: KEY.to_owned(),
            identity: identity.clone(),
            origin_state: CiOriginState::PrDraft,
            class: CiClass::CausalFailure,
            action: CiAction::AskLead,
            transient_fingerprint: "fp".to_owned(),
            retry_budget_key: "rbk".to_owned(),
            head_budget_key: "hbk".to_owned(),
        })
        .await
        .expect("reserve");
    assert!(matches!(reserved, CiReserveOutcome::Reserved(_)));

    let lease = routes
        .open_tier2_lease(
            &subject,
            KEY,
            &identity,
            "lease-key",
            CiTier2Reason::CausalFailure,
        )
        .await
        .expect("lease");
    let CiTier2LeaseOutcome::Opened { lease_id, .. } = lease else {
        panic!("expected an opened lease, got {lease:?}");
    };

    Guarded {
        db,
        routes,
        subject,
        lease_id,
        identity,
    }
}

impl Guarded {
    fn context(&self) -> CiAdjudicationContext {
        CiAdjudicationContext {
            lane: CiLane::PrHead,
            origin_state: CiOriginState::PrDraft,
            guard: CiRouteGuardKeys {
                subject: self.subject.clone(),
                provider_action_key: KEY.to_owned(),
                tier2_lease_id: self.lease_id.clone(),
                identity: self.identity.clone(),
            },
            tier2_reason: CiTier2Reason::CausalFailure,
            repository_commands: vec!["cargo test -p djinn-db".to_owned()],
            evidence_references: vec![RUN_REF.to_owned(), HEAD.to_owned()],
        }
    }

    async fn row(&self) -> CiRouteAttempt {
        self.routes
            .get(&self.subject, KEY)
            .await
            .expect("route read")
            .expect("route row exists")
    }

    async fn worker_attempts(&self) -> i64 {
        djinn_db::test_support::task_attempt_count_for_test(&self.db, &self.subject.id).await
    }

    async fn task_status(&self) -> String {
        djinn_db::test_support::task_status_for_test(&self.db, &self.subject.id).await
    }
}

/// A valid repair, cited and with a repository-valid command.
fn repair(ctx: &CiAdjudicationContext) -> CiAdjudication {
    let payload = serde_json::json!({
        "decision": "reopen",
        "directive": format!("run {RUN_REF} fails in the db crate; add the missing re-export"),
        "verification_command": "cargo test -p djinn-db",
    });
    let adjudication = adjudicate(ctx, LeadResponse::Submitted(&payload));
    assert!(
        adjudication.rejection.is_none(),
        "the fixture payload must be accepted as submitted, got {:?}",
        adjudication.rejection
    );
    adjudication
}

// ---------------------------------------------------------------------------
// The guard holds
// ---------------------------------------------------------------------------

/// The current-identity path: one board transition, exactly one worker, and a
/// durable `repair_reopened`.
#[tokio::test]
async fn a_current_route_applies_exactly_one_reopen() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);

    let effect = apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        &CiObservedNow {
            pr_head_sha: HEAD.to_owned(),
        },
    )
    .await;

    let counts = effect.counts();
    assert_eq!(counts.board_transitions, 1);
    assert_eq!(counts.worker_dispatches, 1, "exactly one worker, not two");

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened),
        "the guard's transaction must have persisted the resolution"
    );
    assert_eq!(row.reopen_mode, Some(djinn_db::CiReopenMode::Repair));
    assert_eq!(row.action_phase, CiActionPhase::Terminal);
    assert!(
        !row.holds_open_tier2_lease(),
        "resolving the lease must release the current-evidence key"
    );
    assert_eq!(
        row.lead_rejection, None,
        "a result Lead actually produced carries no rejection"
    );

    assert!(matches!(
        stage_outcome_after_guard(&adjudication.plan, &effect, "unused"),
        StageOutcome::LeadReopen { .. }
    ));
}

// ---------------------------------------------------------------------------
// The guard refuses
// ---------------------------------------------------------------------------

/// The head moved while Lead was thinking. Nothing may happen.
///
/// **Mutation target.** Force the guard to pass and this fails on the effect
/// counts *and* on the durable outcome, which are two independent witnesses.
#[tokio::test]
async fn a_moved_head_applies_nothing() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);
    let status_before = g.task_status().await;
    let attempts_before = g.worker_attempts().await;

    let effect = apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        &CiObservedNow {
            pr_head_sha: MOVED_HEAD.to_owned(),
        },
    )
    .await;

    assert!(effect.is_noop(), "a stale head may apply nothing");
    let counts = effect.counts();
    assert_eq!(counts.board_transitions, 0, "no reopen");
    assert_eq!(counts.worker_dispatches, 0, "no worker dispatch");

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::SupersededBeforeApply),
        "the repository must close the obsolete attempt in the same transaction"
    );
    assert_eq!(
        row.reopen_mode, None,
        "a superseded attempt records no reopen mode"
    );
    assert!(
        row.superseded_by_evidence.is_some(),
        "the identity that defeated the compare-and-set must be recorded"
    );

    assert_eq!(
        g.task_status().await,
        status_before,
        "no board mutation: the task status must be untouched"
    );
    assert_eq!(
        g.worker_attempts().await,
        attempts_before,
        "no worker dispatch: no task attempt may be created"
    );

    // And the stage outcome the supervisor receives is the inert one, which is
    // what makes "nothing happens" true on the board side too.
    assert!(matches!(
        stage_outcome_after_guard(&adjudication.plan, &effect, "stale"),
        StageOutcome::LeadRouteSuperseded { .. }
    ));
}

/// A merge landed while Lead was thinking. Same answer, different mechanism:
/// `close_routes_for_newer_outcome` resolved the lease, so the guard refuses on
/// the lease-state check before it ever compares identities.
///
/// **Mutation target.** This is the fixture a guard weakened to "compare heads
/// only" would still pass and a guard deleted entirely would not.
#[tokio::test]
async fn a_merged_pr_applies_nothing() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);

    g.routes
        .close_routes_for_newer_outcome(&g.subject, PR, CiRouteOutcome::Merged, None)
        .await
        .expect("close on merge");

    let effect = apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        // The head has NOT moved. Only the merge outranks this route.
        &CiObservedNow {
            pr_head_sha: HEAD.to_owned(),
        },
    )
    .await;

    assert!(effect.is_noop(), "a merged PR outranks a pending route");
    let row = g.row().await;
    assert_ne!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened),
        "a merged PR must not be reopened for rework"
    );
    assert_eq!(g.worker_attempts().await, 0, "no worker dispatch");
}

/// One lease, one adjudication. A second apply against the same lease id is a
/// no-op, so a duplicated delivery cannot dispatch a second worker.
#[tokio::test]
async fn a_resolved_lease_cannot_be_resolved_twice() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);
    let observed = CiObservedNow {
        pr_head_sha: HEAD.to_owned(),
    };

    let first = apply_under_guard(&g.routes, &ctx, &adjudication, &observed).await;
    assert_eq!(first.counts().worker_dispatches, 1);

    let second = apply_under_guard(&g.routes, &ctx, &adjudication, &observed).await;
    assert!(
        second.is_noop(),
        "the lease is resolved; a replayed result must apply nothing"
    );
    assert_eq!(
        first.counts().worker_dispatches + second.counts().worker_dispatches,
        1,
        "two deliveries of one adjudication dispatch exactly one worker in total"
    );
}

/// The guard could not be **evaluated**. Nothing may happen either — and the
/// durable witness is the opposite of the moved-head one.
///
/// A lost guard leaves `superseded_before_apply` behind, because the repository
/// ran and wrote it. A repository *error* leaves nothing at all: the
/// transaction rolled back, the row is still un-adjudicated, and the lease is
/// still open. That is what makes this fixture necessary rather than a
/// re-statement of [`a_moved_head_applies_nothing`] — the two cases are
/// indistinguishable from the returned effect and distinguishable only from the
/// row.
///
/// **Mutation target.** `apply_under_guard` maps the `Err` arm to `false`. Flip
/// that to `true` — "apply the Lead result when the guard could not be
/// evaluated" — and this fails on the counted effects, on the returned stage
/// outcome, and on nothing else in the suite.
#[tokio::test]
async fn a_repository_error_applies_nothing() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);
    let status_before = g.task_status().await;
    let attempts_before = g.worker_attempts().await;
    let activity_before =
        djinn_db::test_support::activity_row_count_for_test(&g.db, &g.subject.id).await;
    let routes_before =
        djinn_db::test_support::ci_route_row_count_for_test(&g.db, &g.subject.id).await;
    let leases_before =
        djinn_db::test_support::ci_route_lease_count_for_test(&g.db, &g.subject.id).await;

    // Installed *after* the fixture reserved the route and opened the lease, so
    // the only statement it defeats is the guard's own resolving write.
    djinn_db::test_support::reject_ci_route_attempt_updates_for_test(&g.db).await;

    let effect = apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        // The head has NOT moved and the lease is open: on a reachable database
        // this exact call is `a_current_route_applies_exactly_one_reopen`, which
        // is what makes the error the only difference between the two.
        &CiObservedNow {
            pr_head_sha: HEAD.to_owned(),
        },
    )
    .await;

    assert!(
        effect.is_noop(),
        "an unevaluated guard cannot authorize anything, got {effect:?}"
    );
    assert_eq!(
        effect.counts(),
        CiEffectCounts::default(),
        "fail-closed means no board transition and no worker dispatch"
    );

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        None,
        "the resolving transaction rolled back, so the route is still \
         un-adjudicated -- NOT superseded_before_apply, which only a repository \
         that actually ran can write"
    );
    assert!(
        row.holds_open_tier2_lease(),
        "an un-resolved lease is what keeps the row visible to the quiescence \
         report instead of silently dropping the Lead result"
    );
    assert_eq!(row.action_phase, CiActionPhase::Reserved);
    assert_eq!(row.reopen_mode, None);
    assert_eq!(row.superseded_by_evidence, None);

    assert_eq!(
        g.task_status().await,
        status_before,
        "no board mutation: the task status must be untouched"
    );
    assert_eq!(
        g.worker_attempts().await,
        attempts_before,
        "no worker dispatch: no task attempt may be created"
    );
    assert_eq!(
        djinn_db::test_support::activity_row_count_for_test(&g.db, &g.subject.id).await,
        activity_before,
        "no board mutation: nothing may be logged either"
    );
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&g.db, &g.subject.id).await,
        routes_before,
        "no route row invented"
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&g.db, &g.subject.id).await,
        leases_before,
        "no second Tier-2 lease"
    );

    assert!(matches!(
        stage_outcome_after_guard(&adjudication.plan, &effect, "db error"),
        StageOutcome::LeadRouteSuperseded { .. }
    ));
}

// ---------------------------------------------------------------------------
// The rejection is durable
// ---------------------------------------------------------------------------

/// A Lead **timeout** on a `causal_failure` route must be distinguishable from
/// a Lead that answered and found no remedy.
///
/// Both write `diagnostic_reason = no_grounded_remedy` — that is the trap. The
/// row must carry `lead_rejection = timed_out` as well, or reporting conflates
/// "Lead diagnosed" with "Lead never answered".
#[tokio::test]
async fn a_lead_timeout_is_distinguishable_from_a_delivered_diagnosis() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = adjudicate(&ctx, LeadResponse::TimedOut);
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::TimedOut),
        "a timeout is a rejection, not a delivered diagnosis"
    );

    let effect = apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        &CiObservedNow {
            pr_head_sha: HEAD.to_owned(),
        },
    )
    .await;
    assert_eq!(effect.counts().worker_dispatches, 1);

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::DiagnosticReopened)
    );
    assert_eq!(
        row.diagnostic_reason,
        Some(djinn_db::CiDiagnosticReason::NoGroundedRemedy),
        "the fallback's reason is derived from the route, which is exactly why \
         it cannot be the only thing recorded"
    );
    assert_eq!(
        row.lead_rejection,
        Some(djinn_db::CiLeadRejection::TimedOut),
        "without this column a timeout and a delivered no_grounded_remedy are \
         byte-identical rows"
    );
    assert!(
        row.lead_rejection
            .expect("just asserted")
            .is_absent_result(),
        "a timeout is Lead never answering, not Lead answering something refused"
    );
}

/// A **delivered** diagnosis carries the same reason and no rejection. Paired
/// with the test above so the distinction is proven from both sides.
#[tokio::test]
async fn a_delivered_diagnosis_carries_no_rejection() {
    let g = guarded().await;
    let ctx = g.context();
    let payload = serde_json::json!({
        "decision": "reopen",
        "directive": format!("run {RUN_REF} shows no captured section; the remedy is unknown"),
        "diagnostic_reason": "no_grounded_remedy",
    });
    let adjudication = adjudicate(&ctx, LeadResponse::Submitted(&payload));
    assert_eq!(adjudication.rejection, None);

    apply_under_guard(
        &g.routes,
        &ctx,
        &adjudication,
        &CiObservedNow {
            pr_head_sha: HEAD.to_owned(),
        },
    )
    .await;

    let row = g.row().await;
    assert_eq!(
        row.diagnostic_reason,
        Some(djinn_db::CiDiagnosticReason::NoGroundedRemedy),
        "same reason as the timeout above"
    );
    assert_eq!(
        row.lead_rejection, None,
        "and this is the only column that tells them apart"
    );
}

// ---------------------------------------------------------------------------
// The `directive` column carries two facts
// ---------------------------------------------------------------------------

/// Applying a reopen must not destroy the `ci_route` block.
///
/// `start_monitored_reopen` writes the reopen payload onto the arbitration
/// row's `directive` column -- the same column the coordinator writes the route
/// block into. Overwriting it made a task that was mid-adjudication read as
/// `NoRoute` on the very next look.
///
/// **Mutation target.** Restore the old from-scratch construction and this goes
/// red on both the route block and the budget flag.
#[test]
fn applying_a_reopen_preserves_the_route_block_and_the_budget_flag() {
    use crate::direct_services::merge_reopen_into_directive;

    let existing = serde_json::json!({
        "kind": "ci_evidence_routing",
        "terminal_disposition_required": true,
        "ci_route": {
            "lane": "pr_head",
            "tier2_lease_id": "lease-abc",
            "provider_action_key": "pak-abc",
        },
    });
    let merged = merge_reopen_into_directive(Some(&existing), "fix the missing re-export");

    assert_eq!(merged["decision"], "reopen");
    assert_eq!(merged["directive"], "fix the missing re-export");
    assert_eq!(
        merged["ci_route"]["tier2_lease_id"], "lease-abc",
        "the route block must survive the reopen that the route itself produced"
    );
    assert_eq!(
        merged["ci_route"]["provider_action_key"], "pak-abc",
        "without this the guard cannot name its row on the next read"
    );
    assert_eq!(
        merged["terminal_disposition_required"], true,
        "the cumulative-budget flag had the same bug and was never noticed"
    );

    // And the no-route case is unchanged: a plain arbiter reopen still writes
    // exactly the two keys it always did.
    let fresh = merge_reopen_into_directive(None, "do the thing");
    assert_eq!(fresh["decision"], "reopen");
    assert_eq!(fresh["directive"], "do the thing");
    assert_eq!(
        fresh.as_object().expect("object").len(),
        2,
        "a reopen with no prior directive invents nothing"
    );

    // A non-object directive (an old row, or a hand-written string) is replaced
    // rather than panicked on.
    let scalar = merge_reopen_into_directive(Some(&serde_json::json!("legacy text")), "d");
    assert_eq!(scalar["decision"], "reopen");
}
