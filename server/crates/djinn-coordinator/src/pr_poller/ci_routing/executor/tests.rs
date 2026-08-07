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
    CiActionPhase, CiCallingRecoveryReason, CiEvidenceIdentity, CiLane, CiOriginState,
    CiQuiescenceProof, CiRouteOutcome, CiRouteSubject, CiSubjectKind, Database,
};
use djinn_provider::github_api::{
    CheckAnnotation, CheckRun, CheckRunsResponse, CheckSetIncompleteReason, GitHubApiError,
    MergeMethod, ReproductionJob, ReproductionStep, RequiredCheckReproduction,
    RequiredCheckReproductionContext, RequiredCheckUnreproducible,
    RequiredCheckUnreproducibleReason,
};

use super::*;
use crate::pr_poller::ci_lane_routing::CiLaneDisposition;
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
    /// Wave 5's repair-corpus read. Deliberately NOT in `mutations`.
    reproduction: usize,
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
    /// `CheckApiError`'s producer: `list_check_runs_for_ref` refusing *after*
    /// the merge-group run identity is already known.
    fail_check_runs: bool,
    /// `LogApiError`'s producer: the annotation read failing, likewise after an
    /// immutable run is known.
    fail_annotations: bool,
    /// What `list_check_runs_for_ref` returns when it does not fail.
    check_runs: Vec<CheckRun>,
    /// The completeness verdict `list_check_runs_for_ref` reports. Defaults to
    /// `Complete`; a fixture sets it to drive the merge-group lane's
    /// *lane-level* incomplete captures, which are the ones that used to be
    /// keyed on a fabricated identity.
    check_runs_completeness: Option<CheckSetIncompleteReason>,
    /// The command `required_check_reproduction_context` reports as the one CI
    /// actually ran. `None` makes the check unreproducible, which is the case
    /// that leaves a route with an empty repair corpus.
    reproduction_command: Option<String>,
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

    fn set_fail_check_runs(&self) {
        self.state
            .lock()
            .expect("fake provider mutex")
            .fail_check_runs = true;
    }

    fn set_fail_annotations(&self) {
        self.state
            .lock()
            .expect("fake provider mutex")
            .fail_annotations = true;
    }

    fn set_check_runs(&self, runs: Vec<CheckRun>) {
        self.state.lock().expect("fake provider mutex").check_runs = runs;
    }

    /// Make `list_check_runs_for_ref` report an incomplete enumeration rather
    /// than failing outright — the shape a truncated or short read has.
    fn set_check_runs_incomplete(&self, reason: CheckSetIncompleteReason) {
        self.state
            .lock()
            .expect("fake provider mutex")
            .check_runs_completeness = Some(reason);
    }

    /// The command `required_check_reproduction_context` reports. `None` makes
    /// every check unreproducible, which is how the empty-corpus case is
    /// exercised.
    fn set_reproduction_command(&self, command: Option<String>) {
        self.state
            .lock()
            .expect("fake provider mutex")
            .reproduction_command = command;
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
        let runs = state.check_runs.clone();
        Ok(match state.check_runs_completeness {
            Some(reason) => CheckRunsResponse::incomplete(
                u32::try_from(runs.len()).unwrap_or(u32::MAX) + 1,
                runs,
                reason,
            ),
            None => CheckRunsResponse::complete(runs),
        })
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

    /// The repair-corpus read (wave 5).
    ///
    /// Counted like every other call so a fixture can prove it is a **read**:
    /// `ProviderCalls::mutations` deliberately excludes it, and the discard
    /// fixtures still assert zero mutations after a route that queried it.
    async fn required_check_reproduction_context(
        &self,
        _owner: &str,
        _repo: &str,
        observed_head_sha: &str,
        required_check_name: &str,
    ) -> Result<RequiredCheckReproduction, GitHubApiError> {
        let mut state = self.state.lock().expect("fake provider mutex");
        state.calls.reproduction += 1;
        let Some(command) = state.reproduction_command.clone() else {
            return Ok(RequiredCheckReproduction::Unreproducible(
                RequiredCheckUnreproducible {
                    required_check_name: required_check_name.to_owned(),
                    observed_head_sha: observed_head_sha.to_owned(),
                    reason: RequiredCheckUnreproducibleReason::CommandNotFound,
                    details: Some("fixture has no reproduction command".to_owned()),
                },
            ));
        };
        Ok(RequiredCheckReproduction::Reproducible(
            RequiredCheckReproductionContext {
                required_check_name: required_check_name.to_owned(),
                observed_head_sha: observed_head_sha.to_owned(),
                check_run_id: 1,
                workflow_run_id: 1,
                workflow_name: None,
                job: ReproductionJob {
                    id: 1,
                    name: required_check_name.to_owned(),
                    html_url: String::new(),
                },
                failing_step: ReproductionStep {
                    number: 1,
                    name: "run".to_owned(),
                },
                command,
                setup_steps: Vec::new(),
                log_tail: String::new(),
            },
        ))
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

    async fn ci_snapshot(&self) -> Option<djinn_core::models::TaskPrCiSnapshot> {
        djinn_db::TaskRepository::new(
            self.db.clone(),
            djinn_db::test_support::event_bus_for(&tokio::sync::broadcast::channel(4).0),
        )
        .get_ci_snapshot_for_task_pr(&self.task_id, PR)
        .await
        .expect("snapshot read")
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

/// Drive one route from an explicit capture, for the lane-level captures that
/// have no per-run blocking set (complete-empty, hold, incomplete).
async fn run_capture(
    f: &Fixture,
    target: &CiLaneTarget<'_>,
    identity: &CiEvidenceIdentity,
    capture: CiCapture<'_>,
) -> CiLaneOutcome {
    let observation = CiObservation {
        evidence: identity,
        observed_current: identity,
        capture,
    };
    execute_route(&f.routes, &f.provider, &f.scope, target, &observation, &[]).await
}

/// A check run that concluded green.
fn passing_check(name: &str) -> CheckRun {
    CheckRun {
        conclusion: Some("success".to_owned()),
        ..inconclusive_check(name, 1)
    }
}

const MERGE_GROUP_SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";
const DEQUEUE_ID: &str = "refs/heads/gh-readonly-queue/main/pr-4242-a@2026-08-06T00:00:00Z";

/// A terminal merge-group run that `correlate_merge_group_run` will accept:
/// the `pr-4242-` marker in its branch and a failing conclusion.
fn merge_group_run(id: u64) -> djinn_provider::github_api::WorkflowRun {
    djinn_provider::github_api::WorkflowRun {
        id,
        workflow_id: None,
        name: Some("CI".to_owned()),
        path: Some(".github/workflows/ci.yml".to_owned()),
        head_branch: Some("gh-readonly-queue/main/pr-4242-abc".to_owned()),
        head_sha: MERGE_GROUP_SHA.to_owned(),
        status: Some("completed".to_owned()),
        conclusion: Some("failure".to_owned()),
    }
}

fn dequeue_event() -> djinn_provider::github_api::DequeueEvent {
    djinn_provider::github_api::DequeueEvent {
        reason: Some("failed_checks".to_owned()),
        merge_group_ref: Some("refs/heads/gh-readonly-queue/main/pr-4242-a".to_owned()),
        created_at: Some("2026-08-06T00:00:00Z".to_owned()),
        before_commit_sha: None,
    }
}

// ---------------------------------------------------------------------------
// The lane-wrapper harness
// ---------------------------------------------------------------------------

/// A real `CoordinatorActor` over a real ephemeral database.
///
/// `crate::actor::actor_with_test_db` uses the same
/// `CoordinatorActor::new(CoordinatorDeps::new(..))` constructor production
/// does, so a method driven through this is the production method — not a
/// reimplementation of it.
struct LaneHarness {
    actor: crate::actor::CoordinatorActor,
    db: Database,
    task_id: String,
}

impl LaneHarness {
    async fn ci_snapshot(&self) -> Option<djinn_core::models::TaskPrCiSnapshot> {
        djinn_db::TaskRepository::new(
            self.db.clone(),
            djinn_db::test_support::event_bus_for(&tokio::sync::broadcast::channel(4).0),
        )
        .get_ci_snapshot_for_task_pr(&self.task_id, PR)
        .await
        .expect("snapshot read")
    }
}

async fn lane_harness() -> LaneHarness {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project = djinn_db::test_support::make_project(&db, std::path::Path::new("ci-lane")).await;
    let task_id = djinn_db::test_support::seed_task_row(
        &db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project.id,
            status: "pr_review",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    LaneHarness {
        actor: crate::actor::actor_with_test_db(db.clone()),
        db,
        task_id,
    }
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
    //
    // The `age_calling` is load-bearing and its absence made this test a label.
    // Without it the row's `calling_at` is seconds old, so `recover_calling_owner`
    // defers on `TimeoutNotElapsed` and never evaluates the quiescence
    // predicate at all — mutating `CiQuiescenceProof::None` to "recoverable"
    // survived the whole coordinator suite, caught only by W1's own repository
    // test. Aging past the 300s window is what makes the timeout stop being the
    // reason, so the deferral this asserts is the one the name claims.
    age_calling(&f, &key, 400).await;

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
    assert_eq!(report.deferred, 1);
    assert_eq!(report.outcome_unknown, 0);

    // And assert *which* refusal, from the repository's own audit trail rather
    // than from a count that any of seven reasons would satisfy.
    let audit = f
        .routes
        .calling_recovery_audit(&f.subject, &key)
        .await
        .expect("calling-recovery audit");
    let reasons: Vec<CiCallingRecoveryReason> =
        audit.iter().map(|record| record.recovery_reason).collect();
    assert!(
        reasons.contains(&CiCallingRecoveryReason::LiveOwnerDeferred),
        "a live, undrained owner must be refused for *that* reason, not a timeout; got {reasons:?}",
    );
    assert!(
        !reasons.contains(&CiCallingRecoveryReason::TimeoutNotElapsed),
        "the 300s window must already have elapsed, or the quiescence predicate \
         is never reached; got {reasons:?}",
    );
    assert!(
        audit.iter().all(|record| !record.cas_won),
        "no compare-and-set may win against a live owner"
    );
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

/// A closed scope refuses new routes, and the drain stamp cannot be taken while
/// an in-flight provider future is still live.
///
/// The ordering matters because leadership releases the advisory lock on the
/// drain stamp, and the lock is the exclusion authority for `calling` rows. A
/// scope that reported drained while a future was live would make exclusion a
/// claim rather than a fact.
///
/// # What this test does NOT prove
///
/// It drives the scope by hand — this body calls `close_admission`,
/// `wait_until_empty`, and `mark_drained` itself. So it pins the
/// `ProviderActionScope` CONTRACT: that the primitive refuses admission when
/// closed and withholds the drain stamp until in-flight reaches zero.
///
/// It does not prove that anything in production drives the scope that way,
/// because it never calls `run_with_leadership`, never cancels a token, and
/// never touches the advisory lock. A test cannot witness an ordering it is
/// itself performing. Deleting the `quiesce_provider_actions(...)` call from
/// `server/src/leadership.rs` leaves this test green.
///
/// The production ordering — cancellation, drain, lock release, then a NEW
/// acquisition by a second connection — is pinned in
/// `server/tests/leadership_quiescence.rs`, which passes a real scope and
/// observes lock availability from its own Postgres session. Both halves are
/// required: this one for the primitive, that one for the caller.
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

// ===========================================================================
// `from_env` — the function every production call site actually uses
// ===========================================================================

/// The *absent* case is the production default, and it had zero coverage.
///
/// `from_value` was exhaustively tested and tested nothing that mattered: the
/// default lived in `from_env`'s `unwrap_or_default()`, and changing that one
/// call to `unwrap_or(Enabled)` turned `ci_evidence_routing` on fleet-wide with
/// all 2148 tests green. The default now lives in `from_lookup`, which takes the
/// environment as an argument, so this drives it directly.
#[test]
fn the_absent_gate_value_resolves_to_disabled_clean() {
    // Absent — the production default on every machine that has not opted in.
    assert_eq!(
        CiRoutingGate::from_lookup(|_| None),
        CiRoutingGate::DisabledClean,
        "an unset DJINN_CI_EVIDENCE_ROUTING must leave the feature off",
    );

    // Present but meaningless. None of these may opt in by accident, and the
    // boolean spellings are called out because the older flags in this crate
    // use them — an operator who types `true` must not get a feature whose
    // three states they did not choose between.
    for value in [
        "",
        " ",
        "\t",
        "0",
        "1",
        "true",
        "TRUE",
        "yes",
        "no",
        "on",
        "off",
        "disabled",
        "disabled_clean",
        "enable",
        "quiesce",
        "garbage",
        "Enabled=1",
    ] {
        assert_eq!(
            CiRoutingGate::from_lookup(|_| Some(value.to_owned())),
            CiRoutingGate::DisabledClean,
            "gate value {value:?} must not enable the feature",
        );
    }

    // The two opt-ins, and only these two.
    for (value, expected) in [
        ("enabled", CiRoutingGate::Enabled),
        ("  Enabled  ", CiRoutingGate::Enabled),
        ("ENABLED", CiRoutingGate::Enabled),
        ("quiescing", CiRoutingGate::Quiescing),
        ("  Quiescing\n", CiRoutingGate::Quiescing),
    ] {
        assert_eq!(
            CiRoutingGate::from_lookup(|_| Some(value.to_owned())),
            expected,
            "gate value {value:?}",
        );
    }
}

/// The lookup is asked for the documented variable and nothing else.
#[test]
fn the_gate_reads_exactly_one_environment_variable() {
    let mut seen: Vec<String> = Vec::new();
    let gate = CiRoutingGate::from_lookup(|key| {
        seen.push(key.to_owned());
        None
    });
    assert_eq!(gate, CiRoutingGate::DisabledClean);
    assert_eq!(seen, vec!["DJINN_CI_EVIDENCE_ROUTING".to_owned()]);
}

/// `from_env` must stay a bare delegation to the covered function.
///
/// Textual, and deliberately so: the default logic is now in `from_lookup`
/// where a test can reach it, and the only way to reintroduce the fleet-wide
/// mutation is to put logic back into `from_env`. This is the same
/// source-inspection guard `context.rs` uses for its own env wiring, and it is
/// the cheap half of a pair — the expensive half is the exhaustive
/// `from_lookup` test above.
#[test]
fn from_env_delegates_to_the_covered_lookup() {
    let source = include_str!("../gate.rs");
    let body = source
        .split("pub(crate) fn from_env() -> Self {")
        .nth(1)
        .expect("from_env is defined in gate.rs")
        .split("\n    }")
        .next()
        .expect("from_env has a body");
    assert!(
        body.contains("Self::from_lookup(|key| std::env::var(key).ok())"),
        "from_env must delegate to from_lookup; found: {body}",
    );
    assert!(
        !body.contains("unwrap_or"),
        "the default belongs in from_lookup, where a test can reach it; found: {body}",
    );
}

// ===========================================================================
// The complete-empty compatibility paths
// ===========================================================================

/// An authoritatively complete *empty* enumeration is a verdict of green, and
/// somebody has to write it down.
///
/// This was a wedge. `CiCompleteEmptyRoute` was produced by the classifier and
/// read by nobody: `fold` consulted only `suppresses_legacy_path`, which is true
/// of everything except `GateClosed`, so a merge-group dequeue whose correlated
/// run had no failing checks returned "routed", `handle_queue_failure` returned
/// early, and *nothing* recorded `Passing`, re-enqueued, or reopened. The task
/// sat in `pr_review` until a human noticed.
#[tokio::test]
async fn a_complete_empty_merge_group_records_passing_and_allows_the_gate() {
    let f = fixture().await;
    let snapshot_before = f.ci_snapshot().await;
    assert!(snapshot_before.is_none(), "precondition: no snapshot yet");

    let outcome = run_capture(
        &f,
        &f.target(),
        &merge_group_identity(970),
        CiCapture::prove_complete(
            djinn_provider::github_api::CheckSetCompleteness::Complete,
            &[],
        ),
    )
    .await;

    assert_eq!(
        outcome,
        CiLaneOutcome::CompleteEmpty(CiCompleteEmptyRoute::MergeGroupRecordPassing),
        "the classifier's answer for the review lane",
    );
    // The executor itself writes nothing durable — the snapshot is the lane
    // wrapper's job, and that is what the disposition test below drives.
    let effects = f.effects().await;
    assert_eq!(effects.route_rows, 0, "complete-empty creates no route row");
    assert_eq!(effects.tier2_leases, 0);
    assert_eq!(effects.provider_mutations, 0);
    assert_eq!(effects.worker_attempts, 0);
}

/// The lane wrapper executes the decision: `Passing` is persisted for the
/// current head, and the caller is told to skip remediation.
///
/// This is the assertion the wedge would fail. It drives
/// `route_merge_group_ci_evidence` — the production entry point
/// `handle_queue_failure` calls — with a correlated terminal merge-group run
/// whose check set comes back empty.
#[tokio::test]
async fn the_merge_group_lane_executes_the_complete_empty_route() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(970)],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert_eq!(
        disposition.complete_empty(),
        Some(CiCompleteEmptyRoute::MergeGroupRecordPassing),
    );
    assert!(
        disposition.is_routed(),
        "a verdict of green must not fall through to the reopen-for-rework path",
    );
    let snapshot = h
        .ci_snapshot()
        .await
        .expect("the review lane must record a verdict, or the merge gate holds forever");
    assert_eq!(snapshot.ci_status, djinn_core::models::CiStatus::Passing);
    assert_eq!(snapshot.head_sha, HEAD);
    assert_eq!(provider.calls().mutations(), 0, "no re-enqueue, no rerun");
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        0,
        "complete-empty creates no route row",
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
        "and dispatches no worker",
    );
}

/// The PR-head lane's half of the same contract.
#[tokio::test]
async fn the_pr_head_lane_executes_the_complete_empty_route() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    let disposition = h
        .actor
        .route_pr_head_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            &CheckRunsResponse::complete(Vec::new()),
            &[],
            CiRoutingGate::Enabled,
        )
        .await;

    assert_eq!(
        disposition.complete_empty(),
        Some(CiCompleteEmptyRoute::PrHeadProceed),
    );
    let snapshot = h
        .ci_snapshot()
        .await
        .expect("current-head verdict recorded");
    assert_eq!(snapshot.ci_status, djinn_core::models::CiStatus::Passing);
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        0,
    );
}

// ===========================================================================
// The lane wrappers, and the two producer-only incompleteness reasons
// ===========================================================================

/// `CheckApiError` has a producer, and this is it.
///
/// The reason is scoped to "after an immutable run is known", which is why it
/// can only arise here — once `correlate_merge_group_run` has named exactly one
/// terminal run and the check API *then* refuses. It is complete-but-unusable,
/// so it takes the guarded Tier-2 route rather than holding, and it keys its
/// route row on the known run rather than on a synthetic identity.
#[tokio::test]
async fn a_check_api_failure_after_correlation_routes_to_guarded_tier_two() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();
    provider.set_fail_check_runs();

    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(971)],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(provider.calls().list_check_runs, 1);
    assert_eq!(provider.calls().mutations(), 0, "no provider mutation");
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
        "complete-but-unusable evidence earns exactly one adjudication",
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
        "and no worker",
    );
    // Keyed on the known run, not on a synthetic per-head identity.
    let identity = CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: 971,
        run_head_sha: MERGE_GROUP_SHA.to_owned(),
        dequeue_id: Some(DEQUEUE_ID.to_owned()),
    };
    let subject = CiRouteSubject::task(h.task_id.clone());
    assert!(
        CiRouteAttemptRepository::new(h.db.clone())
            .get(
                &subject,
                &provider_action_key(&subject, &identity, CiAction::AskLead)
            )
            .await
            .expect("route read")
            .is_some(),
        "the route row must be keyed on the run the correlation named",
    );
}

/// `LogApiError`'s producer: the annotation read the evidence bundle depends on,
/// failing after the run identity is already known.
#[tokio::test]
async fn an_annotation_failure_after_correlation_routes_to_guarded_tier_two() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();
    provider.set_fail_annotations();
    provider.set_check_runs(vec![causal_check("merge-group / integration", 972)]);

    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(972)],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(
        provider.calls().annotations,
        1,
        "the annotation read is what produces LogApiError",
    );
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
    );
}

/// A dequeue this poll cannot name leaves the lane to the legacy path rather
/// than inventing an identity it could not revalidate on a later poll.
#[tokio::test]
async fn an_unidentifiable_dequeue_leaves_the_lane_to_the_legacy_path() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(973)],
            None,
            CiRoutingGate::Enabled,
        )
        .await;

    assert_eq!(disposition, CiLaneDisposition::Legacy);
    assert!(!disposition.is_routed());
    assert_eq!(provider.calls().mutations(), 0);
}

// ===========================================================================
// Lane-level captures are keyed on a real identity, or they are not keyed at all
// ===========================================================================

/// The fabricated identity `drive_lane` used to hand every lane-level capture.
///
/// Nothing may be findable at this key. It is the shape the audit named:
/// `run_id: 0` with `dequeue_id: None`, which collapses two merge-group dequeues
/// of one PR head onto a single `provider_action_key` — the second resolves
/// `AlreadyPresent` and never gets a route, and `stale_field` can never
/// supersede on a newer run or dequeue because both fields are constants.
fn fabricated_merge_group_identity() -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: 0,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

/// Whether a route row exists at the key `identity` derives.
async fn route_exists(h: &LaneHarness, identity: &CiEvidenceIdentity, action: CiAction) -> bool {
    let subject = CiRouteSubject::task(h.task_id.clone());
    CiRouteAttemptRepository::new(h.db.clone())
        .get(&subject, &provider_action_key(&subject, identity, action))
        .await
        .expect("route read")
        .is_some()
}

/// The merge-group lane's *lane-level* incomplete capture keys on the run the
/// correlation named — because that run is right there in scope.
///
/// `capture_merge_group_evidence` runs strictly after
/// `correlate_merge_group_run` has named exactly one terminal run, so every
/// verdict it can return has a real immutable identity available. The call site
/// nevertheless passed `None`, so a truncated merge-group enumeration took a
/// Tier-2 route row keyed on `run_id: 0` / `dequeue_id: None` while the real run
/// id, the real run head SHA, and the real dequeue id were all in scope.
#[tokio::test]
async fn a_lane_level_merge_group_capture_keys_on_the_correlated_run() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();
    // A truncated enumeration: not an enumeration *failure*, so it classifies
    // to Tier 2 and really does write a route row.
    provider.set_check_runs(vec![causal_check("merge-group / integration", 975)]);
    provider.set_check_runs_incomplete(CheckSetIncompleteReason::MaxPagesTruncated);

    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(975)],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(provider.calls().mutations(), 0, "no provider mutation");
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "precondition: this reason really does create a route row",
    );

    assert!(
        route_exists(&h, &merge_group_identity(975), CiAction::AskLead).await,
        "the lane-level capture must be keyed on the correlated run — run id, \
         run head SHA, and dequeue id were all already known",
    );
    assert!(
        !route_exists(&h, &fabricated_merge_group_identity(), CiAction::AskLead).await,
        "no route row may be keyed on the synthetic identity",
    );
}

/// Ambiguous correlation keeps its Tier-2 route (the proposal puts it there
/// explicitly) — and keys it on the **real dequeue id**, which is what stops two
/// dequeues of one head sharing a single provider-action key.
///
/// The dequeue id is resolved *before* correlation is attempted, so it was never
/// unknown at this branch; it was simply dropped.
#[tokio::test]
async fn an_ambiguous_correlation_keys_its_route_on_the_real_dequeue() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    // Two terminal merge-group runs for one PR: the queue ran it twice and
    // nothing can say which this dequeue refers to.
    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(976), merge_group_run(977)],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
    );

    // The lane identity: everything real except `run_id`, which stays 0 because
    // "ambiguous" means no single run exists to name. That residual is the one
    // this fixture pins — it must not silently grow a second field.
    let lane_identity = CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: 0,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: Some(DEQUEUE_ID.to_owned()),
    };
    assert!(
        route_exists(&h, &lane_identity, CiAction::AskLead).await,
        "the route must be keyed on the dequeue the poll actually named",
    );
    assert!(
        !route_exists(&h, &fabricated_merge_group_identity(), CiAction::AskLead).await,
        "dropping the dequeue id is what collapsed two dequeues onto one key",
    );
}

/// No correlated merge-group run at all **holds**, and holding writes nothing.
///
/// This is `MergeGroupCorrelationUnavailable`. Nothing was named, so there is no
/// run/lane identity a route row could honestly be keyed on — and AC5 admits to
/// Tier 2 only "the closed complete-but-unusable cases with a constructible
/// immutable run/lane identity". It used to take a Tier-2 row on the fabricated
/// identity; now it waits for the queue run to appear, which a later poll gets
/// for free.
///
/// It still suppresses the legacy remediation path: holding *is* the answer, and
/// falling through would reopen the task for rework on the strength of evidence
/// nobody has seen.
#[tokio::test]
async fn no_correlated_merge_group_run_holds_without_a_route_row() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    // A run for a *different* PR: the `pr-4242-` marker does not match, so the
    // candidate set is empty.
    let other_pr = djinn_provider::github_api::WorkflowRun {
        head_branch: Some("gh-readonly-queue/main/pr-9999-abc".to_owned()),
        ..merge_group_run(978)
    };
    let disposition = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[other_pr],
            Some(&dequeue_event()),
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(
        disposition.is_routed(),
        "a hold must not fall through to the legacy reopen",
    );
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        0,
        "an unnameable merge group may not create a route row",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        0,
        "and may not spend a Lead adjudication",
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
    );
}

/// A blocking check attributable to no Actions run **holds** on the PR-head lane.
///
/// `RunAttributionUnavailable`: there is no run identity to key on, and
/// `rerun_failed_jobs` has no run to act on either. It used to take a Tier-2 row
/// on the fabricated `run_id: 0` identity.
#[tokio::test]
async fn an_unattributable_blocking_check_holds_without_a_route_row() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    // No `run_id`, and an `html_url` that `parse_actions_run_id` cannot read.
    let mut orphan = causal_check("External / policy", 979);
    orphan.run_id = None;
    orphan.html_url = "https://example.test/checks/1".to_owned();
    let runs = vec![orphan];
    let blocking = refs(&runs);

    let disposition = h
        .actor
        .route_pr_head_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            &CheckRunsResponse::complete(runs.clone()),
            &blocking,
            CiRoutingGate::Enabled,
        )
        .await;

    assert!(disposition.is_routed(), "holding is the answer");
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        0,
        "no run was named, so nothing may be keyed on a fabricated one",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        0,
    );
}

/// Both lane wrappers decline with the gate off, without touching the database.
#[tokio::test]
async fn the_lane_wrappers_decline_when_the_gate_is_off() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    let merge = h
        .actor
        .route_merge_group_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            "PR_node",
            MergeMethod::Squash,
            &[merge_group_run(974)],
            Some(&dequeue_event()),
            CiRoutingGate::DisabledClean,
        )
        .await;
    let head = h
        .actor
        .route_pr_head_ci_evidence(
            &provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            &CheckRunsResponse::complete(vec![inconclusive_check("q", 974)]),
            &[],
            CiRoutingGate::DisabledClean,
        )
        .await;

    assert_eq!(merge, CiLaneDisposition::Legacy);
    assert_eq!(head, CiLaneDisposition::Legacy);
    assert_eq!(
        provider.calls(),
        ProviderCalls::default(),
        "no API call at all"
    );
    assert!(h.ci_snapshot().await.is_none(), "and no snapshot written");
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        0,
    );
}

// ===========================================================================
// `record_ci_snapshot`'s completeness gate, driven
// ===========================================================================

/// The snapshot writer refuses to record a verdict it cannot prove.
///
/// This is the third completeness guard and the only one reachable without a
/// network seam: the early return fires before the GitHub client is used at all.
/// It is what converts an unproven enumeration into `Unknown` — which the merge
/// gate maps to `Hold` — instead of into a green verdict for a prefix that may
/// be missing the one causal failure nobody saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_ci_snapshot_refuses_an_unproven_enumeration() {
    let h = lane_harness().await;
    let client = djinn_provider::github_api::GitHubApiClient::for_installation(1);

    // Establish the head first. Writing this test surfaced an ordering fact
    // worth recording: the completeness gate sits *after* the head-SHA-change
    // branch, and that branch returns `Pending` outright for an empty or
    // still-running check list. So on the very first observation of a new head
    // the gate is not reached at all — the hold comes from the reset branch
    // instead. Same outcome (`Pending` and `Unknown` both map to `Hold` at the
    // merge gate, and neither writes a verdict), different mechanism, and a
    // test that did not seed the head would have been asserting the reset
    // branch while claiming to assert the gate.
    let established = h
        .actor
        .record_ci_snapshot(
            &h.task_id,
            "task-short",
            PR,
            HEAD,
            "main",
            PR as u64,
            &client,
            "acme",
            "widgets",
            &CheckRunsResponse::complete(Vec::new()),
        )
        .await;
    assert_eq!(
        established,
        djinn_core::models::CiStatus::Pending,
        "precondition: the head-change branch answers first on a fresh head",
    );

    for (reason, runs) in [
        (
            djinn_provider::github_api::CheckSetIncompleteReason::PageFetchFailed,
            Vec::new(),
        ),
        (
            djinn_provider::github_api::CheckSetIncompleteReason::ShortRead,
            vec![passing_check("Quality Gate / build")],
        ),
        (
            djinn_provider::github_api::CheckSetIncompleteReason::MaxPagesTruncated,
            vec![passing_check("Quality Gate / build")],
        ),
    ] {
        let checks = CheckRunsResponse::incomplete(9, runs, reason);
        let status = h
            .actor
            .record_ci_snapshot(
                &h.task_id,
                "task-short",
                PR,
                HEAD,
                "main",
                PR as u64,
                &client,
                "acme",
                "widgets",
                &checks,
            )
            .await;

        assert_eq!(
            status,
            djinn_core::models::CiStatus::Unknown,
            "{reason:?} must hold, not resolve to a verdict",
        );
        assert_ne!(
            h.ci_snapshot().await.map(|s| s.ci_status),
            Some(djinn_core::models::CiStatus::Passing),
            "{reason:?} must never be recorded as Passing",
        );
    }

    // The filter has to let the real thing through, or a no-CI repository
    // wedges in `pr_draft` forever.
    let status = h
        .actor
        .record_ci_snapshot(
            &h.task_id,
            "task-short",
            PR,
            HEAD,
            "main",
            PR as u64,
            &client,
            "acme",
            "widgets",
            &CheckRunsResponse::complete(vec![passing_check("Quality Gate / build")]),
        )
        .await;
    assert_eq!(status, djinn_core::models::CiStatus::Passing);
}

/// Both lane fast paths must keep asking the completeness question.
///
/// Source-level, and honestly labelled as such. Driving `poll_pr_draft_tasks`
/// or `poll_pr_review_tasks` end to end would need a GitHub base-URL seam that
/// does not exist — `resolve_installation_client` builds
/// `GitHubApiClient::for_installation`, which hard-codes `api.github.com` — so
/// the reachable guarantees are the predicate test (which proves the shared
/// function is right) and this one (which proves both branches still call it).
/// Reverting either branch to a bare `checks.check_runs.is_empty()` fails here.
#[test]
fn both_lane_fast_paths_consult_the_completeness_predicate() {
    for (label, source) in [
        ("pr_draft", include_str!("../../pr_watcher.rs")),
        ("pr_review", include_str!("../../pr_review_watcher.rs")),
    ] {
        assert!(
            source.contains("empty_check_set_is_authoritatively_green"),
            "the {label} no-CI fast path must consult the completeness predicate",
        );
        assert!(
            !source.contains("checks.check_runs.is_empty() && checks.completeness.is_complete()"),
            "the {label} fast path must not re-inline the predicate; there is one definition",
        );
    }
}

// ===========================================================================
// Fail-closed on a route-table outage
// ===========================================================================

/// A route-table outage suppresses CI remediation on both lanes, and that is
/// deliberate — but it must be observable rather than discovered in production.
///
/// `Deferred(RepositoryError)` suppresses the legacy path, so an outage silently
/// stops all remediation. Fail-closed is the right direction (the alternative is
/// re-running the legacy reopen against evidence whose route state is unknown,
/// which can double-remedy), but it is also a total feature outage and the
/// operator needs to know it is the designed behaviour.
#[tokio::test]
async fn a_route_table_outage_fails_closed_on_both_lanes() {
    let f = fixture().await;
    // Cascade: the budget-counter and lease tables reference this one, so a
    // plain DROP is refused and the outage is not simulated at all.
    djinn_db::test_support::drop_table_cascade_for_test(&f.db, "ci_route_attempts").await;

    let checks = [inconclusive_check("Quality Gate / test", 980)];
    let blocking = refs(&checks);
    let head = run(
        &f,
        &f.target(),
        &pr_head_identity(980),
        &pr_head_identity(980),
        &blocking,
    )
    .await;

    let auto = CiAutoMergeTarget {
        node_id: "PR_node",
        commit_headline: "h",
        method: MergeMethod::Squash,
    };
    let merge_id = merge_group_identity(981);
    let merge = run(&f, &f.merge_target(&auto), &merge_id, &merge_id, &blocking).await;

    assert_eq!(head, CiLaneOutcome::Deferred(CiDeferral::RepositoryError));
    assert_eq!(merge, CiLaneOutcome::Deferred(CiDeferral::RepositoryError));
    assert!(
        head.suppresses_legacy_path() && merge.suppresses_legacy_path(),
        "an unknown route state must not hand the evidence to a second remedy",
    );
    assert_eq!(
        f.provider.calls().mutations(),
        0,
        "no provider mutation without a committed reservation",
    );
}

// ---------------------------------------------------------------------------
// The repair corpus (wave 5)
// ---------------------------------------------------------------------------

/// A Tier-2 route carries the commands CI actually ran, and reading them is a
/// read.
///
/// Without this, `repository_commands` is empty on every route and
/// `command_is_repository_valid` is always false — so **every** repair silently
/// degrades to a diagnosis and no test notices, because a diagnosis is a
/// perfectly valid outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tier_two_route_carries_the_commands_ci_actually_ran() {
    let f = fixture().await;
    f.provider
        .set_reproduction_command(Some("cargo nextest run -p djinn-db".to_owned()));
    let checks = vec![causal_check("Server Test / test", 77)];
    let blocking = refs(&checks);
    let id = pr_head_identity(77);
    let before = f.effects().await;

    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;
    let CiLaneOutcome::Tier2 {
        handoff: Some(handoff),
        lease_opened: true,
        ..
    } = outcome
    else {
        panic!("a complete causal failure opens an adjudication, got {outcome:?}");
    };
    assert_eq!(
        handoff.repository_commands,
        vec!["cargo nextest run -p djinn-db".to_owned()],
        "the corpus must be the command the runner executed"
    );
    assert!(
        !handoff.evidence_references.is_empty(),
        "and the evidence bundle is never empty, or grounding fails closed"
    );

    let after = f.effects().await;
    assert_eq!(
        before.provider_mutations, after.provider_mutations,
        "reading the reproduction context is a READ; it must mutate nothing"
    );
    assert!(
        f.provider.calls().reproduction > 0,
        "the corpus read must actually have happened"
    );
}

/// An unreproducible check leaves the corpus empty rather than failing the
/// route.
///
/// The route still escalates and still holds its lease; only the repair's
/// command is unavailable, which is the degradation the proposal specifies
/// ("if either the remedy or a valid command is unavailable, repair is invalid
/// and Lead must use diagnose").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreproducible_check_still_opens_the_adjudication() {
    let f = fixture().await;
    f.provider.set_reproduction_command(None);
    let checks = vec![causal_check("Server Test / test", 78)];
    let blocking = refs(&checks);
    let id = pr_head_identity(78);

    let outcome = run(&f, &f.target(), &id, &id, &blocking).await;
    let CiLaneOutcome::Tier2 {
        handoff: Some(handoff),
        lease_opened: true,
        ..
    } = outcome
    else {
        panic!("an unreproducible check must still escalate, got {outcome:?}");
    };
    assert!(
        handoff.repository_commands.is_empty(),
        "no command was exposed by CI, and none may be invented"
    );
}
