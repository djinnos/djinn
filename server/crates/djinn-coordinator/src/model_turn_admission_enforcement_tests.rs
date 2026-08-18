//! Leader-side conformance for the Phase-D guarded enforcement pass (`5mqp`).
//!
//! Every assertion reads a persisted column or a table digest. Nothing here
//! asserts a log line, and nothing reads the process-global boundary-observation
//! recorder — commit `c3d3bc675` (`2dt3`) removed those from coordinator tests
//! because they collide across a single-process run.
//!
//! # Time
//!
//! There is no wall-clock sleep anywhere in this file or in the pass it drives,
//! and `the_pass_contains_no_wall_clock_wait` asserts that over the production
//! source rather than over one sampled run.
//!
//! `tokio::time::pause` is used by `paused_time_never_advances_across_the_time_
//! derived_inputs`, which is the part of the pass that has a clock at all.
//! The database-backed tests deliberately do NOT run under paused time: paused
//! tokio time auto-advances whenever the runtime has no *task* to poll, and a
//! `sqlx` pool acquire is blocked on socket readiness rather than on a task, so
//! every acquire times out instantly with `PoolTimedOut`. Pausing them would
//! not make them more hermetic — it would make them impossible.
//!
//! NAMED FAILING MUTATIONS.
//! (a) Delete `poll_stack::boxed(|| self.run_model_turn_enforcement_pass()).await;`
//!     from `CoordinatorActor::run_tick`: nothing else in the tick can move a
//!     pool's mode on coverage loss, so `the_tick_drains_a_pool_that_lost_coverage`
//!     finds the pool still `shadow`.
//! (b) Delete the `self.cancel.is_cancelled()` guard from
//!     `run_model_turn_enforcement_pass`: the cancelled actor drains the pool
//!     and `a_cancelled_leader_holds_the_last_persisted_mode` fails.
//! (c) Delete the fence re-check from `apply_enforcement_pass_in_transaction`:
//!     `a_superseded_incarnation_mutates_nothing` fails.
//! (d) Drop the `window_trainable` gate: `an_untrainable_window_denies_the_advance`
//!     fails, and with it the only thing standing between a pool that has never
//!     been shown to sustain a window and `enforce`.
//! (e) Evaluate coverage once for the whole pass instead of per pool:
//!     `coverage_loss_drains_only_the_affected_pool` fails on the covered pool.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::models::{Model, Pricing, Provider};
use djinn_db::{
    Database, ModelTurnAdmissionRepository, ModelTurnCapabilityHeartbeatInput,
    ModelTurnCompatibilityPhase, ModelTurnControllerFence, ModelTurnExpectedPathKey,
    ModelTurnIdentityState, repositories::test_support::seed_scoped_model_turn_admission_fixture,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};

use crate::model_turn_admission::controller::project_dispatch_topology_paths_v1;
use crate::model_turn_admission::enforcement::{
    expected_paths_by_pool_v1, run_enforcement_pass_v1,
};

const PROVIDER: &str = "enforce-provider";
const MODEL: &str = "namespace/enforce-model";
const SLOT: &str = "live-slot";
const REVISION: &str = "rev-1";
const GENERATION: i64 = 3;

/// Relations the pass must leave byte-identical. `dispatch_state` is the
/// durable breaker/backoff state; `user_settings` holds `max_sessions` and
/// `lane_max_sessions`; `build_leases` and `admission_handoff` are the
/// Kueue-adjacent workload ledgers that a workload creation would write.
const UNTOUCHED_RELATIONS: [&str; 4] = [
    "dispatch_state",
    "user_settings",
    "build_leases",
    "admission_handoff",
];

/// A fixed inventory that also counts how often it was consulted, so a test can
/// show the pass only ever *read* the cluster.
struct CountingInventory {
    records: Vec<WorkloadRecord>,
    reads: Arc<AtomicUsize>,
}

#[async_trait]
impl WorkloadInventory for CountingInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.records.clone())
    }
    async fn get_uid(&self, _kind: WorkloadObjectKind, _name: &str, _uid: &str) -> UidGetResult {
        UidGetResult::Present
    }
    async fn presence(&self, _kind: WorkloadObjectKind, _name: &str) -> ObjectPresence {
        ObjectPresence::Present {
            uid: Some(SLOT.to_owned()),
        }
    }
}

fn ready_slot() -> WorkloadRecord {
    WorkloadRecord {
        kind: WorkloadObjectKind::Pod,
        name: "slot".into(),
        uid: Some(SLOT.to_owned()),
        labels: std::collections::BTreeMap::new(),
        terminal: false,
        ready: true,
        deployment_revision: Some(REVISION.to_owned()),
        images: vec![],
        commands: vec![],
    }
}

fn enforcement_catalog() -> djinn_provider::catalog::CatalogService {
    let catalog = djinn_provider::catalog::CatalogService::new();
    catalog.add_custom_provider(
        Provider {
            id: PROVIDER.into(),
            name: "Enforcement Provider".into(),
            npm: String::new(),
            env_vars: vec!["ENFORCE_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![Model {
            id: MODEL.into(),
            provider_id: PROVIDER.into(),
            name: "Enforcement Model".into(),
            tool_call: false,
            reasoning: false,
            attachment: false,
            context_window: 1,
            output_limit: 1,
            pricing: Pricing::default(),
        }],
    );
    catalog
}

async fn register_incarnation(db: &Database, incarnation_id: &str) {
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(incarnation_id)
        .await
        .expect("register coordinator incarnation");
}

async fn seed_shadow_pool(db: &Database, name: &str) -> i64 {
    seed_scoped_model_turn_admission_fixture(db, name, PROVIDER, MODEL, "shadow", "supported", 4)
        .await
}

/// Report coverage for the one live slot through the production write path.
async fn cover(repository: &ModelTurnAdmissionRepository, pool_id: i64) {
    repository
        .record_capability_heartbeat(ModelTurnCapabilityHeartbeatInput {
            pool_id,
            slot_pod_uid: SLOT.to_owned(),
            deployment_revision: REVISION.to_owned(),
            provider_id: PROVIDER.to_owned(),
            model_id: MODEL.to_owned(),
        })
        .await
        .expect("record capability heartbeat");
}

async fn mode(repository: &ModelTurnAdmissionRepository, pool_id: i64) -> String {
    repository
        .pool_control_state_for_test(pool_id)
        .await
        .expect("pool state")
        .expect("pool exists")
        .0
}

async fn learned_concurrency(repository: &ModelTurnAdmissionRepository, pool_id: i64) -> i64 {
    repository
        .pool_control_state_for_test(pool_id)
        .await
        .expect("pool state")
        .expect("pool exists")
        .3
}

async fn mode_ledger_rows(repository: &ModelTurnAdmissionRepository, pool_id: i64) -> usize {
    repository
        .pool_mode_transitions(pool_id, 64)
        .await
        .expect("mode ledger")
        .len()
}

fn expected_key() -> ModelTurnExpectedPathKey {
    ModelTurnExpectedPathKey {
        slot_pod_uid: SLOT.to_owned(),
        deployment_revision: REVISION.to_owned(),
    }
}

fn one_pool(pool_id: i64) -> BTreeMap<i64, Vec<ModelTurnExpectedPathKey>> {
    BTreeMap::from([(pool_id, vec![expected_key()])])
}

/// `ended_at` of the last completed window is what the pass measures freshness
/// from, and it is always within the freshness bound of a just-written
/// heartbeat.
fn evaluated_at() -> String {
    ::time::OffsetDateTime::now_utc()
        .format(&::time::format_description::well_known::Rfc3339)
        .expect("format now")
}

fn fence(incarnation_id: &str) -> ModelTurnControllerFence {
    ModelTurnControllerFence {
        incarnation_id: incarnation_id.to_owned(),
        live_since_at: "1970-01-01T00:00:00Z".to_owned(),
    }
}

// ── AC 1: leadership, cancellation, and succession ─────────────────────────

/// Cancelling the leader token stops new mode mutations and holds the last
/// persisted mode. The control arm is the same actor state without the cancel,
/// so the assertion cannot pass because nothing would have happened anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_leader_holds_the_last_persisted_mode() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let uncovered = seed_shadow_pool(&db, "cancel-uncovered").await;

    let mut cancelled = crate::actor::actor_with_test_db(db.clone());
    register_incarnation(&db, &cancelled.coordinator_incarnation_id).await;
    cancelled.catalog = enforcement_catalog();
    cancelled.workload_inventory = Some(Arc::new(CountingInventory {
        records: vec![ready_slot()],
        reads: Arc::new(AtomicUsize::new(0)),
    }));
    cancelled.cancel.cancel();
    cancelled.run_model_turn_enforcement_pass().await;

    assert_eq!(
        mode(&repository, uncovered).await,
        "shadow",
        "a cancelled leader must not mutate a mode"
    );
    assert_eq!(mode_ledger_rows(&repository, uncovered).await, 0);

    // Control arm: the identical pass, not cancelled, does act — so the
    // assertion above is about the cancel, not about an inert pass.
    let mut leading = crate::actor::actor_with_test_db(db.clone());
    register_incarnation(&db, &leading.coordinator_incarnation_id).await;
    leading.catalog = enforcement_catalog();
    leading.workload_inventory = Some(Arc::new(CountingInventory {
        records: vec![ready_slot()],
        reads: Arc::new(AtomicUsize::new(0)),
    }));
    leading.run_model_turn_enforcement_pass().await;
    assert_eq!(
        mode(&repository, uncovered).await,
        "off",
        "the same pass under leadership drains an uncovered pool and settles it"
    );
    assert_eq!(mode_ledger_rows(&repository, uncovered).await, 2);
}

/// A generation that no longer holds the durable lease cannot commit after
/// succession: the fence is re-checked inside the mutating transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_incarnation_mutates_nothing() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "succession").await;

    // A successor registered its own lease; the stale incarnation never did.
    register_incarnation(&db, "00000000-0000-7000-8000-00000000005c").await;
    let outcome = run_enforcement_pass_v1(
        &repository,
        &fence("00000000-0000-7000-8000-0000000057a1"),
        GENERATION,
        &evaluated_at(),
        &one_pool(pool_id),
        false,
    )
    .await
    .expect("enforcement pass");

    assert!(outcome.fenced, "a stale generation must be fenced");
    assert!(outcome.drained_pools.is_empty());
    assert_eq!(
        mode(&repository, pool_id).await,
        "shadow",
        "a fenced pass must leave the last persisted mode exactly as it stands"
    );
    assert_eq!(mode_ledger_rows(&repository, pool_id).await, 0);

    // Control arm: the live successor's own fence is not fenced and does act.
    let live = run_enforcement_pass_v1(
        &repository,
        &fence("00000000-0000-7000-8000-00000000005c"),
        GENERATION,
        &evaluated_at(),
        &one_pool(pool_id),
        false,
    )
    .await
    .expect("enforcement pass");
    assert!(!live.fenced);
    assert_eq!(live.drained_pools, vec![pool_id]);
}

// ── AC 2: only the affected pools drain ────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coverage_loss_drains_only_the_affected_pool() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let covered = seed_shadow_pool(&db, "covered").await;
    let uncovered = seed_shadow_pool(&db, "uncovered").await;
    cover(&repository, covered).await;

    let incarnation = "00000000-0000-7000-8000-0000000000e1";
    register_incarnation(&db, incarnation).await;
    let concurrency_before = learned_concurrency(&repository, covered).await;

    let expected = BTreeMap::from([
        (covered, vec![expected_key()]),
        (uncovered, vec![expected_key()]),
    ]);
    let outcome = run_enforcement_pass_v1(
        &repository,
        &fence(incarnation),
        GENERATION,
        &evaluated_at(),
        &expected,
        false,
    )
    .await
    .expect("enforcement pass");

    assert_eq!(
        outcome.drained_pools,
        vec![uncovered],
        "only the pool that lost coverage may drain"
    );
    assert_eq!(mode(&repository, uncovered).await, "off");
    assert_eq!(
        mode(&repository, covered).await,
        "shadow",
        "a covered pool's mode is untouched"
    );
    assert_eq!(mode_ledger_rows(&repository, covered).await, 0);
    assert_eq!(
        learned_concurrency(&repository, covered).await,
        concurrency_before,
        "a covered pool's learned concurrency is untouched"
    );
}

// ── AC 3: identity eligibility gates `enforce` ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_identity_cannot_be_advanced_to_enforce() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "identity").await;
    cover(&repository, pool_id).await;
    repository
        .set_pool_compatibility_phase_for_test(pool_id, ModelTurnCompatibilityPhase::D)
        .await
        .expect("reach compatibility phase d");
    let incarnation = "00000000-0000-7000-8000-0000000000e2";
    register_incarnation(&db, incarnation).await;

    for state in [
        ModelTurnIdentityState::Ambiguous,
        ModelTurnIdentityState::Colliding,
    ] {
        repository
            .set_pool_identity_for_test(pool_id, state)
            .await
            .expect("set identity");
        let outcome = run_enforcement_pass_v1(
            &repository,
            &fence(incarnation),
            GENERATION,
            &evaluated_at(),
            &one_pool(pool_id),
            true,
        )
        .await
        .expect("enforcement pass");
        assert_eq!(
            outcome.denials,
            vec![(pool_id, "identity_ineligible")],
            "a {state:?} identity must be denied by name"
        );
        assert!(outcome.enforced_pools.is_empty());
        assert_eq!(
            mode(&repository, pool_id).await,
            "shadow",
            "a {state:?} identity pool must not enforce"
        );
        assert_eq!(mode_ledger_rows(&repository, pool_id).await, 0);
    }

    // Control arm: the identical pass with an eligible identity does enforce,
    // so the denial above is about the identity and nothing else.
    repository
        .set_pool_identity_for_test(pool_id, ModelTurnIdentityState::Eligible)
        .await
        .expect("restore identity");
    let outcome = run_enforcement_pass_v1(
        &repository,
        &fence(incarnation),
        GENERATION,
        &evaluated_at(),
        &one_pool(pool_id),
        true,
    )
    .await
    .expect("enforcement pass");
    assert_eq!(outcome.enforced_pools, vec![pool_id]);
    assert_eq!(mode(&repository, pool_id).await, "enforce");
}

/// The fail-closed reality of Phase D: a window that did not qualify cannot
/// advance a pool, and today no production window qualifies, because Phase B
/// never stored a capability coverage interval or an authoritative usage
/// column. This test pins the gate rather than papering over the gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untrainable_window_denies_the_advance() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "untrainable").await;
    cover(&repository, pool_id).await;
    repository
        .set_pool_compatibility_phase_for_test(pool_id, ModelTurnCompatibilityPhase::D)
        .await
        .expect("reach compatibility phase d");
    let incarnation = "00000000-0000-7000-8000-0000000000e3";
    register_incarnation(&db, incarnation).await;

    let outcome = run_enforcement_pass_v1(
        &repository,
        &fence(incarnation),
        GENERATION,
        &evaluated_at(),
        &one_pool(pool_id),
        false,
    )
    .await
    .expect("enforcement pass");
    assert_eq!(outcome.denials, vec![(pool_id, "window_not_trainable")]);
    assert_eq!(mode(&repository, pool_id).await, "shadow");
    assert_eq!(mode_ledger_rows(&repository, pool_id).await, 0);
}

/// A pool that never reached compatibility phase `d` cannot enforce, whatever
/// the window said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pool_short_of_compatibility_phase_d_cannot_enforce() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "phase-short").await;
    cover(&repository, pool_id).await;
    let incarnation = "00000000-0000-7000-8000-0000000000e4";
    register_incarnation(&db, incarnation).await;

    let outcome = run_enforcement_pass_v1(
        &repository,
        &fence(incarnation),
        GENERATION,
        &evaluated_at(),
        &one_pool(pool_id),
        true,
    )
    .await
    .expect("enforcement pass");
    assert_eq!(
        outcome.denials,
        vec![(pool_id, "compatibility_phase_insufficient")]
    );
    assert_eq!(mode(&repository, pool_id).await, "shadow");
}

// ── AC 4: everything else is byte-identical across the pass ────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pass_leaves_breaker_state_session_caps_and_kueue_ledgers_byte_identical() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "isolation").await;
    // A durable breaker/backoff row and a session cap that the pass must not
    // touch, seeded through the same helpers the dispatch tests use.
    djinn_db::test_support::seed_project(&db, "enforcement-isolation", "isolation").await;
    let task_id = djinn_db::test_support::seed_task_row(
        &db,
        djinn_db::test_support::UsageTestTaskSeed {
            project_id: "enforcement-isolation",
            status: "open",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;
    djinn_db::test_support::seed_breaker_open_dispatch_state(&db, &task_id, MODEL, 30).await;

    let mut before = Vec::new();
    for relation in UNTOUCHED_RELATIONS {
        before.push((
            relation,
            djinn_db::test_support::table_digest_for_test(&db, relation).await,
        ));
    }
    assert_ne!(
        before[0].1, "empty",
        "precondition: the breaker relation has a row to be identical to"
    );
    let pools_before = djinn_db::test_support::table_digest_for_test(&db, "model_turn_pools").await;

    let reads = Arc::new(AtomicUsize::new(0));
    let mut actor = crate::actor::actor_with_test_db(db.clone());
    register_incarnation(&db, &actor.coordinator_incarnation_id).await;
    actor.catalog = enforcement_catalog();
    actor.workload_inventory = Some(Arc::new(CountingInventory {
        records: vec![ready_slot()],
        reads: reads.clone(),
    }));
    actor.run_model_turn_enforcement_pass().await;

    let mut after = Vec::new();
    for relation in UNTOUCHED_RELATIONS {
        after.push((
            relation,
            djinn_db::test_support::table_digest_for_test(&db, relation).await,
        ));
    }
    assert_eq!(
        after, before,
        "the enforcement pass must leave these relations byte-identical"
    );
    // Not vacuous: the pass really did run and really did change the one thing
    // it is allowed to change.
    assert_ne!(
        djinn_db::test_support::table_digest_for_test(&db, "model_turn_pools").await,
        pools_before,
        "precondition: the pass must actually have mutated a pool"
    );
    assert_eq!(mode(&repository, pool_id).await, "off");
    // The only cluster interaction the pass has is a read of the inventory.
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the pass reads the live inventory once and creates no workload"
    );
}

// ── Tick wiring: the pass is reachable from production ─────────────────────

/// `run_model_turn_enforcement_pass` has exactly one caller: the line in
/// `CoordinatorActor::run_tick`. Nothing else in the tree can move a pool's
/// mode on coverage loss, so without that line the whole Phase-D enforcement
/// plane is a library that never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tick_drains_a_pool_that_lost_coverage() {
    let db = Database::ephemeral().await.expect("db");
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pool_id = seed_shadow_pool(&db, "tick-enforce").await;

    let mut actor = crate::actor::actor_with_test_db(db.clone());
    register_incarnation(&db, &actor.coordinator_incarnation_id).await;
    actor.catalog = enforcement_catalog();
    actor.workload_inventory = Some(Arc::new(CountingInventory {
        records: vec![ready_slot()],
        reads: Arc::new(AtomicUsize::new(0)),
    }));

    assert_eq!(
        mode(&repository, pool_id).await,
        "shadow",
        "precondition: the pool is admitting in shadow"
    );
    actor.drive_tick_for_test().await;
    assert_eq!(
        mode(&repository, pool_id).await,
        "off",
        "one production tick must drain a pool with no capability coverage"
    );
    let ledger = repository
        .pool_mode_transitions(pool_id, 64)
        .await
        .expect("mode ledger");
    assert_eq!(
        ledger
            .iter()
            .map(|(_, _, reason, _)| reason.as_str())
            .collect::<Vec<_>>(),
        vec!["capability_coverage_loss", "drain_settled"],
        "the durable ledger names why the mode moved"
    );
}

// ── The projection grouping is per pool, from private route fields ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_paths_group_by_the_pool_that_owns_them() {
    let db = Database::ephemeral().await.expect("db");
    let first = seed_shadow_pool(&db, "group-one").await;
    let second = seed_shadow_pool(&db, "group-two").await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let pools = repository
        .list_observable_pools(64)
        .await
        .expect("observable pools");
    let projection =
        project_dispatch_topology_paths_v1(&enforcement_catalog(), &[ready_slot()], &pools);
    let by_pool = expected_paths_by_pool_v1(&projection);
    assert_eq!(
        by_pool.keys().copied().collect::<Vec<_>>(),
        vec![first.min(second), first.max(second)]
    );
    for paths in by_pool.values() {
        assert_eq!(paths, &vec![expected_key()]);
    }
}

// ── Time: the pass has no wall-clock wait ──────────────────────────────────

/// Under paused time nothing advances by itself, so this pins that the pass's
/// only clock reading is a pure derivation of an aligned window: the same
/// virtual instant before and after.
#[tokio::test(start_paused = true)]
async fn paused_time_never_advances_across_the_time_derived_inputs() {
    use crate::model_turn_admission::controller::{last_completed_window_v1, window_bounds_v1};

    let before = tokio::time::Instant::now();
    let window = last_completed_window_v1(1_700_000_123).expect("completed window");
    let (started_at, ended_at) = window_bounds_v1(window).expect("bounds");
    assert_eq!(started_at, "2023-11-14T22:14:00Z");
    assert_eq!(ended_at, "2023-11-14T22:15:00Z");
    assert_eq!(
        tokio::time::Instant::now(),
        before,
        "deriving the window must not consume time"
    );
}

/// A sampled run can only say that this particular pass did not sleep. This
/// says the production source has no wall-clock wait in it at all.
#[test]
fn the_pass_contains_no_wall_clock_wait() {
    let source = include_str!("model_turn_admission_enforcement.rs");
    for forbidden in [
        "tokio::time::sleep",
        "std::thread::sleep",
        "Instant::now",
        "spin_loop",
    ] {
        assert!(
            !source.contains(forbidden),
            "the enforcement pass must not contain `{forbidden}`"
        );
    }
}
