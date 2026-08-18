//! C0 → C4 activation refusal and success matrix for proposal `dser`.
//!
//! Every cell here drives the **production** activation path — the leader pass
//! in [`crate::direct_delivery_activation`] and the single transaction in
//! `djinn_db::DirectDeliveryActivationRepository` — against a real migrated
//! PostgreSQL database. Nothing constructs a refusal by hand, and the epoch row
//! is read back after every refusal, because "the epoch stayed disabled" is the
//! actual claim, not the returned enum.
//!
//! # Why it lives in the crate rather than in `tests/`
//!
//! The C4 tail asserts what activation does to the *consumers*: after a
//! successful activation, `task_pr_eligibility` must refuse a direct identity
//! and no task-PR boundary operation may be recorded. The boundary recorder's
//! scope guard and the coordinator's own gate are reached most faithfully from
//! inside the crate, next to the consumer cutover matrix that shares them.
//!
//! # What each capability declaration is worth
//!
//! `schema` and `repository` are live probes of the persisted C0 relations and
//! epoch row. `provider`, `orchestrator` and `consumer_cutover` are
//! compiled-contract identities. So that the latter three cannot advertise code
//! that no longer exists,
//! [`contract_declarations_name_production_code_that_still_exists`] enumerates
//! the production halves of the sources they name.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use djinn_core::events::EventBus;
use djinn_core::models::{
    DirectDeliveryCapability, DirectDeliveryEpochState, TaskDeliveryIdentity,
};
use djinn_db::test_support::{
    direct_delivery_capability_rows_for_test, direct_delivery_epoch_row_for_test,
    direct_delivery_generations_for_test, direct_delivery_lease_rows_for_test,
    disable_direct_delivery_epoch_for_test, drop_table_cascade_for_test, make_project,
    remove_direct_delivery_epoch_for_test, revoke_direct_delivery_capability_for_test,
    seed_direct_delivery_liveness_fixture_for_test,
    seed_disabled_direct_delivery_epoch_at_generation_for_test,
    seed_unknown_direct_delivery_epoch_for_test,
};
use djinn_db::{
    AcquireDeliveryLeaseInput, AcquireDeliveryLeaseResult, ActivateDirectDeliveryEpochInput,
    ActivateDirectDeliveryEpochResult, CapabilityCensusGap, CoordinatorIncarnationRepository,
    Database, DirectDeliveryActivationRefusal, DirectDeliveryActivationRepository, EpicRepository,
    ProposalBuildAttemptRepository, SettingsRepository, TaskRepository,
};

use crate::direct_delivery::{
    BoundaryOperation, DeliveryLedger, LedgerResult, RepositoryDeliveryLedger, TaskPrEligibility,
    boundary_operations_scope, task_pr_eligibility,
};
use crate::direct_delivery_activation::{
    ACTIVATION_REQUEST_SETTING_KEY, ACTIVATION_REQUEST_SETTING_VALUE, ActivationPassOutcome,
    live_since_iso, observed_capabilities, run_direct_delivery_activation_pass,
};

// ─── Fixture ───────────────────────────────────────────────────────────────

struct ActivationFixture {
    db: Database,
    /// This process's registered `coordinator_incarnations` row: the census
    /// population is that table, so the fence counts exactly this id.
    incarnation_id: String,
}

impl ActivationFixture {
    fn activation(&self) -> DirectDeliveryActivationRepository {
        DirectDeliveryActivationRepository::new(self.db.clone())
    }

    /// One production leader pass.
    async fn leader_pass(&self) -> ActivationPassOutcome {
        run_direct_delivery_activation_pass(&self.db, EventBus::noop(), &self.incarnation_id).await
    }

    /// The activation transaction on its own, without re-advertising. Cells
    /// that withdraw a capability must use this: the leader pass advertises
    /// before it activates and would restore what the cell just removed.
    async fn activate(&self, expected_generation: i64) -> ActivateDirectDeliveryEpochResult {
        self.activation()
            .activate(&ActivateDirectDeliveryEpochInput {
                expected_generation,
                live_since: live_since_iso().expect("census liveness threshold"),
            })
            .await
            .expect("activation transaction")
    }

    async fn request_activation(&self) {
        SettingsRepository::new(self.db.clone(), EventBus::noop())
            .set(
                ACTIVATION_REQUEST_SETTING_KEY,
                ACTIVATION_REQUEST_SETTING_VALUE,
            )
            .await
            .expect("record the operator activation request");
    }

    async fn epoch_row(&self) -> Option<(String, i64)> {
        direct_delivery_epoch_row_for_test(&self.db).await
    }

    async fn assert_epoch(&self, state: &str, generation: i64) {
        assert_eq!(
            self.epoch_row().await,
            Some((state.to_owned(), generation)),
            "the persisted epoch row is the claim, not the returned enum"
        );
    }
}

async fn fixture() -> ActivationFixture {
    let db = Database::open_in_memory().expect("migrated activation-matrix database");
    let incarnation_id = uuid::Uuid::now_v7().to_string();
    CoordinatorIncarnationRepository::new(db.clone())
        .register(&incarnation_id)
        .await
        .expect("register the census process");
    ActivationFixture { db, incarnation_id }
}

fn refusal(result: &ActivateDirectDeliveryEpochResult) -> &DirectDeliveryActivationRefusal {
    result
        .refusal()
        .unwrap_or_else(|| panic!("expected a refusal, got {result:?}"))
}

fn rfc3339_in(seconds: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("rfc3339 instant")
}

/// An epic-owned task with an active build attempt and, optionally, one
/// immutable delivery generation. Seeded through the real repositories.
struct DirectIdentity {
    task_id: String,
    build_attempt_id: String,
}

async fn seed_direct_identity(
    fixture: &ActivationFixture,
    delivery_state: Option<&str>,
) -> DirectIdentity {
    let project = make_project(&fixture.db, Path::new("activation-matrix")).await;
    let epic = EpicRepository::new(fixture.db.clone(), EventBus::noop())
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
                title: "activation",
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
        .expect("seed activation epic");
    let task = TaskRepository::new(fixture.db.clone(), EventBus::noop())
        .create(
            &epic.id,
            "activation source",
            "",
            "",
            "task",
            0,
            "",
            Some("approved"),
        )
        .await
        .expect("seed activation task");
    // This helper activates the epoch so the attempt can be activated at all;
    // every caller below returns it to its shipped disabled state afterwards.
    let seeded = seed_direct_delivery_liveness_fixture_for_test(
        &fixture.db,
        &epic.id,
        &task.id,
        delivery_state,
    )
    .await;
    DirectIdentity {
        task_id: task.id,
        build_attempt_id: seeded.build_attempt_id,
    }
}

// ─── C0: the shipped state ─────────────────────────────────────────────────

/// C0. The migration ships `disabled` and nothing else writes it. A leader tick
/// with a *complete* census still refuses without an explicit operator request,
/// so "default-disabled at rest" is a property of the code, not of an
/// incomplete deployment.
#[tokio::test]
async fn c0_the_shipped_epoch_is_disabled_at_rest_even_with_a_complete_census() {
    let fixture = fixture().await;
    fixture.assert_epoch("disabled", 0).await;
    assert!(
        direct_delivery_capability_rows_for_test(&fixture.db)
            .await
            .is_empty(),
        "no capability row exists before a process advertises"
    );
    assert!(
        direct_delivery_lease_rows_for_test(&fixture.db)
            .await
            .is_empty(),
        "no delivery lease exists before a delivery runs"
    );

    let outcome = fixture.leader_pass().await;
    let ActivationPassOutcome::NotRequested { advertised } = outcome else {
        panic!("an unrequested leader pass must not activate: {outcome:?}");
    };
    assert_eq!(
        advertised.iter().copied().collect::<HashSet<_>>(),
        DirectDeliveryCapability::ALL.into_iter().collect(),
        "this binary against this schema provides every capability"
    );
    fixture.assert_epoch("disabled", 0).await;

    // The guard was the request, not a census gap: prove the census is complete.
    assert_eq!(
        fixture
            .activation()
            .capability_census_gaps(1, &live_since_iso().unwrap())
            .await
            .unwrap(),
        Vec::<CapabilityCensusGap>::new(),
        "the census is complete, so only the missing operator request held activation back"
    );
    // Advertisement is the production writer of the capability relation.
    let rows = direct_delivery_capability_rows_for_test(&fixture.db).await;
    assert_eq!(rows.len(), DirectDeliveryCapability::ALL.len());
    assert!(
        rows.iter()
            .all(|(id, _, generation)| id == &fixture.incarnation_id && *generation == 1),
        "capabilities are advertised by this incarnation at the target generation: {rows:?}"
    );
}

// ─── C1 / C2 / C3: capability prerequisites ────────────────────────────────

/// Each of the five capabilities is an independent prerequisite: withdraw
/// exactly one and activation refuses naming exactly that one, leaving the
/// epoch disabled. Restore it and the same call activates.
#[tokio::test]
async fn each_missing_capability_independently_refuses_activation() {
    // No operator request is recorded: this cell drives the activation
    // transaction directly, because the leader pass would re-advertise the very
    // capability each iteration has just withdrawn.
    let fixture = fixture().await;

    for capability in DirectDeliveryCapability::ALL {
        // Re-advertise the full set through the production writer.
        fixture.leader_pass().await;
        revoke_direct_delivery_capability_for_test(
            &fixture.db,
            &fixture.incarnation_id,
            capability.as_str(),
        )
        .await;

        let result = fixture.activate(0).await;
        assert_eq!(
            refusal(&result),
            &DirectDeliveryActivationRefusal::IncompleteCapabilityCensus {
                gaps: vec![CapabilityCensusGap {
                    process_incarnation_id: fixture.incarnation_id.clone(),
                    missing: vec![capability],
                }],
            },
            "withdrawing {capability} must refuse activation on exactly that capability"
        );
        fixture.assert_epoch("disabled", 0).await;
    }

    // The full census, and nothing else changed, activates.
    fixture.leader_pass().await;
    let result = fixture.activate(0).await;
    let ActivateDirectDeliveryEpochResult::Activated(epoch) = result else {
        panic!("a complete census must activate: {result:?}");
    };
    assert_eq!(epoch.state, DirectDeliveryEpochState::Active);
    assert_eq!(epoch.generation, 1);
    fixture.assert_epoch("active", 1).await;
}

/// A binary that cannot read the C0 relations withholds the two capabilities
/// that are live probes, so it leaves a census gap rather than advertising
/// something it cannot do.
#[tokio::test]
async fn a_binary_that_cannot_read_the_schema_withholds_its_live_probe_capabilities() {
    let fixture = fixture().await;
    assert_eq!(
        observed_capabilities(&fixture.db)
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        DirectDeliveryCapability::ALL.into_iter().collect(),
    );

    drop_table_cascade_for_test(&fixture.db, "direct_delivery_leases").await;
    let observed = observed_capabilities(&fixture.db)
        .await
        .into_iter()
        .collect::<HashSet<_>>();
    assert!(
        !observed.contains(&DirectDeliveryCapability::Schema)
            && !observed.contains(&DirectDeliveryCapability::Repository),
        "a missing C0 relation must withdraw both live-probe capabilities: {observed:?}"
    );
    assert!(
        observed.contains(&DirectDeliveryCapability::Provider),
        "compiled-contract capabilities are unaffected by the database"
    );
}

// ─── C4: census, lease fence, generation CAS ───────────────────────────────

/// A live process from an older release — registered and renewing, but
/// advertising the previous generation — is a census gap. Only when it stops
/// renewing does the census complete. This is the "stale process replay" row.
#[tokio::test]
async fn a_live_process_at_a_stale_generation_refuses_the_census_until_it_stops_renewing() {
    let fixture = fixture().await;
    fixture.leader_pass().await;

    let stale = uuid::Uuid::now_v7().to_string();
    CoordinatorIncarnationRepository::new(fixture.db.clone())
        .register(&stale)
        .await
        .unwrap();
    fixture
        .activation()
        .advertise_capabilities(&stale, 0, &DirectDeliveryCapability::ALL)
        .await
        .expect("the stale process advertises the previous generation");

    let result = fixture.activate(0).await;
    assert_eq!(
        refusal(&result),
        &DirectDeliveryActivationRefusal::IncompleteCapabilityCensus {
            gaps: vec![CapabilityCensusGap {
                process_incarnation_id: stale.clone(),
                missing: DirectDeliveryCapability::ALL.to_vec(),
            }],
        },
        "a process advertising an older generation counts for nothing at the target"
    );
    fixture.assert_epoch("disabled", 0).await;

    djinn_db::test_support::expire_coordinator_incarnation_for_test(&fixture.db, &stale).await;
    assert!(
        matches!(
            fixture.activate(0).await,
            ActivateDirectDeliveryEpochResult::Activated(epoch) if epoch.generation == 1
        ),
        "once the stale process stops renewing the census is complete"
    );
    fixture.assert_epoch("active", 1).await;
}

/// A census over zero live processes is not a full census.
#[tokio::test]
async fn a_census_over_zero_live_processes_refuses() {
    let fixture = fixture().await;
    fixture.leader_pass().await;
    djinn_db::test_support::expire_coordinator_incarnation_for_test(
        &fixture.db,
        &fixture.incarnation_id,
    )
    .await;

    let result = fixture.activate(0).await;
    assert_eq!(
        refusal(&result),
        &DirectDeliveryActivationRefusal::NoLiveProcesses
    );
    fixture.assert_epoch("disabled", 0).await;
}

/// An unexpired, unreleased lease carrying a *legacy* epoch generation blocks
/// activation until it is released. Both the lease and the release go through
/// the production writer.
#[tokio::test]
async fn a_live_legacy_generation_delivery_lease_refuses_activation_until_it_drains() {
    let fixture = fixture().await;
    let identity = seed_direct_identity(&fixture, Some("prepared")).await;
    let delivery = TaskDeliveryIdentity::new(&identity.build_attempt_id, &identity.task_id, 1)
        .expect("fixture delivery identity");
    // The previous epoch generation was 1; activation targets 2.
    seed_disabled_direct_delivery_epoch_at_generation_for_test(&fixture.db, 1).await;

    let lease_id = uuid::Uuid::now_v7().to_string();
    let acquired = fixture
        .activation()
        .acquire_delivery_lease(&AcquireDeliveryLeaseInput {
            lease_id: lease_id.clone(),
            identity: delivery.clone(),
            owner_incarnation_id: "legacy-generation-owner".into(),
            epoch_generation: 1,
            expires_at: rfc3339_in(3600),
        })
        .await
        .expect("acquire the legacy-generation lease");
    assert!(
        matches!(acquired, AcquireDeliveryLeaseResult::Acquired(_)),
        "{acquired:?}"
    );

    fixture.leader_pass().await;
    let result = fixture.activate(1).await;
    match refusal(&result) {
        DirectDeliveryActivationRefusal::LiveLegacyDeliveryLeases { leases } => {
            assert_eq!(leases.len(), 1);
            assert_eq!(leases[0].id, lease_id);
            assert_eq!(leases[0].epoch_generation, 1);
        }
        other => panic!("expected the lease fence to refuse, got {other:?}"),
    }
    fixture.assert_epoch("disabled", 1).await;

    assert!(
        fixture
            .activation()
            .release_delivery_lease(&delivery, "legacy-generation-owner")
            .await
            .expect("release the legacy-generation lease"),
        "the production release writer must move the row"
    );
    let result = fixture.activate(1).await;
    let ActivateDirectDeliveryEpochResult::Activated(epoch) = result else {
        panic!("a drained lease fence must activate: {result:?}");
    };
    assert_eq!(epoch.generation, 2);
    fixture.assert_epoch("active", 2).await;
}

/// An expired legacy lease no longer holds the fence, without anyone releasing
/// it. Same shape as above, drained by expiry instead of by release.
#[tokio::test]
async fn an_expired_legacy_generation_lease_stops_holding_the_activation_fence() {
    let fixture = fixture().await;
    let identity = seed_direct_identity(&fixture, Some("prepared")).await;
    let delivery = TaskDeliveryIdentity::new(&identity.build_attempt_id, &identity.task_id, 1)
        .expect("fixture delivery identity");
    seed_disabled_direct_delivery_epoch_at_generation_for_test(&fixture.db, 1).await;

    let lease_id = uuid::Uuid::now_v7().to_string();
    fixture
        .activation()
        .acquire_delivery_lease(&AcquireDeliveryLeaseInput {
            lease_id: lease_id.clone(),
            identity: delivery,
            owner_incarnation_id: "crashed-owner".into(),
            epoch_generation: 1,
            expires_at: rfc3339_in(3600),
        })
        .await
        .unwrap();
    fixture.leader_pass().await;
    assert!(
        matches!(
            refusal(&fixture.activate(1).await),
            DirectDeliveryActivationRefusal::LiveLegacyDeliveryLeases { .. }
        ),
        "a live lease from a crashed owner still fences activation"
    );

    djinn_db::test_support::expire_direct_delivery_lease_for_test(&fixture.db, &lease_id).await;
    assert!(
        matches!(
            fixture.activate(1).await,
            ActivateDirectDeliveryEpochResult::Activated(epoch) if epoch.generation == 2
        ),
        "an expired lease no longer holds the fence"
    );
    fixture.assert_epoch("active", 2).await;
}

/// Competing and stale activation generations. One activation wins, its exact
/// replay is idempotent, and a competitor that planned a different generation
/// is refused without moving the epoch.
#[tokio::test]
async fn competing_and_stale_activation_generations_are_refused() {
    let fixture = fixture().await;
    fixture.leader_pass().await;

    // A plan built from a generation this epoch has never had.
    let result = fixture.activate(7).await;
    assert_eq!(
        refusal(&result),
        &DirectDeliveryActivationRefusal::CompetingGeneration {
            observed: 7,
            persisted: 0,
        }
    );
    fixture.assert_epoch("disabled", 0).await;

    // The winner.
    assert!(matches!(
        fixture.activate(0).await,
        ActivateDirectDeliveryEpochResult::Activated(_)
    ));
    fixture.assert_epoch("active", 1).await;

    // The exact same plan, replayed after a crash, is idempotent.
    assert!(
        matches!(
            fixture.activate(0).await,
            ActivateDirectDeliveryEpochResult::Replayed(epoch) if epoch.generation == 1
        ),
        "replaying the winning plan must not mint a second generation"
    );
    fixture.assert_epoch("active", 1).await;

    // A competitor that had already planned the next generation is refused;
    // epoch downgrade and re-activation are both unsupported.
    let result = fixture.activate(1).await;
    assert_eq!(
        refusal(&result),
        &DirectDeliveryActivationRefusal::AlreadyActive { generation: 1 }
    );
    fixture.assert_epoch("active", 1).await;
}

/// An unreadable contract — missing relation, missing epoch row, unparseable
/// state — refuses before any mutation.
#[tokio::test]
async fn an_unreadable_contract_refuses_activation_without_touching_the_epoch() {
    let missing_schema = fixture().await;
    missing_schema.leader_pass().await;
    drop_table_cascade_for_test(&missing_schema.db, "direct_delivery_leases").await;
    assert_eq!(
        refusal(&missing_schema.activate(0).await),
        &DirectDeliveryActivationRefusal::MissingSchema {
            missing_relations: vec!["direct_delivery_leases".to_owned()],
        }
    );
    missing_schema.assert_epoch("disabled", 0).await;

    let unknown_state = fixture().await;
    unknown_state.leader_pass().await;
    seed_unknown_direct_delivery_epoch_for_test(&unknown_state.db).await;
    assert_eq!(
        refusal(&unknown_state.activate(0).await),
        &DirectDeliveryActivationRefusal::UnknownEpochState {
            state: "unknown".to_owned(),
            generation: 0,
        }
    );
    assert_eq!(
        unknown_state.epoch_row().await,
        Some(("unknown".to_owned(), 0)),
        "an unknown state is reported, never overwritten"
    );

    let missing_epoch = fixture().await;
    missing_epoch.leader_pass().await;
    remove_direct_delivery_epoch_for_test(&missing_epoch.db).await;
    assert_eq!(
        refusal(&missing_epoch.activate(0).await),
        &DirectDeliveryActivationRefusal::MissingEpoch
    );
    assert_eq!(missing_epoch.epoch_row().await, None);
}

// ─── C4 tail: what activation does to writers ──────────────────────────────

/// A lease acquired at a generation older than the persisted epoch is rejected.
/// This is what stops a process that probed before activation from continuing
/// to mutate after it.
#[tokio::test]
async fn a_lease_at_a_generation_older_than_the_epoch_is_rejected() {
    let fixture = fixture().await;
    let identity = seed_direct_identity(&fixture, Some("prepared")).await;
    let delivery = TaskDeliveryIdentity::new(&identity.build_attempt_id, &identity.task_id, 1)
        .expect("fixture delivery identity");
    // The seeded epoch is active at generation 1; a lease at that generation is
    // the ordinary case.
    let live = fixture
        .activation()
        .acquire_delivery_lease(&AcquireDeliveryLeaseInput {
            lease_id: uuid::Uuid::now_v7().to_string(),
            identity: delivery.clone(),
            owner_incarnation_id: "owner-a".into(),
            epoch_generation: 1,
            expires_at: rfc3339_in(3600),
        })
        .await
        .unwrap();
    assert!(matches!(live, AcquireDeliveryLeaseResult::Acquired(_)));
    fixture
        .activation()
        .release_delivery_lease(&delivery, "owner-a")
        .await
        .unwrap();

    // A later activation moves the epoch to generation 2.
    seed_disabled_direct_delivery_epoch_at_generation_for_test(&fixture.db, 1).await;
    fixture.leader_pass().await;
    assert!(matches!(
        fixture.activate(1).await,
        ActivateDirectDeliveryEpochResult::Activated(_)
    ));
    fixture.assert_epoch("active", 2).await;

    let before = direct_delivery_lease_rows_for_test(&fixture.db).await.len();
    let stale = fixture
        .activation()
        .acquire_delivery_lease(&AcquireDeliveryLeaseInput {
            lease_id: uuid::Uuid::now_v7().to_string(),
            identity: delivery,
            owner_incarnation_id: "owner-a".into(),
            epoch_generation: 1,
            expires_at: rfc3339_in(3600),
        })
        .await
        .unwrap();
    assert_eq!(
        stale,
        AcquireDeliveryLeaseResult::StaleGeneration {
            requested: 1,
            persisted: 2,
        }
    );
    assert_eq!(
        direct_delivery_lease_rows_for_test(&fixture.db).await.len(),
        before,
        "a rejected acquisition must not write a lease row"
    );
}

/// The delivery lease fence at the production seam. `begin_apply` is the last
/// durable fact before the remote compare-and-set; a competing live owner makes
/// it decline without moving the generation.
///
/// Both owners present the **same** transition id on purpose. Without the
/// fence, owner B's call is a plain replay of an already-`applying` generation
/// and returns `Replayed` — which is exactly what the last step of this test
/// observes once owner A's lease expires. So `Stale` here can only come from
/// the lease, never from the transition-id compare-and-set.
#[tokio::test]
async fn a_competing_live_delivery_lease_declines_the_applying_transition() {
    const TRANSITION: &str = "activation-matrix-apply";

    let fixture = fixture().await;
    let identity = seed_direct_identity(&fixture, Some("prepared")).await;
    let delivery = TaskDeliveryIdentity::new(&identity.build_attempt_id, &identity.task_id, 1)
        .expect("fixture delivery identity");

    let ledger = |owner: &str| {
        RepositoryDeliveryLedger::new(
            fixture.db.clone(),
            ProposalBuildAttemptRepository::new(fixture.db.clone()),
            TaskRepository::new(fixture.db.clone(), EventBus::noop()),
        )
        .with_owner_incarnation(owner)
    };

    // Owner A takes the fence through the production seam.
    assert_eq!(
        ledger("owner-a")
            .begin_apply(&delivery, TRANSITION)
            .await
            .unwrap(),
        LedgerResult::Applied
    );
    let leases = direct_delivery_lease_rows_for_test(&fixture.db).await;
    assert_eq!(leases.len(), 1, "one live fence: {leases:?}");
    let (first_lease_id, _, released) = leases[0].clone();
    assert!(!released);

    // Owner B is refused and, crucially, the generation does not move.
    let generations_before =
        direct_delivery_generations_for_test(&fixture.db, &identity.task_id).await;
    let declined = ledger("owner-b")
        .begin_apply(&delivery, TRANSITION)
        .await
        .unwrap();
    assert_eq!(
        declined,
        LedgerResult::Stale,
        "a competing live owner must decline the applying transition"
    );
    assert_eq!(
        direct_delivery_generations_for_test(&fixture.db, &identity.task_id).await,
        generations_before,
        "a declined fence must leave the immutable generation exactly as it was"
    );
    assert_eq!(
        direct_delivery_lease_rows_for_test(&fixture.db).await.len(),
        1,
        "a refused acquisition writes no second lease"
    );

    // Owner A re-entering its own fence replays rather than contending.
    assert_eq!(
        ledger("owner-a")
            .begin_apply(&delivery, TRANSITION)
            .await
            .unwrap(),
        LedgerResult::Replayed
    );

    // Owner A crashes: once its lease expires, owner B takes over. This is the
    // positive control for the refusal above — the byte-identical call now
    // succeeds, so `Stale` there came from the lease and nothing else. The
    // handover stays inspectable as a released row plus a new one.
    djinn_db::test_support::expire_direct_delivery_lease_for_test(&fixture.db, &first_lease_id)
        .await;
    assert_eq!(
        ledger("owner-b")
            .begin_apply(&delivery, TRANSITION)
            .await
            .unwrap(),
        LedgerResult::Replayed,
        "an expired fence may be taken over"
    );
    let leases = direct_delivery_lease_rows_for_test(&fixture.db).await;
    assert_eq!(leases.len(), 2, "takeover retains the old row: {leases:?}");
    assert_eq!(
        leases.iter().filter(|(_, _, released)| !*released).count(),
        1,
        "exactly one live fence after takeover: {leases:?}"
    );
}

/// The C4 tail the proposal names: after a successful activation, a legacy
/// task-PR write is refused for the exact identity that was eligible before it,
/// and the refusal costs no task-PR forge operation.
#[tokio::test]
async fn activation_rejects_legacy_task_pr_writes_for_a_direct_identity() {
    let fixture = fixture().await;
    let identity = seed_direct_identity(&fixture, None).await;
    // Return the epoch to its shipped state: before activation this identity is
    // an ordinary legacy task-PR task.
    disable_direct_delivery_epoch_for_test(&fixture.db).await;
    assert_eq!(
        task_pr_eligibility(fixture.db.clone(), &identity.task_id)
            .await
            .unwrap(),
        TaskPrEligibility::LegacyAllowed,
        "while the epoch is disabled the task-PR path stays open"
    );

    fixture.request_activation().await;
    let outcome = fixture.leader_pass().await;
    assert_eq!(
        outcome,
        ActivationPassOutcome::Activated { generation: 1 },
        "the production leader pass is what activates"
    );
    fixture.assert_epoch("active", 1).await;

    let scope = boundary_operations_scope().await;
    let checkpoint = scope.checkpoint();
    let eligibility = task_pr_eligibility(fixture.db.clone(), &identity.task_id)
        .await
        .unwrap();
    let observed = scope.operations_since(checkpoint);
    match eligibility {
        TaskPrEligibility::DirectDeliveryIneligible { attempt } => {
            assert_eq!(attempt.build_attempt_id, identity.build_attempt_id);
        }
        other => panic!("activation must reject legacy task-PR writes, got {other:?}"),
    }
    assert!(
        !observed.iter().any(|operation| matches!(
            operation,
            BoundaryOperation::TaskPrCreate
                | BoundaryOperation::TaskPrAdopt
                | BoundaryOperation::TaskPrLookup
                | BoundaryOperation::TaskPrMerge
                | BoundaryOperation::TaskPrAutoMerge
                | BoundaryOperation::TaskPrApproval
                | BoundaryOperation::TaskPrSignoff
                | BoundaryOperation::TaskPrCustomEnqueue
                | BoundaryOperation::SupervisorPrOpen
        )),
        "the refusal must cost no task-PR forge operation: {observed:?}"
    );
}

// ─── Source-enumerating guards ─────────────────────────────────────────────

/// `source` with every `#[cfg(test)]`-guarded **inline module** removed.
///
/// A "cut at the first `#[cfg(test)]`" rule is unusable here, and unusable in
/// the dangerous direction. `direct_delivery.rs` names the attribute inside a
/// doc comment on line 72; `actor.rs` uses it for `cfg`-alternative
/// *production* items, for a test-only struct field, and for two inline test
/// modules that sit ~900 lines *before* `run_tick`. A prefix cut would silently
/// reduce the audited half to a few dozen lines — an audit that passes by
/// seeing almost nothing.
///
/// So this strips whole `#[cfg(test)] mod NAME { ... }` blocks (matched by the
/// closing brace at the module's own indentation) and keeps everything else.
/// Moving any audited call into a test module still reddens the audit, which is
/// the property AC2 asks for.
fn production(source: &str) -> String {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut kept = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if !line.trim_start().starts_with("#[cfg(test)]") {
            kept.push_str(line);
            index += 1;
            continue;
        }
        // Look past any further attributes at the item this one guards.
        let mut item = index + 1;
        while item < lines.len() {
            let trimmed = lines[item].trim();
            if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with("//") {
                item += 1;
            } else {
                break;
            }
        }
        let Some(module) = lines.get(item).filter(|candidate| {
            candidate.trim_start().starts_with("mod ") && candidate.contains('{')
        }) else {
            // Not an inline test module: a cfg-alternative item, an out-of-line
            // `mod x;` registration, or a test-only field. Keep it — the
            // production body continues right after it.
            kept.push_str(line);
            index += 1;
            continue;
        };
        let indent = &module[..module.len() - module.trim_start().len()];
        let closer = format!("{indent}}}");
        index = item + 1;
        while index < lines.len() {
            let done = lines[index].trim_end() == closer;
            index += 1;
            if done {
                break;
            }
        }
    }
    kept
}

/// The body of the item introduced by `header`, up to the next item declared at
/// the same indentation. Used to pin a call site inside one production function
/// rather than merely somewhere in a 3000-line file.
fn body_of<'a>(source: &'a str, header: &str) -> &'a str {
    let start = source
        .find(header)
        .unwrap_or_else(|| panic!("`{header}` no longer exists"));
    let rest = &source[start + header.len()..];
    let end = rest
        .find("\n    async fn ")
        .into_iter()
        .chain(rest.find("\n    fn "))
        .chain(rest.find("\n    pub "))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("server workspace root")
        .to_path_buf()
}

/// Every non-test Rust source under the workspace's `src` trees.
fn production_sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let root = workspace_root();
    let mut roots = vec![root.join("src")];
    if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
        for entry in crates.flatten() {
            roots.push(entry.path().join("src"));
        }
    }
    let mut files = Vec::new();
    for dir in roots {
        walk(&dir, &mut files);
    }
    files
        .into_iter()
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            // Test modules, test-support helpers and this matrix are allowed to
            // name anything: they are exactly what the guard exempts.
            !name.ends_with("_tests.rs")
                && !name.starts_with("test_support")
                && name != "direct_delivery_activation_matrix.rs"
                && !path.components().any(|component| {
                    matches!(
                        component.as_os_str().to_str(),
                        Some("tests" | "test_support")
                    )
                })
        })
        .filter_map(|path| {
            std::fs::read_to_string(&path)
                .ok()
                .map(|source| (path, source))
        })
        .collect()
}

/// AC5. The activation transaction is the only production statement in the
/// workspace that can set the epoch active. Everything else reads it.
#[test]
fn activation_is_the_only_production_writer_of_an_active_epoch() {
    let root = workspace_root();
    let mut writers = BTreeSet::new();
    for (path, source) in production_sources() {
        let head = production(&source);
        if !head.contains("direct_delivery_epochs") {
            continue;
        }
        if head.contains("UPDATE direct_delivery_epochs")
            || head.contains("INSERT INTO direct_delivery_epochs")
            || head.contains("DELETE FROM direct_delivery_epochs")
            || head.contains("SET state = 'active'")
        {
            writers.insert(
                path.strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    assert_eq!(
        writers,
        BTreeSet::from([
            "crates/djinn-db/src/repositories/direct_delivery_activation.rs".to_owned()
        ]),
        "the C4 activation transaction must be the sole production epoch writer"
    );
}

/// AC2. `direct_delivery_process_capabilities` and `direct_delivery_leases`
/// each have production readers *and* writers, and each of those is reached
/// from a production call site.
///
/// The audit reads the production halves of the real sources, so moving any of
/// these calls into a `#[cfg(test)]` module reddens it — which is the whole
/// point: both relations shipped with zero readers and zero writers, and
/// "the table exists" was never evidence.
#[test]
fn both_activation_relations_have_production_readers_and_writers() {
    const REPOSITORY: &str =
        include_str!("../../djinn-db/src/repositories/direct_delivery_activation.rs");
    const LEADER: &str = include_str!("direct_delivery_activation.rs");
    const ACTOR: &str = include_str!("actor.rs");
    const DELIVERY: &str = include_str!("direct_delivery.rs");

    // direct_delivery_process_capabilities: one writer, one reader.
    for statement in [
        "INSERT INTO direct_delivery_process_capabilities",
        "LEFT JOIN direct_delivery_process_capabilities",
    ] {
        assert!(
            production(REPOSITORY).contains(statement),
            "`{statement}` is not a production statement in the activation repository"
        );
    }
    // direct_delivery_leases: two writers, two readers.
    for statement in [
        "INSERT INTO direct_delivery_leases",
        "UPDATE direct_delivery_leases SET released_at = now()",
        "FROM direct_delivery_leases \\\n         WHERE released_at IS NULL AND expires_at > now()",
        "FROM direct_delivery_leases \\\n         WHERE build_attempt_id = $1",
    ] {
        assert!(
            production(REPOSITORY).contains(statement),
            "`{statement}` is not a production statement in the activation repository"
        );
    }

    // Each of those repository entry points is called from production code.
    for callee in [
        ".advertise_capabilities(process_incarnation_id, target_generation, &capabilities)",
        ".activate(&ActivateDirectDeliveryEpochInput",
    ] {
        assert!(
            production(LEADER).contains(callee),
            "{callee} has no production caller in the coordinator activation module"
        );
    }
    for callee in [
        ".acquire_delivery_lease(&AcquireDeliveryLeaseInput",
        ".release_delivery_lease(identity, &self.owner_incarnation_id)",
        "self.hold_delivery_lease(identity).await?",
    ] {
        assert!(
            production(DELIVERY).contains(callee),
            "{callee} has no production caller in the direct-delivery ledger"
        );
    }
    // Pinned to the leader tick body specifically: "somewhere in actor.rs" is
    // not the claim, "every leader tick runs it" is.
    let actor = production(ACTOR);
    assert!(
        body_of(&actor, "async fn run_tick(&mut self) {")
            .contains("poll_stack::boxed(|| self.run_direct_delivery_activation_pass()).await;"),
        "the activation pass is not called from the coordinator leader tick"
    );
    assert!(
        body_of(
            &actor,
            "async fn run_direct_delivery_activation_pass(&self) {"
        )
        .contains("crate::direct_delivery_activation::run_direct_delivery_activation_pass("),
        "the leader tick's activation pass does not reach the production module"
    );
}

/// The three compiled-contract capabilities name code that still exists.
/// Without this, `provider` / `orchestrator` / `consumer_cutover` would be
/// constants that keep advertising after their implementation is deleted.
#[test]
fn contract_declarations_name_production_code_that_still_exists() {
    const CONTENTS: &str = include_str!("../../djinn-provider/src/github_api/contents.rs");
    const DELIVERY: &str = include_str!("direct_delivery.rs");
    const POLLER: &str = include_str!("pr_poller/mod.rs");

    for operation in djinn_provider::github_api::DIRECT_DELIVERY_REF_OPERATIONS {
        assert!(
            production(CONTENTS).contains(&format!("pub async fn {operation}(")),
            "the provider capability advertises `{operation}`, which contents.rs no longer defines"
        );
    }
    for entry_point in crate::direct_delivery::ORCHESTRATOR_CONTRACT_ENTRY_POINTS {
        assert!(
            production(DELIVERY).contains(entry_point),
            "the orchestrator capability advertises `{entry_point}`, which direct_delivery.rs \
             no longer defines"
        );
    }
    let gates = production(DELIVERY) + &production(POLLER);
    for gate in crate::direct_delivery::CONSUMER_CUTOVER_CONTRACT_GATES {
        assert!(
            gates.contains(&format!("fn {gate}(")),
            "the consumer capability advertises the `{gate}` gate, which no longer exists"
        );
    }
}
