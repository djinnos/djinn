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
    tier2_reason: CiTier2Reason,
}

fn identity(head: &str) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: head.to_owned(),
        run_id: Some(RUN),
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

/// A reserved route with an open Tier-2 lease, exactly as the coordinator
/// leaves it before dispatching Lead.
async fn guarded() -> Guarded {
    guarded_route(
        identity(HEAD),
        CiClass::CausalFailure,
        CiTier2Reason::CausalFailure,
    )
    .await
}

/// The same, for a route whose evidence identity **names no run** — what
/// `tier2_diagnose_only` reserves after irrecoverable incompleteness.
///
/// The row really carries `run_id IS NULL`, so the repository's own
/// `is_run_absent` fence is live underneath these fixtures rather than mocked.
async fn run_absent_guarded() -> Guarded {
    let mut identity = identity(HEAD);
    identity.run_id = None;
    guarded_route(identity, CiClass::Unknown, CiTier2Reason::EvidenceUnknown).await
}

async fn guarded_route(
    identity: CiEvidenceIdentity,
    class: CiClass,
    tier2_reason: CiTier2Reason,
) -> Guarded {
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

    let reserved = routes
        .reserve(&CiRouteReservation {
            subject: subject.clone(),
            provider_action_key: KEY.to_owned(),
            identity: identity.clone(),
            origin_state: CiOriginState::PrDraft,
            class,
            action: CiAction::AskLead,
            transient_fingerprint: "fp".to_owned(),
            retry_budget_key: "rbk".to_owned(),
            head_budget_key: "hbk".to_owned(),
        })
        .await
        .expect("reserve");
    assert!(matches!(reserved, CiReserveOutcome::Reserved(_)));

    let lease = routes
        .open_tier2_lease(&subject, KEY, &identity, "lease-key", tier2_reason)
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
        tier2_reason,
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
            tier2_reason: self.tier2_reason,
            repository_commands: vec!["cargo test -p djinn-db".to_owned()],
            // A run-absent route never named a run, so `RUN_REF` is not one of
            // its handles. Handing it one anyway would let a directive ground
            // itself on evidence the route does not have.
            evidence_references: match self.identity.run_id {
                Some(_) => vec![RUN_REF.to_owned(), HEAD.to_owned()],
                None => vec![HEAD.to_owned()],
            },
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

    /// Make the board believe `head` is this task's PR head, the way the PR
    /// poller does.
    ///
    /// `Task::ci_head_sha` is not a column: every task SELECT derives it from
    /// the newest `task_pr_ci_snapshots` row. So a snapshot upsert — not a
    /// task update — is what "the head moved" looks like to the re-read
    /// inside `apply_lead_ci_result`, and going through the production writer
    /// is what stops this fixture from staging a value the production path
    /// could never see.
    async fn observe_pr_head(&self, head: &str) {
        djinn_db::TaskRepository::new(self.db.clone(), djinn_core::events::EventBus::noop())
            .upsert_ci_snapshot(djinn_core::models::TaskPrCiSnapshotInput {
                task_id: self.subject.id.clone(),
                pr_number: PR,
                head_sha: head.to_owned(),
                ci_status: djinn_core::models::CiStatus::Failing,
                blocking_required_check_names: vec!["Quality Gate / test".to_owned()],
                primary_blocking_check: Some("Quality Gate / test".to_owned()),
                failure_annotations: None,
                failure_fingerprint: None,
                same_signature_count: 0,
                last_remediation_base_sha: None,
            })
            .await
            .expect("seed the board's PR head observation");
    }

    /// The production dependency bundle `apply_lead_ci_result` takes, over
    /// **this** fixture's database, so the re-read it performs hits the rows
    /// seeded above.
    fn agent_context(&self) -> crate::context::AgentContext {
        crate::test_helpers::agent_context_from_db(
            self.db.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
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

// ---------------------------------------------------------------------------
// S20: where the guard's `observed` head actually comes from
// ---------------------------------------------------------------------------

/// The production derivation reads the **live** head, not the stored one.
///
/// # Why this fixture has to exist
///
/// Every other fixture in this file calls [`apply_under_guard`] with a
/// `CiObservedNow` it built itself, which means the value under test is the
/// test's own. That leaves the one line production actually depends on — the
/// re-read of `Task::ci_head_sha` inside `stage::apply_lead_ci_result` —
/// completely unwitnessed, and this mutation survives the entire `nafu`
/// acceptance list green:
///
/// ```ignore
/// &ci_routing::CiObservedNow { pr_head_sha: ci.guard.identity.pr_head_sha.clone() }
/// ```
///
/// `observed_identity` then equals the stored identity by construction,
/// `resolve_tier2_lease`'s head comparison can never fail, and **every** delayed
/// or stale Lead result applies: a reopen plus a worker dispatch against
/// evidence a newer head already superseded. That is the double-spent session
/// the proposal exists to stop.
///
/// So this drives the production function, and the only difference between the
/// two halves below is one row in the database.
///
/// NAMED FAILING MUTATIONS.
/// (a) The mutation above (or any other way of deriving `observed_head` from
///     `ci.guard.identity` rather than from the re-read): the moved half's
///     `LeadRouteSuperseded` assertion fails AND its durable
///     `SupersededBeforeApply` assertion fails — two independent witnesses, one
///     the value the supervisor acts on and one the row the next poll reads.
///     (The board-status and worker-attempt assertions are negative space, not
///     the kill: nothing at this layer writes them on either branch. They are
///     here to catch a future edit that makes the guarded path mutate the board
///     directly, which is what "applies nothing" has to keep meaning.)
/// (b) Drop the `.and_then(|task| task.ci_head_sha)` and re-read some other
///     field: the moved half fails the same way, because the moved head is only
///     ever visible through `ci_head_sha`.
/// (c) Invert the guard (`CiGuardOutcome::Current` unconditionally, or a
///     tautological repository comparison): the moved half fails.
/// (d) Make the guard refuse unconditionally: the **current** half fails, which
///     is the vacuity guard — without it every negative in the moved half would
///     also hold for a function that never applies anything.
/// (e) Point the re-read at a different task id: the current half's re-read
///     misses, falls back to the stored head and still passes, but the moved
///     half's re-read also misses and it fails on every assertion.
#[tokio::test]
async fn the_production_derivation_reads_the_live_head_not_the_stored_one() {
    // ── The board still believes in the head the route reserved ────────────
    let current = guarded().await;
    current.observe_pr_head(HEAD).await;
    let current_ctx = current.context();
    let current_adjudication = repair(&current_ctx);

    let applied = crate::supervisor_impl::stage::apply_lead_ci_result(
        &current.agent_context(),
        &current.subject.id,
        &current_ctx,
        &current_adjudication,
    )
    .await;

    assert!(
        matches!(applied, StageOutcome::LeadReopen { .. }),
        "vacuity guard: a current head must still APPLY through the production \
         derivation, or the refusal below proves nothing. Got {applied:?}"
    );
    let current_row = current.row().await;
    assert_eq!(
        current_row.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened),
        "and the guard's own transaction must have persisted the repair"
    );
    assert_eq!(
        current_row.reopen_mode,
        Some(djinn_db::CiReopenMode::Repair)
    );

    // ── The same call, the same payload, one different snapshot row ────────
    let moved = guarded().await;
    moved.observe_pr_head(MOVED_HEAD).await;
    let moved_ctx = moved.context();
    let moved_adjudication = repair(&moved_ctx);
    let status_before = moved.task_status().await;
    let attempts_before = moved.worker_attempts().await;

    let refused = crate::supervisor_impl::stage::apply_lead_ci_result(
        &moved.agent_context(),
        &moved.subject.id,
        &moved_ctx,
        &moved_adjudication,
    )
    .await;

    assert!(
        matches!(refused, StageOutcome::LeadRouteSuperseded { .. }),
        "the live head moved, so the supervisor must be handed the inert \
         outcome rather than a reopen. Got {refused:?}"
    );
    let moved_row = moved.row().await;
    assert_eq!(
        moved_row.adjudicated_outcome(),
        Some(CiRouteOutcome::SupersededBeforeApply),
        "and the obsolete attempt must be closed as superseded in the same \
         transaction, not left open for a second adjudication"
    );
    assert_eq!(
        moved_row.reopen_mode, None,
        "a superseded attempt records no reopen mode"
    );
    assert!(
        moved_row.superseded_by_evidence.is_some(),
        "the identity that defeated the compare-and-set must be recorded"
    );
    assert_eq!(
        moved.task_status().await,
        status_before,
        "no board mutation: a `PrCiFailed` reopen against superseded evidence \
         is the double-spend this guard exists to stop"
    );
    assert_eq!(
        moved.worker_attempts().await,
        attempts_before,
        "and no worker session may be charged for it"
    );
}

/// No snapshot to compare against is **not** evidence that the head moved.
///
/// The re-read is `Option`-typed all the way down and falls back to the stored
/// head. That fallback is load-bearing in the opposite direction from the
/// fixture above: a route whose task carries no `task_pr_ci_snapshots` row yet
/// must still be able to apply, or the guard refuses every Lead result on a
/// task the poller has not written a snapshot for.
///
/// NAMED FAILING MUTATIONS.
/// (a) `unwrap_or_default()` in place of
///     `unwrap_or_else(|| ci.guard.identity.pr_head_sha.clone())`: the observed
///     head becomes `""`, which never equals the stored head, and this fails on
///     both assertions.
/// (b) Any fallback naming a constant other than the stored head: same failure.
///
/// This is also why the fixture above seeds a snapshot on BOTH halves — without
/// it, its "current" half would be exercising this fallback rather than a
/// successful re-read, and mutation (b) there would survive.
#[tokio::test]
async fn an_absent_snapshot_falls_back_to_the_stored_head_rather_than_refusing() {
    let g = guarded().await;
    let ctx = g.context();
    let adjudication = repair(&ctx);

    // Deliberately no `observe_pr_head`: `Task::ci_head_sha` is NULL here.
    let applied = crate::supervisor_impl::stage::apply_lead_ci_result(
        &g.agent_context(),
        &g.subject.id,
        &ctx,
        &adjudication,
    )
    .await;

    assert!(
        matches!(applied, StageOutcome::LeadReopen { .. }),
        "an absent snapshot is silence, not a moved head. Got {applied:?}"
    );
    assert_eq!(
        g.row().await.adjudicated_outcome(),
        Some(CiRouteOutcome::RepairReopened),
        "and the resolution must be persisted, not merely projected"
    );
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
// The run-absent route is diagnose-only, and still guarded
// ---------------------------------------------------------------------------

/// A repair Lead submitted on a run-absent route, well-formed and with a
/// repository-valid command.
fn rejected_repair(ctx: &CiAdjudicationContext) -> CiAdjudication {
    let payload = serde_json::json!({
        "decision": "reopen",
        "directive": format!(
            "enumeration for head {HEAD} never completed, so no run was attributed"
        ),
        "verification_command": "cargo test -p djinn-db",
    });
    let adjudication = adjudicate(ctx, LeadResponse::Submitted(&payload));
    assert_eq!(
        adjudication.rejection,
        Some(CiResultRejection::RepairUnavailableForRoute),
        "the fixture depends on the repair being refused for the ROUTE"
    );
    adjudication
}

/// The refused repair becomes exactly one diagnostic reopen — one worker — and
/// the durable row says the repair was refused rather than that Lead diagnosed.
///
/// This is the positive half. `resolve_tier2_lease` refuses a
/// `repair_reopened` resolution on a run-absent row outright, so if the
/// validator ever stopped converting the plan this call would come back a
/// no-op with the lease still open — which is why the assertion is on the
/// persisted row and not on the returned plan.
#[tokio::test]
async fn a_rejected_repair_on_a_current_run_absent_route_becomes_one_diagnosis() {
    let g = run_absent_guarded().await;
    assert!(
        g.row().await.is_run_absent(),
        "the seeded row must really carry a NULL run id"
    );
    let ctx = g.context();
    let adjudication = rejected_repair(&ctx);

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
    assert_eq!(
        counts.worker_dispatches, 1,
        "a refused repair still costs exactly one worker, via the diagnosis"
    );

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::DiagnosticReopened),
        "a repair reopen must never become durable on a run-absent route"
    );
    assert_eq!(row.reopen_mode, Some(djinn_db::CiReopenMode::Diagnose));
    assert_eq!(
        row.diagnostic_reason,
        Some(djinn_db::CiDiagnosticReason::EvidenceIncomplete),
        "revision 58 names this reason for the run-absent route"
    );
    assert_eq!(
        row.lead_rejection,
        Some(djinn_db::CiLeadRejection::RepairUnavailableForRoute),
        "durably recorded, not merely logged: without this column a refused \
         repair is byte-identical to a diagnosis Lead chose"
    );
    assert!(
        !row.lead_rejection
            .expect("just asserted")
            .is_absent_result(),
        "Lead answered; the contract refused the answer"
    );
}

/// The refusal is not an escape hatch from the guard. A refused repair on a
/// route whose head has moved performs no reopen and dispatches no worker,
/// exactly like an accepted one.
///
/// **Mutation target.** Convert the refused repair through anything that
/// bypasses `apply_under_guard` — or make the `names_no_run` rejection return
/// early with its own transition — and this fails on the effect counts, on the
/// task status, on the worker-attempt count, and on the durable outcome.
#[tokio::test]
async fn a_rejected_repair_on_a_stale_run_absent_route_applies_nothing() {
    let g = run_absent_guarded().await;
    let ctx = g.context();
    let adjudication = rejected_repair(&ctx);
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

    assert!(effect.is_noop(), "a stale route may apply nothing");
    assert_eq!(
        effect.counts(),
        CiEffectCounts::default(),
        "no board transition and no worker dispatch"
    );

    let row = g.row().await;
    assert_eq!(
        row.adjudicated_outcome(),
        Some(CiRouteOutcome::SupersededBeforeApply),
        "the refused repair must be closed by the same guard as every other result"
    );
    assert_eq!(row.reopen_mode, None);
    assert_eq!(
        row.lead_rejection, None,
        "nothing was adjudicated, so nothing records a rejection"
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
    assert!(matches!(
        stage_outcome_after_guard(&adjudication.plan, &effect, "stale"),
        StageOutcome::LeadRouteSuperseded { .. }
    ));
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
/// **What this actually witnesses**, and what it does not. This fixture calls
/// `merge_reopen_into_directive` directly; it never reaches
/// `start_monitored_reopen`. So restoring the from-scratch
/// `json!({"decision": .., "directive": ..})` *at the call site* leaves this
/// green — an earlier version of this comment claimed otherwise, and that claim
/// was wrong. The call site is pinned separately by
/// [`the_reopen_writer_merges_into_the_existing_arbitration_directive`] below;
/// what is pinned here is the merge semantics that call site depends on.
///
/// NAMED FAILING MUTATIONS, all inside `merge_reopen_into_directive`.
/// (a) Rebuild the object from scratch (`json!({})` instead of cloning
///     `existing`): the `ci_route` and `terminal_disposition_required`
///     assertions fail — the route block and the cumulative-budget flag are the
///     two facts the column carries besides the reopen itself.
/// (b) Copy only the recognised keys forward instead of cloning: the same two
///     assertions fail as soon as either key is missed, which is how the budget
///     flag was lost the first time.
/// (c) Make the `None` case inherit a default body: the two-key length
///     assertion on `fresh` fails — a plain arbiter reopen invents nothing.
/// (d) Drop the `is_object()` filter: the scalar case stops producing
///     `decision: "reopen"` (it panics on `as_object_mut` instead).
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

// ---------------------------------------------------------------------------
// The production call sites that reach any of the above
// ---------------------------------------------------------------------------

/// One Rust source with its `//` line comments removed.
///
/// The guards below match on *code*. Without this, a comment that merely names
/// the token under guard would satisfy the assertion the guard exists to make.
/// Quote- and escape-aware, so a `//` inside a string literal is left alone
/// rather than truncating the line it sits on.
fn strip_line_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        let bytes = line.as_bytes();
        let mut quoted = false;
        let mut index = 0usize;
        let mut end = line.len();
        while index < bytes.len() {
            match bytes[index] {
                b'\\' if quoted => index += 1,
                b'"' => quoted = !quoted,
                b'/' if !quoted && bytes.get(index + 1) == Some(&b'/') => {
                    end = index;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        out.push_str(&line[..end]);
        out.push('\n');
    }
    out
}

/// The single argument of the call whose opening parenthesis has just been
/// consumed, with its trailing comma and surrounding whitespace removed.
///
/// Paren-aware, because the argument this file cares about is itself a call
/// (`directive.as_ref()`) and a naive scan to the first `)` would stop inside
/// it. Panics rather than guessing if the argument list is not exactly one
/// item — a second argument means the call's contract changed and the assertion
/// built on it needs a human, not a silent pass.
fn sole_argument(after_open_paren: &str) -> &str {
    let mut depth = 1usize;
    let mut end = after_open_paren.len();
    for (index, character) in after_open_paren.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = index;
                    break;
                }
            }
            _ => {}
        }
    }
    let argument = after_open_paren[..end].trim().trim_end_matches(',').trim();
    assert!(
        !argument.is_empty(),
        "vacuity: the call was given no argument at all",
    );
    assert_eq!(
        argument.matches(',').count(),
        0,
        "expected a single-argument call, got `{argument}`",
    );
    argument
}

/// The supervisor's Lead stage arm is what reaches the `nafu` contract at all.
///
/// SOURCE-LEVEL, and honestly labelled: `execute_stage` is the enclosing
/// function, and driving it needs a live session, a slot, a provider transport
/// and a finalize round trip — none of which this crate carries a double for.
/// Every behavioural fixture in this file and in the sibling `tests.rs`
/// therefore enters *below* that arm, at [`apply_under_guard`],
/// [`stage_outcome_after_guard`], [`adjudicate`], or at the `#[cfg(test)]`
/// projection `stage::lead_stage_outcome_routed`. So the arm itself is
/// unwitnessed, and three separate deletions inside it leave the whole `nafu`
/// command list green while the feature stops existing in production:
///
/// * dropping the `read_arbiter_directive` match reverts every Lead session to
///   the legacy arbiter contract — the "feature disabled" row of the
///   mixed-version matrix, permanently;
/// * dropping the `Malformed` arm lets an unparseable `ci_route` block fall
///   through to that legacy path, which rejects a `diagnose` payload as a
///   `Failed` stage and parks the task at the arbiter cap — a producer bug in
///   one JSON field parking tasks whose Lead answered correctly;
/// * replacing `apply_lead_ci_result` with the bare projection
///   `ci_routing::stage_outcome(&adjudication.plan)` restores exactly the
///   wave-4 behaviour wave 5 replaced: the reopen is applied without ever
///   winning `resolve_tier2_lease`, so a Lead result lands on evidence a newer
///   head has already superseded.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete the `ci_routing::CiAdjudicationContext::read_arbiter_directive(`
///     call from the `RoleKind::Lead` arm: the first assertion fails.
/// (b) Delete the `CiDirectiveRead::Malformed(..)` arm, or make it fall through
///     to `lead_stage_outcome`: the ordering assertion fails, because
///     `LeadRouteSuperseded` no longer sits between the `Malformed` arm and the
///     `Route` arm.
/// (c) Replace `apply_lead_ci_result(..)` at the call site with
///     `ci_routing::stage_outcome(..)` or with the `#[cfg(test)]`
///     `lead_stage_outcome_routed(..)`: the occurrence count drops to one (the
///     definition alone) and the call assertion fails.
/// (d) Inline `apply_lead_ci_result`'s body at the call site: the same failure,
///     for the same reason.
/// (e) `read_arbiter_directive(directive.as_ref())` →
///     `read_arbiter_directive(None)`: the *token* is still there, so the
///     presence assertion below is satisfied and so is every arm-ordering
///     assertion — while every Lead session answers `NoRoute` and takes the
///     legacy path, a `diagnose` payload is rejected as `Failed`, the arbiter
///     decision-failure counter is charged, and the task parks at the cap.
///     `apply_lead_ci_result` becomes dead code with the whole `nafu` command
///     list green. The ARGUMENT assertion is what fails.
/// (f) Rebind `directive` from anything other than the arbitration row — e.g.
///     `let directive: Option<serde_json::Value> = None;`: the binding
///     assertion fails, which is the same mutation wearing a different hat.
#[test]
fn the_supervisors_lead_stage_arm_reads_the_route_and_applies_it_under_the_guard() {
    let code = strip_line_comments(include_str!("../stage.rs"));

    const READ: &str = "ci_routing::CiAdjudicationContext::read_arbiter_directive(";
    assert_eq!(
        code.matches(READ).count(),
        1,
        "exactly one read of the route block in the Lead stage arm; without it \
         every Lead session takes the legacy arbiter path and the feature is \
         unreachable from production. A second one would inherit no guard",
    );

    // The ARGUMENT, not merely the call. `read_arbiter_directive(None)` leaves
    // the token in place and reverts the whole feature.
    let read = code.find(READ).expect("the read call is present") + READ.len();
    let argument = sole_argument(&code[read..]);
    assert_eq!(
        argument, "directive.as_ref()",
        "the Lead stage arm must hand the arbitration row's OWN directive to \
         the reader; a literal `None` (or any other stand-in) makes every Lead \
         session answer `NoRoute`, which is the feature-disabled row of the \
         mixed-version matrix — permanently, in production",
    );

    // …and `directive` must be the row's, read from the hold cycle the
    // coordinator wrote the `ci_route` block onto.
    let binding = code
        .find("let directive = {")
        .expect("the Lead stage arm binds the arbitration directive");
    assert!(
        binding < read,
        "the directive is resolved BEFORE it is read; a later binding is a \
         different value",
    );
    assert!(
        code[binding..read].contains(".resolve_current_hold_cycle(&task.id)"),
        "and it must come from THIS task's current arbitration hold cycle — the \
         row `tier2_dispatch` writes the `ci_route` block onto. Any other \
         source is a directive no coordinator produced",
    );

    // The application half, and that it is the GUARDED one.
    assert_eq!(
        code.matches("apply_lead_ci_result(").count(),
        2,
        "exactly two occurrences: the definition and its single production call \
         site. One means the call was deleted, inlined, or swapped for the \
         pre-guard projection",
    );
    assert!(
        code.contains("async fn apply_lead_ci_result("),
        "and one of them must be the definition",
    );

    // The unparseable block fails closed, and does so BEFORE the route arm.
    let malformed = code
        .find("ci_routing::CiDirectiveRead::Malformed(")
        .expect("the Lead arm must answer an unparseable route block");
    let route_arm = code
        .find("ci_routing::CiDirectiveRead::Route(")
        .expect("the Lead arm must answer a parsed route block");
    assert!(
        malformed < route_arm,
        "the match arms are read in order; `Malformed` must be answered before \
         `Route`",
    );
    assert!(
        code[malformed..route_arm].contains("StageOutcome::LeadRouteSuperseded"),
        "an unguardable route block must apply nothing; falling through to the \
         legacy path parks a task whose Lead answered correctly",
    );
}

/// The reopen writer MERGES into the directive rather than rebuilding it.
///
/// SOURCE-LEVEL for one specific reason: the sibling fixture
/// [`applying_a_reopen_preserves_the_route_block_and_the_budget_flag`] calls
/// `merge_reopen_into_directive` directly, so its own "restore the old
/// from-scratch construction and this goes red" note is not true of the *call
/// site*. Restoring `json!({"decision": "reopen", "directive": directive})` at
/// `start_monitored_reopen` leaves that fixture green, because it never reaches
/// `start_monitored_reopen` — and the consequence is the wave-5 bug verbatim: a
/// Lead reopen overwrites the `ci_route` block on the arbitration row it came
/// from, so the very next read of that hold cycle answers `NoRoute` and the
/// guard has no row to name.
///
/// Driving `start_monitored_reopen` behaviourally is out of reach here: it is a
/// `DirectServices` method that needs a dispatch ledger, a slot pool and a
/// monitored session, none of which this crate carries a double for.
///
/// NAMED FAILING MUTATIONS.
/// (a) Replace the call with a from-scratch `serde_json::json!({..})`: the
///     binding assertion fails and the occurrence count drops to one.
/// (b) Delete the call and the binding entirely: the same two failures.
/// (c) Add a second reopen writer that rebuilds the directive: the count
///     assertion fails, which is what forces a new writer to be looked at.
#[test]
fn the_reopen_writer_merges_into_the_existing_arbitration_directive() {
    let code = strip_line_comments(include_str!("../../direct_services.rs"));

    assert!(
        code.contains("let directive_json = merge_reopen_into_directive("),
        "`start_monitored_reopen` must MERGE the reopen payload into the row's \
         existing directive; rebuilding it destroys the `ci_route` block the \
         guard reads on the next look",
    );
    assert_eq!(
        code.matches("merge_reopen_into_directive(").count(),
        2,
        "exactly two occurrences: the definition and its single call site",
    );
    assert!(
        code.contains("fn merge_reopen_into_directive("),
        "and one of them must be the definition",
    );
}
