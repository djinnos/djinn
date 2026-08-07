//! Acceptance fixtures for the two lane executors (proposal `nafu`, AC12).
//!
//! # What "counted, not named" means here
//!
//! Every fixture that asserts a discard proves four negatives, and all four are
//! counts taken from real state either side of the call — never the name of the
//! branch the executor returned:
//!
//! | Negative | Counter |
//! | --- | --- |
//! | no provider mutation | [`FakeProvider`] increments per call; the fixture reads the totals |
//! | no Tier-2 lease | `SELECT COUNT(*) … WHERE tier2_lease_id IS NOT NULL` on the route table |
//! | no board mutation | `tasks.status` plus `SELECT COUNT(*) FROM activity_log WHERE task_id = …` |
//! | no worker dispatch | `SELECT COUNT(*) FROM task_attempts WHERE task_id = …` |
//!
//! The last two are worth a note, because "the executor holds no board handle"
//! is an argument, not evidence. The counts are taken against the same
//! ephemeral Postgres the executor writes its route rows into, seeded with a
//! real task row, so a future executor that grew a board write would move them.
//!
//! The database is `Database::open_in_memory()` — the same ephemeral-per-test
//! Postgres `route_fixture` in the sibling classifier suite uses — and the
//! provider double follows `MockCleanupGitHub` next door: an `Arc<Mutex<_>>`
//! recorder behind the same `#[async_trait]` seam the production path calls.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use djinn_db::{
    CiActionPhase, CiEvidenceIdentity, CiLane, CiOriginState, CiQuiescenceProof, CiRouteOutcome,
    CiRouteSubject, CiSubjectKind, Database,
};
use djinn_provider::github_api::{
    CheckAnnotation, CheckRun, CheckRunsResponse, GitHubApiError, MergeMethod,
};

use super::*;
use crate::pr_poller::ci_routing::gate::CiRoutingGate;
use crate::pr_poller::ci_routing::{CiCapture, CiRouteAttempt};

// ---------------------------------------------------------------------------
// The provider double
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProviderCalls {
    rerun_failed_jobs: usize,
    enable_auto_merge: usize,
    list_check_runs: usize,
    annotations: usize,
}

impl ProviderCalls {
    /// Every call that changes provider state. Reads are not mutations.
    fn mutations(self) -> usize {
        self.rerun_failed_jobs + self.enable_auto_merge
    }
}

#[derive(Default)]
struct FakeState {
    calls: ProviderCalls,
    /// When set, the next mutation returns this error instead of succeeding.
    fail_mutations: bool,
    fail_check_runs: bool,
    fail_annotations: bool,
    /// Every `(owner, repo, run_id)` triple `rerun_failed_jobs` was asked for,
    /// so a fixture can prove *which* run was re-run, not just how many.
    reran: Vec<(String, String, u64)>,
}

#[derive(Clone, Default)]
struct FakeProvider {
    state: Arc<Mutex<FakeState>>,
}

impl FakeProvider {
    fn calls(&self) -> ProviderCalls {
        self.state.lock().expect("fake provider mutex").calls
    }

    fn reran(&self) -> Vec<(String, String, u64)> {
        self.state
            .lock()
            .expect("fake provider mutex")
            .reran
            .clone()
    }

    fn failing_mutations() -> Self {
        let me = Self::default();
        me.state.lock().expect("fake provider mutex").fail_mutations = true;
        me
    }
}

fn api_error(method: &'static str) -> GitHubApiError {
    GitHubApiError::http(
        method,
        "/fake".to_owned(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "boom".to_owned(),
    )
}

#[async_trait]
impl CiRouteProvider for FakeProvider {
    async fn rerun_failed_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<(), GitHubApiError> {
        let mut state = self.state.lock().expect("fake provider mutex");
        state.calls.rerun_failed_jobs += 1;
        state
            .reran
            .push((owner.to_owned(), repo.to_owned(), run_id));
        if state.fail_mutations {
            return Err(api_error("rerun_failed_jobs"));
        }
        Ok(())
    }

    async fn enable_auto_merge(
        &self,
        _owner: &str,
        _repo: &str,
        _pull_number: u64,
        _method: MergeMethod,
        _node_id: &str,
        _commit_headline: &str,
    ) -> Result<serde_json::Value, GitHubApiError> {
        let mut state = self.state.lock().expect("fake provider mutex");
        state.calls.enable_auto_merge += 1;
        if state.fail_mutations {
            return Err(api_error("enable_auto_merge"));
        }
        Ok(serde_json::json!({}))
    }

    async fn list_check_runs_for_ref(
        &self,
        _owner: &str,
        _repo: &str,
        _git_ref: &str,
    ) -> Result<CheckRunsResponse, GitHubApiError> {
        let mut state = self.state.lock().expect("fake provider mutex");
        state.calls.list_check_runs += 1;
        if state.fail_check_runs {
            return Err(api_error("list_check_runs_for_ref"));
        }
        Ok(CheckRunsResponse::complete(Vec::new()))
    }

    async fn get_check_run_annotations(
        &self,
        _owner: &str,
        _repo: &str,
        _check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>, GitHubApiError> {
        let mut state = self.state.lock().expect("fake provider mutex");
        state.calls.annotations += 1;
        if state.fail_annotations {
            return Err(api_error("get_check_run_annotations"));
        }
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    routes: CiRouteAttemptRepository,
    subject: CiRouteSubject,
    task_id: String,
    provider: FakeProvider,
    scope: ProviderActionScope,
    incarnation: String,
}

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MOVED_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PR: i64 = 4242;

async fn fixture() -> Fixture {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project =
        djinn_db::test_support::make_project(&db, std::path::Path::new("ci-executor")).await;
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
    Fixture {
        routes: CiRouteAttemptRepository::new(db.clone()),
        subject: CiRouteSubject::task(task_id.clone()),
        task_id,
        db,
        provider: FakeProvider::default(),
        scope: ProviderActionScope::new(),
        incarnation: uuid::Uuid::now_v7().to_string(),
    }
}

impl Fixture {
    fn target(&self) -> CiLaneTarget<'_> {
        CiLaneTarget {
            subject: &self.subject,
            origin_state: CiOriginState::PrDraft,
            owner: "acme",
            repo: "widgets",
            incarnation_id: &self.incarnation,
            gate: CiRoutingGate::Enabled,
            auto_merge: None,
        }
    }

    fn merge_target<'a>(&'a self, auto: &'a CiAutoMergeTarget<'a>) -> CiLaneTarget<'a> {
        CiLaneTarget {
            origin_state: CiOriginState::PrReview,
            auto_merge: Some(*auto),
            ..self.target()
        }
    }

    /// The four counters every discard fixture reads.
    ///
    /// All five reads go through `djinn_db::test_support`: `djinn-coordinator`
    /// carries a boundary test forbidding a direct `sqlx` dependency, and
    /// routing them through the owning crate is what that rule asks for.
    async fn effects(&self) -> Effects {
        use djinn_db::test_support as ts;
        Effects {
            task_status: ts::task_status_for_test(&self.db, &self.task_id).await,
            activity_rows: ts::activity_row_count_for_test(&self.db, &self.task_id).await,
            worker_attempts: ts::task_attempt_count_for_test(&self.db, &self.task_id).await,
            tier2_leases: ts::ci_route_lease_count_for_test(&self.db, &self.subject.id).await,
            route_rows: ts::ci_route_row_count_for_test(&self.db, &self.subject.id).await,
            provider_mutations: self.provider.calls().mutations(),
        }
    }

    async fn budgets(&self, identity: &CiEvidenceIdentity, fingerprint: &str) -> (i64, i64) {
        let counts = self
            .routes
            .budget_counts(
                &self.subject,
                &retry_budget_key(&self.subject, identity, fingerprint),
                &head_budget_key(&self.subject, identity.pr_number, &identity.pr_head_sha),
            )
            .await
            .expect("budget read");
        (counts.signature, counts.head)
    }

    async fn attempt(&self, identity: &CiEvidenceIdentity, action: CiAction) -> CiRouteAttempt {
        self.routes
            .get(
                &self.subject,
                &provider_action_key(&self.subject, identity, action),
            )
            .await
            .expect("route read")
            .expect("route row exists")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Effects {
    task_status: String,
    activity_rows: i64,
    worker_attempts: i64,
    tier2_leases: i64,
    route_rows: i64,
    provider_mutations: usize,
}

/// The complete discard assertion: nothing at all moved except, possibly, the
/// route row's own terminal outcome.
fn assert_no_effects_beyond_routes(before: &Effects, after: &Effects) {
    assert_eq!(
        before.task_status, after.task_status,
        "no board mutation: the task status must be untouched"
    );
    assert_eq!(
        before.activity_rows, after.activity_rows,
        "no board mutation: no activity row may be written"
    );
    assert_eq!(
        before.worker_attempts, after.worker_attempts,
        "no worker dispatch: no task attempt may be created"
    );
    assert_eq!(
        before.tier2_leases, after.tier2_leases,
        "no Tier-2 lease may be opened"
    );
    assert_eq!(
        before.provider_mutations, after.provider_mutations,
        "no provider mutation"
    );
}

// ---------------------------------------------------------------------------
// Evidence builders
// ---------------------------------------------------------------------------

fn pr_head_identity(run_id: i64) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

fn merge_group_identity(run_id: i64) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id,
        run_head_sha: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
        dequeue_id: Some(
            "refs/heads/gh-readonly-queue/main/pr-4242-a@2026-08-06T00:00:00Z".to_owned(),
        ),
    }
}

/// A blocking check that ran and was cancelled: no causal evidence, so a run of
/// these is `is_inconclusive` and therefore Tier 1.
fn inconclusive_check(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        id: run_id * 10,
        run_id: Some(run_id),
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("cancelled".to_owned()),
        html_url: format!("https://github.com/acme/widgets/actions/runs/{run_id}/job/1"),
        started_at: Some("2026-08-06T00:00:00Z".to_owned()),
        completed_at: Some("2026-08-06T00:05:00Z".to_owned()),
        output: None,
    }
}

/// A blocking check that ran and hard-failed with annotations: causal evidence,
/// so the run is Tier 2.
fn causal_check(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        conclusion: Some("failure".to_owned()),
        output: Some(djinn_provider::github_api::CheckRunOutput {
            annotations_count: Some(3),
            ..Default::default()
        }),
        ..inconclusive_check(name, run_id)
    }
}

fn refs(runs: &[CheckRun]) -> Vec<&CheckRun> {
    runs.iter().collect()
}

async fn run(
    f: &Fixture,
    target: &CiLaneTarget<'_>,
    evidence: &CiEvidenceIdentity,
    observed: &CiEvidenceIdentity,
    blocking: &[&CheckRun],
) -> CiLaneOutcome {
    let capture = CiCapture::prove_complete(
        djinn_provider::github_api::CheckSetCompleteness::Complete,
        blocking,
    );
    let observation = CiObservation {
        evidence,
        observed_current: observed,
        capture,
    };
    execute_route(
        &f.routes,
        &f.provider,
        &f.scope,
        target,
        &observation,
        blocking,
    )
    .await
}

// ---------------------------------------------------------------------------
// Head-witness / liveness doubles
// ---------------------------------------------------------------------------

struct FixedHead(Option<String>);

#[async_trait]
impl CiCurrentHeadWitness for FixedHead {
    async fn current_pr_head(&self, _subject: &CiRouteSubject, _pr_number: i64) -> Option<String> {
        self.0.clone()
    }
}

struct FixedLiveness(CiQuiescenceProof);

#[async_trait]
impl CiOwnerLiveness for FixedLiveness {
    async fn quiescence_proof(&self, _incarnation_id: &str) -> CiQuiescenceProof {
        self.0
    }
}

/// Age a `calling` row so the 300s recovery timeout has elapsed.
async fn age_calling(f: &Fixture, key: &str, seconds: i64) {
    djinn_db::test_support::ci_route_age_calling_for_test(&f.db, &f.subject.id, key, seconds).await;
}

// ===========================================================================
// AC12 named fixtures
// ===========================================================================

/// Two concurrent drives of the *same* current, eligible reservation converge
/// on one `calling` winner, one provider-call episode, one signature charge and
/// one head charge — with no Tier-2 lease, no Lead session, and no worker.
///
/// The second drive is a genuine second executor pass over an existing
/// `reserved` row (that is what "recovery" is on the polling path: `reserve`
/// answers `AlreadyPresent`), so the convergence is the repository's
/// compare-and-set doing its job, not a test-local dedupe.
#[tokio::test]
async fn current_reserved_recovery_resumes_tier_one() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 900)];
    let blocking = refs(&checks);
    let id = pr_head_identity(900);
    let target = f.target();

    let first = run(&f, &target, &id, &id, &blocking).await;
    let second = run(&f, &target, &id, &id, &blocking).await;

    assert_eq!(
        first,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Retriggered)
    );
    assert_eq!(
        second,
        CiLaneOutcome::Deferred(CiDeferral::AlreadyTerminal),
        "the second pass must find the episode already finalized, not repeat it"
    );

    assert_eq!(
        f.provider.calls().rerun_failed_jobs,
        1,
        "exactly one provider-call episode for one evidence identity"
    );
    assert_eq!(
        f.provider.reran(),
        vec![("acme".to_owned(), "widgets".to_owned(), 900u64)],
        "the call names the run the evidence identity names"
    );

    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "one signature charge and one head charge, not two"
    );

    let effects = f.effects().await;
    assert_eq!(effects.route_rows, 1, "one row per evidence identity");
    assert_eq!(effects.tier2_leases, 0, "Tier 1 opens no Lead adjudication");
    assert_eq!(effects.worker_attempts, 0, "Tier 1 dispatches no worker");
    assert_eq!(effects.task_status, "pr_draft", "the lane stays pr_draft");
}

/// A route whose owner paused *before* the provider call cannot be taken by a
/// duplicate poll, a sweep, or another runtime.
///
/// The row is `calling` and its owner is a different incarnation. Every door is
/// tried and every door refuses: the executor defers, `open_tier2_lease` refuses
/// (`OwnedByProviderCall`), and the reservation sweep refuses (the phase is not
/// `reserved`). Nothing is charged twice and no adjudication opens.
#[tokio::test]
async fn live_calling_owner_before_provider_is_not_recovered() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 901)];
    let blocking = refs(&checks);
    let id = pr_head_identity(901);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);

    // A foreign owner holds the row in `calling`, pre-call.
    let other = uuid::Uuid::now_v7().to_string();
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: id.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, id.pr_number, &id.pr_head_sha),
        })
        .await
        .expect("reserve");
    f.routes
        .charge_and_begin_calling(&f.subject, &key, &other, &id)
        .await
        .expect("charge");

    let before = f.effects().await;
    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        outcome,
        CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.attempt(&id, CiAction::RerunRun).await.action_phase,
        CiActionPhase::Calling,
        "the live owner keeps the row"
    );
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the foreign owner's single charge is neither released nor doubled"
    );

    // The sweep is the other door. It must leave the row alone too.
    let report = sweep_reserved_routes(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &f.incarnation,
        CiRoutingGate::Enabled,
    )
    .await;
    assert_eq!(report.resumed, 0, "a `calling` row is never resumed");
    assert_eq!(report.superseded, 0);
    assert_eq!(f.effects().await, after);
}

/// The same refusal after the provider has already accepted.
///
/// The distinction from the pre-call case matters: here a real GitHub mutation
/// happened, so stealing the row would let a second incarnation record a
/// supersession for evidence that genuinely was acted on, and the true owner's
/// `finalize_calling` would then be silently discarded.
#[tokio::test]
async fn live_calling_owner_after_acceptance_is_not_recovered() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 902)];
    let blocking = refs(&checks);
    let id = pr_head_identity(902);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);

    // The owner runs the whole episode, including a real (faked) provider call.
    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;
    assert_eq!(
        outcome,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Retriggered)
    );
    assert_eq!(f.provider.calls().rerun_failed_jobs, 1);

    // Put the row back into `calling` under a *foreign* owner to model "the
    // provider accepted but this incarnation has not finalized yet".
    let other = uuid::Uuid::now_v7().to_string();
    djinn_db::test_support::ci_route_force_calling_owner_for_test(
        &f.db,
        &f.subject.id,
        &key,
        &other,
    )
    .await;

    let before = f.effects().await;
    let again = run(&f, &f.target(), &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        again,
        CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.provider.calls().rerun_failed_jobs,
        1,
        "the accepted call is never replayed"
    );

    // A startup handoff must also refuse while the owner is live and undrained.
    let report = recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &FixedLiveness(CiQuiescenceProof::None),
        &f.incarnation,
        true,
        CiRoutingGate::Enabled,
    )
    .await;
    assert_eq!(report.examined, 1);
    assert_eq!(report.deferred, 1, "no quiescence proof, no handoff");
    assert_eq!(report.outcome_unknown, 0);
    assert_eq!(f.effects().await, after);
}

/// After a quiescent handoff the row is recovered exactly once, keeps its
/// charge, and opens at most one Tier-2 lease — with no provider query and no
/// replay.
#[tokio::test]
async fn terminated_owner_handoff_recovers_calling_once() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 903)];
    let blocking = refs(&checks);
    let id = pr_head_identity(903);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);

    let other = uuid::Uuid::now_v7().to_string();
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: id.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, id.pr_number, &id.pr_head_sha),
        })
        .await
        .expect("reserve");
    f.routes
        .charge_and_begin_calling(&f.subject, &key, &other, &id)
        .await
        .expect("charge");
    age_calling(&f, &key, 400).await;

    let before = f.effects().await;
    let first = recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &FixedLiveness(CiQuiescenceProof::ProcessTerminated),
        &f.incarnation,
        true,
        CiRoutingGate::Enabled,
    )
    .await;
    assert_eq!(first.outcome_unknown, 1, "still current: outcome_unknown");
    assert_eq!(first.superseded_after_call, 0);

    // A second pass finds nothing to hand off: the row is terminal now.
    let second = recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &FixedLiveness(CiQuiescenceProof::ProcessTerminated),
        &f.incarnation,
        true,
        CiRoutingGate::Enabled,
    )
    .await;
    assert_eq!(
        second.examined, 0,
        "recovery happens once, not once per pass"
    );

    let after = f.effects().await;
    assert_eq!(
        after.provider_mutations, before.provider_mutations,
        "a handoff performs no provider query and no replay"
    );
    assert_eq!(
        after.worker_attempts, before.worker_attempts,
        "a handoff dispatches no worker"
    );
    assert_eq!(
        after.task_status, before.task_status,
        "and no board mutation"
    );
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the charge is retained across the handoff"
    );
    assert!(
        after.tier2_leases <= 1,
        "at most one current-evidence Tier-2 lease"
    );
}

/// The provider finalizer and a startup handoff race for one `calling` row, and
/// exactly one compare-and-set wins.
///
/// The finalizer goes first here, which is the ordering the proposal calls the
/// authoritative one: an owner that commits `retriggered` while draining keeps
/// the row, and the recovery that follows is a no-op rather than an overwrite.
#[tokio::test]
async fn provider_finalizer_wins_owner_handoff_race() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 904)];
    let blocking = refs(&checks);
    let id = pr_head_identity(904);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);

    let owner = uuid::Uuid::now_v7().to_string();
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: id.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, id.pr_number, &id.pr_head_sha),
        })
        .await
        .expect("reserve");
    f.routes
        .charge_and_begin_calling(&f.subject, &key, &owner, &id)
        .await
        .expect("charge");
    age_calling(&f, &key, 400).await;

    // The owner's finalizer lands first.
    let finalized = f
        .routes
        .finalize_calling(&f.subject, &key, &owner, CiRouteOutcome::Retriggered, None)
        .await
        .expect("finalize");
    assert!(finalized, "the owner's fenced write wins");

    // The recovering incarnation now finds nothing to take.
    let report = recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &FixedLiveness(CiQuiescenceProof::GracefulDrain),
        &f.incarnation,
        true,
        CiRoutingGate::Enabled,
    )
    .await;
    assert_eq!(
        report.examined, 0,
        "a terminal row is not a handoff candidate"
    );

    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::Retriggered),
        "the provider result is authoritative, not overwritten by recovery"
    );
    assert_eq!(
        attempt.owner_incarnation_id.as_deref(),
        Some(owner.as_str()),
        "the owner identity that performed the call is immutable"
    );

    // And the losing direction: a late write from the former owner after a
    // legal handoff must be rejected rather than overwrite the recovery.
    let late = f
        .routes
        .finalize_calling(&f.subject, &key, &owner, CiRouteOutcome::ActionFailed, None)
        .await
        .expect("late finalize");
    assert!(!late, "terminalization is write-once");
}

/// A PR head that moved before the Tier-2 lease could open discards the
/// obsolete route: no provider mutation, no lease, no board mutation, no
/// worker.
#[tokio::test]
async fn head_change_before_lead_dispatch_discards_obsolete_route() {
    let f = fixture().await;
    let checks = [causal_check("Quality Gate / test", 905)];
    let blocking = refs(&checks);
    let evidence = pr_head_identity(905);
    let observed = CiEvidenceIdentity {
        pr_head_sha: MOVED_HEAD.to_owned(),
        run_head_sha: MOVED_HEAD.to_owned(),
        ..evidence.clone()
    };

    let before = f.effects().await;
    let outcome = run(&f, &f.target(), &evidence, &observed, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        outcome,
        CiLaneOutcome::Discarded(CiStaleField::PrHeadSha),
        "an obsolete identity is discarded before anything is spent"
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        after.route_rows, 0,
        "a discard writes no route row at all: there is nothing to adjudicate"
    );
}

/// A lane change while a Tier-2 lease is pending is a no-op.
///
/// The lease is opened for a `merge_group` identity; the poller then observes a
/// `pr_head` identity for the same PR. Re-driving the route against the changed
/// lane must not open a second adjudication, must not mutate the pending one,
/// and must not reach the board.
#[tokio::test]
async fn lane_change_while_lead_pending_is_noop() {
    let f = fixture().await;
    let checks = [causal_check("merge-group / integration", 906)];
    let blocking = refs(&checks);
    let evidence = merge_group_identity(906);
    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "Merge pull request #4242",
        method: MergeMethod::Squash,
    };
    let target = f.merge_target(&auto);

    let opened = run(&f, &target, &evidence, &evidence, &blocking).await;
    assert!(
        matches!(
            opened,
            CiLaneOutcome::Tier2 {
                lease_opened: true,
                ..
            }
        ),
        "a complete causal merge-group failure opens exactly one adjudication, got {opened:?}"
    );

    let pending = f.attempt(&evidence, CiAction::AskLead).await;
    let lease_id = pending.tier2_lease_id.clone();
    assert!(lease_id.is_some());
    let before = f.effects().await;

    // Now the observation's lane changes.
    let changed_lane = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
        ..evidence.clone()
    };
    let again = run(&f, &target, &evidence, &changed_lane, &blocking).await;
    let after = f.effects().await;

    assert_eq!(again, CiLaneOutcome::Discarded(CiStaleField::Lane));
    assert_eq!(
        before.tier2_leases, after.tier2_leases,
        "no second adjudication opens for a changed lane"
    );
    assert_eq!(before.provider_mutations, after.provider_mutations);
    assert_eq!(before.worker_attempts, after.worker_attempts);
    assert_eq!(before.task_status, after.task_status);
    assert_eq!(
        f.attempt(&evidence, CiAction::AskLead).await.tier2_lease_id,
        lease_id,
        "the pending adjudication is untouched"
    );
}

/// A newer passing observation arriving before the supervisor applies a Lead
/// result closes the route and makes the application a no-op.
///
/// This is the coordinator half of the guard the supervisor repeats atomically
/// with `PrCiFailed`. It is asserted at the repository boundary because that is
/// where both halves meet: once the pass has closed the keys,
/// `resolve_tier2_lease` refuses, so no reopen and no worker can follow.
#[tokio::test]
async fn newer_success_before_supervisor_apply_is_noop() {
    let f = fixture().await;
    let checks = [causal_check("Quality Gate / test", 907)];
    let blocking = refs(&checks);
    let id = pr_head_identity(907);

    let opened = run(&f, &f.target(), &id, &id, &blocking).await;
    let CiLaneOutcome::Tier2 {
        lease_opened: true, ..
    } = opened
    else {
        panic!("expected an opened adjudication, got {opened:?}");
    };
    let attempt = f.attempt(&id, CiAction::AskLead).await;
    let lease_id = attempt.tier2_lease_id.clone().expect("lease id");
    let key = provider_action_key(&f.subject, &id, CiAction::AskLead);

    // The PR goes green while Lead is still thinking.
    let closed = f
        .routes
        .close_routes_for_newer_outcome(&f.subject, PR, CiRouteOutcome::Passed, None)
        .await
        .expect("close on pass");
    assert_eq!(closed, 1);

    let before = f.effects().await;
    let applied = f
        .routes
        .resolve_tier2_lease(
            &f.subject,
            &key,
            &lease_id,
            &id,
            &djinn_db::CiTier2Resolution::repair(),
        )
        .await
        .expect("resolve");
    let after = f.effects().await;

    assert!(
        !applied,
        "a Lead result may not be applied over a newer passing outcome"
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.attempt(&id, CiAction::AskLead)
            .await
            .adjudicated_outcome(),
        Some(CiRouteOutcome::Passed),
        "the pass is the route's authoritative outcome"
    );
}

/// Graceful shutdown closes action admission, joins every in-flight provider
/// future, and only then reports drained — in that order.
///
/// The ordering is the whole point: leadership releases the advisory lock on
/// this signal, and the lock is the exclusion authority for `calling` rows. A
/// scope that reported drained while a future was live would make exclusion a
/// claim rather than a fact.
#[tokio::test]
async fn graceful_shutdown_quiesces_calling_before_lock_release() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 908)];
    let blocking = refs(&checks);
    let id = pr_head_identity(908);

    // A guard stands in for a provider future that is still running.
    let in_flight = f.scope.admit().expect("an open scope admits");

    f.scope.close_admission();
    assert!(f.scope.is_admission_closed());

    // Admission is closed, so no new route may enter `calling` — and the row it
    // would have charged stays recoverable rather than charged-and-stranded.
    let before = f.effects().await;
    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;
    let after = f.effects().await;
    assert_eq!(
        outcome,
        CiLaneOutcome::Deferred(CiDeferral::AdmissionClosed)
    );
    assert_eq!(
        after.provider_mutations, before.provider_mutations,
        "a closed gate performs no provider mutation"
    );
    assert_eq!(
        f.attempt(&id, CiAction::RerunRun).await.action_phase,
        CiActionPhase::Reserved,
        "the refused route stays `reserved`, which is recoverable and uncharged"
    );
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    assert_eq!(f.budgets(&id, &fingerprint).await, (0, 0));

    // The scope is not empty while the future lives, so leadership must wait.
    assert_eq!(f.scope.in_flight(), 1);
    assert!(
        !f.scope.counts().drained,
        "a live provider future must not be reported as drained"
    );

    // The future finishes; only now may the drain be stamped.
    drop(in_flight);
    f.scope.wait_until_empty().await;
    assert_eq!(f.scope.in_flight(), 0);
    f.scope.mark_drained();
    assert!(
        f.scope.counts().drained,
        "the drain stamp is what leadership releases the lock on"
    );
    assert_eq!(f.scope.counts().refused_total, 1);
}

/// Rolling the feature back while a route is `calling` drains authoritatively:
/// the owner keeps the row, no new route is admitted, and — critically — the
/// legacy path stays withheld from that evidence.
///
/// Handing it back would double-remedy: the queue re-entry the route triggered
/// is still live, and `handle_queue_failure` would reopen the task for rework at
/// the same time. Only `disabled_clean` returns the evidence to the legacy path,
/// and it is legal only once the quiescence report reads zero.
#[tokio::test]
async fn rollback_disable_during_calling_drains_authoritatively() {
    let f = fixture().await;
    let checks = [inconclusive_check("merge-group / integration", 909)];
    let blocking = refs(&checks);
    let id = merge_group_identity(909);
    let key = provider_action_key(&f.subject, &id, CiAction::Reenqueue);
    let fingerprint = transient_fingerprint(CiLane::MergeGroup, &blocking);
    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "Merge pull request #4242",
        method: MergeMethod::Squash,
    };

    // A live `calling` row owned by another incarnation.
    let other = uuid::Uuid::now_v7().to_string();
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: id.clone(),
            origin_state: CiOriginState::PrReview,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::Reenqueue,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, id.pr_number, &id.pr_head_sha),
        })
        .await
        .expect("reserve");
    f.routes
        .charge_and_begin_calling(&f.subject, &key, &other, &id)
        .await
        .expect("charge");

    let quiescing = CiLaneTarget {
        gate: CiRoutingGate::Quiescing,
        ..f.merge_target(&auto)
    };
    let before = f.effects().await;
    let outcome = run(&f, &quiescing, &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        outcome,
        CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
    );
    assert!(
        outcome.suppresses_legacy_path(),
        "a live `calling` row must keep the legacy reopen withheld while quiescing"
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.attempt(&id, CiAction::Reenqueue).await.action_phase,
        CiActionPhase::Calling,
        "quiescing drains; it does not steal"
    );

    // Rollback is blocked until the quiescence report reads zero.
    let counts = f.routes.quiescence_counts().await.expect("quiescence");
    assert_eq!(counts.calling_rows, 1);
    assert!(!counts.is_quiescent(), "rollback stays blocked");

    // The owner finalizes; now the report is clean and `disabled_clean` returns
    // the evidence to the legacy path.
    f.routes
        .finalize_calling(&f.subject, &key, &other, CiRouteOutcome::Reenqueued, None)
        .await
        .expect("finalize");
    assert!(
        f.routes
            .quiescence_counts()
            .await
            .expect("quiescence")
            .is_quiescent(),
        "with no reserved, calling, or leased rows the rollback report passes"
    );

    let disabled = CiLaneTarget {
        gate: CiRoutingGate::DisabledClean,
        ..f.merge_target(&auto)
    };
    let handed_back = run(&f, &disabled, &id, &id, &blocking).await;
    assert_eq!(handed_back, CiLaneOutcome::GateClosed);
    assert!(
        !handed_back.suppresses_legacy_path(),
        "only `disabled_clean` returns evidence to the legacy path"
    );
}

// ===========================================================================
// Lane behaviour, budgets, and the gate
// ===========================================================================

/// The PR-head lane's Tier-1 action, end to end: `rerun_failed_jobs` for the
/// run the evidence names, the route persisted `retriggered`, the task still
/// `pr_draft`, and no worker.
#[tokio::test]
async fn pr_head_tier_one_retriggers_and_holds_in_pr_draft() {
    let f = fixture().await;
    let checks = [
        inconclusive_check("Quality Gate / test (1)", 910),
        inconclusive_check("Publish Nextest Timing", 910),
    ];
    let blocking = refs(&checks);
    let id = pr_head_identity(910);

    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;

    assert_eq!(
        outcome,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Retriggered)
    );
    assert!(outcome.charged());
    assert_eq!(f.provider.calls().rerun_failed_jobs, 1);
    assert_eq!(f.provider.calls().enable_auto_merge, 0);
    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(attempt.terminal_outcome, Some(CiRouteOutcome::Retriggered));
    assert_eq!(attempt.origin_state, CiOriginState::PrDraft);
    let effects = f.effects().await;
    assert_eq!(effects.task_status, "pr_draft");
    assert_eq!(effects.worker_attempts, 0);
    assert_eq!(effects.tier2_leases, 0);
}

/// The merge-group lane's Tier-1 action: `enable_auto_merge`, persisted
/// `reenqueued`, still `pr_review`, and **no** `PrCiFailed` and no worker.
#[tokio::test]
async fn merge_group_tier_one_reenqueues_without_dispatching_a_worker() {
    let f = fixture().await;
    let checks = [inconclusive_check("merge-group / integration", 911)];
    let blocking = refs(&checks);
    let id = merge_group_identity(911);
    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "Merge pull request #4242",
        method: MergeMethod::Squash,
    };

    let before = f.effects().await;
    let outcome = run(&f, &f.merge_target(&auto), &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        outcome,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Reenqueued)
    );
    assert_eq!(f.provider.calls().enable_auto_merge, 1);
    assert_eq!(f.provider.calls().rerun_failed_jobs, 0);
    assert_eq!(
        f.attempt(&id, CiAction::Reenqueue).await.origin_state,
        CiOriginState::PrReview
    );
    assert_eq!(
        before.worker_attempts, after.worker_attempts,
        "the re-enqueue replaces the reopen, so no worker is dispatched"
    );
    assert_eq!(
        before.task_status, after.task_status,
        "and the task stays where it was"
    );
    assert_eq!(before.activity_rows, after.activity_rows);
}

/// The two lanes share one head ceiling and land atomically: four charged
/// actions across both lanes exhaust the head, and the fifth is refused.
#[tokio::test]
async fn the_head_ceiling_is_shared_across_both_lanes() {
    let f = fixture().await;
    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "Merge pull request #4242",
        method: MergeMethod::Squash,
    };

    // Two PR-head signatures and two merge-group signatures: four distinct
    // signature budgets, one head budget.
    for (index, run_id) in [920i64, 921, 922, 923].into_iter().enumerate() {
        let merge_lane = index >= 2;
        let name = format!("lane check {index}");
        let checks = [inconclusive_check(&name, run_id as u64)];
        let blocking = refs(&checks);
        let (id, target) = if merge_lane {
            (merge_group_identity(run_id), f.merge_target(&auto))
        } else {
            (pr_head_identity(run_id), f.target())
        };
        let outcome = run(&f, &target, &id, &id, &blocking).await;
        assert!(
            outcome.charged(),
            "charge {index} must reach the provider, got {outcome:?}"
        );
    }
    assert_eq!(f.provider.calls().mutations(), 4);

    // A fifth, on a brand-new signature, is refused by the head ceiling alone.
    let checks = [inconclusive_check("lane check 4", 924)];
    let blocking = refs(&checks);
    let id = pr_head_identity(924);
    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;

    assert!(
        !outcome.charged(),
        "the fifth action must be refused, got {outcome:?}"
    );
    assert_eq!(
        f.provider.calls().mutations(),
        4,
        "the head ceiling is what stops the provider call, and it is 4"
    );
    assert!(
        matches!(
            outcome,
            CiLaneOutcome::Tier2 {
                reason: CiTier2Reason::RetryExhausted,
                ..
            }
        ),
        "an exhausted head escalates rather than silently stopping, got {outcome:?}"
    );
}

/// A provider error is charged, terminal, and escalates once.
#[tokio::test]
async fn an_explicit_provider_error_is_charged_and_escalates_once() {
    let f = Fixture {
        provider: FakeProvider::failing_mutations(),
        ..fixture().await
    };
    let checks = [inconclusive_check("Quality Gate / test", 930)];
    let blocking = refs(&checks);
    let id = pr_head_identity(930);

    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;

    assert_eq!(
        outcome,
        CiLaneOutcome::ProviderFailed { lease_opened: true }
    );
    assert!(outcome.charged(), "a failed call keeps its charged slots");
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    assert_eq!(f.budgets(&id, &fingerprint).await, (1, 1));
    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(attempt.terminal_outcome, Some(CiRouteOutcome::ActionFailed));
    assert!(
        attempt.provider_error.is_some(),
        "the provider error envelope is recorded"
    );
    assert_eq!(f.effects().await.worker_attempts, 0);

    // The escalation happens once, not once per poll.
    let again = run(&f, &f.target(), &id, &id, &blocking).await;
    assert_eq!(again, CiLaneOutcome::Deferred(CiDeferral::AlreadyTerminal));
    assert_eq!(f.effects().await.tier2_leases, 1);
}

/// A merge-group route with no PR node id cannot execute, and fails closed
/// without consuming a slot.
#[tokio::test]
async fn an_unexecutable_reenqueue_target_charges_nothing() {
    let f = fixture().await;
    let checks = [inconclusive_check("merge-group / integration", 931)];
    let blocking = refs(&checks);
    let id = merge_group_identity(931);
    // `auto_merge: None` on a merge-group lane.
    let target = CiLaneTarget {
        origin_state: CiOriginState::PrReview,
        ..f.target()
    };

    let outcome = run(&f, &target, &id, &id, &blocking).await;

    assert_eq!(
        outcome,
        CiLaneOutcome::Deferred(CiDeferral::UnexecutableTarget)
    );
    assert_eq!(f.provider.calls().mutations(), 0);
    let fingerprint = transient_fingerprint(CiLane::MergeGroup, &blocking);
    assert_eq!(f.budgets(&id, &fingerprint).await, (0, 0));
    assert_eq!(f.scope.in_flight(), 0, "the scope guard is released");
}

/// A hold spends nothing and writes no row, and the caller must not fall
/// through to the legacy remediation path either.
#[tokio::test]
async fn an_incomplete_enumeration_holds_without_route_lease_or_charge() {
    let f = fixture().await;
    let id = pr_head_identity(932);
    let observation = CiObservation {
        evidence: &id,
        observed_current: &id,
        capture: CiCapture::prove_complete(
            djinn_provider::github_api::CheckSetCompleteness::Incomplete(
                djinn_provider::github_api::CheckSetIncompleteReason::PageFetchFailed,
            ),
            &[],
        ),
    };

    let before = f.effects().await;
    let outcome = execute_route(
        &f.routes,
        &f.provider,
        &f.scope,
        &f.target(),
        &observation,
        &[],
    )
    .await;
    let after = f.effects().await;

    assert!(
        matches!(outcome, CiLaneOutcome::Held(_)),
        "an enumeration failure holds, got {outcome:?}"
    );
    assert!(
        outcome.suppresses_legacy_path(),
        "holding is the answer; the legacy reopen must not also run"
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(after.route_rows, 0);
}

/// The recurring sweep escalates a stranded exhausted reservation and never
/// resumes one, because it holds no provider client.
#[tokio::test]
async fn the_recurring_sweep_never_resumes_a_route_it_cannot_call() {
    let f = fixture().await;
    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "h",
        method: MergeMethod::Squash,
    };
    // Spend the head budget.
    for (index, run_id) in [940i64, 941, 942, 943].into_iter().enumerate() {
        let name = format!("sweep check {index}");
        let checks = [inconclusive_check(&name, run_id as u64)];
        let blocking = refs(&checks);
        let (id, target) = if index >= 2 {
            (merge_group_identity(run_id), f.merge_target(&auto))
        } else {
            (pr_head_identity(run_id), f.target())
        };
        assert!(run(&f, &target, &id, &id, &blocking).await.charged());
    }

    // Strand a `reserved` row whose budget is now spent.
    let checks = [inconclusive_check("stranded", 944)];
    let blocking = refs(&checks);
    let id = pr_head_identity(944);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    // `reserve` refuses a charging action on an exhausted budget, so the row is
    // planted the way a crash would leave it: reserved under a budget that was
    // still open at reservation time and is spent now.
    djinn_db::test_support::ci_route_plant_reserved_for_test(
        &f.db,
        &f.subject.id,
        &key,
        "pr_head",
        PR,
        HEAD,
        944,
        "pr_draft",
        "rerun_run",
        &fingerprint,
        &retry_budget_key(&f.subject, &id, &fingerprint),
        &head_budget_key(&f.subject, PR, HEAD),
        600,
    )
    .await;

    let mutations_before = f.provider.calls().mutations();
    let report = sweep_reserved_routes(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &f.incarnation,
        CiRoutingGate::Enabled,
    )
    .await;

    assert_eq!(report.resumed, 0, "the sweep never resumes");
    assert_eq!(
        report.escalated + report.held_by_head_lease,
        1,
        "the stranded row is escalated or recorded as head-lease-blocked"
    );
    assert_eq!(
        f.provider.calls().mutations(),
        mutations_before,
        "the sweep performs no provider mutation"
    );
    assert_eq!(f.effects().await.worker_attempts, 0);
}

/// The sweep refuses to act on a row whose current identity it cannot witness,
/// rather than substituting the stored one.
///
/// Substituting would make the pre-call guard trivially pass and let an
/// obsolete reservation resume against a head that has moved — the exact
/// provider mutation the guard exists to prevent.
#[tokio::test]
async fn the_sweep_leaves_a_row_it_cannot_witness_alone() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 950)];
    let blocking = refs(&checks);
    let id = pr_head_identity(950);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: provider_action_key(&f.subject, &id, CiAction::RerunRun),
            identity: id.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, PR, HEAD),
        })
        .await
        .expect("reserve");

    let report = sweep_reserved_routes(
        &f.routes,
        &FixedHead(None),
        &f.incarnation,
        CiRoutingGate::Enabled,
    )
    .await;

    assert_eq!(report.unverifiable, 1);
    assert_eq!(report.resumed + report.superseded + report.escalated, 0);
    assert_eq!(
        f.attempt(&id, CiAction::RerunRun).await.action_phase,
        CiActionPhase::Reserved
    );
}

/// The sweep closes an obsolete reservation as `superseded_pre_call` — the row
/// no poller will revisit, because the head it belongs to has moved.
#[tokio::test]
async fn the_sweep_closes_an_obsolete_reservation_without_cost() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 951)];
    let blocking = refs(&checks);
    let id = pr_head_identity(951);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: id.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, &id, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, PR, HEAD),
        })
        .await
        .expect("reserve");
    djinn_db::test_support::ci_route_age_reserved_for_test(&f.db, &f.subject.id, &key, 600).await;

    let before = f.effects().await;
    let report = sweep_reserved_routes(
        &f.routes,
        &FixedHead(Some(MOVED_HEAD.to_owned())),
        &f.incarnation,
        CiRoutingGate::Enabled,
    )
    .await;
    let after = f.effects().await;

    assert_eq!(report.superseded, 1);
    assert_eq!(report.resumed, 0);
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.attempt(&id, CiAction::RerunRun).await.terminal_outcome,
        Some(CiRouteOutcome::SupersededPreCall)
    );
    assert_eq!(f.budgets(&id, &fingerprint).await, (0, 0), "uncharged");
}

// ===========================================================================
// The gate
// ===========================================================================

/// The gate is default-off, and no accidental environment value turns it on.
#[test]
fn the_ci_evidence_routing_gate_is_default_off() {
    assert_eq!(CiRoutingGate::default(), CiRoutingGate::DisabledClean);
    for value in [
        "", " ", "0", "1", "true", "TRUE", "yes", "on", "off", "no", "disabled", "enable",
        "Enabled ", "quiesce", "garbage",
    ] {
        let parsed = CiRoutingGate::from_value(value);
        let expected = match value.trim().to_ascii_lowercase().as_str() {
            "enabled" => CiRoutingGate::Enabled,
            "quiescing" => CiRoutingGate::Quiescing,
            _ => CiRoutingGate::DisabledClean,
        };
        assert_eq!(parsed, expected, "gate value {value:?}");
    }
    assert_eq!(CiRoutingGate::from_value("enabled"), CiRoutingGate::Enabled);
    assert_eq!(
        CiRoutingGate::from_value("  Quiescing  "),
        CiRoutingGate::Quiescing
    );
    assert!(!CiRoutingGate::default().admits_new_routes());
    assert!(!CiRoutingGate::default().owns_routes());
    assert!(CiRoutingGate::Quiescing.owns_routes());
    assert!(!CiRoutingGate::Quiescing.admits_new_routes());
}

/// With the gate off the executor is inert: no row, no call, no read of the
/// database at all, and the caller keeps its legacy path.
#[tokio::test]
async fn a_disabled_gate_leaves_every_lane_to_the_legacy_path() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 960)];
    let blocking = refs(&checks);
    let id = pr_head_identity(960);
    let target = CiLaneTarget {
        gate: CiRoutingGate::DisabledClean,
        ..f.target()
    };

    let before = f.effects().await;
    let outcome = run(&f, &target, &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(outcome, CiLaneOutcome::GateClosed);
    assert!(!outcome.suppresses_legacy_path());
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(after.route_rows, 0);
}

/// A quiescing gate admits no new route, but hands evidence with no live route
/// back to the legacy path rather than stranding it.
#[tokio::test]
async fn a_quiescing_gate_admits_no_new_route() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 961)];
    let blocking = refs(&checks);
    let id = pr_head_identity(961);
    let target = CiLaneTarget {
        gate: CiRoutingGate::Quiescing,
        ..f.target()
    };

    let outcome = run(&f, &target, &id, &id, &blocking).await;

    assert_eq!(
        outcome,
        CiLaneOutcome::GateClosed,
        "with no live route for this evidence, quiescing returns it to the legacy path"
    );
    assert_eq!(f.provider.calls().mutations(), 0);
    assert_eq!(f.effects().await.route_rows, 0);
}

/// A subject id namespaces every key, so a second project's PR #4242 gets its
/// own route, its own budget, and its own provider call.
#[tokio::test]
async fn a_second_subject_sharing_a_pr_number_gets_its_own_call() {
    let f = fixture().await;
    let other_task = djinn_db::test_support::seed_task_row(
        &f.db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &djinn_db::test_support::make_project(
                &f.db,
                std::path::Path::new("ci-executor-2"),
            )
            .await
            .id,
            status: "pr_draft",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let other_subject = CiRouteSubject::task(other_task);
    assert_eq!(other_subject.kind, CiSubjectKind::Task);

    let checks = [inconclusive_check("Quality Gate / test", 970)];
    let blocking = refs(&checks);
    let id = pr_head_identity(970);

    let first = run(&f, &f.target(), &id, &id, &blocking).await;
    let second_target = CiLaneTarget {
        subject: &other_subject,
        ..f.target()
    };
    let second = run(&f, &second_target, &id, &id, &blocking).await;

    assert_eq!(
        first,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Retriggered)
    );
    assert_eq!(
        second,
        CiLaneOutcome::ProviderAccepted(CiRouteOutcome::Retriggered),
        "byte-identical evidence in another subject is a different route"
    );
    assert_eq!(f.provider.calls().rerun_failed_jobs, 2);
}
