//! The complete direct-delivery consumer cutover matrix.
//!
//! Every consumer named by proposal `dser` is entered at its **production**
//! entry point, over repository fixtures persisted once, across every routing
//! state the landed contract can be in. Nothing here constructs an admission
//! variant by hand or re-derives proposal ownership: the fixtures persist rows,
//! the consumers read them, and the assertions compare exact observations taken
//! at production boundaries.
//!
//! # Why this lives in the crate rather than in `tests/`
//!
//! Six of the ten consumers are `pub(crate)` or private —
//! `continue_ready_dispatch`, `admit_zombie_session_release`,
//! `admit_execution_state_orphan_release`, `admit_second_strike_retry`,
//! `poll_pr_draft_tasks`, `poll_pr_review_tasks`,
//! `reconcile_blindspot_merged_prs`. An integration test could only reach them
//! through a wrapper, and a wrapper is exactly the thing that stops proving
//! what production runs. The suite is therefore an in-crate sibling module
//! under `dispatch/`, following the `#[path = "..."]` convention already used
//! across that module — and it has to live there specifically, because
//! `dispatch::retry` is private to `dispatch` and reachable only from inside
//! it.
//!
//! # What "one table" means here
//!
//! [`Consumer`] × [`ContractCase`] is the table. Each cell persists a fixture,
//! runs one consumer seam, and produces a [`ConsumerObservation`] whose fields
//! are all counted or read at a production boundary — the boundary-operation
//! recorder, the task rows, the immutable ledger, the attempt row, the
//! persisted PR identity, and whether anything persisted moved at all.
//!
//! Two kinds of assertion sit on top of that. The invariants that must hold for
//! a whole *class* of cells — no task-PR forge effect for a direct identity, no
//! forge effect and no append for a fail-closed row, appends only from a
//! mid-flight generation, a retained-legacy row keeping its exact PR identity —
//! run against every cell in that class, so a new consumer cannot quietly skip
//! them. On top of that, each seam pins its own rendered rows, so a routing
//! change has to be re-stated rather than absorbed.
//!
//! The suite is split one `#[tokio::test]` per consumer seam only because every
//! cell builds its own migrated database and the repository caps a single test
//! at 90s. All of them go through [`matrix_rows`], over one fixture builder.
//! [`every_named_consumer_seam_has_a_full_matrix_expectation`] is the guard
//! that the split stays complete.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_core::models::{TaskDeliveryIdentity, TransitionAction};
use djinn_db::{
    Database, DispositionScope, EpicRepository, ListQuery, ProposalBuildAttemptRepository,
    TaskRepository,
};

use crate::direct_delivery::{
    AttemptRef, BoundaryOperation, Candidate, CandidateBuild, CandidateBuilder, DeliveryOutcome,
    DeliverySource, DirectDeliveryEngine, LEGACY_DELIVERY_LABEL, RemoteUpdate,
    RepositoryDeliveryLedger, boundary_operations_scope,
};

// ─── The ten consumers ─────────────────────────────────────────────────────

/// Each production consumer proposal `dser` cuts over, named by the entry point
/// this suite actually calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Consumer {
    /// `dispatch::task_dispatch::continue_ready_dispatch`
    ReadyDispatch,
    /// `dispatch::respawn_guard::run_respawn_guard_with_reconciler`
    RespawnGuard,
    /// `dispatch::session_recovery::admit_zombie_session_release` and
    /// `admit_execution_state_orphan_release` — both, per call.
    SessionRecovery,
    /// `dispatch::retry::admit_second_strike_retry`
    SecondStrikeRetry,
    /// `TaskRepository::transition`, which drives `emit_unblocked_tasks`.
    BlockerRelease,
    /// `TaskRepository::classify_parent_disposition`
    ParentDisposition,
    /// `TaskRepository::board_health`
    BoardHealth,
    /// `TaskRepository::list_filtered` with the `merged` pseudo-status.
    MergedClassification,
    /// `supervisor_impl::supervisor_pr_open`
    TaskPrAdoption,
    /// `poll_pr_draft_tasks`, `poll_pr_review_tasks`, and
    /// `reconcile_blindspot_merged_prs` — all three, per call.
    PrPoller,
}

impl Consumer {
    const ALL: [Consumer; 10] = [
        Consumer::ReadyDispatch,
        Consumer::RespawnGuard,
        Consumer::SessionRecovery,
        Consumer::SecondStrikeRetry,
        Consumer::BlockerRelease,
        Consumer::ParentDisposition,
        Consumer::BoardHealth,
        Consumer::MergedClassification,
        Consumer::TaskPrAdoption,
        Consumer::PrPoller,
    ];

    /// How many independently-reachable production seams this consumer has.
    ///
    /// Session recovery has two release sites; the PR poller has three polling
    /// entry points selecting three different statuses. Each seam gets its own
    /// fixture and its own row, so one seam's safety is never inferred from
    /// another's coverage.
    fn seams(self) -> usize {
        match self {
            Consumer::SessionRecovery => 2,
            Consumer::PrPoller => 3,
            _ => 1,
        }
    }

    /// The status the production loop behind this seam actually selects.
    ///
    /// This is not cosmetic. The recovery loops select `in_progress` and never
    /// `approved`; `poll_pr_draft_tasks` selects `pr_draft` and
    /// `poll_pr_review_tasks` selects `pr_review`. Running every consumer from
    /// one convenient status would test a task shape production never hands it.
    fn fixture_status(self, seam: usize) -> &'static str {
        match (self, seam) {
            (Consumer::SessionRecovery | Consumer::SecondStrikeRetry, _) => "in_progress",
            (Consumer::PrPoller, 1) => "pr_review",
            (Consumer::PrPoller, _) => "pr_draft",
            _ => "approved",
        }
    }

    /// Whether a resolved-direct fixture reaches its resolution by running the
    /// real engine, or by persisting a settled generation directly.
    ///
    /// `task_integrated` closes only from `approved`, so a consumer whose loop
    /// selects `in_progress` or `pr_draft` can never observe an integration it
    /// caused — for those, "resolved direct" means a settled ledger under a
    /// task the delivery already finished with.
    fn resolves_direct_through_the_engine(self, seam: usize) -> bool {
        match self {
            // Its own invocation *is* the terminal transition. A fixture that
            // had already integrated would leave nothing for it to do.
            Consumer::BlockerRelease => false,
            _ => self.fixture_status(seam) == "approved",
        }
    }

    /// Whether this consumer has a direct-delivery *admission gate* it can fail
    /// closed at.
    ///
    /// Six do: they call `admit_direct_delivery` (or the liveness fence over
    /// it) before deciding anything, so an unreadable contract or unresolvable
    /// owner must stop them before any mutation beyond their own durable park.
    ///
    /// The other four are pure repository reads whose direct-delivery logic is
    /// a SQL predicate, not a gate. With no epoch row there is no direct
    /// ownership to respect, and their correct behaviour is simply "do not
    /// classify this as direct" — which their own tables pin. Asserting a
    /// no-mutation invariant on them would assert something they never claimed.
    fn has_an_admission_gate(self) -> bool {
        !matches!(
            self,
            Consumer::BlockerRelease
                | Consumer::ParentDisposition
                | Consumer::BoardHealth
                | Consumer::MergedClassification
        )
    }

    /// Whether a retained-legacy fixture for this consumer should land its PR.
    ///
    /// Only the merged-classification consumer reads a *terminal* legacy row;
    /// every other consumer reads the task while the work is still in flight,
    /// and landing the PR early would put it in a state its loop never selects.
    fn lands_the_legacy_pr(self) -> bool {
        matches!(self, Consumer::MergedClassification)
    }
}

// ─── The seven persisted contract states ───────────────────────────────────

/// Every state the landed contract can be persisted in, named by what the row
/// actually says rather than by the decision it is expected to produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContractCase {
    /// Schema present, epoch disabled. Legacy delivery is retained.
    SupportedDisabled,
    /// Epoch active, but the task carries the explicit legacy label.
    ActiveExplicitLegacy,
    /// Epoch active, canonical owner, terminal applied generation.
    ActiveResolvedDirect,
    /// Epoch active, canonical owner, generation mid-flight.
    DirectApplying,
    /// Epoch active, canonical owner, terminal conflicted generation.
    DirectConflict,
    /// Epoch active, canonical owner, **and** a persisted task-PR URL.
    ///
    /// A contradictory row on purpose. Routing must come from canonical
    /// ownership, so a stray nullable PR field must not buy back a single
    /// forge effect — and a poller that can only *select* rows with a PR URL
    /// must still refuse this one at its eligibility gate rather than never
    /// having seen it.
    DirectWithStrayPrIdentity,
    /// Epoch active, but the epic's proposal has no build-attempt owner.
    UnresolvedOwner,
    /// The epoch row is absent.
    MissingContract,
    /// The epoch row carries a state this build does not define.
    UnknownContract,
}

impl ContractCase {
    const ALL: [ContractCase; 9] = [
        ContractCase::SupportedDisabled,
        ContractCase::ActiveExplicitLegacy,
        ContractCase::ActiveResolvedDirect,
        ContractCase::DirectApplying,
        ContractCase::DirectConflict,
        ContractCase::DirectWithStrayPrIdentity,
        ContractCase::UnresolvedOwner,
        ContractCase::MissingContract,
        ContractCase::UnknownContract,
    ];

    /// Legacy delivery is retained: the consumer's pre-existing task-PR
    /// behaviour, including its persisted PR identity, must survive untouched.
    fn is_retained_legacy(self) -> bool {
        matches!(
            self,
            ContractCase::SupportedDisabled | ContractCase::ActiveExplicitLegacy
        )
    }

    /// A canonical direct identity owns this task. No task-PR forge effect is
    /// permitted, in any consumer, in any of these states.
    fn is_direct_identity(self) -> bool {
        matches!(
            self,
            ContractCase::ActiveResolvedDirect
                | ContractCase::DirectApplying
                | ContractCase::DirectConflict
                | ContractCase::DirectWithStrayPrIdentity
        )
    }

    /// The fixture persists a generation that is still mid-flight, so a
    /// consumer whose contract is to consume `Applying` may legitimately move
    /// the attempt branch. No other case may.
    fn has_midflight_generation(self) -> bool {
        matches!(
            self,
            ContractCase::DirectApplying | ContractCase::DirectWithStrayPrIdentity
        )
    }

    /// Ownership or the contract itself could not be read. Consumers must fail
    /// closed: no mutation, no forge effect, nothing guessed.
    fn is_fail_closed(self) -> bool {
        matches!(
            self,
            ContractCase::UnresolvedOwner
                | ContractCase::MissingContract
                | ContractCase::UnknownContract
        )
    }
}

// ─── Production-boundary observation ───────────────────────────────────────

/// Every task-PR forge operation a direct identity must never reach.
///
/// Named exhaustively rather than by a catch-all so a new boundary operation
/// has to be classified deliberately.
const TASK_PR_FORGE_OPERATIONS: [BoundaryOperation; 15] = [
    BoundaryOperation::SupervisorPrOpen,
    BoundaryOperation::TaskPrLookup,
    BoundaryOperation::TaskPrAdopt,
    BoundaryOperation::TaskPrStatusPoll,
    BoundaryOperation::TaskPrReviewPoll,
    BoundaryOperation::TaskPrMergedPoll,
    BoundaryOperation::TaskPrInlineCleanup,
    BoundaryOperation::TaskPrStaleCleanup,
    BoundaryOperation::TaskPrCreate,
    BoundaryOperation::TaskPrMerge,
    BoundaryOperation::TaskPrAutoMerge,
    BoundaryOperation::TaskPrApproval,
    BoundaryOperation::TaskPrSignoff,
    BoundaryOperation::TaskPrCustomEnqueue,
    BoundaryOperation::AttemptPrCreateOrAdoptRequest,
];

/// One immutable delivery generation, read back exactly as persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LedgerSnapshot {
    generation: i64,
    state: String,
    candidate_sha: String,
}

/// Everything persisted that a consumer could have changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PersistedSnapshot {
    source_task: serde_json::Value,
    dependent_task: serde_json::Value,
    attempt: Option<serde_json::Value>,
    ledger: Vec<LedgerSnapshot>,
    attempt_count: Option<i64>,
    delivery_count: Option<i64>,
    task_attempt_count: i64,
}

/// What one cell of the matrix observed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConsumerObservation {
    /// The consumer's own typed answer, normalized to a string so ten
    /// different return types can share one table.
    decision: String,
    /// Task-PR forge operations recorded while the consumer ran.
    task_pr_effects: Vec<BoundaryOperation>,
    /// Direct appends recorded while the consumer ran.
    direct_appends: usize,
    /// Whether anything persisted changed.
    persisted_changed: bool,
    /// The task-PR identity as persisted after the consumer ran.
    persisted_pr_url: Option<String>,
    /// Whether every persisted fact except the source task's own status is
    /// byte-identical to the snapshot taken before the consumer ran.
    ///
    /// A durable fail-closed park is allowed to move `status` (and the
    /// `updated_at` that always rides with it). Nothing else may move: not the
    /// attempt row, not the immutable ledger, not the row counts, not the
    /// dependent, not the attempt-ledger cardinality.
    unchanged_beyond_a_park: bool,
}

// ─── Fixture ───────────────────────────────────────────────────────────────

/// The remote the engine CASes against. Counts pushes so a cell can prove the
/// ref was — or was not — moved.
#[derive(Clone)]
struct FixtureRemote(Arc<Mutex<(String, usize)>>);

#[async_trait]
impl AttemptRef for FixtureRemote {
    async fn observe(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(Some(self.0.lock().unwrap().0.clone()))
    }
    async fn update_expected_old(
        &self,
        _: &str,
        old: &str,
        new: &str,
    ) -> anyhow::Result<RemoteUpdate> {
        let mut state = self.0.lock().unwrap();
        state.1 += 1;
        if state.0 == old {
            state.0 = new.into();
            Ok(RemoteUpdate::Updated { sha: new.into() })
        } else {
            Ok(RemoteUpdate::Stale {
                observed_sha: Some(state.0.clone()),
            })
        }
    }
}

struct FixtureBuilder;

#[async_trait]
impl CandidateBuilder for FixtureBuilder {
    async fn build(
        &self,
        _: &TaskDeliveryIdentity,
        _: &DeliverySource,
        parent: &str,
    ) -> anyhow::Result<CandidateBuild> {
        Ok(CandidateBuild::Clean(Candidate {
            candidate_sha: "fixture-candidate".into(),
            patch_digest: "fixture-patch".into(),
            selected_parent_sha: parent.into(),
        }))
    }
}

struct ConflictingBuilder;

#[async_trait]
impl CandidateBuilder for ConflictingBuilder {
    async fn build(
        &self,
        _: &TaskDeliveryIdentity,
        _: &DeliverySource,
        _: &str,
    ) -> anyhow::Result<CandidateBuild> {
        Ok(CandidateBuild::Conflict {
            patch_digest: "fixture-patch".into(),
            reason: "fixture conflict".into(),
        })
    }
}

const LEGACY_PR_URL: &str = "https://github.com/acme/widget/pull/42";
const FIXTURE_INSTALLATION_ID: u64 = 4_242;
/// Mirrors `pr_poller::PR_DRAFT_MIN_AGE_SECS`, which is private to that module.
/// The positive control below asserts the poller fetches once this has elapsed,
/// so a widened production guard fails that test rather than silently passing.
const DRAFT_POLL_MIN_AGE_SECS: u64 = 10;

struct Fixture {
    db: Database,
    tasks: TaskRepository,
    attempts: ProposalBuildAttemptRepository,
    events: EventBus,
    project_id: String,
    epic_id: String,
    source_id: String,
    dependent_id: String,
    build_attempt_id: String,
    remote: Arc<Mutex<(String, usize)>>,
    /// Task-updated events keyed by task id, so a dependent release is counted
    /// where production emits it rather than inferred from a status read.
    updates: Arc<Mutex<Vec<String>>>,
}

impl Fixture {
    fn engine(
        &self,
    ) -> DirectDeliveryEngine<RepositoryDeliveryLedger, FixtureRemote, FixtureBuilder> {
        DirectDeliveryEngine::new(
            RepositoryDeliveryLedger::new(
                self.db.clone(),
                ProposalBuildAttemptRepository::new(self.db.clone()),
                TaskRepository::new(self.db.clone(), self.events.clone()),
            ),
            FixtureRemote(self.remote.clone()),
            FixtureBuilder,
        )
    }

    /// The same engine, with a candidate builder that always conflicts, so a
    /// conflict park is produced by the production path rather than seeded.
    fn conflicting_engine(
        &self,
    ) -> DirectDeliveryEngine<RepositoryDeliveryLedger, FixtureRemote, ConflictingBuilder> {
        DirectDeliveryEngine::new(
            RepositoryDeliveryLedger::new(
                self.db.clone(),
                ProposalBuildAttemptRepository::new(self.db.clone()),
                TaskRepository::new(self.db.clone(), self.events.clone()),
            ),
            FixtureRemote(self.remote.clone()),
            ConflictingBuilder,
        )
    }

    fn delivery_source(&self) -> DeliverySource {
        DeliverySource {
            task_id: self.source_id.clone(),
            delivery_generation: 1,
            transition_id: "fixture-prepare".into(),
            source_sha: "fixture-source".into(),
            normalized_patch: "fixture-patch".into(),
        }
    }

    async fn run_engine(&self) -> anyhow::Result<DeliveryOutcome> {
        let engine = self.engine();
        crate::dispatch::wave_dispatch::run_direct_completion(|| {
            engine.deliver(self.delivery_source())
        })
        .await
    }

    async fn task_json(&self, id: &str) -> serde_json::Value {
        match self.tasks.get(id).await {
            Ok(Some(task)) => serde_json::to_value(&task).unwrap(),
            other => serde_json::json!({ "unreadable": format!("{other:?}") }),
        }
    }

    /// Read every persisted fact a consumer could have touched.
    ///
    /// The epoch-gated readers are allowed to be unreadable — that is itself a
    /// persisted fact under the missing/unknown-contract fixtures, and it is
    /// compared exactly rather than skipped.
    async fn snapshot(&self) -> PersistedSnapshot {
        let counts = djinn_db::test_support::direct_delivery_matrix_counts_for_test(&self.db).await;
        PersistedSnapshot {
            source_task: self.task_json(&self.source_id).await,
            dependent_task: self.task_json(&self.dependent_id).await,
            attempt: self
                .attempts
                .get(&self.build_attempt_id)
                .await
                .ok()
                .flatten()
                .map(|attempt| serde_json::to_value(&attempt).unwrap()),
            ledger: djinn_db::test_support::direct_delivery_generations_if_readable_for_test(
                &self.db,
                &self.source_id,
            )
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| LedgerSnapshot {
                generation: row.delivery_generation,
                state: row.state,
                candidate_sha: row.candidate_sha,
            })
            .collect(),
            attempt_count: counts.build_attempts,
            delivery_count: counts.deliveries,
            task_attempt_count: djinn_db::test_support::task_attempt_count_for_test(
                &self.db,
                &self.source_id,
            )
            .await,
        }
    }
}

/// Persist one cell's fixture: a project, an epic owned by a proposal, a source
/// task in the status its consumer's loop selects, and a dependent blocked on
/// it.
async fn build_fixture(consumer: Consumer, case: ContractCase, seam: usize) -> Fixture {
    let db = Database::open_in_memory().unwrap();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let recorded = updates.clone();
    let events = EventBus::new(move |event| {
        if event.entity_type == "task" && event.action == "updated" {
            let id = event.payload["task"]["id"].as_str().unwrap_or_default();
            recorded.lock().unwrap().push(id.to_owned());
        }
    });

    let project = djinn_db::test_support::make_project(&db, std::path::Path::new("cutover")).await;
    djinn_db::test_support::persist_project_github_installation_for_test(
        &db,
        &project.id,
        "acme",
        "widget",
        FIXTURE_INSTALLATION_ID,
    )
    .await;
    let epic = EpicRepository::new(db.clone(), EventBus::noop())
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
                title: "cutover",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events.clone());
    let source = tasks
        .create(
            &epic.id,
            "cutover source",
            "",
            "",
            "task",
            0,
            "",
            Some(consumer.fixture_status(seam)),
        )
        .await
        .unwrap();
    // The dependent carries acceptance criteria so that `start` is legal once
    // its blocker clears: without them the transition is refused by the AC gate
    // and a blocker assertion would be passing for the wrong reason.
    let dependent = tasks
        .create_fixture_with_ac(
            &epic.id,
            "cutover dependent",
            "",
            "",
            "task",
            0,
            "",
            Some("open"),
            Some(r#"["criterion"]"#),
        )
        .await
        .unwrap();
    tasks.add_blocker(&dependent.id, &source.id).await.unwrap();
    assert!(
        source.pr_url.is_none(),
        "routing must never have nullable PR data to infer from"
    );

    let fixture = Fixture {
        attempts: ProposalBuildAttemptRepository::new(db.clone()),
        build_attempt_id: format!("a{}", &source.id[1..]),
        db: db.clone(),
        tasks,
        events,
        project_id: project.id.clone(),
        epic_id: epic.id.clone(),
        source_id: source.id.clone(),
        dependent_id: dependent.id.clone(),
        remote: Arc::new(Mutex::new(("fixture-base".to_owned(), 0usize))),
        updates,
    };

    match case {
        ContractCase::UnresolvedOwner => {
            // An active epoch and a proposal, but no build attempt: ownership
            // is unresolvable, and nothing may be guessed from that.
            djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
            djinn_db::test_support::seed_direct_delivery_proposal_for_test(
                &db,
                &source.id,
                &source.id[..8],
            )
            .await;
        }
        _ => {
            let ledger_state = match case {
                ContractCase::DirectConflict => "conflict",
                // A consumer whose loop selects a status `task_integrated`
                // cannot close from can only ever meet an already-settled
                // generation, so persist that rather than re-derive it.
                ContractCase::ActiveResolvedDirect
                    if !consumer.resolves_direct_through_the_engine(seam) =>
                {
                    "applied"
                }
                _ => "applying",
            };
            djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test(
                &db,
                &epic.id,
                &source.id,
                Some(ledger_state),
            )
            .await;
        }
    }

    match case {
        ContractCase::SupportedDisabled => {
            djinn_db::test_support::disable_direct_delivery_epoch_for_test(&db).await;
            legacy_pr_identity(&fixture, false).await;
        }
        ContractCase::ActiveExplicitLegacy => legacy_pr_identity(&fixture, true).await,
        ContractCase::DirectWithStrayPrIdentity => {
            fixture
                .tasks
                .set_pr_url(&fixture.source_id, LEGACY_PR_URL)
                .await
                .unwrap();
        }
        ContractCase::ActiveResolvedDirect => {
            if consumer.resolves_direct_through_the_engine(seam) {
                // Reach the terminal state the way production does: through the
                // real engine and the real `TaskIntegrated` transition.
                let outcome = fixture
                    .run_engine()
                    .await
                    .expect("engine settles the resolved-direct fixture");
                assert!(
                    matches!(outcome, DeliveryOutcome::Integrated { .. }),
                    "resolved-direct fixture must integrate, got {outcome:?}"
                );
            }
        }
        ContractCase::MissingContract => {
            djinn_db::test_support::remove_direct_delivery_epoch_for_test(&db).await
        }
        ContractCase::UnknownContract => {
            djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&db).await
        }
        ContractCase::DirectApplying
        | ContractCase::DirectConflict
        | ContractCase::UnresolvedOwner => {}
    }

    if case.is_retained_legacy() && consumer.lands_the_legacy_pr() {
        land_legacy_pr(&fixture).await;
    }

    fixture
}

/// Walk a retained-legacy fixture through the legacy landing path production
/// uses: `approved → pr_draft → closed`, with the PR identity intact.
async fn land_legacy_pr(fixture: &Fixture) {
    for action in [TransitionAction::PrCreated, TransitionAction::PrMerge] {
        fixture
            .tasks
            .transition(
                &fixture.source_id,
                action,
                "coordinator",
                "system",
                None,
                None,
            )
            .await
            .expect("legacy fixture must walk its own landing path");
    }
}

/// Give a retained-legacy fixture the task-PR identity it is expected to keep.
async fn legacy_pr_identity(fixture: &Fixture, explicit_label: bool) {
    fixture
        .tasks
        .set_pr_url(&fixture.source_id, LEGACY_PR_URL)
        .await
        .unwrap();
    if explicit_label {
        fixture
            .tasks
            .update_labels(
                &fixture.source_id,
                &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#),
            )
            .await
            .unwrap();
    }
}

// ─── Entering each consumer at its production entry point ──────────────────

/// Run one consumer seam once against one fixture, and return its normalized
/// answer.
///
/// Every arm calls the real entry point. None of them constructs a
/// `DirectDeliveryAdmission`, a `TaskPrEligibility`, or a delivery outcome by
/// hand — routing comes out of the persisted rows the fixture wrote.
async fn invoke(consumer: Consumer, fixture: &Fixture, seam: usize) -> String {
    match consumer {
        Consumer::ReadyDispatch => {
            let engine = fixture.engine();
            let source = fixture.delivery_source();
            let continuation = crate::dispatch::task_dispatch::continue_ready_dispatch(
                fixture.db.clone(),
                &fixture.tasks,
                &fixture.source_id,
                || async move {
                    crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source))
                        .await
                },
                || async { Ok(()) },
            )
            .await;
            match continuation {
                Ok(continuation) => format!("{continuation:?}"),
                Err(_) => "Error".to_owned(),
            }
        }
        Consumer::RespawnGuard => {
            let engine = fixture.engine();
            let source = fixture.delivery_source();
            let task = fixture
                .tasks
                .get(&fixture.source_id)
                .await
                .unwrap()
                .unwrap();
            let decision = crate::dispatch::respawn_guard::run_respawn_guard_with_reconciler(
                &fixture.db,
                &fixture.source_id,
                "worker",
                task.pr_url.as_deref(),
                None,
                || async move {
                    crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source))
                        .await
                },
            )
            .await;
            format!("{decision:?}")
        }
        Consumer::SessionRecovery => {
            let engine = fixture.engine();
            let source = fixture.delivery_source();
            let reconcile = || async move {
                crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source))
                    .await
            };
            let admission = if seam == 0 {
                crate::dispatch::session_recovery::admit_zombie_session_release(
                    fixture.db.clone(),
                    &fixture.tasks,
                    &fixture.source_id,
                    reconcile,
                )
                .await
            } else {
                crate::dispatch::session_recovery::admit_execution_state_orphan_release(
                    fixture.db.clone(),
                    &fixture.tasks,
                    &fixture.source_id,
                    reconcile,
                )
                .await
            };
            format!("{admission:?}")
        }
        Consumer::SecondStrikeRetry => {
            let engine = fixture.engine();
            let source = fixture.delivery_source();
            let admission = super::retry::admit_second_strike_retry(
                fixture.db.clone(),
                &fixture.tasks,
                &fixture.source_id,
                || async move {
                    crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source))
                        .await
                },
            )
            .await;
            format!("{admission:?}")
        }
        Consumer::BlockerRelease => {
            // The production terminal transition. `emit_unblocked_tasks` runs
            // inside it and decides, from the ledger, whether the dependent is
            // genuinely released — a closed direct task without exact applied
            // evidence for its persisted merge SHA must not release anything.
            fixture.updates.lock().unwrap().clear();
            let mut transitioned = Ok(());
            for action in [TransitionAction::PrCreated, TransitionAction::PrMerge] {
                if let Err(error) = fixture
                    .tasks
                    .transition(
                        &fixture.source_id,
                        action,
                        "coordinator",
                        "system",
                        None,
                        None,
                    )
                    .await
                {
                    transitioned = Err(error);
                    break;
                }
            }
            let released = fixture
                .updates
                .lock()
                .unwrap()
                .iter()
                .filter(|id| id.as_str() == fixture.dependent_id.as_str())
                .count();
            match transitioned {
                Ok(_) => format!("closed:releases={released}"),
                Err(_) => format!("refused:releases={released}"),
            }
        }
        Consumer::ParentDisposition => {
            let plan = fixture
                .tasks
                .classify_parent_disposition(&DispositionScope::for_proposal_abort(
                    &fixture.source_id,
                    vec![fixture.epic_id.clone()],
                ))
                .await;
            match plan {
                Ok(plan) => plan
                    .findings
                    .iter()
                    .find(|finding| finding.task_id == fixture.source_id)
                    .map(|finding| format!("{:?}", finding.disposition))
                    .unwrap_or_else(|| "NoFinding".to_owned()),
                Err(_) => "Error".to_owned(),
            }
        }
        Consumer::BoardHealth => match fixture.tasks.board_health(30).await {
            Ok(health) => health["direct_delivery"]["findings"]
                .as_array()
                .and_then(|findings| {
                    findings
                        .iter()
                        .find(|finding| finding["id"] == fixture.source_id.as_str())
                })
                .map(|finding| {
                    format!(
                        "classified={}",
                        finding["classification"].as_str().unwrap_or("<absent>")
                    )
                })
                .unwrap_or_else(|| "absent".to_owned()),
            Err(_) => "Error".to_owned(),
        },
        Consumer::MergedClassification => {
            let listed = fixture
                .tasks
                .list_filtered(ListQuery {
                    project_id: Some(fixture.project_id.clone()),
                    status: Some("merged".into()),
                    issue_type: None,
                    priority: None,
                    label: None,
                    text: None,
                    parent: None,
                    sort: "created".into(),
                    limit: 50,
                    offset: 0,
                })
                .await;
            match listed {
                Ok(result) if result.tasks.iter().any(|task| task.id == fixture.source_id) => {
                    "merged".to_owned()
                }
                Ok(_) => "not_merged".to_owned(),
                Err(_) => "Error".to_owned(),
            }
        }
        Consumer::TaskPrAdoption => {
            let task = fixture
                .tasks
                .get(&fixture.source_id)
                .await
                .unwrap()
                .unwrap();
            let outcome = crate::supervisor_impl::supervisor_pr_open(
                &task_run_spec(fixture, &task),
                &task,
                &supervisor_callbacks(fixture),
            )
            .await;
            outcome_kind(&outcome).to_owned()
        }
        Consumer::PrPoller => {
            // The pollers only reach a forge effect through a real
            // installation-authenticated client. Route it at a local server so
            // the retained-legacy half is a positive observation rather than an
            // absence caused by missing deployment configuration.
            let server = wiremock::MockServer::start().await;
            djinn_provider::github_app::installations::prime_cache_for_tests(
                FIXTURE_INSTALLATION_ID,
                "ghs_cutover_fixture",
            );
            crate::pr_poller::installation::set_installation_client_base_url_for_test(Some(
                server.uri(),
            ));

            let (tx, _rx) = tokio::sync::broadcast::channel(64);
            let (mut actor, cancel) =
                crate::test_helpers::make_coordinator_actor_cancellable(&fixture.db, &tx);
            match seam {
                0 => actor.poll_pr_draft_tasks().await,
                1 => actor.poll_pr_review_tasks().await,
                _ => actor.reconcile_blindspot_merged_prs().await,
            }
            cancel.cancel();
            crate::pr_poller::installation::set_installation_client_base_url_for_test(None);
            "polled".to_owned()
        }
    }
}

/// Name the outcome variant without depending on its payload rendering.
fn outcome_kind(outcome: &djinn_runtime::TaskRunOutcome) -> String {
    let rendered = format!("{outcome:?}");
    rendered
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .find(|part| !part.is_empty())
        .unwrap_or("Unknown")
        .to_owned()
}

fn task_run_spec(fixture: &Fixture, task: &djinn_core::models::Task) -> djinn_runtime::TaskRunSpec {
    djinn_runtime::TaskRunSpec {
        task_run_id: "cutover-run".into(),
        task_attempt_id: None,
        task_id: task.id.clone(),
        execution_generation: 0,
        project_id: fixture.project_id.clone(),
        trigger: djinn_core::models::TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "task/cutover".into(),
        flow: djinn_runtime::SupervisorFlow::NewTask,
        model_id_per_role: std::collections::HashMap::new(),
        read_source_project_ids: vec![],
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

fn supervisor_callbacks(fixture: &Fixture) -> crate::supervisor_impl::SupervisorCallbackContext {
    crate::supervisor_impl::SupervisorCallbackContext {
        agent_context: crate::test_helpers::coordinator_context_from_db(
            fixture.db.clone(),
            tokio_util::sync::CancellationToken::new(),
        ),
        cancel: tokio_util::sync::CancellationToken::new(),
        provider_override: None,
    }
}

/// Persist a cell, run one seam of its consumer, and observe the result at
/// production boundaries.
async fn run_cell(consumer: Consumer, case: ContractCase, seam: usize) -> ConsumerObservation {
    let boundary = boundary_operations_scope().await;
    let fixture = build_fixture(consumer, case, seam).await;
    let before = fixture.snapshot().await;
    let checkpoint = boundary.checkpoint();

    let decision = invoke(consumer, &fixture, seam).await;

    let operations = boundary.operations_since(checkpoint);
    let after = fixture.snapshot().await;
    ConsumerObservation {
        decision,
        task_pr_effects: operations
            .iter()
            .filter(|op| TASK_PR_FORGE_OPERATIONS.contains(op))
            .copied()
            .collect(),
        direct_appends: operations
            .iter()
            .filter(|op| matches!(op, BoundaryOperation::DirectAppend))
            .count(),
        persisted_changed: after != before,
        persisted_pr_url: after.source_task["pr_url"].as_str().map(str::to_owned),
        unchanged_beyond_a_park: unchanged_beyond_a_park(&before, &after),
    }
}

/// True when the only persisted difference is the source task's own status.
fn unchanged_beyond_a_park(before: &PersistedSnapshot, after: &PersistedSnapshot) -> bool {
    let without_park = |task: &serde_json::Value| {
        let mut task = task.clone();
        if let Some(task) = task.as_object_mut() {
            for field in ["status", "updated_at"] {
                task.remove(field);
            }
        }
        task
    };
    without_park(&before.source_task) == without_park(&after.source_task)
        && before.dependent_task == after.dependent_task
        && before.attempt == after.attempt
        && before.ledger == after.ledger
        && before.attempt_count == after.attempt_count
        && before.delivery_count == after.delivery_count
        && before.task_attempt_count == after.task_attempt_count
}

/// One row of the matrix, rendered so the expectation states the decision, the
/// forge effects, the appends, and whether anything persisted moved.
fn render(consumer: Consumer, case: ContractCase, seam: usize, o: &ConsumerObservation) -> String {
    format!(
        "{consumer:?}/{case:?}/{seam} => {} | pr_effects={:?} | appends={} | changed={}",
        o.decision, o.task_pr_effects, o.direct_appends, o.persisted_changed
    )
}

// ─── AC1 / AC3 / AC4: the matrix ───────────────────────────────────────────

/// Run one consumer seam against every persisted contract state, assert the
/// invariants that hold for whole classes of cells, and return the rendered
/// rows for comparison against that seam's expectation.
///
/// The suite is split one test per consumer seam only because each cell builds
/// its own migrated database and the repository caps a single test at 90s.
/// Every seam goes through this same runner, over the same fixture builder, and
/// produces the same row shape.
async fn matrix_rows(consumer: Consumer, seam: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for case in ContractCase::ALL {
        let observation = run_cell(consumer, case, seam).await;

        // AC3: no task-PR forge effect may be reached for an active direct
        // identity, at any consumer, in any direct state — and a stray
        // persisted PR URL must not buy one back.
        if case.is_direct_identity() {
            assert!(
                observation.task_pr_effects.is_empty(),
                "{consumer:?}/{case:?}/{seam}: direct identity reached task-PR effects {:?}",
                observation.task_pr_effects
            );
        }
        // AC4: an unreadable contract or unresolvable owner reaches no forge
        // effect and no delivery append before it refuses.
        if case.is_fail_closed() {
            assert!(
                observation.task_pr_effects.is_empty(),
                "{consumer:?}/{case:?}/{seam}: fail-closed row reached task-PR effects {:?}",
                observation.task_pr_effects
            );
            assert_eq!(
                observation.direct_appends, 0,
                "{consumer:?}/{case:?}/{seam}: fail-closed row must reach no append"
            );
            if consumer.has_an_admission_gate() {
                assert!(
                    observation.unchanged_beyond_a_park,
                    "{consumer:?}/{case:?}/{seam}: a fail-closed row guessed state \
                     beyond its own durable park"
                );
            }
        }
        // A direct identity is routed, never guessed. A mid-flight generation
        // is excluded on purpose: reconciling it is exactly what these
        // consumers own, and integrating it legitimately moves the ledger, the
        // attempt head, the task, and the dependent. A *settled* or conflicted
        // direct identity has nothing left to do and must move nothing.
        if case.is_direct_identity()
            && !case.has_midflight_generation()
            && consumer.has_an_admission_gate()
        {
            assert!(
                observation.unchanged_beyond_a_park,
                "{consumer:?}/{case:?}/{seam}: a direct identity moved state its \
                 consumer does not own"
            );
        }
        // Only a mid-flight generation may move the attempt branch.
        if observation.direct_appends > 0 {
            assert!(
                case.has_midflight_generation(),
                "{consumer:?}/{case:?}/{seam}: only a mid-flight generation may append"
            );
        }
        // AC3: retained-legacy rows keep their persisted PR identity.
        if case.is_retained_legacy() {
            assert_eq!(
                observation.persisted_pr_url.as_deref(),
                Some(LEGACY_PR_URL),
                "{consumer:?}/{case:?}/{seam}: retained legacy lost its PR identity"
            );
        }
        rows.push(render(consumer, case, seam, &observation));
    }
    rows
}

async fn assert_matrix(consumer: Consumer, seam: usize, expected: &str) {
    assert_eq!(
        matrix_rows(consumer, seam).await.join("\n"),
        expected.trim(),
        "{consumer:?}/seam {seam}: the consumer cutover matrix changed"
    );
}

macro_rules! consumer_matrix_test {
    ($name:ident, $consumer:expr, $seam:expr, $expected:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            assert_matrix($consumer, $seam, $expected).await;
        }
    };
}

consumer_matrix_test!(
    ready_dispatch_routes_by_persisted_contract_state,
    Consumer::ReadyDispatch,
    0,
    EXPECTED_READY_DISPATCH
);
consumer_matrix_test!(
    respawn_guard_routes_by_persisted_contract_state,
    Consumer::RespawnGuard,
    0,
    EXPECTED_RESPAWN_GUARD
);
consumer_matrix_test!(
    session_recovery_zombie_seam_routes_by_persisted_contract_state,
    Consumer::SessionRecovery,
    0,
    EXPECTED_SESSION_RECOVERY_ZOMBIE
);
consumer_matrix_test!(
    session_recovery_orphan_seam_routes_by_persisted_contract_state,
    Consumer::SessionRecovery,
    1,
    EXPECTED_SESSION_RECOVERY_ORPHAN
);
consumer_matrix_test!(
    second_strike_retry_routes_by_persisted_contract_state,
    Consumer::SecondStrikeRetry,
    0,
    EXPECTED_SECOND_STRIKE_RETRY
);
consumer_matrix_test!(
    blocker_release_routes_by_persisted_contract_state,
    Consumer::BlockerRelease,
    0,
    EXPECTED_BLOCKER_RELEASE
);
consumer_matrix_test!(
    parent_disposition_routes_by_persisted_contract_state,
    Consumer::ParentDisposition,
    0,
    EXPECTED_PARENT_DISPOSITION
);
consumer_matrix_test!(
    board_health_routes_by_persisted_contract_state,
    Consumer::BoardHealth,
    0,
    EXPECTED_BOARD_HEALTH
);
consumer_matrix_test!(
    merged_classification_routes_by_persisted_contract_state,
    Consumer::MergedClassification,
    0,
    EXPECTED_MERGED_CLASSIFICATION
);
consumer_matrix_test!(
    task_pr_adoption_routes_by_persisted_contract_state,
    Consumer::TaskPrAdoption,
    0,
    EXPECTED_TASK_PR_ADOPTION
);
consumer_matrix_test!(
    pr_draft_poller_routes_by_persisted_contract_state,
    Consumer::PrPoller,
    0,
    EXPECTED_PR_DRAFT_POLLER
);
consumer_matrix_test!(
    pr_review_poller_routes_by_persisted_contract_state,
    Consumer::PrPoller,
    1,
    EXPECTED_PR_REVIEW_POLLER
);
consumer_matrix_test!(
    merged_pr_reconciler_routes_by_persisted_contract_state,
    Consumer::PrPoller,
    2,
    EXPECTED_MERGED_PR_RECONCILER
);

/// Every consumer named by the proposal is in the table, every seam of every
/// consumer has an expectation, and every expectation covers every persisted
/// state. Guards against a consumer or seam being added and silently skipped.
#[test]
fn every_named_consumer_seam_has_a_full_matrix_expectation() {
    assert_eq!(Consumer::ALL.len(), 10, "the proposal names ten consumers");
    for consumer in Consumer::ALL {
        for seam in 0..consumer.seams() {
            let expected = expected_for(consumer, seam);
            assert_eq!(
                expected.trim().lines().count(),
                ContractCase::ALL.len(),
                "{consumer:?}/{seam}: expectation does not cover every persisted state"
            );
            for (row, case) in expected.trim().lines().zip(ContractCase::ALL) {
                assert!(
                    row.starts_with(&format!("{consumer:?}/{case:?}/{seam} =>")),
                    "{consumer:?}/{seam}: expectation row out of order: {row}"
                );
            }
        }
    }
}

/// The two recovery-release seams are the same question asked at two sites, so
/// their observations must be identical — otherwise one seam's safety is being
/// inferred from the other's coverage.
#[test]
fn both_recovery_release_seams_observe_the_same_matrix() {
    let normalize = |expected: &str, seam: usize| {
        expected
            .trim()
            .replace(&format!("/{seam} =>"), " =>")
            .to_owned()
    };
    assert_eq!(
        normalize(EXPECTED_SESSION_RECOVERY_ZOMBIE, 0),
        normalize(EXPECTED_SESSION_RECOVERY_ORPHAN, 1),
        "the zombie and orphan release seams diverged"
    );
}

fn expected_for(consumer: Consumer, seam: usize) -> &'static str {
    match (consumer, seam) {
        (Consumer::ReadyDispatch, _) => EXPECTED_READY_DISPATCH,
        (Consumer::RespawnGuard, _) => EXPECTED_RESPAWN_GUARD,
        (Consumer::SessionRecovery, 0) => EXPECTED_SESSION_RECOVERY_ZOMBIE,
        (Consumer::SessionRecovery, _) => EXPECTED_SESSION_RECOVERY_ORPHAN,
        (Consumer::SecondStrikeRetry, _) => EXPECTED_SECOND_STRIKE_RETRY,
        (Consumer::BlockerRelease, _) => EXPECTED_BLOCKER_RELEASE,
        (Consumer::ParentDisposition, _) => EXPECTED_PARENT_DISPOSITION,
        (Consumer::BoardHealth, _) => EXPECTED_BOARD_HEALTH,
        (Consumer::MergedClassification, _) => EXPECTED_MERGED_CLASSIFICATION,
        (Consumer::TaskPrAdoption, _) => EXPECTED_TASK_PR_ADOPTION,
        (Consumer::PrPoller, 0) => EXPECTED_PR_DRAFT_POLLER,
        (Consumer::PrPoller, 1) => EXPECTED_PR_REVIEW_POLLER,
        (Consumer::PrPoller, _) => EXPECTED_MERGED_PR_RECONCILER,
    }
}

/// Legacy states enter the pre-existing spawn/task-PR continuation; a settled
/// or conflicted generation stops dispatch without entering it; a mid-flight
/// generation is consumed by the engine first; an unreadable contract or
/// unresolvable owner parks.
const EXPECTED_READY_DISPATCH: &str = r#"
ReadyDispatch/SupportedDisabled/0 => LegacyDispatch(()) | pr_effects=[] | appends=0 | changed=false
ReadyDispatch/ActiveExplicitLegacy/0 => LegacyDispatch(()) | pr_effects=[] | appends=0 | changed=false
ReadyDispatch/ActiveResolvedDirect/0 => Settled | pr_effects=[] | appends=0 | changed=false
ReadyDispatch/DirectApplying/0 => Reconciled | pr_effects=[] | appends=1 | changed=true
ReadyDispatch/DirectConflict/0 => Settled | pr_effects=[] | appends=0 | changed=false
ReadyDispatch/DirectWithStrayPrIdentity/0 => Reconciled | pr_effects=[] | appends=1 | changed=true
ReadyDispatch/UnresolvedOwner/0 => Parked | pr_effects=[] | appends=0 | changed=true
ReadyDispatch/MissingContract/0 => Parked | pr_effects=[] | appends=0 | changed=true
ReadyDispatch/UnknownContract/0 => Parked | pr_effects=[] | appends=0 | changed=true
"#;
/// The legacy rows keep the guard's pre-existing open-PR adoption — the exact
/// persisted PR identity comes back out of the decision. Every direct state
/// defers instead, and the fail-closed states defer after parking.
const EXPECTED_RESPAWN_GUARD: &str = r#"
RespawnGuard/SupportedDisabled/0 => Adopted { pr_url: "https://github.com/acme/widget/pull/42" } | pr_effects=[] | appends=0 | changed=false
RespawnGuard/ActiveExplicitLegacy/0 => Adopted { pr_url: "https://github.com/acme/widget/pull/42" } | pr_effects=[] | appends=0 | changed=false
RespawnGuard/ActiveResolvedDirect/0 => Defer(RespawnGuard) | pr_effects=[] | appends=0 | changed=false
RespawnGuard/DirectApplying/0 => Defer(RespawnGuard) | pr_effects=[] | appends=1 | changed=true
RespawnGuard/DirectConflict/0 => Defer(RespawnGuard) | pr_effects=[] | appends=0 | changed=false
RespawnGuard/DirectWithStrayPrIdentity/0 => Defer(RespawnGuard) | pr_effects=[] | appends=1 | changed=true
RespawnGuard/UnresolvedOwner/0 => Defer(RespawnGuard) | pr_effects=[] | appends=0 | changed=true
RespawnGuard/MissingContract/0 => Defer(RespawnGuard) | pr_effects=[] | appends=0 | changed=true
RespawnGuard/UnknownContract/0 => Defer(RespawnGuard) | pr_effects=[] | appends=0 | changed=true
"#;
/// `Refuse(ReconcileFailed)` for a mid-flight generation is `i5fn` behaviour and
/// not a defect: the recovery loops select `in_progress`, which
/// `task_integrated` can never close from, so the engine appends and then
/// reports the generation unintegrable rather than spinning. The release is
/// still refused, which is the property that matters here.
const EXPECTED_SESSION_RECOVERY_ZOMBIE: &str = r#"
SessionRecovery/SupportedDisabled/0 => Release | pr_effects=[] | appends=0 | changed=false
SessionRecovery/ActiveExplicitLegacy/0 => Release | pr_effects=[] | appends=0 | changed=false
SessionRecovery/ActiveResolvedDirect/0 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SessionRecovery/DirectApplying/0 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SessionRecovery/DirectConflict/0 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SessionRecovery/DirectWithStrayPrIdentity/0 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SessionRecovery/UnresolvedOwner/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SessionRecovery/MissingContract/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SessionRecovery/UnknownContract/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
"#;
/// Identical to the zombie seam by contract — see
/// [`both_recovery_release_seams_observe_the_same_matrix`].
const EXPECTED_SESSION_RECOVERY_ORPHAN: &str = r#"
SessionRecovery/SupportedDisabled/1 => Release | pr_effects=[] | appends=0 | changed=false
SessionRecovery/ActiveExplicitLegacy/1 => Release | pr_effects=[] | appends=0 | changed=false
SessionRecovery/ActiveResolvedDirect/1 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SessionRecovery/DirectApplying/1 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SessionRecovery/DirectConflict/1 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SessionRecovery/DirectWithStrayPrIdentity/1 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SessionRecovery/UnresolvedOwner/1 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SessionRecovery/MissingContract/1 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SessionRecovery/UnknownContract/1 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
"#;
/// The arbiter's retry escalation is the third site behind the same shared
/// admission body, entered on its own so its safety is not inferred from the
/// recovery seams' coverage.
const EXPECTED_SECOND_STRIKE_RETRY: &str = r#"
SecondStrikeRetry/SupportedDisabled/0 => Release | pr_effects=[] | appends=0 | changed=false
SecondStrikeRetry/ActiveExplicitLegacy/0 => Release | pr_effects=[] | appends=0 | changed=false
SecondStrikeRetry/ActiveResolvedDirect/0 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SecondStrikeRetry/DirectApplying/0 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SecondStrikeRetry/DirectConflict/0 => Refuse(Settled) | pr_effects=[] | appends=0 | changed=false
SecondStrikeRetry/DirectWithStrayPrIdentity/0 => Refuse(ReconcileFailed) | pr_effects=[] | appends=1 | changed=false
SecondStrikeRetry/UnresolvedOwner/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SecondStrikeRetry/MissingContract/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
SecondStrikeRetry/UnknownContract/0 => Refuse(FailedClosed) | pr_effects=[] | appends=0 | changed=true
"#;
/// Every cell walks the legacy landing path (`approved → pr_draft → closed`).
/// A retained-legacy blocker releases its dependent; an active direct blocker
/// closed that way does **not**, because `emit_unblocked_tasks` demands an
/// applied generation whose candidate equals the blocker's persisted merge SHA
/// and this path sets none. That is the fail-closed property: closing a direct
/// task through the legacy path cannot release anything.
///
/// # Observed divergence — reported, not bent
///
/// `DirectWithStrayPrIdentity` releases its dependent. `emit_unblocked_tasks`,
/// `board_health`'s direct section, and the `merged` classification all use
/// `pr_url IS NULL` as their legacy discriminator, while the coordinator's
/// `admit_direct_delivery` uses the explicit legacy **label**. A canonically
/// direct-owned task that somehow carries a PR URL is therefore direct to the
/// coordinator and legacy to the ledger SQL. Production never mints that row —
/// task-PR adoption is refused for direct identities, so `pr_url` stays null —
/// but the two discriminators are not the same predicate, and this row is
/// pinned so the difference is visible rather than latent.
const EXPECTED_BLOCKER_RELEASE: &str = r#"
BlockerRelease/SupportedDisabled/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/ActiveExplicitLegacy/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/ActiveResolvedDirect/0 => closed:releases=0 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/DirectApplying/0 => closed:releases=0 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/DirectConflict/0 => closed:releases=0 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/DirectWithStrayPrIdentity/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/UnresolvedOwner/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/MissingContract/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
BlockerRelease/UnknownContract/0 => closed:releases=1 | pr_effects=[] | appends=0 | changed=true
"#;
/// Disposition follows the status the delivery drove the child into. Only the
/// resolved-direct child is already terminal; everything else — including a
/// conflicted generation — parks rather than completing the parent.
const EXPECTED_PARENT_DISPOSITION: &str = r#"
ParentDisposition/SupportedDisabled/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/ActiveExplicitLegacy/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/ActiveResolvedDirect/0 => RetainedAlreadyTerminal | pr_effects=[] | appends=0 | changed=false
ParentDisposition/DirectApplying/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/DirectConflict/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/DirectWithStrayPrIdentity/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/UnresolvedOwner/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/MissingContract/0 => Park | pr_effects=[] | appends=0 | changed=false
ParentDisposition/UnknownContract/0 => Park | pr_effects=[] | appends=0 | changed=false
"#;
/// The additive direct-delivery section admits a task only under an active
/// epoch, a canonical owner, an active attempt, and a null PR identity —
/// `integrated` only for the exact applied candidate the task closed with.
const EXPECTED_BOARD_HEALTH: &str = r#"
BoardHealth/SupportedDisabled/0 => absent | pr_effects=[] | appends=0 | changed=false
BoardHealth/ActiveExplicitLegacy/0 => absent | pr_effects=[] | appends=0 | changed=false
BoardHealth/ActiveResolvedDirect/0 => classified=integrated | pr_effects=[] | appends=0 | changed=false
BoardHealth/DirectApplying/0 => classified=applying | pr_effects=[] | appends=0 | changed=false
BoardHealth/DirectConflict/0 => classified=conflict | pr_effects=[] | appends=0 | changed=false
BoardHealth/DirectWithStrayPrIdentity/0 => absent | pr_effects=[] | appends=0 | changed=false
BoardHealth/UnresolvedOwner/0 => absent | pr_effects=[] | appends=0 | changed=false
BoardHealth/MissingContract/0 => absent | pr_effects=[] | appends=0 | changed=false
BoardHealth/UnknownContract/0 => absent | pr_effects=[] | appends=0 | changed=false
"#;
/// The retained-legacy rows land their PR and are classified merged by the
/// pre-existing rule. The direct row is classified merged only on exact applied
/// evidence for its persisted merge SHA; `applying`, `conflict`, an unresolved
/// owner, and an unreadable contract all fail closed to not-merged.
const EXPECTED_MERGED_CLASSIFICATION: &str = r#"
MergedClassification/SupportedDisabled/0 => merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/ActiveExplicitLegacy/0 => merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/ActiveResolvedDirect/0 => merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/DirectApplying/0 => not_merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/DirectConflict/0 => not_merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/DirectWithStrayPrIdentity/0 => not_merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/UnresolvedOwner/0 => not_merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/MissingContract/0 => not_merged | pr_effects=[] | appends=0 | changed=false
MergedClassification/UnknownContract/0 => not_merged | pr_effects=[] | appends=0 | changed=false
"#;
/// `Escalated` is the direct exclusion: the supervisor refuses adoption at its
/// eligibility gate, before any mirror clone or forge call. `Failed` is the
/// legacy row getting *past* that gate and stopping at this deployment's
/// unconfigured GitHub App — the distinction is exactly what proves legacy is
/// not excluded. The fail-closed rows escalate too, but only after durably
/// parking, which is why they alone move persisted state.
const EXPECTED_TASK_PR_ADOPTION: &str = r#"
TaskPrAdoption/SupportedDisabled/0 => Failed | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/ActiveExplicitLegacy/0 => Failed | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/ActiveResolvedDirect/0 => Escalated | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/DirectApplying/0 => Escalated | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/DirectConflict/0 => Escalated | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/DirectWithStrayPrIdentity/0 => Escalated | pr_effects=[] | appends=0 | changed=false
TaskPrAdoption/UnresolvedOwner/0 => Escalated | pr_effects=[] | appends=0 | changed=true
TaskPrAdoption/MissingContract/0 => Escalated | pr_effects=[] | appends=0 | changed=true
TaskPrAdoption/UnknownContract/0 => Escalated | pr_effects=[] | appends=0 | changed=true
"#;
/// Every row reaches zero forge effects here because the draft poller's
/// minimum-age guard fires before its first fetch on a freshly-seen task. That
/// makes this table a proof of the *fail-closed* half only — the fail-closed
/// rows still park, direct rows still change nothing — so the positive control
/// that the same poller does reach its forge boundary for a legacy row is
/// [`pr_draft_poller_reaches_its_forge_boundary_for_a_retained_legacy_row`].
const EXPECTED_PR_DRAFT_POLLER: &str = r#"
PrPoller/SupportedDisabled/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/ActiveExplicitLegacy/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/ActiveResolvedDirect/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectApplying/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectConflict/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectWithStrayPrIdentity/0 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/UnresolvedOwner/0 => polled | pr_effects=[] | appends=0 | changed=true
PrPoller/MissingContract/0 => polled | pr_effects=[] | appends=0 | changed=true
PrPoller/UnknownContract/0 => polled | pr_effects=[] | appends=0 | changed=true
"#;
/// The positive control for the cutover: a retained-legacy row reaches a real
/// installation-authenticated `TaskPrReviewPoll`, and every direct identity —
/// including one carrying a stray PR URL, which is the only reason the poller
/// could even consider it — reaches none.
const EXPECTED_PR_REVIEW_POLLER: &str = r#"
PrPoller/SupportedDisabled/1 => polled | pr_effects=[TaskPrReviewPoll] | appends=0 | changed=false
PrPoller/ActiveExplicitLegacy/1 => polled | pr_effects=[TaskPrReviewPoll] | appends=0 | changed=false
PrPoller/ActiveResolvedDirect/1 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectApplying/1 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectConflict/1 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectWithStrayPrIdentity/1 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/UnresolvedOwner/1 => polled | pr_effects=[] | appends=0 | changed=true
PrPoller/MissingContract/1 => polled | pr_effects=[] | appends=0 | changed=true
PrPoller/UnknownContract/1 => polled | pr_effects=[] | appends=0 | changed=true
"#;
/// The blind-spot reconciler is the last poller that could adopt a merge into a
/// direct task. A retained-legacy row reaches `TaskPrMergedPoll`; the stray-PR
/// direct row is selected by the same query and then refused at the eligibility
/// gate, reaching nothing.
const EXPECTED_MERGED_PR_RECONCILER: &str = r#"
PrPoller/SupportedDisabled/2 => polled | pr_effects=[TaskPrMergedPoll] | appends=0 | changed=false
PrPoller/ActiveExplicitLegacy/2 => polled | pr_effects=[TaskPrMergedPoll] | appends=0 | changed=false
PrPoller/ActiveResolvedDirect/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectApplying/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectConflict/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/DirectWithStrayPrIdentity/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/UnresolvedOwner/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/MissingContract/2 => polled | pr_effects=[] | appends=0 | changed=false
PrPoller/UnknownContract/2 => polled | pr_effects=[] | appends=0 | changed=false
"#;

// ─── Positive control for the draft poller's own forge boundary ────────────

/// The draft poller's minimum-age guard makes its matrix rows an absence, so
/// this pays the guard's wall-clock cost once and shows the same poller does
/// reach `TaskPrStatusPoll` for a retained-legacy row — and still reaches
/// nothing for a direct identity that the same query selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr_draft_poller_reaches_its_forge_boundary_for_a_retained_legacy_row() {
    let server = wiremock::MockServer::start().await;
    djinn_provider::github_app::installations::prime_cache_for_tests(
        FIXTURE_INSTALLATION_ID,
        "ghs_cutover_fixture",
    );

    let mut observed = Vec::new();
    for case in [
        ContractCase::SupportedDisabled,
        ContractCase::DirectWithStrayPrIdentity,
    ] {
        let boundary = boundary_operations_scope().await;
        let fixture = build_fixture(Consumer::PrPoller, case, 0).await;
        let (tx, _rx) = tokio::sync::broadcast::channel(64);
        let (mut actor, cancel) =
            crate::test_helpers::make_coordinator_actor_cancellable(&fixture.db, &tx);
        crate::pr_poller::installation::set_installation_client_base_url_for_test(Some(
            server.uri(),
        ));

        // First tick records when the task was first seen in `pr_draft`; the
        // guard deliberately refuses to fetch check-runs before they can exist.
        actor.poll_pr_draft_tasks().await;
        tokio::time::sleep(std::time::Duration::from_secs(DRAFT_POLL_MIN_AGE_SECS + 1)).await;

        let checkpoint = boundary.checkpoint();
        actor.poll_pr_draft_tasks().await;
        let effects: Vec<BoundaryOperation> = boundary
            .operations_since(checkpoint)
            .into_iter()
            .filter(|op| TASK_PR_FORGE_OPERATIONS.contains(op))
            .collect();
        cancel.cancel();
        crate::pr_poller::installation::set_installation_client_base_url_for_test(None);
        observed.push(format!("{case:?} => {effects:?}"));
    }

    assert_eq!(
        observed,
        vec![
            "SupportedDisabled => [TaskPrStatusPoll]".to_owned(),
            "DirectWithStrayPrIdentity => []".to_owned(),
        ],
        "the draft poller must keep its legacy fetch and refuse a direct identity"
    );
}

// ─── AC2: the delivery-shaped facts the matrix rows summarise ──────────────

/// Direct `Applied` is terminal with no `pr_url`, `Applying` is reconciled
/// before any spawn or recovery decision, `Conflict` stays parked and holds
/// both its dependent and its parent, and a corrected rework integrates exactly
/// once and releases its dependent exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_delivery_terminal_states_are_exact_and_release_exactly_once() {
    // ── Applying reconciles before the legacy spawn continuation ──────────
    let applying = build_fixture(Consumer::ReadyDispatch, ContractCase::DirectApplying, 0).await;
    let fixture = &applying;
    let engine = fixture.engine();
    let source = fixture.delivery_source();
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = spawns.clone();
    fixture.updates.lock().unwrap().clear();
    let continuation = crate::dispatch::task_dispatch::continue_ready_dispatch(
        fixture.db.clone(),
        &fixture.tasks,
        &fixture.source_id,
        || async move {
            crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source)).await
        },
        || async move {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(
        continuation,
        crate::dispatch::task_dispatch::ReadyDispatchContinuation::Reconciled
    );
    assert_eq!(
        spawns.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a reconciled generation must never enter the legacy spawn continuation"
    );

    // ── Applied/closed is terminal, exact, and carries no PR identity ─────
    let settled = fixture
        .tasks
        .get(&fixture.source_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            settled.status.as_str(),
            settled.merge_commit_sha.as_deref(),
            settled.close_reason.as_deref(),
            settled.pr_url.as_deref(),
        ),
        ("closed", Some("fixture-candidate"), Some("completed"), None),
        "direct integration must close on its exact candidate with no PR identity"
    );
    let generations = djinn_db::test_support::direct_delivery_generations_for_test(
        &fixture.db,
        &fixture.source_id,
    )
    .await;
    assert_eq!(generations.len(), 1);
    assert_eq!(
        (generations[0].state.as_str(), generations[0].applied),
        ("applied", true)
    );
    // The dependent is released exactly once, by the integration itself.
    assert_eq!(
        fixture
            .updates
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == fixture.dependent_id.as_str())
            .count(),
        1,
        "an integrated generation releases its dependent exactly once"
    );
    // Replaying the same ready-dispatch pass adds nothing.
    let engine = fixture.engine();
    let source = fixture.delivery_source();
    fixture.updates.lock().unwrap().clear();
    let replay = crate::dispatch::task_dispatch::continue_ready_dispatch(
        fixture.db.clone(),
        &fixture.tasks,
        &fixture.source_id,
        || async move {
            crate::dispatch::wave_dispatch::run_direct_completion(|| engine.deliver(source)).await
        },
        || async { Ok(()) },
    )
    .await
    .unwrap();
    assert_eq!(
        replay,
        crate::dispatch::task_dispatch::ReadyDispatchContinuation::Settled
    );
    assert!(
        fixture.updates.lock().unwrap().is_empty(),
        "a settled generation must emit no second release"
    );

    // ── Conflict stays parked and holds its dependent and its parent ──────
    //
    // The conflict is produced by the engine, not seeded: a fixture that simply
    // wrote `state='conflict'` would prove nothing about who parks the attempt.
    let conflicted = build_fixture(Consumer::ReadyDispatch, ContractCase::DirectApplying, 0).await;
    assert_eq!(
        djinn_db::test_support::remove_task_delivery_rows_for_test(
            &conflicted.db,
            &conflicted.source_id
        )
        .await,
        1,
        "the conflict fixture must start from an attempt with no generation yet"
    );
    let conflicting_engine = conflicted.conflicting_engine();
    let conflict_outcome = crate::dispatch::wave_dispatch::run_direct_completion(|| {
        conflicting_engine.deliver(conflicted.delivery_source())
    })
    .await
    .unwrap();
    assert_eq!(
        conflict_outcome,
        DeliveryOutcome::ConflictParked {
            reason: "fixture conflict".into()
        }
    );
    assert_eq!(
        conflicted
            .attempts
            .get(&conflicted.build_attempt_id)
            .await
            .unwrap()
            .unwrap()
            .park_reason,
        Some(djinn_core::models::DirectDeliveryParkReason::DeliveryConflict),
        "a conflicted generation must leave its attempt parked"
    );
    assert!(
        conflicted
            .tasks
            .transition(
                &conflicted.dependent_id,
                TransitionAction::Start,
                "coordinator",
                "system",
                None,
                None,
            )
            .await
            .is_err(),
        "a conflicted blocker must not release its dependent"
    );
    let plan = conflicted
        .tasks
        .classify_parent_disposition(&DispositionScope::for_proposal_abort(
            &conflicted.source_id,
            vec![conflicted.epic_id.clone()],
        ))
        .await
        .unwrap();
    assert_eq!(
        plan.findings
            .iter()
            .find(|finding| finding.task_id == conflicted.source_id)
            .map(|finding| finding.disposition.clone()),
        Some(djinn_db::ChildDisposition::Park),
        "a conflicted child must park its parent rather than complete it"
    );

    // ── The corrected rework integrates once and releases once ────────────
    let engine = conflicted.engine();
    conflicted.updates.lock().unwrap().clear();
    let corrected = crate::dispatch::wave_dispatch::run_direct_completion(|| {
        engine.deliver(DeliverySource {
            task_id: conflicted.source_id.clone(),
            delivery_generation: 2,
            transition_id: "fixture-rework".into(),
            source_sha: "fixture-source-2".into(),
            normalized_patch: "fixture-patch-2".into(),
        })
    })
    .await
    .unwrap();
    assert_eq!(
        corrected,
        DeliveryOutcome::Integrated {
            candidate_sha: "fixture-candidate".into()
        }
    );
    let generations = djinn_db::test_support::direct_delivery_generations_for_test(
        &conflicted.db,
        &conflicted.source_id,
    )
    .await;
    assert_eq!(
        generations
            .iter()
            .map(|row| (row.delivery_generation, row.state.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "conflict"), (2, "applied")],
        "the corrected rework must be a new immutable generation, not a rewrite"
    );
    assert_eq!(
        conflicted
            .updates
            .lock()
            .unwrap()
            .iter()
            .filter(|id| id.as_str() == conflicted.dependent_id.as_str())
            .count(),
        1,
        "the corrected generation releases the dependent exactly once"
    );
    assert!(
        conflicted
            .tasks
            .transition(
                &conflicted.dependent_id,
                TransitionAction::Start,
                "coordinator",
                "system",
                None,
                None,
            )
            .await
            .is_ok(),
        "the released dependent must now be startable"
    );
}
