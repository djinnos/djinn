//! Proof that the Phase-C plane is reachable from a production tick (wnrd).
//!
//! Everything else in this epic is a library. These two tests are the only
//! thing standing between "Phase C is implemented" and "Phase C runs": each
//! drives `CoordinatorActor::run_tick` — the real pass, through the same
//! `drive_tick_for_test` shim the CI-route sweep uses — and asserts a durable
//! side effect that nothing else in the tick can produce.
//!
//! NAMED FAILING MUTATIONS.
//! (a) Delete `poll_stack::boxed(|| self.sweep_stale_model_turn_leases()).await;`
//!     from `run_tick`: nothing else in the tick touches a model-turn lease, so
//!     the stale lease stays in flight and
//!     `the_tick_reaps_a_stale_model_turn_lease` fails on its post-tick
//!     assertion.
//! (b) Delete `poll_stack::boxed(|| self.run_completed_phase_c_window()).await;`
//!     from `run_tick`: nothing else in the tree writes
//!     `model_turn_controller_windows`, so
//!     `the_tick_persists_a_phase_c_controller_window` finds no row.
//! (c) Return early from `run_completed_phase_c_window` when the topology is
//!     non-empty, or drop the catalog gate: the same test fails, because the
//!     row it reads back carries the canonical catalog labels.
//! (d) Widen the reconstructed heartbeat coverage to the window bounds: the
//!     persisted window would become trainable, and the assertion that an
//!     instant-only heartbeat stays diagnostic fails.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_core::models::{Model, Pricing, Provider};
use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionRepository,
    ModelTurnBucketDebit, ModelTurnBucketKind,
    repositories::test_support::seed_scoped_model_turn_admission_fixture,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};

use crate::model_turn_admission::controller::{last_completed_window_v1, window_bounds_v1};

const PROVIDER: &str = "tick-provider";
const MODEL: &str = "namespace/tick-model";

/// A fixed inventory of live Ready slots. Nothing here is a Phase-C report:
/// the inventory only says which slots exist and are Ready.
struct FixedInventory(Vec<WorkloadRecord>);

#[async_trait]
impl WorkloadInventory for FixedInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(self.0.clone())
    }
    async fn get_uid(&self, _kind: WorkloadObjectKind, _name: &str, _uid: &str) -> UidGetResult {
        UidGetResult::Present
    }
    async fn presence(&self, _kind: WorkloadObjectKind, _name: &str) -> ObjectPresence {
        ObjectPresence::Present {
            uid: Some("live-slot".to_owned()),
        }
    }
}

fn ready_slot(uid: &str, revision: &str) -> WorkloadRecord {
    WorkloadRecord {
        kind: WorkloadObjectKind::Pod,
        name: "slot".into(),
        uid: Some(uid.to_owned()),
        labels: std::collections::BTreeMap::new(),
        terminal: false,
        ready: true,
        deployment_revision: Some(revision.to_owned()),
        images: vec![],
        commands: vec![],
    }
}

fn tick_catalog() -> djinn_provider::catalog::CatalogService {
    let catalog = djinn_provider::catalog::CatalogService::new();
    catalog.add_custom_provider(
        Provider {
            id: PROVIDER.into(),
            name: "Tick Provider".into(),
            npm: String::new(),
            env_vars: vec!["TICK_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![Model {
            id: MODEL.into(),
            provider_id: PROVIDER.into(),
            name: "Tick Model".into(),
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

/// Register the actor's own incarnation lease, exactly as startup does. The
/// controller fence is that lease, so without the row nothing can commit.
async fn register_incarnation(db: &Database, incarnation_id: &str) {
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(incarnation_id)
        .await
        .expect("register coordinator incarnation");
}

/// `sweep_stale_model_turn_leases` has exactly one caller: the line in
/// `CoordinatorActor::run_tick`. Nothing else in this crate expires a model-turn
/// lease, so without that line a lease whose owner died sits `reserved` forever,
/// holding its reservation accounting, until the process restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tick_reaps_a_stale_model_turn_lease() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_scoped_model_turn_admission_fixture(
        &db,
        "tick-reaper",
        PROVIDER,
        MODEL,
        "enforce",
        "supported",
        4,
    )
    .await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    repository
        .seed_request_bucket_binding_for_test(pool_id, 8, 8)
        .await
        .expect("seed binding");
    let ModelTurnAcquireOutcome::Admitted { lease, .. } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id,
            request_id: "tick-stale".into(),
            owner_pod_uid: Some("dead-owner".into()),
            generation: 1,
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 1,
            }],
        })
        .await
        .expect("acquire turn")
    else {
        panic!("expected admission");
    };
    // Injected time: age the only clock the reaper reads, so the lease is
    // durably stale at *now* without a sleep.
    repository
        .backdate_lease_for_test(&lease.identity, "1970-01-01T00:00:00Z", None)
        .await
        .expect("backdate lease");

    let stale_before = repository
        .list_stale_lease_observations(
            &::time::OffsetDateTime::now_utc()
                .format(&::time::format_description::well_known::Rfc3339)
                .expect("now"),
            64,
        )
        .await
        .expect("stale observations");
    assert_eq!(
        stale_before.len(),
        1,
        "precondition: exactly one lease is reapable"
    );

    let mut actor = crate::actor::actor_with_test_db(db.clone());
    actor.drive_tick_for_test().await;

    assert!(
        repository
            .list_stale_lease_observations(
                &::time::OffsetDateTime::now_utc()
                    .format(&::time::format_description::well_known::Rfc3339)
                    .expect("now"),
                64,
            )
            .await
            .expect("stale observations")
            .is_empty(),
        "one production tick must EXPIRE the stale lease, not merely observe it"
    );
    assert_eq!(
        repository
            .pool_control_state_for_test(pool_id)
            .await
            .expect("pool state")
            .expect("pool state")
            .4,
        0,
        "expiry must release the reservation's in-flight accounting"
    );
}

/// `run_completed_phase_c_window` has exactly one caller: the line in
/// `CoordinatorActor::run_tick`. Nothing else in the tree writes
/// `model_turn_controller_windows`, so without that line the whole Phase-C
/// plane — projection, qualifier, fenced upsert, learner — is a library that
/// never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tick_persists_a_phase_c_controller_window() {
    let db = Database::ephemeral().await.expect("db");
    let off_pool = seed_scoped_model_turn_admission_fixture(
        &db,
        "tick-off",
        PROVIDER,
        MODEL,
        "off",
        "supported",
        4,
    )
    .await;
    let observed_pool = seed_scoped_model_turn_admission_fixture(
        &db,
        "tick-shadow",
        PROVIDER,
        MODEL,
        "shadow",
        "supported",
        4,
    )
    .await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let mut actor = crate::actor::actor_with_test_db(db.clone());
    register_incarnation(&db, &actor.coordinator_incarnation_id).await;
    actor.catalog = tick_catalog();
    actor.workload_inventory = Some(Arc::new(FixedInventory(vec![
        ready_slot("live-slot", "rev-1"),
        // Not Ready: it must not enter the denominator.
        WorkloadRecord {
            ready: false,
            ..ready_slot("cold-slot", "rev-1")
        },
    ])));

    let window = last_completed_window_v1(::time::OffsetDateTime::now_utc().unix_timestamp())
        .expect("completed window");
    let sequence = window.start_second() / 60;
    let (started_at, ended_at) = window_bounds_v1(window).expect("bounds");

    assert!(
        repository
            .controller_window_summary_for_test(observed_pool, sequence)
            .await
            .expect("summary read")
            .is_none(),
        "precondition: no controller window exists"
    );

    actor.drive_tick_for_test().await;

    let summary = repository
        .controller_window_summary_for_test(observed_pool, sequence)
        .await
        .expect("summary read")
        .expect("one production tick must persist a controller window");
    assert_eq!(
        (summary.provider_id.as_str(), summary.model_id.as_str()),
        (PROVIDER, MODEL),
        "the row carries the canonical active-catalog labels"
    );
    assert!(
        !summary.trainable,
        "a heartbeat-less window did not hold complete coverage and must stay diagnostic"
    );
    assert!(
        summary
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.pool_id == 0 || diagnostic.pool_id == observed_pool),
        "diagnostics stay pool-local: {:?}",
        summary.diagnostics
    );

    // An `off` pool has not been opted in, so it produces nothing at all.
    assert!(
        repository
            .controller_window_summary_for_test(off_pool, sequence)
            .await
            .expect("summary read")
            .is_none(),
        "an `off` pool must not be observed"
    );

    // The window is diagnostic, so the learner seam is blind to it.
    assert!(
        crate::model_turn_admission::learner_catalog_qualified_phase_c_window_v1(
            &repository,
            &actor.catalog,
            observed_pool,
            sequence,
            &started_at,
            &ended_at,
        )
        .await
        .expect("learner read")
        .is_none()
    );

    // A second tick for the same window is one cycle, not one per tick.
    actor.drive_tick_for_test().await;
    assert_eq!(
        actor.last_phase_c_window_start,
        Some(window.start_second()),
        "the completed window is processed once per window, not once per tick"
    );
}

/// The controller fence is this incarnation's own lease row, so a tick that
/// runs before `register_coordinator_incarnation` would find no row, be fenced,
/// and leave Phase C silently inert in production — with every behavioural test
/// above still green, because each registers the lease itself.
///
/// This pins the ordering that makes the fence satisfiable at all: `run`
/// registers the lease before it can reach a tick.
///
/// NAMED FAILING MUTATIONS.
/// (a) Delete `register_coordinator_incarnation` from `run`: the first
///     assertion fails.
/// (b) Move it after `run_dispatch_loop`: the ordering assertion fails.
#[test]
fn the_actor_registers_its_incarnation_lease_before_it_can_tick() {
    let source = include_str!("actor.rs");
    let run = source
        .split("pub(super) async fn run(mut self) {")
        .nth(1)
        .expect("`run` is declared");
    let register = run
        .find("register_coordinator_incarnation()")
        .expect("`run` must register this incarnation's lease");
    let loops = run
        .find("run_dispatch_loop(")
        .expect("`run` must enter the dispatch loop");
    assert!(
        register < loops,
        "the incarnation lease must exist before any tick, or every fenced \
         controller write silently writes nothing"
    );
}

/// A tick whose incarnation lease does not exist writes nothing at all: the
/// fence is real, and the registration above is what makes it satisfiable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unregistered_incarnation_persists_no_controller_window() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_scoped_model_turn_admission_fixture(
        &db,
        "tick-unregistered",
        PROVIDER,
        MODEL,
        "shadow",
        "supported",
        4,
    )
    .await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let mut actor = crate::actor::actor_with_test_db(db.clone());
    // Deliberately NOT registered.
    actor.catalog = tick_catalog();
    actor.workload_inventory = Some(Arc::new(FixedInventory(vec![ready_slot(
        "live-slot",
        "rev-1",
    )])));
    let window = last_completed_window_v1(::time::OffsetDateTime::now_utc().unix_timestamp())
        .expect("completed window");
    let sequence = window.start_second() / 60;

    actor.drive_tick_for_test().await;
    assert!(
        repository
            .controller_window_summary_for_test(pool_id, sequence)
            .await
            .expect("summary read")
            .is_none(),
        "an unregistered generation must not commit a controller window"
    );

    // The very same tick commits once the lease exists, so the difference is
    // the fence and nothing else.
    register_incarnation(&db, &actor.coordinator_incarnation_id).await;
    actor.drive_tick_for_test().await;
    assert!(
        repository
            .controller_window_summary_for_test(pool_id, sequence)
            .await
            .expect("summary read")
            .is_some(),
        "a registered generation commits the same window"
    );
}
