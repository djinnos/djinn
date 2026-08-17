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
use std::time::Duration;

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
use crate::pr_poller::ci_routing::quiescence::{
    CiDrainOutcome, PROVIDER_ACTION_DRAIN_TIMEOUT, quiesce_provider_actions,
};
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
    /// The scope a fixture wants sampled from *inside* a provider mutation.
    ///
    /// This is the seam that makes the scope's **use** observable rather than
    /// its identity. `the_actor_admits_into_the_leaders_provider_action_scope`
    /// proves the actor holds the leader's object; nothing proved that the
    /// object reaches the executor, so `drive_lane` could hand the executor a
    /// fresh `ProviderActionScope::new()` with the whole suite green. Reading
    /// `in_flight()` off the leader's own handle at the instant the provider
    /// mutation is running answers both that question and "is the guard still
    /// held across the call" with one number.
    scope_probe: Option<ProviderActionScope>,
    /// `scope_probe.in_flight()` as observed on entry to each mutation.
    in_flight_during_mutations: Vec<usize>,
}

#[derive(Clone, Default)]
struct FakeProvider {
    state: Arc<Mutex<FakeState>>,
    /// When set, `list_check_runs_for_ref` parks here until it is notified.
    ///
    /// This is the seam that lets a fixture interleave two *real* logical polls
    /// of one lane: the enumeration is the gap the ordering contract exists to
    /// span, so pausing a poll inside it is the only way to make poll A's
    /// reservation genuinely precede poll B's and poll A's apply genuinely
    /// follow it. Nothing in the fixture chooses the sequences — the ledger
    /// assigns them, and the fixture reads them back.
    hold_enumeration: Option<Arc<tokio::sync::Notify>>,
    /// When set, `rerun_failed_jobs` / `enable_auto_merge` park here until they
    /// are notified — *after* recording the call, so the recorded count is the
    /// fixture's "the mutation is now in flight" signal.
    ///
    /// The window this opens is the one leadership's drain has to refuse to
    /// finish inside.
    hold_mutation: Option<Arc<tokio::sync::Notify>>,
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

    /// A provider whose enumeration parks until the returned handle is
    /// notified. See [`FakeProvider::hold_enumeration`].
    fn parked_enumeration() -> (Self, Arc<tokio::sync::Notify>) {
        let gate = Arc::new(tokio::sync::Notify::new());
        let me = Self {
            state: Arc::default(),
            hold_enumeration: Some(gate.clone()),
            hold_mutation: None,
        };
        (me, gate)
    }

    /// A provider whose *mutation* parks until the returned handle is notified.
    /// See [`FakeProvider::hold_mutation`].
    fn parked_mutation() -> (Self, Arc<tokio::sync::Notify>) {
        let gate = Arc::new(tokio::sync::Notify::new());
        let me = Self {
            state: Arc::default(),
            hold_enumeration: None,
            hold_mutation: Some(gate.clone()),
        };
        (me, gate)
    }

    /// Sample `scope.in_flight()` on entry to every provider mutation.
    fn probe_scope(&self, scope: ProviderActionScope) {
        self.state.lock().expect("fake provider mutex").scope_probe = Some(scope);
    }

    /// What the probed scope reported while each mutation was running.
    fn in_flight_during_mutations(&self) -> Vec<usize> {
        self.state
            .lock()
            .expect("fake provider mutex")
            .in_flight_during_mutations
            .clone()
    }

    /// Park inside a provider mutation, if this fixture asked for that.
    ///
    /// Split out of the two mutation methods so neither holds the fake's mutex
    /// across the await — a parked mutation that held it would deadlock every
    /// counter read the fixture takes while it waits.
    async fn park_in_mutation(&self) {
        if let Some(gate) = &self.hold_mutation {
            gate.notified().await;
        }
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

/// Sample the probed scope, if a fixture asked for one.
///
/// Called with the fake's own mutex held and the provider mutation *not yet
/// returned*, which is the only instant at which "the executor is inside the
/// call" and "the guard is still held" are both observable.
fn record_scope_probe(state: &mut FakeState) {
    let Some(scope) = state.scope_probe.clone() else {
        return;
    };
    let in_flight = scope.in_flight();
    state.in_flight_during_mutations.push(in_flight);
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
        let failed = {
            let mut state = self.state.lock().expect("fake provider mutex");
            state.calls.rerun_failed_jobs += 1;
            state
                .reran
                .push((owner.to_owned(), repo.to_owned(), run_id));
            record_scope_probe(&mut state);
            state.fail_mutations
        };
        self.park_in_mutation().await;
        if failed {
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
        let failed = {
            let mut state = self.state.lock().expect("fake provider mutex");
            state.calls.enable_auto_merge += 1;
            record_scope_probe(&mut state);
            state.fail_mutations
        };
        self.park_in_mutation().await;
        if failed {
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
        // Before the lock: a parked enumeration must not hold the mutex, and
        // the await point is the whole point of the seam.
        if let Some(gate) = &self.hold_enumeration {
            gate.notified().await;
        }
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
        run_id: Some(run_id),
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    }
}

fn merge_group_identity(run_id: i64) -> CiEvidenceIdentity {
    CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: Some(run_id),
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

/// The blocking predicate `pr_watcher`'s failing-CI branch passes.
///
/// A plain `fn` rather than a closure because the lane takes a function
/// pointer: the enumeration it filters is the one the lane takes itself, under
/// its own reserved sequence, so there is no captured slice to close over.
fn failing_filter(cr: &CheckRun) -> bool {
    crate::pr_poller::is_failing_conclusion(cr.conclusion.as_deref())
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
    let report =
        sweep_reserved_routes(&f.routes, &FixedHead(Some(HEAD.to_owned())), &f.incarnation).await;
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
///
/// "Terminated" here means the owner reached the END of its own shutdown
/// contract — admission closed, every provider future joined, its own drain
/// stamp written — not that its process was killed. There is no killed-process
/// proof: `CiQuiescenceProof` has only the drain stamp and the absence of one,
/// because the only trace an abrupt death leaves is an advisory-lock release
/// Postgres performs before the (possibly still live) client can react. See
/// `a_lapsed_owner_lease_is_not_a_quiescence_proof` below.
///
/// So the former owner's ledger row is really drained here, and the injected
/// `FixedLiveness` cannot paper over that: `recover_calling_owner` re-reads
/// `provider_actions_drained_at` for itself before honouring a `GracefulDrain`
/// claim.
#[tokio::test]
async fn terminated_owner_handoff_recovers_calling_once() {
    let f = fixture().await;
    let checks = [inconclusive_check("Quality Gate / test", 903)];
    let blocking = refs(&checks);
    let id = pr_head_identity(903);
    let key = provider_action_key(&f.subject, &id, CiAction::RerunRun);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);

    let other = uuid::Uuid::now_v7().to_string();
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    incarnations
        .register(&other)
        .await
        .expect("register the former owner");
    assert!(
        incarnations
            .mark_draining(&other)
            .await
            .expect("mark draining")
    );
    assert!(
        incarnations
            .mark_provider_actions_drained(&other)
            .await
            .expect("stamp the drain"),
        "the former owner must really have drained, or the repository refuses \
         the claim and this fixture tests a deferral instead of a handoff"
    );
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
        &FixedLiveness(CiQuiescenceProof::GracefulDrain),
        &f.incarnation,
        true,
    )
    .await;
    assert_eq!(first.outcome_unknown, 1, "still current: outcome_unknown");
    assert_eq!(first.superseded_after_call, 0);

    // A second pass finds nothing to hand off: the row is terminal now.
    let second = recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        &FixedLiveness(CiQuiescenceProof::GracefulDrain),
        &f.incarnation,
        true,
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

// ---------------------------------------------------------------------------
// The PRODUCTION liveness witness
// ---------------------------------------------------------------------------
//
// Every handoff fixture above injects `FixedLiveness`, so until these three
// existed the one production implementation of `CiOwnerLiveness` —
// `CiIncarnationLiveness` — had no coverage at all: inverting it outright left
// the whole coordinator suite green. These drive the real type against the real
// `coordinator_incarnations` ledger on the same ephemeral Postgres.

/// Plant a charged `calling` row owned by `owner`, aged past the 300s floor.
///
/// The route goes in through `reserve` + `charge_and_begin_calling` rather than
/// a planted UPDATE, so the budgets are really charged and the phase is really
/// the one the executor leaves behind at the provider boundary.
async fn calling_row_owned_by(
    f: &Fixture,
    identity: &CiEvidenceIdentity,
    blocking: &[&CheckRun],
    owner: &str,
) -> (String, String) {
    let key = provider_action_key(&f.subject, identity, CiAction::RerunRun);
    let fingerprint = transient_fingerprint(CiLane::PrHead, blocking);
    f.routes
        .reserve(&CiRouteReservation {
            subject: f.subject.clone(),
            provider_action_key: key.clone(),
            identity: identity.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.clone(),
            retry_budget_key: retry_budget_key(&f.subject, identity, &fingerprint),
            head_budget_key: head_budget_key(&f.subject, identity.pr_number, &identity.pr_head_sha),
        })
        .await
        .expect("reserve");
    f.routes
        .charge_and_begin_calling(&f.subject, &key, owner, identity)
        .await
        .expect("charge");
    age_calling(f, &key, 400).await;
    (key, fingerprint)
}

/// One startup handoff pass driven by the PRODUCTION witness.
async fn handoff_with(f: &Fixture, liveness: &CiIncarnationLiveness) -> CiHandoffReport {
    recover_calling_owners_at_startup(
        &f.routes,
        &FixedHead(Some(HEAD.to_owned())),
        liveness,
        &f.incarnation,
        true,
    )
    .await
}

async fn recovery_reasons(f: &Fixture, key: &str) -> Vec<CiCallingRecoveryReason> {
    f.routes
        .calling_recovery_audit(&f.subject, key)
        .await
        .expect("calling-recovery audit")
        .iter()
        .map(|record| record.recovery_reason)
        .collect()
}

/// A former owner whose renewal lease has lapsed is NOT quiescent.
///
/// The lapsed lease is the trap this fixture exists for. Renewal is scoped to
/// the coordinator's cancellation token, so it stops when leadership is
/// cancelled and not when the process exits: a leader still joining its
/// provider futures past its drain budget reads exactly like a dead one once
/// the expiry window passes. Deriving a quiescence proof from that expiry
/// authorises a second `rerun_failed_jobs` against evidence whose first call
/// may still be in flight, discards the old owner's fenced `finalize_calling`,
/// and spends a Lead session adjudicating an episode that in fact succeeded —
/// which AC5 forbids in terms: "elapsed time, owner-lease expiry, cancellation,
/// or advisory-lock release without provider-action drain never authorizes
/// `calling` recovery".
///
/// NAMED FAILING MUTATION: restore the deleted arm in
/// `CiIncarnationLiveness::quiescence_proof` —
/// `Ok(Some(_)) => match is_live(..) { Ok(Some(false)) => GracefulDrain, .. }`.
/// The backdated lease reads expired, so `quiescence_proof` answers a proof,
/// the first assertion fails, and the handoff below takes the row instead of
/// deferring: `deferred` drops to 0, `outcome_unknown` rises to 1, the phase
/// leaves `calling`, and the audit reason stops being `LiveOwnerDeferred`.
///
/// Note that the repository's own re-read of `provider_actions_drained_at`
/// does NOT catch this one on its own here: this fixture's lapsed incarnation
/// is registered but never drained, so the claim would be refused — which is
/// why the `quiescence_proof` assertion above the handoff is asserted directly
/// rather than inferred from the report.
#[tokio::test]
async fn a_lapsed_owner_lease_is_not_a_quiescence_proof() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());

    // Two real ledger rows: one renewed just now, one an hour stale.
    let lapsed = uuid::Uuid::now_v7().to_string();
    let fresh = uuid::Uuid::now_v7().to_string();
    incarnations
        .register(&lapsed)
        .await
        .expect("register the former owner");
    incarnations
        .register(&fresh)
        .await
        .expect("register the control incarnation");
    djinn_db::test_support::backdate_coordinator_incarnation_lease(&f.db, &lapsed, "1 hour").await;

    // The precondition, proven rather than assumed. Without it this fixture
    // would pass against a heartbeat-reading witness for the wrong reason —
    // because the row was seconds old, not because the witness ignores age.
    // `fresh`'s own `last_renewed_at` is "now" in the column's exact stored
    // spelling, so it is the one threshold that needs no clock arithmetic here.
    let now_iso = incarnations
        .get(&fresh)
        .await
        .expect("ledger read")
        .expect("the control incarnation is registered")
        .last_renewed_at;
    assert_eq!(
        incarnations
            .is_live(&lapsed, &now_iso)
            .await
            .expect("liveness read"),
        Some(false),
        "the backdated lease must read expired, or the assertion below is vacuous"
    );
    assert_eq!(
        incarnations
            .is_live(&fresh, &now_iso)
            .await
            .expect("liveness read"),
        Some(true),
        "and the control must read live, or the threshold itself is wrong"
    );

    let liveness = CiIncarnationLiveness { incarnations };
    assert_eq!(
        liveness.quiescence_proof(&lapsed).await,
        CiQuiescenceProof::None,
        "an expired heartbeat says nothing about where the former owner's \
         provider future went; only its own drain stamp does"
    );

    // End to end: a charged `calling` row owned by that lapsed incarnation must
    // be left exactly where it is.
    let checks = [inconclusive_check("Quality Gate / test", 920)];
    let blocking = refs(&checks);
    let id = pr_head_identity(920);
    let (key, _) = calling_row_owned_by(&f, &id, &blocking, &lapsed).await;

    let before = f.effects().await;
    let report = handoff_with(&f, &liveness).await;
    let after = f.effects().await;

    assert_eq!(report.examined, 1, "the row is a handoff candidate");
    assert_eq!(
        report.deferred, 1,
        "a lapsed lease is not a quiescence proof, so the row is deferred"
    );
    assert_eq!(
        report.outcome_unknown, 0,
        "and nothing is recovered: a second provider call is the cost of \
         getting this wrong"
    );
    assert_eq!(report.superseded_after_call, 0);
    assert_no_effects_beyond_routes(&before, &after);

    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(
        attempt.action_phase,
        CiActionPhase::Calling,
        "the row stays where the former owner left it"
    );
    assert_eq!(
        attempt.owner_incarnation_id.as_deref(),
        Some(lapsed.as_str()),
        "and the owner is not rewritten, so the former owner's fenced \
         finalizer can still land"
    );

    let reasons = recovery_reasons(&f, &key).await;
    assert!(
        reasons.contains(&CiCallingRecoveryReason::LiveOwnerDeferred),
        "the refusal must be the missing quiescence proof; got {reasons:?}"
    );
    assert!(
        !reasons.contains(&CiCallingRecoveryReason::TimeoutNotElapsed),
        "the 300s window has already elapsed, or the quiescence predicate is \
         never reached; got {reasons:?}"
    );
}

/// The former owner's drain stamp — and nothing weaker — recovers a `calling`
/// row.
///
/// A differential: one incarnation, one route row, one startup handoff, run
/// three times. The only thing that changes between passes is how far the
/// former owner got through its own shutdown contract — unregistered, admission
/// closed, futures joined and stamped.
///
/// NAMED FAILING MUTATIONS. (a) Stub `quiescence_proof` to a constant
/// `CiQuiescenceProof::None`: the third pass recovers nothing, so
/// `outcome_unknown` stays 0 and the phase never leaves `calling`. (b) Stub it
/// to a constant `GracefulDrain`, or read `draining_at` in place of
/// `provider_actions_drained_at`: the second pass takes a row whose owner has
/// closed admission but has NOT yet joined its futures, so the `deferred` and
/// `Calling` assertions there fail. Note that (b) is not caught by the
/// repository's own check — `recover_calling_owner` re-reads
/// `provider_actions_drained_at` only for a `GracefulDrain` claim, so a witness
/// that manufactures that claim from `draining_at` would be believed on its
/// second predicate and refused on the first. This asserts the witness.
#[tokio::test]
async fn only_the_former_owners_drain_stamp_recovers_a_calling_row() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    let former = uuid::Uuid::now_v7().to_string();
    incarnations
        .register(&former)
        .await
        .expect("register the former owner");

    let checks = [inconclusive_check("Quality Gate / test", 921)];
    let blocking = refs(&checks);
    let id = pr_head_identity(921);
    let (_key, fingerprint) = calling_row_owned_by(&f, &id, &blocking, &former).await;
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the call was charged before the owner went away"
    );

    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };

    // ── Pass 1: registered, renewing or not, nothing drained. ──
    assert_eq!(
        liveness.quiescence_proof(&former).await,
        CiQuiescenceProof::None
    );
    assert_eq!(handoff_with(&f, &liveness).await.deferred, 1);

    // ── Pass 2: admission is closed but the futures are not yet joined. ──
    // This is the interesting half-state: the owner is on its way out and its
    // `rerun_failed_jobs` future is still running.
    assert!(
        incarnations
            .mark_draining(&former)
            .await
            .expect("mark draining"),
        "the drain must actually start, or pass 2 tests nothing"
    );
    assert_eq!(
        liveness.quiescence_proof(&former).await,
        CiQuiescenceProof::None,
        "a started drain is an intention, not a join"
    );
    let report = handoff_with(&f, &liveness).await;
    assert_eq!(report.deferred, 1);
    assert_eq!(report.outcome_unknown, 0);
    assert_eq!(
        f.attempt(&id, CiAction::RerunRun).await.action_phase,
        CiActionPhase::Calling,
        "a draining owner still owns its row"
    );

    // ── Pass 3: the scope emptied and the owner stamped its own row. ──
    assert!(
        incarnations
            .mark_provider_actions_drained(&former)
            .await
            .expect("stamp the drain"),
        "the stamp must land, or pass 3 tests nothing"
    );
    assert_eq!(
        liveness.quiescence_proof(&former).await,
        CiQuiescenceProof::GracefulDrain
    );

    let before = f.effects().await;
    let report = handoff_with(&f, &liveness).await;
    let after = f.effects().await;

    assert_eq!(
        report.outcome_unknown, 1,
        "a stamped drain is the proof, and the still-current row becomes \
         outcome_unknown"
    );
    assert_eq!(report.deferred, 0);
    assert_eq!(report.superseded_after_call, 0);

    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(attempt.action_phase, CiActionPhase::Terminal);
    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::OutcomeUnknown)
    );
    assert_eq!(
        attempt.owner_incarnation_id.as_deref(),
        Some(f.incarnation.as_str()),
        "the handoff rewrites the owner to the recovering incarnation"
    );
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the charge is retained across the handoff"
    );
    assert_eq!(
        after.provider_mutations, before.provider_mutations,
        "a handoff performs no provider query and no replay"
    );
    assert_eq!(after.worker_attempts, before.worker_attempts);
    assert_eq!(after.task_status, before.task_status);
    assert!(after.tier2_leases <= 1);
}

/// An owner the ledger never recorded is not a proof either.
///
/// NAMED FAILING MUTATION: fold the missing row into the drained arm — e.g.
/// `Ok(row) if row.is_none_or(|r| r.provider_actions_drained_at.is_some())`.
/// A vanished incarnation is precisely the case where nothing can attest that
/// its futures are gone, so "no row" must read as "no proof".
#[tokio::test]
async fn an_unrecorded_owner_has_no_quiescence_proof() {
    let f = fixture().await;
    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };
    let never_registered = uuid::Uuid::now_v7().to_string();

    assert_eq!(
        liveness.quiescence_proof(&never_registered).await,
        CiQuiescenceProof::None
    );

    let checks = [inconclusive_check("Quality Gate / test", 922)];
    let blocking = refs(&checks);
    let id = pr_head_identity(922);
    let (key, _) = calling_row_owned_by(&f, &id, &blocking, &never_registered).await;

    let report = handoff_with(&f, &liveness).await;
    assert_eq!(report.deferred, 1);
    assert_eq!(report.outcome_unknown, 0);
    assert!(
        recovery_reasons(&f, &key)
            .await
            .contains(&CiCallingRecoveryReason::LiveOwnerDeferred)
    );
}

// ---------------------------------------------------------------------------
// EARNING the drain stamp
// ---------------------------------------------------------------------------
//
// The three fixtures above prove the witness READS the stamp correctly. They
// plant it with `mark_draining` / `mark_provider_actions_drained` directly, so
// they say nothing about whether anything in production earns it — stubbing
// `quiescence::quiesce_provider_actions` to stamp unconditionally leaves every
// one of them green, which silently restores the pre-AC5 bug by another route.
//
// Since the witness reads exactly one fact, the ORDER in which that fact is
// written is now the whole of `calling`-row exclusion. These fixtures drive the
// production drain — the sole writer of the column — with a live provider
// future held across it, because a premature stamp is only observable while
// something is still in flight.

/// How many times a sampling loop re-checks a "must not have happened yet"
/// property, as a fixed COUNT rather than a wall-clock deadline: a slow machine
/// makes the window longer, never thinner. One check would be a race a
/// premature stamp could win.
const DRAIN_SAMPLES: u32 = 20;
const DRAIN_POLL: Duration = Duration::from_millis(25);
/// Fewer, because each sample runs a full startup-handoff pass against Postgres.
const HANDOFF_SAMPLES: u32 = 6;
/// Generous: every transition waited on is sub-millisecond in the passing case,
/// so a long ceiling costs nothing and removes the only source of flake.
const PATIENCE: Duration = Duration::from_secs(30);

/// Block until a spawned drain has reached step 1.
///
/// Mandatory before any sampling loop: without it the loop could run to
/// completion before `quiesce_provider_actions` was ever polled, and would then
/// be asserting that a future which had not started had not stamped — vacuous.
async fn wait_until_admission_closed(scope: &ProviderActionScope) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !scope.is_admission_closed() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the spawned drain never closed admission, so nothing below is under test"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn drain_stamp_in(db: &Database, incarnation: &str) -> Option<String> {
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .get(incarnation)
        .await
        .expect("ledger read")
        .expect("the incarnation is registered")
        .provider_actions_drained_at
}

async fn drain_stamp(f: &Fixture, incarnation: &str) -> Option<String> {
    drain_stamp_in(&f.db, incarnation).await
}

/// The stamp is not written while a provider future is still in flight.
///
/// `provider_actions_drained_at` is the single fact `CiIncarnationLiveness`
/// reads, so writing it early is writing a lie: the next incarnation believes
/// the former owner's futures are gone while one is running in its address
/// space. This drives the real `quiesce_provider_actions` with a live guard and
/// samples the ledger throughout, so a stamp that lands before the join is
/// caught rather than raced.
///
/// NAMED FAILING MUTATIONS. (a) Hoist the `mark_draining` +
/// `mark_provider_actions_drained` block above the `wait_until_empty` timeout
/// in `ci_routing::quiescence::quiesce_provider_actions`: the stamp lands
/// within a millisecond of the spawn, so the very first sample reads
/// `provider_actions_drained_at` as `Some` and fails — as does the
/// `CiQuiescenceProof::None` assertion beside it. (b) Hoist
/// `scope.mark_drained()` above the join: in a debug build its own
/// `in_flight == 0` assertion panics the drain task, which the
/// `!drain.is_finished()` vacuity guard catches; with assertions compiled out
/// the loop's `!is_drained()` assertion fires instead — and that flag is what
/// leadership releases the advisory lock on. (c) Make the final
/// `Ok(outcome) => CiDrainOutcome::Stamped` unconditional on the join actually
/// having happened, i.e. return `Stamped` from the `!joined` arm: the closing
/// `outcome == Stamped` assertion still passes here, so this fixture does NOT
/// kill that one — `a_drain_that_times_out_stamps_nothing_and_reports_no_proof`
/// does, with a budget short enough to reach the timeout inside the test.
///
/// The `in_flight == 1` and `!drain.is_finished()` checks after the loop are
/// the vacuity guards: they prove the window really did span a live future and
/// an unfinished drain, rather than a scope that had quietly emptied. The
/// window is deliberately far shorter than `PROVIDER_ACTION_DRAIN_TIMEOUT`, so
/// what it observes is the join withholding the stamp, never the budget
/// expiring.
#[tokio::test]
async fn the_drain_stamp_is_withheld_until_the_last_provider_future_is_joined() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    let owner = uuid::Uuid::now_v7().to_string();
    incarnations
        .register(&owner)
        .await
        .expect("register the draining owner");
    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };

    // The stand-in for a live `rerun_failed_jobs` future.
    let in_flight = f.scope.admit().expect("an open scope admits");

    let drain = tokio::spawn({
        let scope = f.scope.clone();
        let db = f.db.clone();
        let owner = owner.clone();
        async move {
            let incarnations = djinn_db::CoordinatorIncarnationRepository::new(db);
            quiesce_provider_actions(&scope, &incarnations, &owner, PROVIDER_ACTION_DRAIN_TIMEOUT)
                .await
        }
    });
    wait_until_admission_closed(&f.scope).await;

    for sample in 0..DRAIN_SAMPLES {
        tokio::time::sleep(DRAIN_POLL).await;
        assert!(
            drain_stamp(&f, &owner).await.is_none(),
            "sample {sample}: `provider_actions_drained_at` was written while a provider \
             future was still in flight (in_flight={}); a new incarnation reading that \
             stamp takes a charged `calling` row whose call may still be running",
            f.scope.in_flight(),
        );
        assert_eq!(
            liveness.quiescence_proof(&owner).await,
            CiQuiescenceProof::None,
            "sample {sample}: the witness must find no proof while the owner is joining"
        );
        assert!(
            !f.scope.is_drained(),
            "sample {sample}: leadership releases the advisory lock on this flag"
        );
    }

    // Vacuity: the window above spanned a genuinely live future and a drain
    // that had genuinely not finished.
    assert_eq!(
        f.scope.in_flight(),
        1,
        "the guard must still be held, or the loop proved nothing"
    );
    assert!(
        !drain.is_finished(),
        "the drain must still be inside its join, or the loop proved nothing"
    );

    // The future returns; only now is the stamp earned.
    drop(in_flight);
    let outcome = tokio::time::timeout(PATIENCE, drain)
        .await
        .expect("the drain must finish once the scope empties")
        .expect("the drain task must not panic");

    assert_eq!(outcome, CiDrainOutcome::Stamped);
    assert!(
        drain_stamp(&f, &owner).await.is_some(),
        "a joined drain must stamp, or the handoff can never happen at all"
    );
    assert_eq!(
        liveness.quiescence_proof(&owner).await,
        CiQuiescenceProof::GracefulDrain
    );
    assert!(
        f.scope.is_drained(),
        "the scope's drained flag is what leadership waits on"
    );
    assert_eq!(f.scope.in_flight(), 0);
}

/// A drain that never joins stamps nothing at all.
///
/// This is the arm the AC5 fix exists for: leadership releases the advisory
/// lock after its own 45-second wait whether or not the coordinator finished,
/// so the *only* thing keeping a new incarnation off a live owner's `calling`
/// row is that no proof was written. AC5: "elapsed time, owner-lease expiry,
/// cancellation, or advisory-lock release without provider-action drain never
/// authorizes `calling` recovery."
///
/// `draining_at` is asserted NULL as well, and that is not incidental tidiness.
/// `mark_provider_actions_drained`'s own WHERE clause requires
/// `draining_at IS NOT NULL`, so leaving it unwritten until the join succeeds
/// is the database's independent refusal of an out-of-order stamp. A drain that
/// recorded its *intent* before joining would disarm that second gate.
///
/// NAMED FAILING MUTATIONS. (a) Delete the `if !joined { return … }` guard: the
/// timeout falls through to step 3, both columns are written, and every
/// assertion below about NULL fails, as does the `CiQuiescenceProof::None` one.
/// (b) Return `CiDrainOutcome::Stamped` on the timeout path (a "close enough"
/// simplification): the first assertion fails. The vacuity guards —
/// `in_flight == 1` and `is_admission_closed()` — prove the call really did
/// time out with a live future rather than never running.
#[tokio::test]
async fn a_drain_that_times_out_stamps_nothing_and_reports_no_proof() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    let owner = uuid::Uuid::now_v7().to_string();
    incarnations
        .register(&owner)
        .await
        .expect("register the draining owner");

    // Held for the whole call: the join cannot complete.
    let held = f.scope.admit().expect("an open scope admits");

    let outcome =
        quiesce_provider_actions(&f.scope, &incarnations, &owner, Duration::from_millis(150)).await;

    assert_eq!(
        outcome,
        CiDrainOutcome::NotJoined,
        "an unjoined drain is a degraded outcome and must report itself as one"
    );
    // Vacuity: the timeout is what ended the call, and the call really ran.
    assert!(
        f.scope.is_admission_closed(),
        "step 1 must have run, or this fixture is asserting about a call that did nothing"
    );
    assert_eq!(
        f.scope.in_flight(),
        1,
        "the future must still be live, or the call joined and the timeout is untested"
    );
    assert!(
        !f.scope.is_drained(),
        "a scope whose futures are still live must never report a drain"
    );

    let row = incarnations
        .get(&owner)
        .await
        .expect("ledger read")
        .expect("the incarnation is registered");
    assert!(
        row.provider_actions_drained_at.is_none(),
        "an unjoined drain must hand the next incarnation no proof; releasing the lock \
         without one costs recovery latency, releasing it WITH one costs exclusion"
    );
    assert!(
        row.draining_at.is_none(),
        "nothing is written before the join, so the repository's own \
         `draining_at IS NOT NULL` precondition still stands between an \
         out-of-order caller and the stamp"
    );

    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };
    assert_eq!(
        liveness.quiescence_proof(&owner).await,
        CiQuiescenceProof::None
    );

    drop(held);
}

/// End to end: a charged `calling` row is not handed over while its owner's
/// provider future is alive, and IS handed over once that owner's own drain has
/// joined and stamped.
///
/// This is the property the whole wave-3 ordering exists to produce, asserted
/// on the consequence rather than on the stamp: the recovering incarnation runs
/// the real `recover_calling_owners_at_startup` against the real
/// `CiIncarnationLiveness`, while the former owner runs the real
/// `quiesce_provider_actions` with a guard standing in for the
/// `rerun_failed_jobs` future behind that very row.
///
/// NAMED FAILING MUTATION: hoist the stamp above the join in
/// `ci_routing::quiescence::quiesce_provider_actions`. The stamp then exists
/// from the moment the drain starts, so
/// `CiIncarnationLiveness` answers `GracefulDrain` at the first sample, the
/// handoff takes the row, and the loop fails on `deferred == 1` /
/// `outcome_unknown == 0` / `action_phase == Calling` — with the row's owner
/// rewritten to the recovering incarnation, which is exactly the state that
/// discards the old owner's fenced `finalize_calling` and either re-runs the
/// provider mutation or adjudicates `outcome_unknown` an episode that
/// succeeded.
///
/// Note that neither existing sibling catches this. `terminated_owner_handoff_
/// recovers_calling_once` injects `FixedLiveness`, and
/// `only_the_former_owners_drain_stamp_recovers_a_calling_row` plants the stamp
/// by hand — a hand-planted stamp is by construction earned.
#[tokio::test]
async fn a_live_provider_future_blocks_the_calling_handoff_until_its_owner_stamps() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    let former = uuid::Uuid::now_v7().to_string();
    incarnations
        .register(&former)
        .await
        .expect("register the former owner");

    let checks = [inconclusive_check("Quality Gate / test", 923)];
    let blocking = refs(&checks);
    let id = pr_head_identity(923);
    let (key, fingerprint) = calling_row_owned_by(&f, &id, &blocking, &former).await;
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the call was charged before the owner began shutting down"
    );

    // The former owner's scope, in its own process. The guard is the provider
    // future behind the `calling` row planted above.
    let owner_scope = ProviderActionScope::new();
    let in_flight = owner_scope.admit().expect("an open scope admits");
    let drain = tokio::spawn({
        let scope = owner_scope.clone();
        let db = f.db.clone();
        let former = former.clone();
        async move {
            let incarnations = djinn_db::CoordinatorIncarnationRepository::new(db);
            quiesce_provider_actions(
                &scope,
                &incarnations,
                &former,
                PROVIDER_ACTION_DRAIN_TIMEOUT,
            )
            .await
        }
    });
    wait_until_admission_closed(&owner_scope).await;

    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };

    let before = f.effects().await;
    for sample in 0..HANDOFF_SAMPLES {
        tokio::time::sleep(DRAIN_POLL).await;
        let report = handoff_with(&f, &liveness).await;
        assert_eq!(
            report.examined, 1,
            "sample {sample}: the row is a candidate"
        );
        assert_eq!(
            report.deferred, 1,
            "sample {sample}: the former owner is mid-drain with a live provider \
             future, so no proof exists and the row must be left alone"
        );
        assert_eq!(
            report.outcome_unknown, 0,
            "sample {sample}: recovering here runs the same provider mutation twice"
        );
        assert_eq!(report.superseded_after_call, 0);

        let attempt = f.attempt(&id, CiAction::RerunRun).await;
        assert_eq!(
            attempt.action_phase,
            CiActionPhase::Calling,
            "sample {sample}: the row stays where its live owner left it"
        );
        assert_eq!(
            attempt.owner_incarnation_id.as_deref(),
            Some(former.as_str()),
            "sample {sample}: the owner is not rewritten, so the former owner's \
             fenced finalizer can still land"
        );
    }
    let after = f.effects().await;
    assert_no_effects_beyond_routes(&before, &after);

    // The refusal is the missing quiescence proof, not the 300s floor — the row
    // was aged past it when it was planted.
    let reasons = recovery_reasons(&f, &key).await;
    assert!(
        reasons.contains(&CiCallingRecoveryReason::LiveOwnerDeferred),
        "the deferral must be the quiescence predicate; got {reasons:?}"
    );
    assert!(
        !reasons.contains(&CiCallingRecoveryReason::TimeoutNotElapsed),
        "the recovery timeout has elapsed, or the quiescence predicate is never \
         reached and this fixture proves nothing; got {reasons:?}"
    );

    // Vacuity: the whole window really did span a live future and an unfinished
    // drain.
    assert_eq!(owner_scope.in_flight(), 1);
    assert!(!drain.is_finished());
    assert!(
        drain_stamp(&f, &former).await.is_none(),
        "and the ledger really did hold no stamp throughout"
    );

    // ── The provider call returns; the owner joins and stamps. ──
    drop(in_flight);
    let outcome = tokio::time::timeout(PATIENCE, drain)
        .await
        .expect("the drain must finish once the scope empties")
        .expect("the drain task must not panic");
    assert_eq!(outcome, CiDrainOutcome::Stamped);
    assert_eq!(
        liveness.quiescence_proof(&former).await,
        CiQuiescenceProof::GracefulDrain
    );

    let report = handoff_with(&f, &liveness).await;
    assert_eq!(
        report.outcome_unknown, 1,
        "an EARNED stamp is what hands the row over; without this half the \
         fixture above would also pass against a drain that never stamps at all"
    );
    assert_eq!(report.deferred, 0);
    let attempt = f.attempt(&id, CiAction::RerunRun).await;
    assert_eq!(attempt.action_phase, CiActionPhase::Terminal);
    assert_eq!(
        attempt.terminal_outcome,
        Some(CiRouteOutcome::OutcomeUnknown)
    );
    assert_eq!(
        f.budgets(&id, &fingerprint).await,
        (1, 1),
        "the charge is retained across the handoff"
    );
}

/// A join that lands no ledger row does not claim a drain.
///
/// `mark_provider_actions_drained` is fenced and write-once, so it returns
/// `Ok(false)` — not `Err` — when the ledger has no row for this incarnation.
/// Treating that as success made `ProviderActionScope::mark_drained` assert a
/// stamp that no recovering incarnation could ever read, and leadership then
/// logged a graceful handoff it did not have. That is a reporting lie rather
/// than an exclusion hole (the absent stamp still makes the next incarnation
/// defer), but `drained` also feeds the rollback quiescence report, and a flag
/// that can be true without its durable fact is not worth reading.
///
/// NAMED FAILING MUTATION: restore `Ok(_) => { scope.mark_drained(); … }` in
/// `ci_routing::quiescence::quiesce_provider_actions` — i.e. stop distinguishing
/// `Ok(true)` from `Ok(false)`. The scope is then marked drained and the
/// `!is_drained()` assertion fails, as does the `NotStamped` one.
#[tokio::test]
async fn a_joined_drain_with_no_ledger_row_does_not_claim_a_stamp() {
    let f = fixture().await;
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(f.db.clone());
    let unregistered = uuid::Uuid::now_v7().to_string();
    assert!(
        incarnations
            .get(&unregistered)
            .await
            .expect("ledger read")
            .is_none(),
        "the incarnation must be absent, or this fixture is testing the registered path"
    );

    // The scope is empty, so the join succeeds immediately and the call reaches
    // step 3 — which is the step under test.
    let outcome = quiesce_provider_actions(
        &f.scope,
        &incarnations,
        &unregistered,
        PROVIDER_ACTION_DRAIN_TIMEOUT,
    )
    .await;

    assert_eq!(outcome, CiDrainOutcome::NotStamped);
    assert!(
        f.scope.is_admission_closed(),
        "the join half still runs: no new route may enter `calling` on the way out"
    );
    assert!(
        !f.scope.is_drained(),
        "the scope must not report a drain the ledger cannot corroborate"
    );
    assert!(
        incarnations
            .get(&unregistered)
            .await
            .expect("ledger read")
            .is_none(),
        "and the drain does not conjure the row it failed to find"
    );

    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(f.db.clone()),
    };
    assert_eq!(
        liveness.quiescence_proof(&unregistered).await,
        CiQuiescenceProof::None
    );
}

/// The PRODUCTION cancellation arm is what runs the drain at all.
///
/// Every fixture above calls `quiesce_provider_actions` directly, so all of
/// them stay green if `CoordinatorActor`'s `cancel.cancelled()` arm stops
/// calling it. That deletion is not cosmetic: the drain would then never run in
/// production, `provider_actions_drained_at` would never be written by anybody,
/// `CiIncarnationLiveness` would answer `None` forever, and every charged
/// `calling` row a leadership handover left behind would strand permanently.
/// The one thing that makes the column producible at all is one `.await` in one
/// `select!` arm, and nothing in this crate ran that arm.
///
/// So this drives the real loop, through `CoordinatorActor::new(
/// CoordinatorDeps::new(..))` — the production constructor — with the token
/// already cancelled, which the arm's `biased` ordering makes the arm that
/// fires. A guard admitted before the loop starts stands in for a live
/// `rerun_failed_jobs` future behind a `calling` row.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `poll_stack::boxed(|| self.quiesce_provider_actions()).await;`
///     from the arm: admission is never closed, so
///     `wait_until_admission_closed` exhausts `PATIENCE` and panics.
/// (b) Move the call out of the loop entirely — up into `run()` after
///     `run_dispatch_loop` returns, which is where leadership's own cancelled
///     `select!` arm can already have released the lock: the loop returns with
///     admission still open and the same wait panics. (Moving it after the
///     `break` but still inside `run_dispatch_loop` is deliberately NOT killed:
///     the drain is still awaited before the function returns, which is the
///     property that matters.)
/// (c) Spawn it instead of awaiting it — `tokio::spawn(async move { … })` —
///     which is the shape that lets leadership release the lock while a
///     provider future is still live: admission closes, but the loop exits
///     while the guard is held, so the `!loop_task.is_finished()` vacuity guard
///     fails.
/// (d) Stamp unconditionally inside the drain: the sampling loop reads a stamp
///     while the guard is still held and fails on the first sample.
#[tokio::test]
async fn the_actors_cancellation_arm_earns_the_drain_stamp_before_the_loop_exits() {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let mut actor = crate::actor::actor_with_test_db(db.clone());

    // Everything the assertions need, taken before the actor is moved.
    let incarnation = actor.coordinator_incarnation_id.clone();
    let scope = actor.provider_action_scope.clone();
    let cancel = actor.cancel.clone();

    // `run()` registers the incarnation before reaching the loop; the loop
    // itself does not, so the fixture stands in for that half. Without a row,
    // `mark_draining` matches nothing and the drain reports `NotStamped`.
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(&incarnation)
        .await
        .expect("register the incarnation the actor will stamp");

    // The stand-in for a live provider call behind a charged `calling` row.
    let in_flight = scope.admit().expect("an open scope admits before shutdown");
    assert!(
        drain_stamp_in(&db, &incarnation).await.is_none(),
        "nothing may be stamped before the loop has even run"
    );
    assert!(
        !scope.is_admission_closed(),
        "admission must start open, or the wait below proves nothing"
    );

    cancel.cancel();
    let loop_task = tokio::spawn(async move { actor.drive_dispatch_loop_for_test().await });

    // The arm ran and reached step 1 of the drain. This is the assertion that
    // dies if the call is deleted or moved out of the arm.
    wait_until_admission_closed(&scope).await;

    let liveness = CiIncarnationLiveness {
        incarnations: djinn_db::CoordinatorIncarnationRepository::new(db.clone()),
    };
    for sample in 0..DRAIN_SAMPLES {
        tokio::time::sleep(DRAIN_POLL).await;
        assert!(
            drain_stamp_in(&db, &incarnation).await.is_none(),
            "sample {sample}: the arm stamped while a provider future was still in flight"
        );
        assert_eq!(
            liveness.quiescence_proof(&incarnation).await,
            CiQuiescenceProof::None,
            "sample {sample}: a recovering incarnation must find no proof yet"
        );
    }

    // Vacuity: the window really did span a live future AND a loop that had not
    // exited — i.e. the arm awaits the drain inline rather than detaching it.
    assert_eq!(
        scope.in_flight(),
        1,
        "the guard must still be held, or the loop above proved nothing"
    );
    assert!(
        !loop_task.is_finished(),
        "the cancellation arm must await the drain, not spawn it: a loop that \
         exits here lets leadership release the advisory lock with a provider \
         future still alive"
    );

    // The provider call returns; only now may the arm finish.
    drop(in_flight);
    tokio::time::timeout(PATIENCE, loop_task)
        .await
        .expect("the loop must exit once the scope empties")
        .expect("the dispatch loop must not panic");

    assert!(
        drain_stamp_in(&db, &incarnation).await.is_some(),
        "the production arm must EARN the stamp, or no `calling` row is ever recoverable"
    );
    assert_eq!(
        liveness.quiescence_proof(&incarnation).await,
        CiQuiescenceProof::GracefulDrain
    );
    assert!(
        scope.is_drained(),
        "and leadership releases the advisory lock on this flag"
    );
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
/// observes lock availability from its own Postgres session. The coordinator's
/// half — that the durable `provider_actions_drained_at` stamp is written only
/// after the join, which is what a *later* incarnation reads — is pinned by
/// `the_drain_stamp_is_withheld_until_the_last_provider_future_is_joined` and
/// `a_live_provider_future_blocks_the_calling_handoff_until_its_owner_stamps`
/// above. All three are required: this one for the primitive, those for the
/// producer, and the leadership suite for the lock.
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

/// A live `calling` row drains authoritatively: its owner keeps it, the poll
/// that finds it takes ownership of the evidence rather than handing it back,
/// and the drain report stays non-quiescent until that owner finalizes.
///
/// Handing the evidence back would double-remedy: the queue re-entry the route
/// triggered is still live, and `handle_queue_failure` would reopen the task for
/// rework at the same time. The counts are the AC10 evidence a binary rollback
/// is read against, and this is the transition that makes them move.
#[tokio::test]
async fn a_live_calling_row_defers_without_stealing_and_blocks_the_drain() {
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

    let target = f.merge_target(&auto);
    let before = f.effects().await;
    let outcome = run(&f, &target, &id, &id, &blocking).await;
    let after = f.effects().await;

    assert_eq!(
        outcome,
        CiLaneOutcome::Deferred(CiDeferral::ProviderCallInFlight)
    );
    assert_no_effects_beyond_routes(&before, &after);
    assert_eq!(
        f.attempt(&id, CiAction::Reenqueue).await.action_phase,
        CiActionPhase::Calling,
        "the route defers to the live owner; it does not steal"
    );

    // The drain is blocked until the quiescence counts read zero.
    let counts = f.routes.quiescence_counts().await.expect("quiescence");
    assert_eq!(counts.calling_rows, 1);
    assert!(!counts.is_quiescent(), "the drain stays blocked");

    // The owner finalizes; only now do the counts converge.
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
        "with no reserved, calling, or leased rows the drain report reads clean"
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
    assert_eq!(
        crate::pr_poller::ci_lane_routing::fold(std::slice::from_ref(&outcome)),
        CiLaneDisposition::Routed,
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
    let report =
        sweep_reserved_routes(&f.routes, &FixedHead(Some(HEAD.to_owned())), &f.incarnation).await;

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

    let report = sweep_reserved_routes(&f.routes, &FixedHead(None), &f.incarnation).await;

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

/// The recurring reservation sweep has to fire inside a process lifetime.
///
/// The sweep fixtures below drive the production tick by winding
/// `last_ci_route_sweep` back with [`a_sweep_interval_ago`] — which is defined
/// as `2 * CI_ROUTE_SWEEP_INTERVAL`, i.e. *relative to the constant under
/// test*. That is a second opinion from the same witness: set
/// `RESERVED_SWEEP_INTERVAL` to a year and every one of those fixtures still
/// passes, because the fixture's idea of "long enough ago" moves with it, while
/// in production the sweep never runs once. The rows it exists for are exactly
/// the ones no poller revisits — a `reserved` row whose head has moved — so
/// they would sit stranded, holding their charge, until the process restarts.
///
/// So the bound here is ABSOLUTE, in units the constant cannot redefine.
///
/// NAMED FAILING MUTATIONS.
/// (a) `from_secs(60)` → any value above five minutes: the ceiling fails.
/// (b) `from_secs(60)` → a sub-second value: the floor fails (that is a hot
///     loop over the route table on every tick, not a sweep).
/// (c) Pointing `CI_ROUTE_SWEEP_INTERVAL` at a second, independent constant:
///     the equality fails, because the tick and the recovery contract would be
///     free to drift apart.
#[test]
fn the_reserved_sweep_interval_is_bounded_in_absolute_time() {
    const CEILING: Duration = Duration::from_secs(5 * 60);
    const FLOOR: Duration = Duration::from_secs(1);

    assert!(
        RESERVED_SWEEP_INTERVAL <= CEILING,
        "the reserved sweep must fire within a process lifetime: \
         {RESERVED_SWEEP_INTERVAL:?} exceeds the {CEILING:?} ceiling, which \
         strands every `reserved` row the polling path cannot revisit until \
         the coordinator restarts",
    );
    assert!(
        RESERVED_SWEEP_INTERVAL >= FLOOR,
        "and it must remain a sweep: {RESERVED_SWEEP_INTERVAL:?} is below the \
         {FLOOR:?} floor, which makes it a hot loop over the route table",
    );
    assert_eq!(
        crate::pr_poller::CI_ROUTE_SWEEP_INTERVAL,
        RESERVED_SWEEP_INTERVAL,
        "the tick's interval IS the executor's constant; a second, independent \
         constant lets the tick and the recovery contract drift apart",
    );
}

/// The coordinator admits into the LEADER's provider-action scope.
///
/// `CoordinatorActor::new` destructures `CoordinatorDeps` and moves the scope
/// across. Replace that move with a fresh `ProviderActionScope::new()` and the
/// actor admits every `rerun_failed_jobs` future into a private registry nobody
/// waits on: `server/src/leadership.rs` polls the scope *it* built, reads zero
/// in flight, and releases the coordinator advisory lock while a provider
/// mutation is still running — which is the single thing AC "the lock is not
/// released while a provider future is live" forbids. Every other fixture in
/// this file reads the scope back off the actor it built, so the actor agrees
/// with itself and all of them stay green.
///
/// Behavioural: no new seam is needed. `provider_action_scope` is already
/// `pub(super)` and `CoordinatorDeps::with_provider_action_scope` is the same
/// builder production uses.
///
/// NAMED FAILING MUTATIONS.
/// (a) `provider_action_scope,` → `provider_action_scope: ProviderActionScope::new()`
///     in `CoordinatorActor::new`: the leader-to-actor assertion fails on the
///     first admit.
/// (b) Dropping `.with_provider_action_scope(..)` from the deps builder: the
///     same failure, for the same reason.
/// (c) Making `ProviderActionScope::clone` deep rather than `Arc`-shared: both
///     directions fail.
#[tokio::test]
async fn the_actor_admits_into_the_leaders_provider_action_scope() {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let leader = ProviderActionScope::new();
    let actor = crate::actor::actor_with_test_db_and_scope(db, leader.clone());

    // Vacuity: both halves start empty, so a non-zero reading below is caused
    // by the admit and not by the fixture's own setup.
    assert_eq!(leader.in_flight(), 0, "vacuity: the leader starts empty");
    assert_eq!(
        actor.provider_action_scope.in_flight(),
        0,
        "vacuity: the actor starts empty",
    );

    // Leader → actor. This is the direction leadership reads.
    let held = leader.admit().expect("an open scope admits");
    assert_eq!(
        actor.provider_action_scope.in_flight(),
        1,
        "the actor must share the LEADER's scope; a private one reports zero \
         while a provider future is live, and the advisory lock is released \
         out from under it",
    );
    drop(held);
    assert_eq!(actor.provider_action_scope.in_flight(), 0);

    // Actor → leader. This is the direction the routing executor writes.
    let held = actor
        .provider_action_scope
        .admit()
        .expect("an open scope admits");
    assert_eq!(
        leader.in_flight(),
        1,
        "a future the coordinator admitted must be visible to the half that \
         holds the advisory lock",
    );
    drop(held);
    assert_eq!(leader.in_flight(), 0);

    // And the rollback posture is one fact, not two.
    leader.close_admission();
    assert!(
        actor.provider_action_scope.is_admission_closed(),
        "closing admission on the leader's half must close the coordinator's; \
         two scopes would let the coordinator keep admitting after leadership \
         declared the drain",
    );

    // ── The one hop above this that nothing can read back ──────────────────
    //
    // Production reaches `CoordinatorActor::new` through
    // `CoordinatorHandle::spawn`, which rebuilds the deps with a struct-update
    // to default the consolidation runner. `..deps` carries the scope; naming
    // the field explicitly there would silently replace it, and `spawn` returns
    // a handle with no scope accessor, so no fixture can observe the swap.
    // Adding an accessor purely to observe it would be inventing the seam under
    // test, so this hop is asserted textually: the spawn helper may not name a
    // scope at all.
    let handle = strip_line_comments(include_str!("../../../handle.rs"));
    assert!(
        handle.contains("..deps"),
        "`CoordinatorHandle::spawn` must carry the caller's deps forward with a \
         struct update; enumerating the fields makes a dropped one a silent \
         behaviour change rather than a compile error",
    );
    assert!(
        !handle.contains("ProviderActionScope"),
        "and it must never name the scope: the only correct value is the one \
         `AppState` built, and spawning with a fresh one hands leadership an \
         empty registry to wait on",
    );
}

/// Block until the fake provider has recorded `want` provider mutations.
///
/// The wait is on the seam's own counter, which is incremented *before* the
/// mutation parks — so reaching it means the executor is genuinely inside the
/// call, not that a sleep was probably long enough.
async fn wait_for_mutations(provider: &FakeProvider, want: usize) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while provider.calls().mutations() < want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lane never reached its provider mutation, so nothing below is under test"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The lane's provider call runs **inside the leader's scope**, and leadership
/// cannot finish draining while it does.
///
/// # The gap this closes
///
/// [`the_actor_admits_into_the_leaders_provider_action_scope`] proves the actor
/// *holds* the leader's object. It proves nothing about what the actor does
/// with it, and the object was pinned at nothing downstream:
/// `ci_lane_routing::drive_lane` opens with
/// `let scope = self.provider_action_scope.clone();` and hands that clone to
/// every `execute_route`. Replace it with `ProviderActionScope::new()` and each
/// `rerun_failed_jobs` future is admitted into a registry leadership never
/// waits on and `close_admission()` never reaches — with the whole acceptance
/// list green, because every other fixture either drives `execute_route` with a
/// scope it built itself or reads the actor's scope back off the actor.
///
/// Separately, `execute_route`'s `drop(admitted)` sits *after* the provider
/// call for one reason: leadership's join is only a quiescence proof if the
/// guard outlives the call. Move that drop above the call and
/// `wait_until_drained` returns, `provider_actions_drained_at` is stamped, and
/// the advisory lock is released while `rerun_failed_jobs` is still in flight —
/// so the next incarnation legally recovers a `calling` row whose call is still
/// running.
///
/// **One number kills both.** `in_flight()`, read off the *leader's* handle
/// from inside the parked mutation, is `1` only if the scope the executor
/// admitted into is the leader's *and* the guard is still held. A private scope
/// reports `0`; an early `drop(admitted)` reports `0`.
///
/// The drain half is then driven for real rather than argued: the production
/// `quiesce_provider_actions` runs against the same leader scope while the
/// mutation is parked, and the fixture samples the ledger throughout.
/// `the_drain_stamp_is_withheld_until_the_last_provider_future_is_joined`
/// asserts the same withholding but admits its own stand-in guard into its own
/// scope, so it stays green under both mutations above; this one does not,
/// because the guard it waits on is the one the production lane took.
///
/// NAMED FAILING MUTATIONS.
/// (a) `let scope = self.provider_action_scope.clone();` →
///     `ProviderActionScope::new()` in `drive_lane`: the in-flight assertion
///     reads 0, and the drain stamps within a millisecond of being spawned, so
///     the first sample fails too.
/// (b) Hoist `drop(admitted)` above the `match admitted.kind()` provider call
///     in `execute_route`: identical readings, for the other reason.
/// (c) Pass the scope by value into `execute_route` and drop it early: same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lane_holds_the_leaders_scope_open_across_its_provider_call() {
    let h = lane_harness().await;
    let scope = h.actor.provider_action_scope.clone();
    let incarnation = h.actor.coordinator_incarnation_id.clone();
    djinn_db::CoordinatorIncarnationRepository::new(h.db.clone())
        .register(&incarnation)
        .await
        .expect("register the coordinator incarnation the drain stamps");

    let (parked, release) = FakeProvider::parked_mutation();
    parked.set_check_runs(vec![inconclusive_check("Quality Gate / test", 77)]);
    parked.probe_scope(scope.clone());

    // Vacuity: the reading below is caused by the lane, not by the setup.
    assert_eq!(scope.in_flight(), 0, "the leader's scope starts empty");
    assert!(drain_stamp_in(&h.db, &incarnation).await.is_none());

    // Leadership's half, spawned up front but held until the lane's own call is
    // genuinely in flight: closing admission any earlier would make the route a
    // *refusal* rather than something the drain has to join.
    let drain = tokio::spawn({
        let scope = scope.clone();
        let db = h.db.clone();
        let incarnation = incarnation.clone();
        let parked = parked.clone();
        async move {
            wait_for_mutations(&parked, 1).await;
            let incarnations = djinn_db::CoordinatorIncarnationRepository::new(db);
            quiesce_provider_actions(
                &scope,
                &incarnations,
                &incarnation,
                PROVIDER_ACTION_DRAIN_TIMEOUT,
            )
            .await
        }
    });

    let lane = poll_pr_head(&h, &parked, None);
    let observer = async {
        wait_for_mutations(&parked, 1).await;

        // ── The scope's USE, not its identity ───────────────────────────────
        assert_eq!(
            scope.in_flight(),
            1,
            "the lane's provider call must be inside the LEADER's scope and must \
             still hold its guard; a scope built inside `drive_lane`, or a \
             `drop(admitted)` hoisted above the call, both read 0 here",
        );

        // ── …and leadership's drain must not finish inside that window ──────
        wait_until_admission_closed(&scope).await;

        for sample in 0..DRAIN_SAMPLES {
            tokio::time::sleep(DRAIN_POLL).await;
            assert!(
                drain_stamp_in(&h.db, &incarnation).await.is_none(),
                "sample {sample}: `provider_actions_drained_at` was stamped while the \
                 lane's own `rerun_failed_jobs` was still running; a new incarnation \
                 reading that stamp recovers a charged `calling` row whose call is live",
            );
            assert!(
                !scope.is_drained(),
                "sample {sample}: leadership releases the advisory lock on this flag",
            );
        }

        // Vacuity: the window above really spanned a live call and an unfinished
        // drain, rather than a scope that had quietly emptied.
        assert_eq!(scope.in_flight(), 1, "the lane's guard must still be held");
        assert!(
            !drain.is_finished(),
            "the drain must still be inside its join"
        );
        assert_eq!(
            parked.calls().rerun_failed_jobs,
            1,
            "and the mutation must still be exactly the one parked call",
        );

        release.notify_one();
    };
    let (disposition, ()) = tokio::join!(lane, observer);

    let outcome = tokio::time::timeout(PATIENCE, drain)
        .await
        .expect("the drain must finish once the lane's call returns")
        .expect("the drain task must not panic");
    assert_eq!(outcome, CiDrainOutcome::Stamped);
    assert!(
        drain_stamp_in(&h.db, &incarnation).await.is_some(),
        "and a joined drain must stamp, or no handoff could ever happen",
    );

    assert_eq!(
        parked.in_flight_during_mutations(),
        vec![1],
        "exactly one mutation ran, and the leader's scope counted it while it ran",
    );
    assert_eq!(
        parked.reran(),
        vec![("acme".to_owned(), "widgets".to_owned(), 77)],
        "vacuity: the call was the Tier-1 re-run for the run the evidence names",
    );
    assert!(disposition.is_routed());
    assert_eq!(scope.in_flight(), 0, "and the guard was released, once");
}

/// A **closed** leader scope stops the lane's provider call.
///
/// The other half of the same pin, and the one that does not depend on
/// observing a running call: `close_admission()` is what leadership calls
/// first, and it can only reach the lane's route if the lane consults the
/// leader's scope. `refused_total` is incremented by
/// `ProviderActionScope::admit` on the object that refused, so a non-zero
/// reading on the *leader's* handle is direct evidence of which scope the
/// executor asked.
///
/// The route row is asserted `reserved`, not merely absent-of-effects: that is
/// the vacuity guard proving the route reached the admission gate at all rather
/// than being declined earlier for some unrelated reason — and it is also the
/// contract, since a refusal must leave the row recoverable rather than
/// charged.
///
/// NAMED FAILING MUTATIONS. Both mutations in
/// [`the_lane_holds_the_leaders_scope_open_across_its_provider_call`]'s (a)
/// class: a private scope in `drive_lane` is open, so it admits, the provider
/// is called, the row terminalizes `retriggered`, a slot is charged, and
/// `refused_total` on the leader's scope stays 0. Four assertions fail.
#[tokio::test]
async fn a_closed_leader_scope_refuses_the_lanes_provider_call() {
    let h = lane_harness().await;
    let scope = h.actor.provider_action_scope.clone();
    let provider = FakeProvider::default();
    provider.set_check_runs(vec![inconclusive_check("Quality Gate / test", 77)]);
    provider.probe_scope(scope.clone());

    scope.close_admission();
    assert_eq!(
        scope.counts().refused_total,
        0,
        "vacuity: nothing has been refused before the lane runs",
    );

    let disposition = poll_pr_head(&h, &provider, None).await;
    assert!(
        disposition.is_routed(),
        "a refused admission still owns the evidence: the row is reserved and \
         recoverable, and the legacy remediation path must stay withheld",
    );

    assert_eq!(
        scope.counts().refused_total,
        1,
        "the lane must have asked the LEADER's scope for admission; a scope built \
         inside `drive_lane` would be open, would admit, and would call GitHub \
         after leadership declared the drain",
    );
    assert_eq!(
        provider.calls().mutations(),
        0,
        "a closed scope performs no provider mutation",
    );
    assert!(provider.in_flight_during_mutations().is_empty());
    assert_eq!(
        charged_budget_counters(&h.db).await,
        0,
        "and an unadmitted route consumes no Tier-1 charge",
    );

    // Vacuity: the route really got as far as the admission gate.
    let subject = CiRouteSubject::task(h.task_id.clone());
    let identity = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: Some(77),
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let row = CiRouteAttemptRepository::new(h.db.clone())
        .get(
            &subject,
            &provider_action_key(&subject, &identity, CiAction::RerunRun),
        )
        .await
        .expect("route read")
        .expect("the Tier-1 route reserved its row before admission was asked");
    assert_eq!(
        row.action_phase,
        CiActionPhase::Reserved,
        "a refusal leaves the row recoverable rather than charged and `calling`",
    );
    assert_eq!(row.terminal_outcome, None);
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
            failing_filter,
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
        run_id: Some(971),
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

/// A lane-opened Tier-2 lease becomes a Lead session, bound to that route.
///
/// `CiLaneRouting::settle` calls `dispatch_opened_tier2_leases`, and until this
/// fixture existed that call could be deleted with the entire `nafu` command
/// list green. Every other lane fixture asserts the *lease* (`1`) and the
/// absence of a worker (`0`), and neither number moves when the dispatch is
/// removed. The one fixture that does assert an arbitration row — the
/// twelve-poll hold escalation — reaches Lead through `ci_hold`'s own
/// `dispatch_escalated_hold`, a different call site entirely, so it stays green
/// too.
///
/// The consequence of the deletion is precisely the wedge the comment at that
/// call site describes: the lane withholds the legacy remediation path because
/// an adjudication is pending, and nothing ever adjudicates. The task sits in
/// `pr_review` with an open head-scoped lease that also blocks every other
/// Tier-2 route for that head.
///
/// `lead_session_id` is asserted, not merely the arbitration count: a row that
/// happens to exist proves the coordinator wrote *an* arbitration, while the
/// binding proves it wrote the one **this** route is adjudicated under. That
/// binding is also what makes `unapplied_lead_results` in the rollback
/// quiescence report mean "a Lead was dispatched" rather than "a lease exists".
///
/// # What this fixture found on its first run
///
/// A real one, and not the call site it was written to guard. The dispatch
/// bound the route under `format!("arbitration:{task_id}:{hold_cycle}")` — a
/// ~50 character string — and `ci_route_attempts.lead_session_id` is
/// `VARCHAR(36)`. Postgres refuses an over-long value rather than truncating
/// it, the dispatch logged the resulting error at `warn` and escalated the
/// board anyway, so every other observable effect of a Tier-2 dispatch landed
/// while the binding never did on any lane. The two counters that read this
/// column — `unapplied_lead_results` and the route report's `lead_invocations`
/// — were therefore pinned at zero by construction. Nothing else could have
/// caught it: the `tier2_dispatch` fixtures hand the dispatch a handoff whose
/// route row does not exist, so their attach misses the fence and returns
/// `NotFound` before the column is ever written. The dispatch now binds the
/// arbitration row's own id.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `self.dispatch_opened_tier2_leases(outcomes, task_id).await;`
///     from `settle`: no arbitration row, the task stays `pr_review`, and the
///     route row's `lead_session_count` stays `0`.
/// (b) Read the boolean instead of the handoff in `dispatch_opened_tier2_leases`
///     (`Tier2 { lease_opened: true, .. } => …` with no handoff to bind): there
///     is nothing to pass to `dispatch_ci_tier2_lead`, so it cannot compile —
///     and if it is made to, the `provider_action_key` assertion fails.
/// (c) Move the dispatch above `fold` and into the `CompleteEmpty` arm: this
///     route is not complete-empty, so nothing dispatches, as in (a).
/// (d) Drop the `attach_lead_session` call from `dispatch_ci_tier2_lead`: the
///     arbitration and the board transition still land, and the two
///     `lead_session_*` assertions fail alone.
/// (e) Bind any handle that is not a row id — the old
///     `arbitration:{task_id}:{hold_cycle}`, or anything else over 36
///     characters: the `UPDATE` is refused by the column type, the dispatch
///     swallows the error, and (d)'s two assertions fail in exactly the same
///     way. That is the mutation this fixture actually caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lane_opened_tier2_lease_becomes_a_lead_session_bound_to_its_route() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();
    provider.set_check_runs(vec![causal_check("merge-group / integration", 980)]);

    assert_eq!(
        arbitration_rows(&h.db).await,
        0,
        "precondition: nothing has adjudicated anything yet",
    );
    assert_eq!(
        djinn_db::test_support::task_status_for_test(&h.db, &h.task_id).await,
        "pr_review",
        "precondition: the task starts in this lane's origin state",
    );

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
            &[merge_group_run(980)],
            Some(&dequeue_event()),
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
        "precondition: a complete causal failure opens exactly one adjudication",
    );

    // ── The lease became a Lead session ─────────────────────────────────────
    assert_eq!(
        arbitration_rows(&h.db).await,
        1,
        "an opened lease must become a Lead session, or the lane suppresses the \
         legacy path and nothing adjudicates in its place",
    );
    assert_eq!(
        djinn_db::test_support::task_status_for_test(&h.db, &h.task_id).await,
        "needs_lead_intervention",
        "and the board must enter the only lane a Lead session runs from",
    );

    // ── …and it is bound to THIS route, not merely coincident with it ───────
    let identity = CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: Some(980),
        run_head_sha: MERGE_GROUP_SHA.to_owned(),
        dequeue_id: Some(DEQUEUE_ID.to_owned()),
    };
    let subject = CiRouteSubject::task(h.task_id.clone());
    let key = provider_action_key(&subject, &identity, CiAction::AskLead);
    let row = CiRouteAttemptRepository::new(h.db.clone())
        .get(&subject, &key)
        .await
        .expect("route read")
        .expect("a complete causal merge-group failure takes one Tier-2 route");
    assert_eq!(
        row.lead_session_count, 1,
        "exactly one Lead session is attached to the route it adjudicates",
    );
    let arbitration =
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(h.db.clone())
            .get_latest_for_task(&h.task_id)
            .await
            .expect("arbitration read")
            .expect("the dispatch writes the arbitration this route is adjudicated under");
    assert_eq!(
        row.lead_session_id.as_deref(),
        Some(arbitration.id.as_str()),
        "the route must name the arbitration row adjudicating it, not merely have \
         some session id",
    );

    let directive = arbitration
        .directive
        .expect("the dispatch writes the directive the Lead session reads");
    assert_eq!(
        directive["ci_route"]["provider_action_key"].as_str(),
        Some(key.as_str()),
        "the block Lead reads must name the route the lease was opened on",
    );

    // ── And the adjudication spends nothing else ────────────────────────────
    assert_eq!(provider.calls().mutations(), 0, "no provider mutation");
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
        "a Tier-2 adjudication dispatches no worker",
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
        run_id: Some(0),
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

/// The merge-group lane's *lane-level* incomplete capture keys on a REAL
/// identity — never on the synthetic `run_id: 0` / `dequeue_id: None` one.
///
/// Two things are pinned here, and revision 58 separates them:
///
/// * the **dequeue id** is a fact the poll named, and it stays. It is what keeps
///   two dequeues of one head distinct, and dropping it is what let the second
///   dequeue resolve `AlreadyPresent` and never get a route at all.
/// * the **run id** is dropped, because `MaxPagesTruncated` is irrecoverable and
///   revision 58 routes every irrecoverable reason under the run-absent
///   identity. That is not a lost fact: it is the collapse that makes two
///   irrecoverable reasons on one head share one row, one lease and one Lead
///   session instead of opening a pair of each. The merge lane reaches these
///   reasons *after* correlation named a run and the PR-head lane reaches them
///   before, so keying on the run here would split one head's adjudication in
///   two purely by which lane happened to observe it.
#[tokio::test]
async fn a_lane_level_merge_group_capture_keys_on_a_real_identity() {
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
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(provider.calls().mutations(), 0, "no provider mutation");
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "precondition: this reason really does create a route row",
    );

    // The real dequeue id survives; the run is genuinely absent, not `0`.
    let run_absent = CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: Some(DEQUEUE_ID.to_owned()),
    };
    assert!(
        route_exists(&h, &run_absent, CiAction::AskLead).await,
        "an irrecoverable lane-level capture must be keyed on the run-absent \
         identity, carrying the dequeue id the poll actually named",
    );
    assert!(
        !route_exists(&h, &merge_group_identity(975), CiAction::AskLead).await,
        "and NOT on the correlated run: that would split one head's \
         adjudication across the two lanes that reach this reason",
    );
    assert!(
        !route_exists(&h, &fabricated_merge_group_identity(), CiAction::AskLead).await,
        "no route row may be keyed on the synthetic `run_id: 0` identity",
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
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(provider.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
    );

    // The lane identity: everything real except `run_id`, which is genuinely
    // ABSENT because "ambiguous" means no single run exists to name. Revision 58
    // encodes that as NULL rather than the `0` sentinel this used to pin.
    let lane_identity = CiEvidenceIdentity {
        lane: CiLane::MergeGroup,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
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

/// A blocking check attributable to no Actions run takes ONE run-absent route.
///
/// `RunAttributionUnavailable`: there is no run identity to key on, and
/// `rerun_failed_jobs` has no run to act on either.
///
/// Wave 3b made this **hold**, and revision 58 names that as a wedge: a check
/// belonging to no nameable Actions run belongs to no nameable Actions run on
/// every subsequent poll too, so the hold never resolved — no route, no
/// adjudication, nothing on the board, and a CI gate that never cleared. It is
/// irrecoverable, so it takes one diagnose-only Tier-2 route under `run_id`
/// NULL. What it must NOT do is key that route on the fabricated `run_id: 0`
/// identity, which is what collapsed two distinct observations onto one key.
#[tokio::test]
async fn an_unattributable_blocking_check_takes_one_run_absent_route() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    // No `run_id`, and an `html_url` that `parse_actions_run_id` cannot read.
    let mut orphan = causal_check("External / policy", 979);
    orphan.run_id = None;
    orphan.html_url = "https://example.test/checks/1".to_owned();
    provider.set_check_runs(vec![orphan]);

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
            failing_filter,
        )
        .await;

    assert!(disposition.is_routed());
    assert_eq!(
        provider.calls().mutations(),
        0,
        "there is no run to re-run, so no provider mutation is legal",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "exactly one diagnose-only route",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
        "under exactly one Tier-2 lease",
    );

    // Keyed on genuine absence, not on the `run_id: 0` sentinel.
    let subject = CiRouteSubject::task(h.task_id.clone());
    let run_absent = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let routes = CiRouteAttemptRepository::new(h.db.clone());
    let row = routes
        .get(
            &subject,
            &provider_action_key(&subject, &run_absent, CiAction::AskLead),
        )
        .await
        .expect("route read")
        .expect("the route is keyed on the run-absent identity");
    assert_eq!(row.identity.run_id, None);
    assert!(
        routes
            .get(
                &subject,
                &provider_action_key(
                    &subject,
                    &CiEvidenceIdentity {
                        // The `run_id: 0` sentinel revision 58 abolished.
                        run_id: Some(0),
                        ..run_absent.clone()
                    },
                    CiAction::AskLead
                ),
            )
            .await
            .expect("route read")
            .is_none(),
        "and nothing is findable at the fabricated sentinel key",
    );
}

/// Unusable execution-timestamp evidence takes **one run-absent route**, end to
/// end, whichever of the two layers catches it.
///
/// # What this pins that the fixture above does not
///
/// The fixture above reaches the run-absent identity through
/// `RunAttributionUnavailable` — a check belonging to no Actions run at all. AC5
/// puts a second, quite different reason on the same side: "a
/// `blocking_evidence_completeness` timestamp reason". Here the check *is*
/// attributable (`run_id: Some(4242)`), terminal and causal; only its completion
/// timestamp is missing. AC14 requires it to route "under the same nullable-run
/// identity", so that every irrecoverable reason on one head shares **one**
/// diagnose-only route rather than opening a second head-scoped lease.
///
/// # Why this is a contract witness and not a line witness
///
/// The collapse is implemented twice on purpose, and `capture_pr_head_evidence`
/// says so: the lane fails closed on `blocking_evidence_completeness` before it
/// fans out, and `CiCapture::prove_complete` re-checks the same predicate per
/// run behind `drive_one`'s own `run_absent_if_required`. That is deliberate
/// defence in depth, so **either layer alone satisfies this fixture** — which
/// also means each is individually deletable with the whole list green, and
/// that is a property of the duplication rather than a hole. Removing *both* is
/// what breaks the observable contract, and that is what this fails on.
///
/// NAMED FAILING MUTATION (both are required, and neither alone suffices):
/// delete `if let Some(reason) = blocking_evidence_completeness(blocking)` from
/// `capture_pr_head_evidence` **and** replace `drive_one`'s
/// `run_absent_if_required(…)` with `run.identity.clone()`. The route is then
/// keyed on run 4242, the run-absent lookup finds nothing, and Tier 1 would
/// re-run a workflow whose own evidence contradicts its conclusion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unusable_timestamp_evidence_takes_one_run_absent_route() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();

    // Attributable to run 4242 and hard-failing, but with no completion
    // timestamp — `blocking_evidence_completeness`'s irrecoverable case.
    let mut unusable = causal_check("Quality Gate / test", 4242);
    unusable.completed_at = None;
    provider.set_check_runs(vec![unusable]);

    let disposition = poll_pr_head(&h, &provider, None).await;
    assert!(disposition.is_routed());
    assert_eq!(
        provider.calls().mutations(),
        0,
        "an irrecoverably incomplete capture authorizes no provider mutation",
    );

    let subject = CiRouteSubject::task(h.task_id.clone());
    let run_named = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: Some(4242),
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let run_absent = CiEvidenceIdentity {
        run_id: None,
        ..run_named.clone()
    };

    // Vacuity: exactly one route exists, so "found at the run-absent key" is not
    // one of two rows.
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "one head, one diagnose-only route — not one per irrecoverable reason",
    );
    assert!(
        route_exists(&h, &run_absent, CiAction::AskLead).await,
        "the per-run capture must collapse onto the run-absent identity, or a \
         second head-scoped lease is contended for the same PR head",
    );
    assert!(
        !route_exists(&h, &run_named, CiAction::AskLead).await,
        "and nothing may be keyed on the run whose evidence was never usable",
    );

    let routes = CiRouteAttemptRepository::new(h.db.clone());
    let row = routes
        .get(
            &subject,
            &provider_action_key(&subject, &run_absent, CiAction::AskLead),
        )
        .await
        .expect("route read")
        .expect("the run-absent route row");
    assert_eq!(row.identity.run_id, None);
    assert_eq!(
        row.identity.run_head_sha, HEAD,
        "and the run head is normalised to the observed PR head, or the row \
         would still be distinct from its run-absent sibling",
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
///
/// Comments are stripped before matching — this was the one guard in the family
/// that did not do so, which made it satisfiable by writing prose: a comment
/// merely *naming* `empty_check_set_is_authoritatively_green` in a branch that
/// had reverted to `checks.check_runs.is_empty()` kept it green.
#[test]
fn both_lane_fast_paths_consult_the_completeness_predicate() {
    for (label, raw) in [
        ("pr_draft", include_str!("../../pr_watcher.rs")),
        ("pr_review", include_str!("../../pr_review_watcher.rs")),
    ] {
        let source = strip_line_comments(raw);
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
    assert_eq!(
        (
            crate::pr_poller::ci_lane_routing::fold(std::slice::from_ref(&head)),
            crate::pr_poller::ci_lane_routing::fold(std::slice::from_ref(&merge)),
        ),
        (CiLaneDisposition::Routed, CiLaneDisposition::Routed),
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

// ===========================================================================
// The poll-ordering contract (proposal `nafu`, revision 58; ACs 9, 12, 14)
// ===========================================================================
//
// # What these fixtures count
//
// One logical poll of one lane identity is three steps — reserve, enumerate,
// apply — and the whole clause is about what happens when two of them
// interleave. So none of these fixtures asserts the name of the enum the lane
// returned. Every claim below is a count taken from real state:
//
// | Claim | Counted as |
// | --- | --- |
// | the streak's place in the authority order | `CiIncompleteHoldRepository::get` → `next_poll_sequence` / `last_applied_poll_sequence` |
// | "this poll applied nothing" | one `ci_incomplete_hold_observations` row marked `superseded_observation` |
// | no route | `SELECT count(*) FROM ci_route_attempts …` |
// | no Tier-2 lease | `… WHERE tier2_lease_id IS NOT NULL` |
// | no Lead session | `SELECT count(*) FROM task_arbitrations` — `dispatch_ci_tier2_lead` writes that row *before* it transitions the board, so it is the earliest durable trace a Lead adjudication leaves |
// | no worker dispatch | `SELECT count(*) FROM task_attempts` |
// | no board mutation | `tasks.status` plus `activity_log` |
// | no Tier-1 charge | `SELECT count(*) FROM ci_route_budget_counters` |
// | no provider mutation | [`FakeProvider`]'s own counters |
//
// # Why the interleaving is real
//
// [`FakeProvider::parked_enumeration`] parks a poll *inside* the provider call —
// the exact gap the two short transactions straddle. A fixture that instead
// called `reserve` twice and then `apply` twice in the order it wanted would be
// performing the ordering it claims to witness: the sequences would be whatever
// the fixture's call order made them. Here the fixture never picks a sequence.
// It waits on the ledger ([`wait_for_reservations`]) and reads the assigned
// sequences back out of it.

/// The hold identity the PR-head lane derives for [`HEAD`].
///
/// Built with the production constructor rather than a literal struct, so a
/// change to what the lane keys a streak on breaks these fixtures instead of
/// silently giving them a streak nothing writes to.
fn pr_head_hold_identity(task_id: &str) -> djinn_db::CiHoldIdentity {
    crate::actor::CoordinatorActor::ci_hold_identity(
        &CiRouteSubject::task(task_id),
        "acme",
        "widgets",
        PR,
        HEAD,
        CiLane::PrHead,
        None,
    )
}

/// Count observations sitting in one terminal state.
///
/// `count_rows_for_test` is the only relation-counting seam `djinn-coordinator`
/// has — a boundary test forbids a direct `sqlx` dependency — and it
/// interpolates its argument into `SELECT count(*) FROM {…}`. Every argument
/// passed here is a literal in this file.
async fn observations_marked(db: &Database, outcome: &str) -> i64 {
    djinn_db::test_support::count_rows_for_test(
        db,
        &format!("ci_incomplete_hold_observations WHERE apply_outcome = '{outcome}'"),
    )
    .await
}

async fn observation_rows(db: &Database) -> i64 {
    djinn_db::test_support::count_rows_for_test(db, "ci_incomplete_hold_observations").await
}

async fn arbitration_rows(db: &Database) -> i64 {
    djinn_db::test_support::count_rows_for_test(db, "task_arbitrations").await
}

/// Count budget counters that have actually been **charged**.
///
/// Not the row count: `reserve` inserts both counters at zero for every route
/// it admits, so a row proves only that a route existed. `charged_count > 0` is
/// what "consumed a Tier-1 charge" means, and it is the value the charging
/// transaction writes.
async fn charged_budget_counters(db: &Database) -> i64 {
    djinn_db::test_support::count_rows_for_test(
        db,
        "ci_route_budget_counters WHERE charged_count > 0",
    )
    .await
}

/// Block until the ledger says `want` sequences have been reserved.
///
/// The wait is on **durable state**, never on a sleep long enough to "probably"
/// be enough: these fixtures assert an order, so they have to observe it.
async fn wait_for_reservations(
    holds: &djinn_db::CiIncompleteHoldRepository,
    identity: &djinn_db::CiHoldIdentity,
    want: i64,
) -> djinn_db::CiHoldStreak {
    for _ in 0..2_000 {
        if let Some(streak) = holds.get(identity).await.expect("streak read")
            && streak.next_poll_sequence >= want
        {
            return streak;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the ledger never reached {want} reserved sequences");
}

/// The whole remediation negative space, as counts over real tables.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HoldNegativeSpace {
    route_rows: i64,
    tier2_leases: i64,
    lead_sessions: i64,
    worker_attempts: i64,
    activity_rows: i64,
    task_status: String,
    tier1_charges: i64,
    provider_mutations: usize,
    has_ci_snapshot: bool,
}

async fn hold_negative_space(h: &LaneHarness, provider: &FakeProvider) -> HoldNegativeSpace {
    use djinn_db::test_support as ts;
    HoldNegativeSpace {
        route_rows: ts::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        tier2_leases: ts::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        lead_sessions: arbitration_rows(&h.db).await,
        worker_attempts: ts::task_attempt_count_for_test(&h.db, &h.task_id).await,
        activity_rows: ts::activity_row_count_for_test(&h.db, &h.task_id).await,
        task_status: ts::task_status_for_test(&h.db, &h.task_id).await,
        tier1_charges: charged_budget_counters(&h.db).await,
        provider_mutations: provider.calls().mutations(),
        has_ci_snapshot: h.ci_snapshot().await.is_some(),
    }
}

/// The complete pre-escalation negative space, asserted in one place.
///
/// Every field is a COUNT over state the mechanism writes, so a route, lease,
/// Lead session, worker, board row, charge, provider mutation or `Passing`
/// snapshot that appeared would move one of them. None of it is derived from
/// the disposition the lane returned.
#[track_caller]
fn assert_hold_is_free(before: &HoldNegativeSpace, after: &HoldNegativeSpace, what: &str) {
    assert_eq!(after.route_rows, 0, "{what}: no route row may be created");
    assert_eq!(
        after.tier2_leases, 0,
        "{what}: no Tier-2 lease may be opened"
    );
    assert_eq!(
        after.lead_sessions, 0,
        "{what}: no Lead adjudication may be dispatched"
    );
    assert_eq!(
        after.tier1_charges, 0,
        "{what}: a hold consumes no Tier-1 charge"
    );
    assert_eq!(
        after.provider_mutations, 0,
        "{what}: no provider mutation may be made"
    );
    assert!(
        !after.has_ci_snapshot,
        "{what}: an incomplete enumeration may not record a CI snapshot, \
         least of all a Passing one"
    );
    assert_eq!(
        before.worker_attempts, after.worker_attempts,
        "{what}: no worker may be dispatched"
    );
    assert_eq!(
        before.activity_rows, after.activity_rows,
        "{what}: no board activity may be written"
    );
    assert_eq!(
        before.task_status, after.task_status,
        "{what}: the task status must be untouched"
    );
}

/// Drive one whole logical poll of the real PR-head lane.
///
/// `incomplete` sets the enumeration verdict the provider reports; `None`
/// leaves the provider as configured, which by default is an authoritatively
/// complete (and empty) enumeration.
async fn poll_pr_head(
    h: &LaneHarness,
    provider: &FakeProvider,
    incomplete: Option<CheckSetIncompleteReason>,
) -> CiLaneDisposition {
    if let Some(reason) = incomplete {
        provider.set_check_runs_incomplete(reason);
    }
    h.actor
        .route_pr_head_ci_evidence(
            provider,
            &h.task_id,
            "task-short",
            "acme",
            "widgets",
            PR as u64,
            HEAD,
            failing_filter,
        )
        .await
}

/// A recoverably-incomplete enumeration holds, and the hold buys nothing.
///
/// The clause is AC9's: the hold "authorizes no provider action, charge,
/// session, worker, or board mutation". The only thing that may exist
/// afterwards is one streak row at count 1 and one observation — and that is
/// read out of the ledger, not inferred from the lane having said `Routed`.
#[tokio::test]
async fn recoverable_incomplete_set_holds_without_route_or_session() {
    let h = lane_harness().await;
    let provider = FakeProvider::default();
    let before = hold_negative_space(&h, &provider).await;

    let disposition = poll_pr_head(
        &h,
        &provider,
        Some(CheckSetIncompleteReason::PageFetchFailed),
    )
    .await;

    // Not `Legacy`: the ledger absorbed the result, so the caller must withhold
    // its legacy remediation path too.
    assert!(disposition.is_routed());
    assert_eq!(
        disposition.complete_empty(),
        None,
        "an incomplete enumeration is not the no-CI compatibility path",
    );

    // The negative space FIRST, because it is the clause: a hold that bought a
    // route, a lease, a session, a worker, a charge or a board row has already
    // broken the contract, whatever the streak says afterwards.
    let after = hold_negative_space(&h, &provider).await;
    assert_hold_is_free(&before, &after, "a recoverable incomplete poll");

    let identity = pr_head_hold_identity(&h.task_id);
    let streak = h
        .actor
        .ci_holds()
        .get(&identity)
        .await
        .expect("streak read")
        .expect("a recoverable incomplete poll creates its streak");
    assert_eq!(streak.poll_count, 1, "one incomplete poll, counted once");
    assert_eq!(streak.next_poll_sequence, 1);
    assert_eq!(
        streak.last_applied_poll_sequence, 1,
        "the applied poll advances the retained high-watermark",
    );
    assert!(
        !streak.has_escalated(),
        "one poll is nowhere near the bound"
    );
    assert_eq!(observation_rows(&h.db).await, 1);
    assert_eq!(observations_marked(&h.db, "applied_incomplete").await, 1);
}

/// A complete enumeration resets the streak — and **retains the row**.
///
/// "Reset, not deleted" is the load-bearing half. A deleted streak would look
/// tidier and would silently re-admit every observation already overtaken: with
/// the watermark gone, the next delayed apply finds a fresh row at sequence zero
/// and counts itself. So this asserts the row still exists and that
/// `last_applied_poll_sequence` did not decrease.
#[tokio::test]
async fn complete_snapshot_clears_hold_streak() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let holds = h.actor.ci_holds();

    for expected in 1..=3 {
        let provider = FakeProvider::default();
        poll_pr_head(&h, &provider, Some(CheckSetIncompleteReason::ShortRead)).await;
        let streak = holds
            .get(&identity)
            .await
            .expect("streak read")
            .expect("streak exists");
        assert_eq!(streak.poll_count, expected);
    }
    let before = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(before.poll_count, 3);
    assert_eq!(before.last_applied_poll_sequence, 3);

    // One complete poll. The provider reports a complete, empty enumeration —
    // the lane's no-CI compatibility path, which is a *complete* result.
    let complete = FakeProvider::default();
    let disposition = poll_pr_head(&h, &complete, None).await;
    assert_eq!(
        disposition.complete_empty(),
        Some(CiCompleteEmptyRoute::PrHeadProceed),
    );

    let after = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("RESET, NOT DELETED: the streak row must survive its own reset");
    assert_eq!(after.id, before.id, "the same row, not a fresh one");
    assert_eq!(
        after.poll_count, 0,
        "a complete enumeration clears the count"
    );
    assert!(!after.has_escalated());
    assert_eq!(
        after.last_applied_poll_sequence, 4,
        "the retained high-watermark advances to the complete poll's sequence",
    );
    assert!(
        after.last_applied_poll_sequence >= before.last_applied_poll_sequence,
        "the high-watermark is retained across a reset: {} -> {}",
        before.last_applied_poll_sequence,
        after.last_applied_poll_sequence,
    );
    assert_eq!(after.next_poll_sequence, 4);
    assert_eq!(observations_marked(&h.db, "applied_complete").await, 1);

    // The reset is not a remedy; it is the absence of one.
    let space = hold_negative_space(&h, &complete).await;
    assert_eq!(space.route_rows, 0);
    assert_eq!(space.tier2_leases, 0);
    assert_eq!(space.lead_sessions, 0);
    assert_eq!(space.worker_attempts, 0);
    assert_eq!(space.tier1_charges, 0);
    assert_eq!(space.provider_mutations, 0);
}

/// **The marquee.** A delayed incomplete poll cannot resurrect a cleared streak.
///
/// Poll A reserves its sequence and then parks *inside the provider call* — the
/// gap the two short transactions straddle. Poll B then runs end to end,
/// reserves the next sequence, applies a complete result, and clears the
/// streak. Only then is A released.
///
/// Everything about the order is produced by the code:
///
/// * A's reservation precedes B's because A parks after `reserve_poll` has
///   committed, and B waits on the **ledger** rather than on a sleep;
/// * A's apply follows B's because A is not released until B's call returned;
///   and
/// * the sequences are read back out of `ci_incomplete_hold_streaks` rather
///   than asserted from the order the calls appear in this function.
///
/// If `apply_poll`'s comparison against `last_applied_poll_sequence` stopped
/// rejecting the overtaken observation, A would count — and the observation
/// marked `superseded_observation`, which is the durable record that the
/// ordering rule fired, would not exist.
#[tokio::test]
async fn delayed_incomplete_after_newer_complete_is_noop() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let holds = h.actor.ci_holds();

    // Poll A: recoverably incomplete, parked inside its enumeration.
    let (parked, release_a) = FakeProvider::parked_enumeration();
    parked.set_check_runs_incomplete(CheckSetIncompleteReason::PageFetchFailed);

    // Poll B: complete, and it must not start until A has actually reserved.
    let complete = FakeProvider::default();

    let a = poll_pr_head(&h, &parked, None);
    let b = async {
        let reserved_by_a = wait_for_reservations(&holds, &identity, 1).await;
        assert_eq!(
            reserved_by_a.next_poll_sequence, 1,
            "A reserved first, and the ledger says so",
        );
        assert_eq!(
            reserved_by_a.last_applied_poll_sequence, 0,
            "A has reserved but not applied",
        );

        let disposition = poll_pr_head(&h, &complete, None).await;

        let after_b = holds
            .get(&identity)
            .await
            .expect("streak read")
            .expect("streak exists");
        // B's sequence is read out of the ledger, not asserted from call order.
        assert_eq!(
            after_b.next_poll_sequence, 2,
            "B reserved the sequence after A's",
        );
        assert_eq!(
            after_b.last_applied_poll_sequence, 2,
            "B applied, so the watermark is B's sequence — above A's",
        );
        assert_eq!(
            after_b.poll_count, 0,
            "B's complete result cleared the count"
        );

        release_a.notify_one();
        (disposition, after_b)
    };
    let (disposition_a, (disposition_b, after_b)) = tokio::join!(a, b);

    assert_eq!(
        disposition_b.complete_empty(),
        Some(CiCompleteEmptyRoute::PrHeadProceed),
        "B is the authoritative, complete observation",
    );
    // A is absorbed — and it does NOT fall through to legacy remediation either.
    assert!(disposition_a.is_routed());
    assert_eq!(disposition_a.complete_empty(), None);

    let after_a = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(
        after_a.poll_count, 0,
        "the delayed incomplete poll must not restart the streak B cleared",
    );
    assert_eq!(
        after_a.last_applied_poll_sequence, after_b.last_applied_poll_sequence,
        "a superseded observation moves no watermark",
    );
    assert!(!after_a.has_escalated());
    assert_eq!(after_a.next_poll_sequence, 2, "and reserves nothing new");

    // The durable record that the ordering rule fired. This is the assertion the
    // sequence-comparison mutation has to break: with the guard gone, A's
    // observation is not marked superseded — it counts.
    assert_eq!(
        observations_marked(&h.db, "superseded_observation").await,
        1,
        "the overtaken observation must be recorded as superseded",
    );
    assert_eq!(observations_marked(&h.db, "applied_incomplete").await, 0);
    assert_eq!(observations_marked(&h.db, "applied_complete").await, 1);
    assert_eq!(observation_rows(&h.db).await, 2);

    // Nothing at all was recreated.
    let space = hold_negative_space(&h, &parked).await;
    assert_eq!(space.route_rows, 0, "no route may be recreated");
    assert_eq!(space.tier2_leases, 0, "no lease may be recreated");
    assert_eq!(space.lead_sessions, 0, "no Lead session may be recreated");
    assert_eq!(space.worker_attempts, 0, "no worker may be dispatched");
    assert_eq!(space.tier1_charges, 0, "no charge may be consumed");
    assert_eq!(space.provider_mutations, 0, "no provider mutation");
    assert_eq!(
        complete.calls().mutations(),
        0,
        "and none from the complete poll either",
    );
}

/// A delayed poll carrying **complete causal** evidence, overtaken by a newer
/// complete one, routes nothing.
///
/// # Why the fixture above does not cover this
///
/// [`delayed_incomplete_after_newer_complete_is_noop`] drives the real lane,
/// but it gives poll A *recoverably incomplete* evidence — which classifies to
/// `Held` whatever the ledger says. So its negative-space assertions read zero
/// either way, and the early return they are supposed to be witnessing can be
/// deleted with every one of them still green:
///
/// ```text
/// if let Some(absorbed) = self.settle_hold(..).await { return absorbed; }
/// ```
///
/// Without that line in `apply_and_drive`, a `Superseded`, `IdentityAdvanced`,
/// `Escalated` or `LedgerUnavailable` observation falls straight through to
/// `drive_lane` — so a stale-head or already-adjudicated poll routes, charges,
/// and calls the provider on evidence the ordering contract has already ruled
/// out. Poll A here carries evidence that *does* route, so the negative space
/// is zero only because the absorption happened.
///
/// The control at the end is the vacuity guard, and it is the part that makes
/// the zeros mean something: the identical evidence, applied authoritatively,
/// opens exactly one route, one lease and one Lead session. If `causal_check`
/// ever stopped being causal — or the lane stopped reaching Tier 2 — the
/// control fails rather than the fixture silently asserting an empty negative
/// space for an unrelated reason.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete the `settle_hold` early return from `apply_and_drive`: A's
///     superseded observation drives the lane, and `route_rows`,
///     `tier2_leases` and `lead_sessions` all read 1 instead of 0.
/// (b) Weaken it to `if matches!(absorbed, CiLaneDisposition::Routed) {}` — i.e.
///     compute the absorption and discard it: identical failure.
/// (c) Make `apply_poll` stop comparing against `last_applied_poll_sequence`:
///     A applies rather than being superseded, so the
///     `superseded_observation` assertion fails first and the route assertions
///     follow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_causal_poll_after_a_newer_complete_one_routes_nothing() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let holds = h.actor.ci_holds();
    let baseline = hold_negative_space(&h, &FakeProvider::default()).await;

    // Poll A: authoritatively COMPLETE and causal — evidence that routes.
    let (parked, release_a) = FakeProvider::parked_enumeration();
    parked.set_check_runs(vec![causal_check("Quality Gate / test", 4141)]);

    // Poll B: complete and empty. It applies first and advances the watermark.
    let complete = FakeProvider::default();

    let a = poll_pr_head(&h, &parked, None);
    let b = async {
        let reserved_by_a = wait_for_reservations(&holds, &identity, 1).await;
        assert_eq!(
            reserved_by_a.next_poll_sequence, 1,
            "A reserved first, and the ledger says so",
        );
        assert_eq!(reserved_by_a.last_applied_poll_sequence, 0);

        let disposition = poll_pr_head(&h, &complete, None).await;
        let after_b = holds
            .get(&identity)
            .await
            .expect("streak read")
            .expect("streak exists");
        assert_eq!(
            after_b.last_applied_poll_sequence, 2,
            "B applied above A's reserved sequence, read from the ledger",
        );

        release_a.notify_one();
        disposition
    };
    let (disposition_a, disposition_b) = tokio::join!(a, b);

    assert_eq!(
        disposition_b.complete_empty(),
        Some(CiCompleteEmptyRoute::PrHeadProceed),
        "B is the authoritative, complete observation",
    );
    assert!(
        disposition_a.is_routed(),
        "A is absorbed by the ledger, and must NOT fall through to legacy \
         remediation either",
    );
    assert_eq!(disposition_a.complete_empty(), None);

    // Vacuity, ledger side: A was genuinely SUPERSEDED — not held, not applied.
    // That is the disposition the early return has to translate into "stop".
    assert_eq!(
        observations_marked(&h.db, "superseded_observation").await,
        1,
        "A's observation must be recorded superseded, or this fixture is \
         witnessing some other absorption",
    );
    assert_eq!(observations_marked(&h.db, "applied_incomplete").await, 0);
    assert_eq!(observations_marked(&h.db, "applied_complete").await, 1);

    // ── The negative space: a superseded poll routes nothing ────────────────
    let space = hold_negative_space(&h, &parked).await;
    assert_eq!(
        space.route_rows, 0,
        "a superseded observation must not open a route row",
    );
    assert_eq!(space.tier2_leases, 0, "nor a Tier-2 lease");
    assert_eq!(space.lead_sessions, 0, "nor a Lead adjudication");
    assert_eq!(space.tier1_charges, 0, "nor consume a Tier-1 charge");
    assert_eq!(space.provider_mutations, 0, "nor call the provider");
    assert_eq!(
        space.worker_attempts, baseline.worker_attempts,
        "nor dispatch a worker",
    );
    assert_eq!(
        space.task_status, baseline.task_status,
        "nor move the board out from under the head that is actually current",
    );

    // ── Vacuity, evidence side: the SAME evidence, applied, DOES route ──────
    //
    // Without this the zeros above would be satisfied by evidence that could
    // never have routed at all, which is exactly how the incomplete-evidence
    // fixture next door came to assert nothing.
    let control = FakeProvider::default();
    control.set_check_runs(vec![causal_check("Quality Gate / test", 4141)]);
    let disposition_c = poll_pr_head(&h, &control, None).await;
    assert!(disposition_c.is_routed());

    let routed = hold_negative_space(&h, &control).await;
    assert_eq!(
        routed.route_rows, 1,
        "the evidence poll A carried is route-creating when it is authoritative",
    );
    assert_eq!(routed.tier2_leases, 1, "and it opens one adjudication");
    assert_eq!(routed.lead_sessions, 1, "which becomes one Lead session");
    assert_eq!(
        routed.provider_mutations, 0,
        "a complete causal failure adjudicates rather than re-running",
    );
}

/// A genuinely newer incomplete poll starts a fresh streak at one.
///
/// The mirror image of the fixture above, and the reason the ordering rule is a
/// comparison rather than a latch: poll C is *newer* than the complete poll B,
/// so it must count — from zero, because B cleared the streak, and not from the
/// count the polls before B had accumulated.
#[tokio::test]
async fn newer_incomplete_after_complete_starts_at_one() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let holds = h.actor.ci_holds();
    let baseline = hold_negative_space(&h, &FakeProvider::default()).await;

    // Two incomplete polls, then a complete one: the streak is at zero with a
    // watermark of 3.
    for _ in 0..2 {
        let p = FakeProvider::default();
        poll_pr_head(&h, &p, Some(CheckSetIncompleteReason::PageFetchFailed)).await;
    }
    let complete = FakeProvider::default();
    poll_pr_head(&h, &complete, None).await;
    let after_b = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(after_b.poll_count, 0);
    assert_eq!(after_b.last_applied_poll_sequence, 3);

    // Poll C: reserved after B, so it is authoritative.
    let c = FakeProvider::default();
    let disposition = poll_pr_head(&h, &c, Some(CheckSetIncompleteReason::ShortRead)).await;
    assert!(disposition.is_routed());
    assert_eq!(disposition.complete_empty(), None);

    let after_c = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(
        after_c.poll_count, 1,
        "a genuinely newer incomplete poll starts a FRESH streak rather than \
         resuming the two polls before the reset",
    );
    assert_eq!(
        after_c.last_applied_poll_sequence, 4,
        "and its sequence is above B's, read from the ledger",
    );
    assert!(after_c.last_applied_poll_sequence > after_b.last_applied_poll_sequence);
    assert!(!after_c.has_escalated());

    // The complete poll in the middle recorded a `Passing` snapshot, which is
    // its own compatibility path and not a remediation effect; everything the
    // hold is forbidden to buy is still absent.
    let after = hold_negative_space(&h, &c).await;
    assert_eq!(after.route_rows, 0);
    assert_eq!(after.tier2_leases, 0);
    assert_eq!(after.lead_sessions, 0);
    assert_eq!(after.tier1_charges, 0);
    assert_eq!(after.provider_mutations, 0);
    assert_eq!(baseline.worker_attempts, after.worker_attempts);
    assert_eq!(baseline.task_status, after.task_status);
}

/// A result whose identity advanced between reserve and apply is a no-op.
///
/// # Why this drives the coordinator orchestration and not the lane wrapper
///
/// `apply_poll`'s first check compares the identity the poll *reserved* against
/// the identity observed at apply time, and `CoordinatorActor::apply_ci_hold_poll`
/// takes those as two parameters. Both lane wrappers currently pass the same
/// value for both (`apply_and_drive(&poll, &hold_identity, …)`), so the branch
/// is unreachable from `route_pr_head_ci_evidence` today — reported alongside
/// this change rather than papered over. Driving the production orchestration
/// directly is what lets the fixture witness the check that exists instead of
/// asserting one that does not.
#[tokio::test]
async fn head_advance_clears_hold_streak() {
    let h = lane_harness().await;
    let holds = h.actor.ci_holds();
    let baseline = hold_negative_space(&h, &FakeProvider::default()).await;
    let head_a = pr_head_hold_identity(&h.task_id);
    let head_b = djinn_db::CiHoldIdentity {
        pr_head_sha: MOVED_HEAD.to_owned(),
        ..head_a.clone()
    };

    // One ordinary incomplete poll on head A, so there is a count to protect.
    let seeded = h
        .actor
        .reserve_ci_hold_poll(head_a.clone())
        .await
        .expect("reservation");
    let seeded_outcome = h
        .actor
        .apply_ci_hold_poll(
            &seeded,
            &head_a,
            false,
            CiOriginState::PrDraft,
            crate::pr_poller::ci_routing::CiIncompleteReason::EnumerationPageFailed,
            &h.task_id,
        )
        .await;
    assert_eq!(
        seeded_outcome,
        crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(
            crate::pr_poller::ci_hold::CiHoldAbsorption::Held { poll_count: 1 }
        ),
    );

    // A second poll reserves against head A — and the head moves before it can
    // apply.
    let stranded = h
        .actor
        .reserve_ci_hold_poll(head_a.clone())
        .await
        .expect("reservation");
    assert_eq!(stranded.sequence(), 2, "reserved against head A");

    let outcome = h
        .actor
        .apply_ci_hold_poll(
            &stranded,
            &head_b,
            false,
            CiOriginState::PrDraft,
            crate::pr_poller::ci_routing::CiIncompleteReason::EnumerationPageFailed,
            &h.task_id,
        )
        .await;
    assert_eq!(
        outcome,
        crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(
            crate::pr_poller::ci_hold::CiHoldAbsorption::IdentityAdvanced
        ),
    );

    let a_after = holds
        .get(&head_a)
        .await
        .expect("streak read")
        .expect("head A's streak still exists");
    assert_eq!(
        a_after.poll_count, 1,
        "the old head's count must not move: escalating it would open a \
         diagnose route for a head nobody is on any more",
    );
    assert_eq!(
        a_after.last_applied_poll_sequence, 1,
        "and no watermark moves for a head that is no longer current",
    );
    assert!(!a_after.has_escalated());
    assert_eq!(observations_marked(&h.db, "identity_advanced").await, 1);
    assert!(
        holds.get(&head_b).await.expect("streak read").is_none(),
        "an identity-advanced no-op creates nothing for the new head either",
    );

    // A fresh poll on head B starts its own streak at one.
    let fresh = h
        .actor
        .reserve_ci_hold_poll(head_b.clone())
        .await
        .expect("reservation");
    let fresh_outcome = h
        .actor
        .apply_ci_hold_poll(
            &fresh,
            &head_b,
            false,
            CiOriginState::PrDraft,
            crate::pr_poller::ci_routing::CiIncompleteReason::EnumerationPageFailed,
            &h.task_id,
        )
        .await;
    assert_eq!(
        fresh_outcome,
        crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(
            crate::pr_poller::ci_hold::CiHoldAbsorption::Held { poll_count: 1 }
        ),
    );
    let b_after = holds
        .get(&head_b)
        .await
        .expect("streak read")
        .expect("head B's streak");
    assert_eq!(b_after.poll_count, 1, "head B starts at one");
    assert_eq!(
        b_after.next_poll_sequence, 1,
        "head B's sequence space is its own",
    );
    assert_eq!(
        holds
            .get(&head_a)
            .await
            .expect("streak read")
            .expect("head A")
            .poll_count,
        1,
        "and head A's count is still untouched",
    );

    let after = hold_negative_space(&h, &FakeProvider::default()).await;
    assert_hold_is_free(&baseline, &after, "an identity-advanced apply");
}

/// Eleven real polls, a restart, then two racing pollers: exactly one
/// escalation at twelve.
///
/// # Why the streak is driven rather than seeded
///
/// A `poll_count = 11` written by a raw `UPDATE` proves nothing about the
/// mechanism that produces it — it tests the bound against a number the fixture
/// invented. All eleven polls here go through `route_pr_head_ci_evidence`, so
/// the streak is produced by the thing being tested.
///
/// # Why the restart matters
///
/// The second actor is a fresh `CoordinatorActor` over the same database, which
/// is what a coordinator restart is. It re-reads the streak, the watermark and
/// the escalation marker from the ledger; nothing about the bound lives in
/// process memory.
///
/// # Why the race is real
///
/// Both racers park inside their provider enumeration until the ledger shows
/// both sequences reserved, so they genuinely overlap. Which of them applies
/// first is not decided here — and both orders are correct:
///
/// * lower sequence first → it escalates at 12, and the higher one finds
///   `escalated_at` already set (`AlreadyEscalated`);
/// * higher sequence first → it escalates at 12, and the lower one is below the
///   watermark (`Superseded`).
///
/// Either way there is exactly one `escalated_at`, one run-absent route row, one
/// open Tier-2 lease, and one Lead adjudication — bound to that route, because
/// an arbitration row that exists and a route that names it are different
/// claims, and only the second one `unapplied_lead_results` can read.
///
/// This is the escalating poll's own dispatch, where the lease id the payload
/// minted and the one the row stores are the same value: this poll is the one
/// that inserted the row. The other two entries into `dispatch_escalated_hold`
/// — a conflicting insert and the `AlreadyEscalated` re-drive — cannot rely on
/// that, and are covered by
/// `an_escalated_hold_redrive_binds_the_lease_its_route_holds`.
#[tokio::test]
async fn count_eleven_race_escalates_once_at_twelve() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let holds = h.actor.ci_holds();
    let baseline = hold_negative_space(&h, &FakeProvider::default()).await;

    for expected in 1..=(djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS - 1) {
        let p = FakeProvider::default();
        let disposition =
            poll_pr_head(&h, &p, Some(CheckSetIncompleteReason::PageFetchFailed)).await;
        assert!(disposition.is_routed());
        let streak = holds
            .get(&identity)
            .await
            .expect("streak read")
            .expect("streak exists");
        assert_eq!(
            streak.poll_count, expected,
            "each real poll advances the streak by exactly one",
        );
        assert!(
            !streak.has_escalated(),
            "poll {expected} is below the bound of {}",
            djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS,
        );
    }
    let seeded = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(seeded.poll_count, 11);
    assert_eq!(seeded.last_applied_poll_sequence, 11);
    let eleven = hold_negative_space(&h, &FakeProvider::default()).await;
    assert_hold_is_free(&baseline, &eleven, "eleven consecutive incomplete polls");

    // ── Restart: a new actor and a new repository over the same database ────
    let restarted = crate::actor::actor_with_test_db(h.db.clone());
    let restarted_holds = restarted.ci_holds();
    assert_eq!(
        restarted_holds
            .get(&identity)
            .await
            .expect("streak read")
            .expect("the streak survives the restart")
            .poll_count,
        11,
        "the bound is durable, not a number the process was holding",
    );

    // ── Two pollers, genuinely overlapping ─────────────────────────────────
    let (first, release_first) = FakeProvider::parked_enumeration();
    first.set_check_runs_incomplete(CheckSetIncompleteReason::PageFetchFailed);
    let (second, release_second) = FakeProvider::parked_enumeration();
    second.set_check_runs_incomplete(CheckSetIncompleteReason::PageFetchFailed);

    let race_one = restarted.route_pr_head_ci_evidence(
        &first,
        &h.task_id,
        "task-short",
        "acme",
        "widgets",
        PR as u64,
        HEAD,
        failing_filter,
    );
    let race_two = restarted.route_pr_head_ci_evidence(
        &second,
        &h.task_id,
        "task-short",
        "acme",
        "widgets",
        PR as u64,
        HEAD,
        failing_filter,
    );
    let starter = async {
        // Both sequences reserved before either result is applied. That is what
        // makes this a race rather than two sequential polls.
        let streak = wait_for_reservations(&restarted_holds, &identity, 13).await;
        assert_eq!(streak.next_poll_sequence, 13);
        assert_eq!(
            streak.last_applied_poll_sequence, 11,
            "neither racer has applied yet",
        );
        assert_eq!(streak.poll_count, 11);
        release_first.notify_one();
        release_second.notify_one();
    };
    let (one, two, ()) = tokio::join!(race_one, race_two, starter);
    assert!(one.is_routed());
    assert!(two.is_routed());

    // ── Exactly one atomic transition at twelve ────────────────────────────
    let escalated = restarted_holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert!(
        escalated.has_escalated(),
        "the twelfth authoritative incomplete poll must escalate",
    );
    assert_eq!(
        escalated.poll_count,
        djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS,
        "the count stops at the bound: the loser must not increment past it",
    );
    assert_eq!(
        escalated.last_applied_poll_sequence, 13,
        "the winner of the race is the highest sequence either way",
    );

    assert_eq!(
        observations_marked(&h.db, "escalated").await,
        1,
        "EXACTLY ONE observation may be the escalating one",
    );
    let superseded = observations_marked(&h.db, "superseded_observation").await;
    let already = observations_marked(&h.db, "applied_incomplete").await
        - (djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS - 1);
    assert_eq!(
        superseded + already,
        1,
        "the loser is exactly one observation, and is either superseded or \
         already-escalated: superseded={superseded}, already_escalated={already}",
    );
    assert_eq!(observation_rows(&h.db).await, 13);

    // One route, one lease, one adjudication — counted, not named.
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "one diagnose-only run-absent route, not two",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
        "one open Tier-2 lease, not two",
    );
    assert_eq!(
        arbitration_rows(&h.db).await,
        1,
        "one Lead adjudication, not two",
    );

    // The route is keyed on the run-absent identity, and it is diagnose-only.
    let run_absent = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let subject = CiRouteSubject::task(h.task_id.clone());
    let row = CiRouteAttemptRepository::new(h.db.clone())
        .get(
            &subject,
            &provider_action_key(&subject, &run_absent, CiAction::AskLead),
        )
        .await
        .expect("route read")
        .expect("the escalation route is keyed on the run-absent identity");
    assert_eq!(row.identity.run_id, None, "absence, never a sentinel");
    assert_eq!(row.action, CiAction::AskLead);

    // …and the adjudication is BOUND to that route rather than merely coincident
    // with it. Counting `task_arbitrations` proves the escalation wrote *an*
    // arbitration; only `lead_session_id` proves it wrote the one this route is
    // adjudicated under, which is what `unapplied_lead_results` reads. Exactly
    // one attach happens however the race lands: whichever poll creates the
    // arbitration is the one that binds, and the other finds it unconsumed and
    // answers `AlreadyInFlight` before reaching the attach.
    let adjudication =
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(h.db.clone())
            .get_latest_for_task(&h.task_id)
            .await
            .expect("arbitration read")
            .expect("the escalation dispatches one Lead session");
    assert_eq!(
        row.lead_session_count, 1,
        "one Lead session attached to the escalated route, not zero and not two",
    );
    assert_eq!(
        row.lead_session_id.as_deref(),
        Some(adjudication.id.as_str()),
        "and the route names the arbitration row adjudicating it",
    );

    // And an escalation still buys no provider call, no worker, and no charge.
    assert_eq!(first.calls().mutations(), 0);
    assert_eq!(second.calls().mutations(), 0);
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        baseline.worker_attempts,
        "an escalation asks Lead; it does not dispatch a worker",
    );
    assert_eq!(
        charged_budget_counters(&h.db).await,
        0,
        "`ask_lead` consumes no Tier-1 charge",
    );
}

/// An escalated hold's re-drive binds the lease its ROW holds, not the one its
/// payload minted.
///
/// # Why the sibling twelve-poll fixture cannot see this
///
/// It asserts `arbitration_rows == 1` and never reads `lead_session_*`, and the
/// escalating poll there is also the one that INSERTS the route — so the id it
/// minted and the id the row stores are the same value by accident of timing,
/// and every attach matches its fence. Both other ways into
/// `dispatch_escalated_hold` break that coincidence:
///
/// * `escalation_route` mints `tier2_lease_id` fresh on every call while
///   `insert_escalation_route` writes it `ON CONFLICT DO NOTHING`, so
///   `Escalated { route_inserted: false }` dispatches against an id no row
///   holds; and
/// * the `AlreadyEscalated` re-drive — the recovery for "the lease committed
///   but the dispatch did not" — always mints its id after the row was written.
///
/// This fixture drives the second one, because it is the case where the
/// re-drive is the poll that finally creates the arbitration: escalate with the
/// dispatch unable to land, then poll again and require that the Lead session
/// the re-drive dispatches is bound to the durable lease.
///
/// The escalating poll is handed a task id that names no row, which is exactly
/// `dispatch_escalated_hold`'s own `Ok(None) => return` arm and exactly the
/// production shape its `AlreadyEscalated` comment describes: the ledger's
/// transaction commits the route and its open lease, and the board-side half —
/// which cannot be in that transaction — does not happen. Nothing else about
/// the fixture is arranged; the streak, the route, the lease and both
/// dispatches are the production ones.
///
/// # Why the binding, and not just the arbitration row
///
/// The stored lease id fences BOTH ends of the adjudication.
/// `attach_lead_session` writes `lead_session_id` only when the id matches, and
/// the supervisor hands the directive's `tier2_lease_id` to
/// `resolve_tier2_lease`, which is fenced on the same column. A Lead session
/// dispatched under a minted id therefore looks completely successful — row,
/// directive, board transition — while `unapplied_lead_results` reads
/// "quiescent" and the result, whenever it arrives, can never be applied. The
/// escalation would spend a session and stay wedged, which is the failure the
/// re-drive exists to prevent.
///
/// NAMED FAILING MUTATIONS.
/// (a) Restore `tier2_lease_id: escalation.tier2_lease_id.clone()` in the
///     handoff (i.e. drop the read-back): the re-drive's minted id names no
///     row, so `attach_lead_session` misses its fence — `lead_session_count`
///     stays `0` and `lead_session_id` NULL — and the directive assertion fails
///     with the minted id in place of the stored one. Every other observable
///     (arbitration row, directive block, board state) is unchanged, which is
///     why nothing else here catches it.
/// (b) Delete the `dispatch_escalated_hold` call from the `AlreadyEscalated`
///     arm: no arbitration row exists at all, so the `expect` on the
///     arbitration fails. The route keeps an open lease with nothing
///     adjudicating it, and the lease is head-scoped, so it also blocks every
///     other Tier-2 route for that head.
/// (c) Drop the `holds_open_tier2_lease()` guard, or read the lease id without
///     requiring the lease to be open: phase three's re-drive over a RESOLVED
///     lease dispatches a second Lead session, opening a second hold cycle
///     whose result `resolve_tier2_lease` can never apply — `arbitration_rows`
///     becomes 2.
/// (d) Bind the row's id but leave the directive on the payload's (two sources
///     for one lease): the directive assertion fails alone, and the supervisor
///     would resolve nothing.
#[tokio::test]
async fn an_escalated_hold_redrive_binds_the_lease_its_route_holds() {
    let h = lane_harness().await;
    let identity = pr_head_hold_identity(&h.task_id);
    let hold_reason = crate::pr_poller::ci_routing::CiIncompleteReason::EnumerationPageFailed;
    // Named once: every poll below is the same logical lane observation, and a
    // second reason would start a different argument than the one under test.
    let poll_once = async |task_id: &str| {
        let poll = h
            .actor
            .reserve_ci_hold_poll(identity.clone())
            .await
            .expect("reservation");
        h.actor
            .apply_ci_hold_poll(
                &poll,
                &identity,
                false,
                CiOriginState::PrDraft,
                hold_reason,
                task_id,
            )
            .await
    };

    // ── Phase one: the lease commits and the dispatch does not ──────────────
    let orphan_task = uuid::Uuid::now_v7().to_string();
    for expected in 1..=djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS {
        let absorbed = if expected == djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS {
            crate::pr_poller::ci_hold::CiHoldAbsorption::Escalated {
                route_inserted: true,
            }
        } else {
            crate::pr_poller::ci_hold::CiHoldAbsorption::Held {
                poll_count: expected,
            }
        };
        assert_eq!(
            poll_once(&orphan_task).await,
            crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(absorbed),
            "poll {expected} of {}",
            djinn_db::CI_INCOMPLETE_HOLD_MAX_POLLS,
        );
    }

    let subject = CiRouteSubject::task(h.task_id.clone());
    let run_absent = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let routes = CiRouteAttemptRepository::new(h.db.clone());
    let key = provider_action_key(&subject, &run_absent, CiAction::AskLead);
    let escalated = routes
        .get(&subject, &key)
        .await
        .expect("route read")
        .expect("the escalating transaction writes the run-absent route");
    let lease_id = escalated
        .tier2_lease_id
        .clone()
        .expect("the escalation opens the lease on the INSERT itself");
    assert!(
        escalated.holds_open_tier2_lease(),
        "precondition: the durable lease is open and unadjudicated",
    );
    assert_eq!(
        escalated.lead_session_count, 0,
        "precondition: the dispatch did NOT land — otherwise phase two would be \
         witnessing a binding that already existed",
    );
    assert_eq!(escalated.lead_session_id, None);
    assert_eq!(
        arbitration_rows(&h.db).await,
        0,
        "precondition: no Lead session was dispatched for the escalation",
    );
    assert_eq!(
        djinn_db::test_support::task_status_for_test(&h.db, &h.task_id).await,
        "pr_review",
        "precondition: and the board never entered the Lead lane",
    );

    // ── Phase two: the re-drive is the poll that dispatches ─────────────────
    assert_eq!(
        poll_once(&h.task_id).await,
        crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(
            crate::pr_poller::ci_hold::CiHoldAbsorption::AlreadyEscalated
        ),
        "an escalated streak absorbs every later poll",
    );

    let arbitrations =
        djinn_db::repositories::task_arbitration::TaskArbitrationRepository::new(h.db.clone());
    let arbitration = arbitrations
        .get_latest_for_task(&h.task_id)
        .await
        .expect("arbitration read")
        .expect("the re-drive dispatches the Lead session the escalation could not");
    let bound = routes
        .get(&subject, &key)
        .await
        .expect("route read")
        .expect("the route is still the run-absent one");
    assert_eq!(
        bound.tier2_lease_id.as_deref(),
        Some(lease_id.as_str()),
        "the re-drive opens no second lease, so the only id it may bind is the \
         one the row already held",
    );
    assert_eq!(
        bound.lead_session_count, 1,
        "the re-drive's Lead session must be attached to the route it adjudicates",
    );
    assert_eq!(
        bound.lead_session_id.as_deref(),
        Some(arbitration.id.as_str()),
        "and it must name the arbitration row this route is adjudicated under",
    );
    let directive = arbitration
        .directive
        .clone()
        .expect("the dispatch writes the directive the Lead session reads");
    assert_eq!(
        directive["ci_route"]["tier2_lease_id"].as_str(),
        Some(lease_id.as_str()),
        "the supervisor hands this id to `resolve_tier2_lease`, which is fenced \
         on the STORED lease: a minted id makes the adjudication unappliable",
    );
    assert_eq!(
        directive["ci_route"]["provider_action_key"].as_str(),
        Some(key.as_str()),
        "and the directive names the route the lease belongs to",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "one run-absent route, not one per poll",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        1,
        "and one Tier-2 lease",
    );
    assert_eq!(arbitration_rows(&h.db).await, 1, "and one Lead session");
    assert_eq!(
        djinn_db::test_support::task_status_for_test(&h.db, &h.task_id).await,
        "needs_lead_intervention",
        "the re-drive escalates the board as well as binding the route",
    );

    // ── Phase three: a resolved lease ends this route's trip to Tier 2 ──────
    //
    // The adjudication is applied and consumed, which is what a Lead result
    // landing looks like. Later incomplete polls keep arriving — the head's CI
    // is still incomplete — and every one of them lands on `AlreadyEscalated`
    // again. Dispatching there would open a SECOND hold cycle against a lease
    // that is closed, so its result could never be applied: a session spent to
    // be discarded.
    assert!(
        routes
            .resolve_tier2_lease(
                &subject,
                &key,
                &lease_id,
                &run_absent,
                &djinn_db::CiTier2Resolution::diagnose(
                    djinn_db::CiDiagnosticReason::EvidenceIncomplete
                ),
            )
            .await
            .expect("resolve"),
        "precondition: the adjudication applies and closes the lease",
    );
    assert!(
        arbitrations
            .mark_consumed(&h.task_id, arbitration.hold_cycle)
            .await
            .expect("consume"),
        "precondition: and its arbitration is consumed, so nothing is in flight",
    );

    assert_eq!(
        poll_once(&h.task_id).await,
        crate::pr_poller::ci_hold::CiHoldDisposition::Absorbed(
            crate::pr_poller::ci_hold::CiHoldAbsorption::AlreadyEscalated
        ),
    );
    assert_eq!(
        arbitration_rows(&h.db).await,
        1,
        "a route routes to Tier 2 at most once, ever: a re-drive over a resolved \
         lease must dispatch NOTHING",
    );
    assert_eq!(
        routes
            .get(&subject, &key)
            .await
            .expect("route read")
            .expect("route")
            .lead_session_count,
        1,
        "and it attaches no second session to the route it can no longer resolve",
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
        "none of this dispatches a worker",
    );
}

/// One logical poll keeps its sequence, however many times it is retried.
///
/// A retry between reserve and apply — a serialization failure, a pool hiccup, a
/// process restart mid-poll — must read back the sequence it was already
/// assigned. If it reserved a second one it would look like a *newer* poll and
/// would supersede its own earlier self, which is how a crash-loop silently eats
/// a streak.
#[tokio::test]
async fn hold_observation_replay_preserves_its_sequence() {
    let h = lane_harness().await;
    let holds = h.actor.ci_holds();
    let baseline = hold_negative_space(&h, &FakeProvider::default()).await;
    let identity = pr_head_hold_identity(&h.task_id);
    let poll_id = uuid::Uuid::now_v7().to_string();

    let first = holds
        .reserve_poll(&identity, &poll_id)
        .await
        .expect("first reservation");
    assert!(!first.replayed, "the first reservation is not a replay");
    assert_eq!(first.poll_sequence, 1);

    let second = holds
        .reserve_poll(&identity, &poll_id)
        .await
        .expect("replayed reservation");
    assert!(
        second.replayed,
        "the same logical poll id must be recognised as a replay",
    );
    assert_eq!(
        second.poll_sequence, first.poll_sequence,
        "a replay reads back its own sequence rather than reserving another",
    );
    assert_eq!(second.streak_id, first.streak_id);

    let streak = holds
        .get(&identity)
        .await
        .expect("streak read")
        .expect("streak exists");
    assert_eq!(
        streak.next_poll_sequence, 1,
        "and the reservation counter advanced EXACTLY ONCE for two calls",
    );
    assert_eq!(
        observation_rows(&h.db).await,
        1,
        "one logical poll is one observation row",
    );

    // A different logical poll on the same identity does get the next sequence,
    // so the replay is keyed on the poll id and not on the identity.
    let other = holds
        .reserve_poll(&identity, &uuid::Uuid::now_v7().to_string())
        .await
        .expect("second logical poll");
    assert!(!other.replayed);
    assert_eq!(other.poll_sequence, 2);

    let after = hold_negative_space(&h, &FakeProvider::default()).await;
    assert_hold_is_free(&baseline, &after, "two reservations and no apply");
}

/// Two irrecoverable reasons on one PR head share ONE run-absent route row.
///
/// The executor-level twin of the key-derivation fixture in the sibling
/// classifier suite: this one drives the real lane end to end against real
/// Postgres and **counts rows in `ci_route_attempts`**, which is the claim the
/// `NULLS NOT DISTINCT` unique index actually has to satisfy.
///
/// The two reasons are produced by different code paths on purpose:
///
/// * `CheckEnumerationUnavailable` from a `MaxPagesTruncated` enumeration
///   verdict, reached in `capture_pr_head_evidence`'s first branch; and
/// * `RunAttributionUnavailable` from a blocking check belonging to no nameable
///   Actions run, reached four branches later.
///
/// Both are irrecoverable, so both take the diagnose-only route under `run_id`
/// NULL — and the second must find the first's row rather than opening a second
/// lease and spending a second Lead session.
#[tokio::test]
async fn irrecoverable_reasons_share_one_run_absent_route_row() {
    let h = lane_harness().await;

    // Reason one: the enumeration hit `MAX_PAGES`.
    let truncated = FakeProvider::default();
    let first = poll_pr_head(
        &h,
        &truncated,
        Some(CheckSetIncompleteReason::MaxPagesTruncated),
    )
    .await;
    assert!(first.is_routed());
    assert_eq!(
        first.complete_empty(),
        None,
        "an irrecoverable enumeration is not the no-CI path",
    );

    let subject = CiRouteSubject::task(h.task_id.clone());
    let run_absent = CiEvidenceIdentity {
        lane: CiLane::PrHead,
        pr_number: PR,
        pr_head_sha: HEAD.to_owned(),
        run_id: None,
        run_head_sha: HEAD.to_owned(),
        dequeue_id: None,
    };
    let routes = CiRouteAttemptRepository::new(h.db.clone());
    let key = provider_action_key(&subject, &run_absent, CiAction::AskLead);
    let row = routes
        .get(&subject, &key)
        .await
        .expect("route read")
        .expect("an irrecoverable reason takes one diagnose-only route");
    assert_eq!(row.identity.run_id, None, "absence, never a sentinel");
    assert_eq!(row.action, CiAction::AskLead);
    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
    );
    let leases_after_first =
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await;
    let sessions_after_first = arbitration_rows(&h.db).await;
    assert_eq!(leases_after_first, 1, "one lease for the first reason");

    // Reason two: a blocking check attributable to no Actions run, on the same
    // PR head, reached through a different branch entirely.
    let orphaned = FakeProvider::default();
    let mut orphan = causal_check("External / policy", 979);
    orphan.run_id = None;
    orphan.html_url = "https://example.test/checks/1".to_owned();
    orphaned.set_check_runs(vec![orphan]);
    let second = poll_pr_head(&h, &orphaned, None).await;
    assert!(second.is_routed());

    assert_eq!(
        djinn_db::test_support::ci_route_row_count_for_test(&h.db, &h.task_id).await,
        1,
        "a later irrecoverable reason adds evidence to the SAME run-absent row; \
         it does not open a second one",
    );
    assert_eq!(
        djinn_db::test_support::ci_route_lease_count_for_test(&h.db, &h.task_id).await,
        leases_after_first,
        "and it opens no second Tier-2 lease",
    );
    assert_eq!(
        arbitration_rows(&h.db).await,
        sessions_after_first,
        "and spends no second Lead session",
    );
    assert_eq!(truncated.calls().mutations(), 0);
    assert_eq!(orphaned.calls().mutations(), 0);
    assert_eq!(
        charged_budget_counters(&h.db).await,
        0,
        "a diagnose-only route consumes no Tier-1 charge",
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&h.db, &h.task_id).await,
        0,
        "and dispatches no worker",
    );
    // A hold streak that has already routed must not keep counting underneath
    // the adjudication it already has.
    assert!(
        h.actor
            .ci_holds()
            .get(&pr_head_hold_identity(&h.task_id))
            .await
            .expect("streak read")
            .is_none_or(|streak| streak.poll_count == 0),
        "an irrecoverable reason is not a hold, so it accumulates no streak",
    );
}

// ===========================================================================
// The production wiring: the call sites, not the callees (AC12)
// ===========================================================================
//
// Everything above drives `recover_calling_owners_at_startup`,
// `sweep_reserved_routes` and the two lane routers directly, which proves the
// machinery works and proves nothing about whether production reaches it. Each
// of the three fixtures below exists because deleting one production call site
// left this entire file — and the whole `djinn-coordinator` lib suite — green,
// with byte-identical counts.

/// Plant a task, its project, and one ephemeral database, for the fixtures that
/// need a real `CoordinatorActor` beside them.
///
/// Deliberately not [`fixture`]: that one owns a `FakeProvider` and a
/// `ProviderActionScope` the actor would not be using, and an actor built over
/// a *different* database than the assertions read is the exact shape of a
/// fixture that witnesses nothing.
async fn wiring_subject(label: &str) -> (Database, String, CiRouteSubject) {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let project = djinn_db::test_support::make_project(&db, std::path::Path::new(label)).await;
    let task_id = djinn_db::test_support::seed_task_row(
        &db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: &project.id,
            // `pr_draft` with no `pr_url` — the PR poller's own filter drops it
            // before `resolve_installation_client`, so driving a real tick over
            // this row never reaches `api.github.com`.
            status: "pr_draft",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    let subject = CiRouteSubject::task(task_id.clone());
    (db, task_id, subject)
}

/// A Tier-1 reservation for one evidence identity, owned by nobody yet.
async fn plant_reservation(
    routes: &CiRouteAttemptRepository,
    subject: &CiRouteSubject,
    identity: &CiEvidenceIdentity,
    fingerprint: &str,
) -> String {
    let key = provider_action_key(subject, identity, CiAction::RerunRun);
    routes
        .reserve(&CiRouteReservation {
            subject: subject.clone(),
            provider_action_key: key.clone(),
            identity: identity.clone(),
            origin_state: CiOriginState::PrDraft,
            class: djinn_db::CiClass::Inconclusive,
            action: CiAction::RerunRun,
            transient_fingerprint: fingerprint.to_owned(),
            retry_budget_key: retry_budget_key(subject, identity, fingerprint),
            head_budget_key: head_budget_key(subject, identity.pr_number, &identity.pr_head_sha),
        })
        .await
        .expect("reserve");
    key
}

async fn route_row(
    routes: &CiRouteAttemptRepository,
    subject: &CiRouteSubject,
    key: &str,
) -> CiRouteAttempt {
    routes
        .get(subject, key)
        .await
        .expect("route read")
        .expect("route row exists")
}

/// The PRODUCTION startup path is what hands off a stranded `calling` row.
///
/// `recover_ci_calling_owners_at_startup` has exactly one caller in the tree —
/// one `poll_stack::boxed(..).await` in `CoordinatorActor::run`, placed after
/// `register_coordinator_incarnation` because the handoff compare-and-sets the
/// row from the former owner to *this* incarnation and so needs this
/// incarnation's lease to exist first. Nothing in this crate ran `run`, so that
/// line could be deleted with every fixture above still green: a charged
/// `calling` row left behind by a leadership handover would then stay `calling`
/// forever, its head-scoped Tier-2 lease would never open, and the evidence
/// would be adjudicated by nobody.
///
/// So this drives the real `run()` — through
/// `CoordinatorActor::new(CoordinatorDeps::new(..))`, the production
/// constructor — with the cancellation token already fired. The `biased`
/// ordering in `run_dispatch_loop` makes the cancellation arm the one that
/// wins, so the loop breaks on its first poll and `run()` returns: the whole
/// finite startup phase executes and nothing infinite does. The `.await`
/// returning is the happens-before edge the assertions read.
///
/// The former owner is *really* drained here — registered, marked draining,
/// then stamped — because `recover_calling_owner` re-reads
/// `provider_actions_drained_at` for itself and refuses a claim it cannot
/// confirm. There is no injected liveness double in this fixture at all:
/// `run()` builds the production `CiIncarnationLiveness` and passes the
/// production `TaskRepository` head witness.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `poll_stack::boxed(|| self.recover_ci_calling_owners_at_startup()).await;`
///     from `run()`: nothing else in `run()` touches a route row — the tick
///     never fires, because the token is already cancelled — so the row stays
///     `Calling` with `terminal_outcome: None` and the first post-run assertion
///     fails.
/// (b) Move it ABOVE `register_coordinator_incarnation`: the recovering
///     incarnation has no ledger row yet, and the audit assertion below stops
///     reading a single `StartupOwnerHandoff`. This mutation is why the
///     ordering comment at the call site exists.
/// (c) Guard the recovery on anything at all: the row is eligible on every
///     axis, so any added refusal turns the recovery into a deferral and the
///     outcome assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_startup_path_hands_off_a_calling_row_left_by_a_former_incarnation() {
    let (db, task_id, subject) = wiring_subject("ci-startup-wiring").await;
    let routes = CiRouteAttemptRepository::new(db.clone());

    let actor = crate::actor::actor_with_test_db(db.clone());
    let recovering = actor.coordinator_incarnation_id.clone();
    let cancel = actor.cancel.clone();

    // The head witness `run()` passes is the poller's own durable snapshot. A
    // row whose current head cannot be witnessed is counted `unverifiable` and
    // left alone, so without this the fixture would be asserting a skip.
    actor
        .persist_ci_snapshot(
            &task_id,
            PR as u64,
            HEAD,
            djinn_core::models::CiStatus::Pending,
            Vec::new(),
            None,
            0,
            None,
        )
        .await;

    // A former incarnation that reached the END of its own shutdown contract.
    let former = uuid::Uuid::now_v7().to_string();
    assert_ne!(
        former, recovering,
        "precondition: the handoff refuses a row it already owns"
    );
    let incarnations = djinn_db::CoordinatorIncarnationRepository::new(db.clone());
    incarnations
        .register(&former)
        .await
        .expect("register the former owner");
    assert!(
        incarnations
            .mark_draining(&former)
            .await
            .expect("mark draining")
    );
    assert!(
        incarnations
            .mark_provider_actions_drained(&former)
            .await
            .expect("stamp the drain"),
        "the former owner must really have drained, or the repository refuses \
         the claim and this fixture asserts a deferral instead of a handoff"
    );

    // The charged `calling` row a leadership handover left behind.
    let checks = [inconclusive_check("Quality Gate / test", 990)];
    let blocking = refs(&checks);
    let id = pr_head_identity(990);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    let key = plant_reservation(&routes, &subject, &id, &fingerprint).await;
    routes
        .charge_and_begin_calling(&subject, &key, &former, &id)
        .await
        .expect("charge");
    djinn_db::test_support::ci_route_age_calling_for_test(&db, &subject.id, &key, 400).await;

    let before = route_row(&routes, &subject, &key).await;
    assert_eq!(
        before.action_phase,
        CiActionPhase::Calling,
        "precondition: the row is the one only a startup handoff can move"
    );
    assert_eq!(
        before.owner_incarnation_id.as_deref(),
        Some(former.as_str())
    );
    assert_eq!(before.terminal_outcome, None);
    assert!(
        routes
            .calling_recovery_audit(&subject, &key)
            .await
            .expect("calling-recovery audit")
            .is_empty(),
        "precondition: nothing has attempted a handoff yet"
    );

    // ── The production startup path, start to finish ────────────────────────
    //
    // `tokio::spawn(actor.run())` is exactly what `CoordinatorHandle::spawn`
    // does, so this is the production entry point on the production stack.
    cancel.cancel();
    tokio::time::timeout(PATIENCE, tokio::spawn(actor.run()))
        .await
        .expect("the startup phase must complete and the cancelled loop exit")
        .expect("the coordinator startup path must not panic");

    let after = route_row(&routes, &subject, &key).await;
    assert_eq!(
        after.action_phase,
        CiActionPhase::Terminal,
        "startup must ACT on the stranded row, not merely enumerate it"
    );
    assert_eq!(
        after.terminal_outcome,
        Some(CiRouteOutcome::OutcomeUnknown),
        "the row is still current, so the unknowable outcome is recorded rather \
         than guessed"
    );
    assert_eq!(
        after.owner_incarnation_id.as_deref(),
        Some(recovering.as_str()),
        "the handoff compare-and-sets ownership to the incarnation `run()` \
         registered, which is what proves the call ran inside THIS startup"
    );
    assert_eq!(
        routes
            .calling_recovery_audit(&subject, &key)
            .await
            .expect("calling-recovery audit")
            .into_iter()
            .map(|record| record.recovery_reason)
            .collect::<Vec<_>>(),
        vec![CiCallingRecoveryReason::StartupOwnerHandoff],
        "exactly one handoff, and it is the startup one"
    );

    let counts = routes
        .budget_counts(&subject, &after.retry_budget_key, &after.head_budget_key)
        .await
        .expect("budget read");
    assert_eq!(
        (counts.signature, counts.head),
        (1, 1),
        "the charge is retained across the handoff — neither replayed nor refunded"
    );
    assert_eq!(
        djinn_db::test_support::task_attempt_count_for_test(&db, &task_id).await,
        0,
        "a startup handoff dispatches no worker"
    );
}

/// An `Instant` far enough in the past that a sweep interval has elapsed.
fn a_sweep_interval_ago() -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(2 * crate::pr_poller::CI_ROUTE_SWEEP_INTERVAL)
        .expect("the monotonic clock must be older than two sweep intervals")
}

/// The PRODUCTION tick is what runs the reserved sweep and takes the rollback
/// report.
///
/// `sweep_ci_routes` has exactly one caller: the `CI_ROUTE_SWEEP_INTERVAL`
/// block in `CoordinatorActor::run_tick`. It is in turn the only caller of
/// `emit_ci_route_report` and of `record_ci_rollback_quiescence_report`.
/// Nothing in this crate ran `run_tick`, so that four-line block could be
/// deleted outright with every sweep fixture above still green — and the
/// consequence is not cosmetic. A `reserved` row whose head has moved is
/// exactly the row no poller will ever revisit (nothing polls a head that is
/// gone), so without the sweep it sits `reserved` until the process restarts;
/// and the rollback quiescence report — the single repository-checkable row the
/// proposal puts in front of a binary rollback — would exist only if an
/// operator thought to ask for it by hand, which is the difference between a
/// report and a query.
///
/// The tick is driven ONCE rather than through the loop, so the `.await`
/// returning is a happens-before edge and the assertions read a finished pass.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete the whole `if self.last_ci_route_sweep.elapsed() >=
///     CI_ROUTE_SWEEP_INTERVAL { … }` block from `run_tick`: no other pass in
///     the tick touches a route row, so the planted reservation stays
///     `Reserved` with `terminal_outcome: None` and the first post-tick
///     assertion fails; the second half then finds no rollback report either.
/// (b) Delete `self.record_ci_rollback_quiescence_report()` from
///     `sweep_ci_routes`, or put it back behind a condition: the sweep half
///     still passes and the report half fails on `None`.
/// (c) Drop the interval guard and sweep on every tick: the vacuity guard
///     between the halves — a tick taken WITHOUT backdating, which must record
///     no NEW report — fails.
/// (d) Reset `last_ci_route_sweep` before the sweep rather than after, or reset
///     it outside the block: the second half's backdate is overwritten by an
///     earlier tick and the report is never taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ticks_sweep_block_closes_a_stranded_reservation_and_takes_the_rollback_report() {
    let (db, task_id, subject) = wiring_subject("ci-sweep-wiring").await;
    let routes = CiRouteAttemptRepository::new(db.clone());

    let mut actor = crate::actor::actor_with_test_db(db.clone());
    let incarnation = actor.coordinator_incarnation_id.clone();

    // The witness reports a head that has MOVED past the reservation's, which
    // is what makes this the row no poller will revisit.
    actor
        .persist_ci_snapshot(
            &task_id,
            PR as u64,
            MOVED_HEAD,
            djinn_core::models::CiStatus::Pending,
            Vec::new(),
            None,
            0,
            None,
        )
        .await;

    let checks = [inconclusive_check("Quality Gate / test", 991)];
    let blocking = refs(&checks);
    let id = pr_head_identity(991);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    let key = plant_reservation(&routes, &subject, &id, &fingerprint).await;
    djinn_db::test_support::ci_route_age_reserved_for_test(&db, &subject.id, &key, 600).await;

    let before = route_row(&routes, &subject, &key).await;
    assert_eq!(
        before.action_phase,
        CiActionPhase::Reserved,
        "precondition: a stranded reservation, which only the sweep resolves"
    );
    assert_eq!(before.terminal_outcome, None);
    assert!(
        routes
            .latest_rollback_quiescence_report()
            .await
            .expect("report read")
            .is_none(),
        "precondition: no rollback report has ever been taken"
    );

    // ── One production tick, with the interval genuinely elapsed ────────────
    actor.last_ci_route_sweep = a_sweep_interval_ago();
    actor.drive_tick_for_test().await;

    let swept = route_row(&routes, &subject, &key).await;
    assert_eq!(
        swept.action_phase,
        CiActionPhase::Terminal,
        "the tick must SWEEP the stranded row, not merely count it"
    );
    assert_eq!(
        swept.terminal_outcome,
        Some(CiRouteOutcome::SupersededPreCall),
        "an obsolete reservation is closed pre-call"
    );
    let counts = routes
        .budget_counts(&subject, &swept.retry_budget_key, &swept.head_budget_key)
        .await
        .expect("budget read");
    assert_eq!(
        (counts.signature, counts.head),
        (0, 0),
        "and closed uncharged: the sweep holds no provider client"
    );

    // ── …and the same pass takes the quiescence report ─────────────────────
    let report = routes
        .latest_rollback_quiescence_report()
        .await
        .expect("report read")
        .expect("the sweep must record the quiescence report without being asked");
    assert_eq!(
        report.recorded_by_incarnation, incarnation,
        "the report names the incarnation whose tick took it, which is what ties \
         it to THIS actor rather than to any writer of that table"
    );
    assert_eq!(
        report.reserved_rows, 0,
        "the sweep above closed the only one"
    );
    assert_eq!(report.calling_rows, 0);
    assert_eq!(
        report.permits_rollback,
        report.recomputed_verdict(),
        "the stored verdict must agree with the function it is checked against"
    );

    // ── Vacuity: the interval is a real gate, not a formality ───────────────
    //
    // The block reset `last_ci_route_sweep` to now, so this tick must not
    // re-enter it. Without this, a sweep that ran unconditionally on every tick
    // would satisfy everything above and the half below would prove nothing
    // about the interval.
    actor.drive_tick_for_test().await;
    assert_eq!(
        routes
            .latest_rollback_quiescence_report()
            .await
            .expect("report read")
            .expect("the report taken above is still the latest")
            .id,
        report.id,
        "a tick inside the sweep interval must not run the sweep"
    );

    // ── And the report is taken on EVERY pass, not once per process ─────────
    actor.last_ci_route_sweep = a_sweep_interval_ago();
    actor.drive_tick_for_test().await;
    assert_ne!(
        routes
            .latest_rollback_quiescence_report()
            .await
            .expect("report read")
            .expect("a second sweep records a second report")
            .id,
        report.id,
        "an operator watching the counts converge needs a report per pass, not \
         a single row taken the first time the sweep ever ran"
    );
}

/// The final argument of the call whose open paren `args` starts just after.
///
/// Shared by the two call-site guards below, which both need to know *what* a
/// production call site was handed rather than merely that it exists.
fn final_argument(args: &str) -> &str {
    let mut depth = 1usize;
    let mut end = args.len();
    for (index, character) in args.char_indices() {
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
    args[..end]
        .trim()
        .trim_end_matches(',')
        .rsplit(',')
        .next()
        .unwrap_or_default()
        .trim()
}

/// One Rust source with its `//` line comments removed.
///
/// The source-level guards below match on *code*. Without this, a comment that
/// merely names the token under guard would satisfy the assertion the guard
/// exists to make — which is the failure mode a source guard is most vulnerable
/// to, and the one that would let this whole family of tests be "fixed" by
/// writing prose. Quote- and escape-aware, so a `//` inside a string literal is
/// left alone rather than truncating the line it sits on.
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

/// Comment-stripped source re-joined into **statements** rather than lines.
///
/// The AC11 guard asks whether a comparison is applied to a forbidden key
/// class. Asked per physical line, that question is evaded by a line break:
///
/// ```ignore
/// if cr.name
///     == "Quality Gate / test"
/// ```
///
/// puts the key on one line and the operator on the next, and neither line
/// alone trips a per-line rule. So a line is joined to the following one while
/// it does not end a statement. The terminators are `;`, `{`, `}` and `,` —
/// exactly what ends a Rust statement, a block, or one argument of a call or
/// macro — which is what keeps unrelated neighbours apart:
///
/// ```ignore
/// let n = cr.name.clone();   // ends with `;` — its own statement
/// if a == b {                // never fused with the line above
/// ```
///
/// Verified against all eight routing modules: the rule finds nothing there
/// that the per-line rule did not, so the tightening adds no false positive.
///
/// The `usize` is the 1-based physical line the statement started on, so a
/// failure can still name a line.
fn logical_lines(code: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (index, line) in code.lines().enumerate() {
        let number = index + 1;
        let (start, mut text) = pending.take().unwrap_or_else(|| (number, String::new()));
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(line);
        let trimmed = text.trim_end();
        if trimmed.is_empty() || trimmed.ends_with([';', '{', '}', ',']) {
            out.push((start, text));
        } else {
            pending = Some((start, text));
        }
    }
    if let Some(rest) = pending {
        out.push(rest);
    }
    out
}

/// The PR poller calls both lane routers, on every lane that has one.
///
/// Source-level, and honestly labelled as such, for exactly the reason
/// `both_lane_fast_paths_consult_the_completeness_predicate` above is:
/// `resolve_installation_client` builds `GitHubApiClient::for_installation`,
/// which hard-codes `api.github.com`, so `poll_pr_draft_tasks` and
/// `handle_queue_failure` cannot be driven end to end from this crate. Every
/// behavioural fixture in this file therefore enters at
/// `route_pr_head_ci_evidence` / `route_merge_group_ci_evidence` directly, so
/// nothing else in this crate would notice a deleted call site.
///
/// This used to also pin that each call was handed the live
/// `ci_evidence_routing` gate. That gate is gone — routing is the only path —
/// so what is left to pin is the count: three call sites, two lanes, and no
/// silent loss of one of them.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete a router call site outright: the call count no longer matches.
/// (b) Add a second unguarded call site in either module: same failure, which is
///     what forces a new caller to be looked at rather than silently inheriting
///     the dispositions the two guards below assert.
#[test]
fn the_pr_poller_calls_both_lane_routers() {
    for (label, source, expected_calls) in [
        ("pr_watcher", include_str!("../../pr_watcher.rs"), 2usize),
        ("pr_commands", include_str!("../../pr_commands.rs"), 1usize),
    ] {
        assert_eq!(
            source.split("_ci_evidence(").skip(1).count(),
            expected_calls,
            "{label}: the poller's lane-router call sites are the thing under \
             guard here; a changed count needs this test updated, not ignored",
        );
    }
}

// ===========================================================================
// `close_ci_routes_on_success`: the callee, then its two call sites
// ===========================================================================

/// A newer pass or merge terminalizes this subject's open route and clears the
/// evidence-advance high-watermark.
///
/// Behavioural, over the production repository and the production quiescence
/// report. The assertion is the stored `terminal_outcome` and the durable
/// quiescence row, never the name of the branch the method took.
///
/// The chain is the one the proposal's drain-safety argument rests on: an open
/// route row keeps `current_failed_identity_count` above zero, so a coordinator
/// that never closes its routes on success can never be drained — the report
/// would sit at "1 route identity that is still the current failed evidence for
/// its lane" forever, for a PR that merged.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete the `close_routes_for_newer_outcome` call from
///     `close_ci_routes_on_success`: the row stays `Reserved` with
///     `terminal_outcome: None`, and the high-watermark stays at one.
/// (b) Invert `if !decision.closes_route() { return; }`, or hand `classify` a
///     capture other than `merged()`/`passing()`: nothing closes, as in (a).
/// (c) Swap `CiRouteOutcome::Merged` and `CiRouteOutcome::Passed`: the outcome
///     assertion fails in both halves of the loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_ci_routes_on_success_terminalizes_the_route_and_clears_the_watermark() {
    for (merged, expected) in [
        (false, CiRouteOutcome::Passed),
        (true, CiRouteOutcome::Merged),
    ] {
        let (db, task_id, subject) = wiring_subject("ci-close-on-success").await;
        let routes = CiRouteAttemptRepository::new(db.clone());
        let actor = crate::actor::actor_with_test_db(db.clone());

        let checks = [inconclusive_check("Quality Gate / test", 993)];
        let blocking = refs(&checks);
        let id = pr_head_identity(993);
        let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
        let key = plant_reservation(&routes, &subject, &id, &fingerprint).await;

        let before = route_row(&routes, &subject, &key).await;
        assert_eq!(
            before.action_phase,
            CiActionPhase::Reserved,
            "precondition: one open route for this PR",
        );
        assert_eq!(before.terminal_outcome, None);

        // ── The drain, with the route still open ────────────────────────────
        let blocked = actor
            .record_ci_rollback_quiescence_report()
            .await
            .expect("quiescence report");
        assert_eq!(
            blocked.current_failed_identities, 1,
            "an open route is still the current failed evidence for its lane",
        );

        // ── The newer authoritative outcome ─────────────────────────────────
        actor
            .close_ci_routes_on_success(&task_id, PR as u64, merged)
            .await;

        let after = route_row(&routes, &subject, &key).await;
        assert_eq!(
            after.action_phase,
            CiActionPhase::Terminal,
            "a newer pass or merge outranks the open route, staleness included",
        );
        assert_eq!(
            after.terminal_outcome,
            Some(expected),
            "and records which authoritative outcome closed it (merged: {merged})",
        );

        let clean = actor
            .record_ci_rollback_quiescence_report()
            .await
            .expect("quiescence report");
        assert_eq!(
            clean.current_failed_identities, 0,
            "the closed identity has advanced, so the high-watermark is clear",
        );
        assert!(
            !clean
                .blocking_reasons()
                .iter()
                .any(|reason| reason.contains("current failed evidence")),
            "and the route no longer appears among the drain's blocking counts: {:?}",
            clean.blocking_reasons(),
        );
        assert_eq!(
            clean.permits_rollback,
            clean.recomputed_verdict(),
            "the stored verdict must agree with the function it is checked against",
        );
    }
}

/// The quiescence report attests the **leader's live provider futures**.
///
/// # The one count no query can replace
///
/// Five of the six counts in `ci_route_rollback_reports` come out of the route
/// table. `registered_provider_futures` does not: it is read from this
/// process's `ProviderActionScope`, and it is the difference between "a call
/// episode was claimed" (a `calling` row) and "a future is still talking to
/// GitHub". Because the database CHECK is a function of the *stored* values,
/// storing zero satisfies the constraint — so
/// `registered_provider_futures = scope.in_flight()` in
/// `record_ci_rollback_quiescence_report` could be pinned to `0` and every
/// existing fixture would stay green, since none of them holds a guard while
/// taking a report. The drain would then read zero with a live
/// `rerun_failed_jobs` in flight, which is the one thing this count exists to
/// expose.
///
/// Every database-derived count is asserted zero alongside it, so the live
/// future is provably the *only* non-zero one; and the empty-scope report taken
/// first is the vacuity guard proving the count moves at all.
///
/// NAMED FAILING MUTATIONS.
/// (a) `registered_provider_futures = 0` (or any constant): the count assertion
///     fails, and the drain reads empty with a guard held.
/// (b) Read a fresh `ProviderActionScope::new().in_flight()` instead of
///     `self.provider_action_scope`: identical failure.
/// (c) Take the scope reading *after* the repository counts and let a guard
///     drop in between — not expressible here, but the ordering comment on the
///     read is what (a) and (b) protect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_quiescence_report_counts_the_leaders_live_provider_futures() {
    let db = Database::open_in_memory().expect("ephemeral test database");
    let leader = ProviderActionScope::new();
    let actor = crate::actor::actor_with_test_db_and_scope(db.clone(), leader.clone());

    // ── Vacuity: with an empty scope this count is zero ─────────────────────
    let clean = actor
        .record_ci_rollback_quiescence_report()
        .await
        .expect("quiescence report");
    assert_eq!(clean.registered_provider_futures, 0);
    assert!(
        clean
            .blocking_reasons()
            .iter()
            .all(|reason| !reason.contains("provider-action futures")),
        "no future is in flight, so none may be reported: {:?}",
        clean.blocking_reasons(),
    );

    // ── One live provider future — the state a drain would strand ───────────
    let live = leader.admit().expect("an open scope admits");

    let blocked = actor
        .record_ci_rollback_quiescence_report()
        .await
        .expect("quiescence report");
    assert_eq!(
        blocked.registered_provider_futures, 1,
        "the report must attest the LEADER's in-flight count; it is the only one \
         of the six the database cannot derive",
    );
    assert_eq!(
        blocked.permits_rollback,
        blocked.recomputed_verdict(),
        "the stored verdict and the function it is checked against must agree",
    );

    // Every count the DATABASE can answer is still zero, so the future really is
    // the only non-zero one rather than one of several.
    assert_eq!(blocked.reserved_rows, 0);
    assert_eq!(blocked.calling_rows, 0);
    assert_eq!(blocked.open_tier2_leases, 0);
    assert_eq!(blocked.unapplied_lead_results, 0);
    assert_eq!(blocked.current_failed_identities, 0);
    assert!(
        blocked
            .blocking_reasons()
            .contains(&"1 registered provider-action futures".to_owned()),
        "and the operator is told which one: {:?}",
        blocked.blocking_reasons(),
    );

    // ── The future returns; only now is the count clear again ───────────────
    drop(live);
    let after = actor
        .record_ci_rollback_quiescence_report()
        .await
        .expect("quiescence report");
    assert_eq!(after.registered_provider_futures, 0);
}

/// Both production call sites of `close_ci_routes_on_success` still exist, in
/// the branches whose behaviour depends on them.
///
/// SOURCE-LEVEL, and honestly labelled: this is not an integration test and
/// does not pretend to be one. Both call sites sit *after*
/// `gh_client.get_pull_request(..)` inside `poll_pr_draft_tasks`, and
/// `resolve_installation_client` builds `GitHubApiClient::for_installation`,
/// which hard-codes `api.github.com` — the crate carries no HTTP double and no
/// base-URL seam on that path, so nothing here can reach either branch. The
/// fixture above proves the callee end to end; this one proves production still
/// reaches it.
///
/// It has to exist because deleting *both* calls leaves the method entirely
/// dead — `warning: method close_ci_routes_on_success is never used` is the only
/// signal — with the whole `nafu` command list green, while a merged PR silently
/// stops closing its head's Tier-2 lease.
///
/// Comments are stripped before matching, so a comment naming the method
/// satisfies nothing here.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete either call site: the call count is no longer 2.
/// (b) Delete both: the same failure, one assertion earlier.
/// (c) Inline the method's body at either site: the call token disappears, so
///     the count fails exactly as a deletion does.
/// (d) Pass the same flag at both sites (both `true`, or both `false`): the
///     `["true", "false"]` assertion fails — and a merged PR would record
///     `passed`, or a passing one `merged`.
/// (e) Move the merged-branch call after `apply_pr_merge`, or out of the
///     `PrTerminalState::Merged` arm entirely: the first range assertion fails.
/// (f) Move the passing-branch call out of the `CiStatus::Passing` arm: the
///     second range assertion fails.
/// (g) Shadow the lane-routing method with a local `fn` of the same name in
///     `pr_watcher.rs`: the no-local-definition assertion fails.
#[test]
fn the_pr_poller_closes_ci_routes_on_both_merge_and_pass() {
    const CALL: &str = "self.close_ci_routes_on_success(";

    let code = strip_line_comments(include_str!("../../pr_watcher.rs"));

    let mut sites: Vec<(usize, &str)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(offset) = code[cursor..].find(CALL) {
        let at = cursor + offset;
        sites.push((at, final_argument(&code[at + CALL.len()..])));
        cursor = at + CALL.len();
    }

    assert_eq!(
        sites.len(),
        2,
        "the merged branch and the passing branch each close this subject's \
         routes; a changed count needs this test updated, not ignored",
    );
    assert_eq!(
        sites
            .iter()
            .map(|(_, argument)| *argument)
            .collect::<Vec<_>>(),
        vec!["true", "false"],
        "the merged branch closes with `merged: true` and the passing branch \
         with `merged: false`, in that file order",
    );
    assert!(
        !code.contains("fn close_ci_routes_on_success"),
        "pr_watcher must CALL the lane-routing method, not define one of its own",
    );

    // ── Each call site sits in the branch that needs it ─────────────────────
    //
    // The merged branch is the `Merged` arm of the terminal-state match that
    // now runs BEFORE the tripwire active-hold gate (the `4vnt`/`3kza` fix); a
    // merged PR is ground truth and no Djinn-side gate may precede it.
    let merged_branch = code
        .find("merged_reconcile::PrTerminalState::Merged => {")
        .expect("the merged branch is in pr_watcher.rs");
    let apply_merge = code
        .find("self.apply_pr_merge(")
        .expect("the merge transition is in pr_watcher.rs");
    let passing_arm = code
        .find("CiStatus::Passing => {")
        .expect("the passing arm is in pr_watcher.rs");
    let after_the_match = passing_arm
        + code[passing_arm..]
            .find("if pr.mergeable == Some(false) {")
            .expect("the conflict check follows the CI match");

    assert!(
        (merged_branch..apply_merge).contains(&sites[0].0),
        "the merged close must run inside the merged branch and BEFORE \
         `apply_pr_merge`, or a merge closes the task while its route rows stay \
         open",
    );
    assert!(
        (passing_arm..after_the_match).contains(&sites[1].0),
        "the passing close must run inside the `CiStatus::Passing` arm, which is \
         the only place a newer pass is known",
    );
}

/// The end of the `{`-delimited block that starts at `open`.
///
/// Used by the disposition-branch guard below to tell "inside the branch" from
/// "after the branch" — the whole difference between a legacy remedy that is
/// *replaced* by a route and one that runs *in addition to* it.
fn block_end(code: &str, open: usize) -> usize {
    let bytes = code.as_bytes();
    assert_eq!(
        bytes[open], b'{',
        "block_end must start on an opening brace"
    );
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match *byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from offset {open}");
}

/// The PR-draft lane's two legacy remedies are *replaced* by a route, never run
/// alongside one.
///
/// SOURCE-LEVEL, and honestly labelled, for the reason the sibling guards above
/// carry: both branches sit after `gh_client.get_pull_request(..)` inside
/// `poll_pr_draft_tasks`, and `resolve_installation_client` builds
/// `GitHubApiClient::for_installation`, which hard-codes `GITHUB_API_BASE`
/// (`api.github.com`). This crate carries no HTTP double and no base-URL seam on
/// that path, so no fixture here can reach either branch; every behavioural
/// fixture in this file enters at `route_pr_head_ci_evidence` and asserts what
/// the *callee* answered.
///
/// The sibling guards pin the *presence* of the two router calls and the live
/// gate. Neither pins the BRANCH SHAPE around them, and the shape is the whole
/// contract: a `CiLaneOutcome` that routed must turn the legacy machinery OFF.
/// Delete a guard and the legacy remedy runs **in addition to** the route rather
/// than instead of it, so one routed failure buys both an evidence-led remedy
/// and the old generic `PrCiFailed` reopen — the double-spent session this
/// proposal exists to stop — with the entire `nafu` command list green, because
/// the callee still returns exactly what every behavioural fixture asserts.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `if !routed.is_routed() {` around `retrigger_inconclusive_run`:
///     the guard string is gone, so the `expect` on it fails. A routed
///     inconclusive run would then be retriggered by the route layer AND by the
///     in-memory legacy dedupe, double-charging the same evidence.
/// (b) Invert it to `if routed.is_routed() {`: the same `expect` fails, because
///     the guard is matched with its `!` — and the legacy retrigger would fire
///     only when the route layer already handled the run.
/// (c) Move `retrigger_inconclusive_run` out of that block: the containment
///     assertion fails.
/// (d) Move the `continue;` INSIDE the `!routed.is_routed()` block: the
///     "continue after the block" assertion fails — a routed inconclusive lane
///     would fall through to the undraft path and un-draft a PR whose CI said
///     nothing.
/// (e) Delete `if routed.is_routed() && routed.complete_empty().is_none() {`
///     from the failing arm, or drop its `continue;`: the corresponding
///     `expect`/containment assertion fails, and a routed causal failure would
///     reach `handle_ci_failure` as well as its Tier-2 route.
/// (f) Delete `if routed.complete_empty().is_none() {` around
///     `handle_ci_failure`: that `expect` fails. An authoritatively complete
///     *empty* enumeration — the no-CI compatibility path, already recorded
///     `Passing` by the route layer — would be handed to the legacy failure
///     remedy.
/// (g) Reorder so `handle_ci_failure` precedes either guard: the ordering
///     assertions fail.
/// (h) Add a second call to either legacy remedy anywhere in the file: the
///     occurrence-count assertions fail, which is what forces a new caller to be
///     looked at rather than silently inheriting no guard.
#[test]
fn a_routed_pr_draft_disposition_turns_the_legacy_remedy_off() {
    let code = strip_line_comments(include_str!("../../pr_watcher.rs"));

    let find = |needle: &str, what: &str| -> usize {
        code.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` is not in pr_watcher.rs"))
    };
    let count = |needle: &str| code.matches(needle).count();

    // ── Arm boundaries. Everything below is asserted WITHIN one arm, so a
    //    guard that drifted into a neighbouring arm reads as deleted. ────────
    let inconclusive_arm = find("CiStatus::Inconclusive => {", "the inconclusive arm");
    let pending_arm = find(
        "CiStatus::Pending | CiStatus::Unknown => {",
        "the pending arm",
    );
    let failing_arm = find("CiStatus::Failing => {", "the failing arm");
    let passing_arm = find("CiStatus::Passing => {", "the passing arm");
    assert!(
        inconclusive_arm < pending_arm && pending_arm < failing_arm && failing_arm < passing_arm,
        "the four CI-status arms are read in file order; a changed order needs \
         this test updated, not ignored",
    );

    // ── Inconclusive: the legacy retrigger is the else-branch of the route ──
    const RETRIGGER: &str = "self.retrigger_inconclusive_run(";
    const ROUTED_GUARD: &str = "if !routed.is_routed() {";
    assert_eq!(
        count(RETRIGGER),
        1,
        "one legacy retrigger call site; a second one would inherit no guard",
    );
    let retrigger = find(RETRIGGER, "the legacy retrigger");
    let routed_guard = find(ROUTED_GUARD, "the routed-disposition guard");
    assert!(
        (inconclusive_arm..pending_arm).contains(&retrigger)
            && (inconclusive_arm..pending_arm).contains(&routed_guard),
        "both live in the inconclusive arm",
    );
    let routed_block = block_end(&code, routed_guard + ROUTED_GUARD.len() - 1);
    assert!(
        (routed_guard..routed_block).contains(&retrigger),
        "the legacy retrigger must run ONLY when the route layer declined; \
         outside that block it runs in addition to the route",
    );
    assert!(
        code[routed_block..pending_arm].contains("continue;"),
        "and the hold must be OUTSIDE that block — a routed inconclusive run \
         holds too, it just holds without a second retrigger",
    );

    // The no-CI compatibility path is the one thing that does NOT hold.
    let complete_empty = find(
        "if routed.complete_empty().is_some() {",
        "the complete-empty fall-through",
    );
    assert!(
        (inconclusive_arm..routed_guard).contains(&complete_empty),
        "an authoritatively complete EMPTY enumeration falls through to undraft \
         before the hold, or a repository with no CI wedges in `pr_draft`",
    );

    // ── Failing: routed holds, and complete-empty is never a failure ────────
    const LEGACY_FAILURE: &str = ".handle_ci_failure(";
    const HOLD_GUARD: &str = "if routed.is_routed() && routed.complete_empty().is_none() {";
    const EMPTY_GUARD: &str = "if routed.complete_empty().is_none() {";
    assert_eq!(
        count(LEGACY_FAILURE),
        1,
        "one legacy failure-remedy call site in this lane",
    );
    let legacy_failure = find(LEGACY_FAILURE, "the legacy failure remedy");
    let hold_guard = find(HOLD_GUARD, "the routed-holds guard");
    let empty_guard = find(EMPTY_GUARD, "the complete-empty guard");
    assert!(
        (failing_arm..passing_arm).contains(&legacy_failure)
            && (failing_arm..passing_arm).contains(&hold_guard)
            && (failing_arm..passing_arm).contains(&empty_guard),
        "all three live in the failing arm",
    );
    assert!(
        hold_guard < empty_guard && empty_guard < legacy_failure,
        "the routed hold is answered first, then complete-empty, and only then \
         is the legacy remedy reachable",
    );
    let hold_block = block_end(&code, hold_guard + HOLD_GUARD.len() - 1);
    assert!(
        code[hold_guard..hold_block].contains("continue;"),
        "a routed causal failure must LEAVE the poll; falling out of this block \
         hands the same evidence to `handle_ci_failure` as well",
    );
    assert!(
        hold_block < empty_guard,
        "and it must leave before the legacy remedy's own guard",
    );
    let empty_block = block_end(&code, empty_guard + EMPTY_GUARD.len() - 1);
    assert!(
        (empty_guard..empty_block).contains(&legacy_failure),
        "the legacy failure remedy must sit INSIDE the complete-empty guard; \
         outside it, a no-CI enumeration the route layer already recorded \
         `Passing` gets remediated as a failure",
    );
}

/// The merge-queue lane's routed disposition LEAVES `handle_queue_failure`.
///
/// The merge-group twin of the guard above, and source-level for the same
/// reason: `handle_queue_failure` takes a `&GitHubApiClient` built by
/// `resolve_installation_client` against the hard-coded `api.github.com`, so no
/// fixture in this crate can drive it. Every behavioural merge-group fixture
/// enters at `route_merge_group_ci_evidence` and asserts what the *callee*
/// answered.
///
/// The sibling `the_pr_poller_hands_the_live_gate_to_both_lane_routers` pins
/// that this call site exists and is handed the live gate. Neither it nor
/// anything else pins the `return;` — and this router call sits in the MIDDLE
/// of `handle_queue_failure`, not at its head, so without the early return a
/// routed merge-group failure keeps running through the rest of the function
/// and reaches both legacy remedies: the same-signature park and then the
/// generic `PrCiFailed` reopen. One dequeue would buy an evidence-led Tier-2
/// adjudication AND the blind reopen that adjudication exists to replace — the
/// double-spent session this proposal is for — with the whole `nafu` command
/// list green, because the callee still answers exactly what every behavioural
/// fixture asserts.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete the `return;` from the `.is_routed()` branch: the containment
///     assertion fails. In production the routed dequeue would fall through to
///     `apply_pr_transition(PrCiFailed)` in addition to its route.
/// (b) Replace `return;` with a bare log, or with anything that does not leave
///     the function: same assertion, same reason.
/// (c) Move the router call (and its branch) BELOW the reopen: the ordering
///     assertion fails — the legacy remedy would already have been spent by the
///     time the route layer got first refusal.
/// (d) Invert the branch to `if !self.route_merge_group_ci_evidence(..)…`: the
///     routed disposition would fall through to both legacy remedies and the
///     DECLINED one would return — the same double-spend with the cases
///     swapped, and a shape the containment check alone cannot tell apart. The
///     unnegated-head assertion is what fails.
/// (e) Add a second `route_merge_group_ci_evidence` call site or a second
///     legacy remedy anywhere in the file: the occurrence counts fail, which
///     forces a new caller to be looked at rather than silently inheriting no
///     guard.
/// (f) Qualify the condition — `… .is_routed() && false`, or any other
///     conjunct: the head is still
///     `if self`, `return;` is still inside a well-formed block, and the block
///     still ends before both remedies, so (a)–(e) all hold. The
///     nothing-between-the-disposition-and-the-brace assertion is what fails,
///     and it is the only one that can.
#[test]
fn a_routed_merge_group_disposition_replaces_the_legacy_queue_remedy() {
    let code = strip_line_comments(include_str!("../../pr_commands.rs"));

    let find = |needle: &str, what: &str| -> usize {
        code.find(needle)
            .unwrap_or_else(|| panic!("{what}: `{needle}` is not in pr_commands.rs"))
    };
    let count = |needle: &str| code.matches(needle).count();

    const ROUTER: &str = ".route_merge_group_ci_evidence(";
    const ROUTED: &str = ".is_routed()";
    const LEGACY_PARK: &str = "self.escalate_ci_failure_and_park(";
    const LEGACY_REOPEN: &str = "TransitionAction::PrCiFailed";

    assert_eq!(
        count(ROUTER),
        1,
        "one merge-group router call site in this file",
    );
    assert_eq!(
        count(ROUTED),
        1,
        "one routed-disposition test, so the branch located below is that one",
    );
    assert_eq!(
        count(LEGACY_PARK),
        1,
        "one same-signature park; a second one would inherit no guard",
    );
    assert_eq!(
        count(LEGACY_REOPEN),
        1,
        "one generic queue reopen; a second one would inherit no guard",
    );

    let router = find(ROUTER, "the merge-group router call");
    let routed = find(ROUTED, "the routed-disposition test");
    let park = find(LEGACY_PARK, "the same-signature park");
    let reopen = find(LEGACY_REOPEN, "the generic queue reopen");
    assert!(
        router < routed,
        "the disposition must be read from the router's own return value",
    );

    // The branch must test the routed disposition UNNEGATED. `if !…is_routed()`
    // keeps a `return;` inside a well-formed block — containment alone cannot
    // tell the two apart — while returning on the declined path and handing the
    // ROUTED one to both legacy remedies.
    let if_start = code[..router]
        .rfind("if ")
        .expect("the router call must sit in an `if` condition");
    let head: Vec<&str> = code[if_start..router].split_whitespace().collect();
    assert_eq!(
        head.join(" "),
        "if self",
        "the routed branch must be entered when the route layer HANDLED the \
         dequeue, not when it declined",
    );

    // The branch the routed disposition opens, and where it closes. `block_end`
    // is what tells "inside the branch" from "after it" — the whole difference
    // between a legacy remedy REPLACED by a route and one that runs in addition
    // to it.
    let after_routed = routed + ROUTED.len();
    let open = after_routed
        + code[after_routed..]
            .find('{')
            .expect("the routed disposition must open a branch");
    let block = block_end(&code, open);
    assert!(
        code[open..block].contains("return;"),
        "a routed merge-group failure must LEAVE `handle_queue_failure`; falling \
         out of this branch hands the same dequeue to the legacy remedies as well",
    );

    // …and the disposition must be the WHOLE condition.
    //
    // Everything above pins the head (`if self`), the body (`return;`) and the
    // ordering (before both remedies). That leaves exactly one place a conjunct
    // can hide — between `.is_routed()` and the brace it opens — and a conjunct
    // there is not cosmetic: `… .is_routed() && false` satisfies every one of
    // those assertions while the branch becomes unreachable, so the route layer
    // takes the evidence AND `handle_queue_failure` runs on to
    // `PrCiFailed`. One dequeue then reopens the task for rework and re-enters
    // the queue, which is the double-spend the whole proposal exists to stop.
    let qualifier = code[after_routed..open].trim();
    assert!(
        qualifier.is_empty(),
        "the routed disposition is the ENTIRE branch condition; found \
         `{qualifier}` between `.is_routed()` and the branch it opens. A \
         conjunct there turns the early return off while the head, the body and \
         the ordering all still read correctly",
    );

    assert!(
        block < park && park < reopen,
        "and it must leave BEFORE either legacy remedy is reachable: the park at \
         {park} and the reopen at {reopen} both follow the routed branch, which \
         ends at {block}",
    );
}

/// The sweep EMITS the routing report.
///
/// Behavioural, over the report's only production observable. That observable is
/// a `tracing::info!` event by design — the proposal asks for reporting, not for
/// a reporting table, and inventing a durable row to make this assertable would
/// be inventing the thing under test. So the log is not a proxy for the
/// behaviour, it *is* the behaviour, and `tracing_test` is how this crate
/// already asserts on one (see
/// `failover_chain_logging_captures_candidate_events` in
/// `dispatch::task_dispatch`).
///
/// Without this, `self.emit_ci_route_report().await;` could be deleted from
/// `sweep_ci_routes` with the entire `nafu` command list green: the sibling
/// sweep fixture asserts the swept ROW, and every count the report reads is
/// already asserted by `djinn-db`'s own report tests — from a caller that is not
/// the coordinator.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `self.emit_ci_route_report().await;` from `sweep_ci_routes`:
///     nothing emits the event and both assertions fail.
/// (b) Move it ABOVE `sweep_reserved_routes` in `sweep_ci_routes`: the sweep has
///     not yet superseded the planted row, every count the early return tests is
///     zero, the report returns silently, and both assertions fail.
/// (c) Put it behind a condition of any kind — the sweep emits its report on
///     every pass, so any guard at all leaves this fixture emitting nothing.
/// (d) Invert `emit_ci_route_report`'s early return (emit only when every count
///     is zero): the same failure.
/// (e) Drop `suppressed_before_provider_call` from the emitted fields: the
///     second assertion fails while the first still passes, which is why the
///     count is asserted and not only the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[tracing_test::traced_test]
async fn the_route_sweep_emits_the_routing_report() {
    let (db, task_id, subject) = wiring_subject("ci-route-report-wiring").await;
    let routes = CiRouteAttemptRepository::new(db.clone());

    let mut actor = crate::actor::actor_with_test_db(db.clone());

    // A head that has MOVED past the reservation's: the sweep supersedes the row
    // pre-call, which is the one count that makes the report non-silent.
    actor
        .persist_ci_snapshot(
            &task_id,
            PR as u64,
            MOVED_HEAD,
            djinn_core::models::CiStatus::Pending,
            Vec::new(),
            None,
            0,
            None,
        )
        .await;

    let checks = [inconclusive_check("Quality Gate / test", 994)];
    let blocking = refs(&checks);
    let id = pr_head_identity(994);
    let fingerprint = transient_fingerprint(CiLane::PrHead, &blocking);
    let key = plant_reservation(&routes, &subject, &id, &fingerprint).await;
    djinn_db::test_support::ci_route_age_reserved_for_test(&db, &subject.id, &key, 600).await;

    assert!(
        !logs_contain("ci route report"),
        "precondition: nothing has reported yet",
    );

    actor.last_ci_route_sweep = a_sweep_interval_ago();
    actor.drive_tick_for_test().await;

    assert_eq!(
        route_row(&routes, &subject, &key).await.terminal_outcome,
        Some(CiRouteOutcome::SupersededPreCall),
        "precondition for the report: the sweep produced something to report",
    );
    assert!(
        logs_contain("ci route report"),
        "the sweep must emit the routing report without anyone asking for it",
    );
    assert!(
        logs_contain("suppressed_before_provider_call=1"),
        "and it must carry the counts, not merely the message",
    );
}

/// AC11: the routing modules CONSUME `ci_triage`, and no branch keys on a
/// forbidden class.
///
/// SOURCE-LEVEL, and it cannot be anything else. No observable distinguishes
/// `ci_triage::is_inconclusive(blocking)` from a byte-identical copy of its body
/// pasted into this module, so no behavioural mutation can kill a test of
/// provenance — which is exactly why AC11 had no test at all until this one.
/// What *is* checkable is that the call token is present and that no local
/// definition shadows it, and that pair is precisely what "consumed rather than
/// reimplemented" means.
///
/// # A fingerprint keyed on the job name is not a branch
///
/// `transient_fingerprint` hashes `cr.name.trim().to_lowercase()` into its
/// preimage, so the job name genuinely *is* an input to `nafu` code. AC11
/// forbids a branch keyed on the job name — a decision that comes out different
/// because a check happens to be called one thing rather than another. A hash
/// input is the opposite of that: every name is treated identically, and two
/// runs of the same failing job share a budget precisely because the hash does
/// not care what the job is. So the assertion below is written against
/// *comparisons* applied to the key classes rather than against their
/// appearance, and this paragraph is the reason it has to be.
///
/// NAMED FAILING MUTATIONS.
/// (a) Replace `ci_triage::is_inconclusive(blocking)` in `classify` with a local
///     reimplementation (`blocking.iter().all(|cr| …)`): the call token is gone
///     and the first assertion fails.
/// (b) Copy `ci_triage`'s predicate into a routing module as
///     `fn is_inconclusive`: the no-local-definition assertion fails.
/// (c) Add `if cr.name.contains("Quality Gate") { … }` — or any comparison on a
///     check name, `target.repo`, `target.owner`, or `repository_id` — anywhere
///     in a routing module: the forbidden-branch assertion fails, naming the
///     file and line. Including when rustfmt splits it across a line break
///     (`if cr.name\n    == "Quality Gate"`), which the earlier per-physical-line
///     form of this check could not see: see [`logical_lines`].
/// (d) Special-case a migration framework, a build tool, an artifact type, or a
///     provider incident label (`"sqlx"`, `"cargo"`, `"incident"`, …): the
///     forbidden-vocabulary assertion fails. A branch on one of those classes
///     has to name it.
#[test]
fn the_routing_modules_consume_ci_triage_and_branch_on_no_forbidden_key() {
    // Every `nafu`-owned production module in this crate. Test modules are
    // separate files and are deliberately excluded: a fixture may say whatever
    // it likes about a job name.
    const ROUTING_MODULES: [(&str, &str); 7] = [
        ("ci_routing.rs", include_str!("../../ci_routing.rs")),
        (
            "ci_lane_routing.rs",
            include_str!("../../ci_lane_routing.rs"),
        ),
        ("ci_hold.rs", include_str!("../../ci_hold.rs")),
        ("ci_reporting.rs", include_str!("../../ci_reporting.rs")),
        ("ci_routing/executor.rs", include_str!("../executor.rs")),
        ("ci_routing/quiescence.rs", include_str!("../quiescence.rs")),
        (
            "ci_routing/tier2_dispatch.rs",
            include_str!("../tier2_dispatch.rs"),
        ),
    ];

    // The evidence-ranking entry points AC11 requires be consumed.
    const CONSUMED: [&str; 3] = [
        "ci_triage::is_inconclusive(",
        "ci_triage::check_evidence(",
        "ci_triage::completed_after_start(",
    ];

    // Field accesses that reach a forbidden key class.
    const FORBIDDEN_KEYS: [&str; 4] = [".name", "target.repo", "target.owner", "repository_id"];

    // What turns reading a key class into branching on one.
    const COMPARISONS: [&str; 6] = [
        "==",
        "!=",
        ".contains(",
        ".starts_with(",
        ".ends_with(",
        ".eq_ignore_ascii_case(",
    ];

    // Migration frameworks, build tools, artifact types, and incident labels. A
    // branch on any of those classes has to name one of these words.
    const FORBIDDEN_VOCABULARY: [&str; 15] = [
        "sqlx",
        "diesel",
        "flyway",
        "liquibase",
        "alembic",
        "cargo",
        "gradle",
        "maven",
        "bazel",
        "webpack",
        "npm",
        "pnpm",
        "artifact",
        "incident",
        "outage",
    ];

    let classifier = strip_line_comments(ROUTING_MODULES[0].1);
    for consumed in CONSUMED {
        assert!(
            classifier.contains(consumed),
            "the classifier must CONSUME `{consumed}`: the ranking that decides \
             Tier 1 is `ci_triage`'s, and a second copy of it here is the \
             reimplementation AC11 forbids",
        );
    }

    for (label, source) in ROUTING_MODULES {
        let code = strip_line_comments(source);

        for consumed in CONSUMED {
            let local = consumed
                .trim_start_matches("ci_triage::")
                .trim_end_matches('(');
            assert!(
                !code.contains(&format!("fn {local}")),
                "{label}: `{local}` belongs to `ci_triage`; a local definition of \
                 it is a reimplementation, however faithful",
            );
        }

        // Statements, not physical lines: a comparison split across a line
        // break is the one evasion a per-line rule cannot see, and rustfmt
        // produces exactly that split as soon as the expression is long.
        for (number, statement) in logical_lines(&code) {
            if !FORBIDDEN_KEYS.iter().any(|key| statement.contains(key)) {
                continue;
            }
            let text = statement.trim();
            for comparison in COMPARISONS {
                assert!(
                    !statement.contains(comparison),
                    "{label}:{number}: `{text}` applies `{comparison}` to a \
                     forbidden key class. AC11 allows a job name, repository, or \
                     owner to be *carried* — hashed into a fingerprint, logged, \
                     cited as evidence, passed to the provider — and forbids a \
                     route decision that differs because of one.",
                );
            }
        }

        let lowered = code.to_ascii_lowercase();
        for forbidden in FORBIDDEN_VOCABULARY {
            assert!(
                !lowered.contains(forbidden),
                "{label}: routing code must not name `{forbidden}`. A branch keyed \
                 on a migration framework, build tool, artifact type, or provider \
                 incident label has to name one, and AC11 forbids every such \
                 branch — the contract is keyed on execution evidence alone.",
            );
        }
    }
}
