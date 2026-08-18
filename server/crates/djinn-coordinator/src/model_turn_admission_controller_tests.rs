//! Fenced controller and persisted-timestamp reaper regressions (task wnrd).
//!
//! Every clock here is injected: leases are aged by moving the columns the
//! reaper actually reads, and boundaries are passed in. There are no sleeps.

use super::*;
use djinn_core::models::{Model, Pricing, Provider};
use djinn_db::{
    Database, ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionWait,
    ModelTurnBucketDebit, ModelTurnBucketKind,
    repositories::test_support::seed_scoped_model_turn_admission_fixture,
};

use crate::model_turn_admission::{
    JoinedCapabilityReportV1, PhaseCAttemptEvidenceOutcomeV1, PhaseCAttemptStageEvidenceV1,
    PhaseCAttemptStageV1, PhaseCLearnerWindowV1, learner_catalog_qualified_phase_c_window_v1,
};
use djinn_provider::{ProviderAttemptAbortResultV1, ProviderAttemptTerminalV1, ProviderOutcomeV1};

const PROVIDER: &str = "wnrd-provider";
const MODEL: &str = "namespace/wnrd-model";
const WINDOW_START: &str = "1970-01-01T00:02:00Z";
const WINDOW_END: &str = "1970-01-01T00:03:00Z";

fn catalog_service() -> CatalogService {
    let catalog = CatalogService::new();
    catalog.add_custom_provider(
        Provider {
            id: PROVIDER.into(),
            name: "wnrd Provider".into(),
            npm: String::new(),
            env_vars: vec!["WNRD_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![Model {
            id: MODEL.into(),
            provider_id: PROVIDER.into(),
            name: "wnrd Model".into(),
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

async fn seed(db: &Database, credential: &str, phase: &str) -> i64 {
    seed_scoped_model_turn_admission_fixture(db, credential, PROVIDER, MODEL, phase, "supported", 4)
        .await
}

/// Seed a pool that can actually admit: the scoped fixture creates the pool but
/// no bucket binding, and a pool with no binding waits rather than admitting.
async fn seed_admitting(db: &Database, credential: &str) -> i64 {
    let pool_id = seed(db, credential, "enforce").await;
    ModelTurnAdmissionRepository::new(db.clone())
        .seed_request_bucket_binding_for_test(pool_id, 8, 8)
        .await
        .expect("seed request bucket binding");
    pool_id
}

async fn live_fence(db: &Database) -> (String, ModelTurnControllerFence) {
    let incarnation_id = uuid::Uuid::now_v7().to_string();
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(&incarnation_id)
        .await
        .expect("register coordinator incarnation");
    (
        incarnation_id.clone(),
        ModelTurnControllerFence {
            incarnation_id,
            live_since_at: "1970-01-01T00:00:00Z".into(),
        },
    )
}

fn window() -> AlignedPhaseCWindowV1 {
    AlignedPhaseCWindowV1::new(120).expect("aligned window")
}

fn path(pool_id: i64) -> ExpectedAttemptPathV1 {
    ExpectedAttemptPathV1 {
        slot_pod_uid: "wnrd-slot".into(),
        deployment_revision: "wnrd-revision".into(),
        provider: PROVIDER.into(),
        model_scope: MODEL.into(),
        pool_id,
    }
}

fn covered(path: &ExpectedAttemptPathV1) -> PhaseCCapabilityEvidenceV1 {
    PhaseCCapabilityEvidenceV1 {
        path: path.clone(),
        coverage_start_second: 120,
        coverage_end_second: 180,
        observed_at_second: 150,
        covered: true,
    }
}

fn complete_attempt(path: &ExpectedAttemptPathV1) -> PhaseCAdmittedAttemptV1 {
    let provider = ProviderOutcomeV1 {
        terminal: ProviderAttemptTerminalV1::Completed,
        authoritative_usage: None,
        observation: None,
        abort: ProviderAttemptAbortResultV1::NotRequested,
        token_emission: Default::default(),
    };
    PhaseCAdmittedAttemptV1 {
        path: path.clone(),
        admitted_at_second: 120,
        has_authoritative_usage: true,
        lease_expired: false,
        breaker_open: false,
        stages: [
            (PhaseCAttemptStageV1::Decision, 121),
            (PhaseCAttemptStageV1::Dispatch, 122),
            (PhaseCAttemptStageV1::Heartbeat, 123),
            (PhaseCAttemptStageV1::ProviderOutcome, 124),
            (PhaseCAttemptStageV1::Reconcile, 125),
        ]
        .into_iter()
        .map(|(stage, timestamp_second)| PhaseCAttemptStageEvidenceV1 {
            stage,
            timestamp_second,
            outcome: if stage == PhaseCAttemptStageV1::ProviderOutcome {
                PhaseCAttemptEvidenceOutcomeV1::Provider(Box::new(provider.clone()))
            } else {
                PhaseCAttemptEvidenceOutcomeV1::Recorded
            },
        })
        .collect(),
    }
}

fn projection(paths: Vec<ExpectedAttemptPathV1>) -> ExpectedAttemptPathProjectionV1 {
    ExpectedAttemptPathProjectionV1 {
        expected_paths: paths,
        joined_reports: Vec::<JoinedCapabilityReportV1>::new(),
    }
}

fn completed<'a>(
    projection: &'a ExpectedAttemptPathProjectionV1,
    capability_evidence: &'a [PhaseCCapabilityEvidenceV1],
    admitted_attempts: &'a [PhaseCAdmittedAttemptV1],
    counts: BTreeMap<i64, PhaseCWindowCountsV1>,
) -> PhaseCCompletedWindowV1<'a> {
    PhaseCCompletedWindowV1 {
        window: window(),
        started_at: WINDOW_START.into(),
        ended_at: WINDOW_END.into(),
        projection,
        capability_evidence,
        admitted_attempts,
        counts,
    }
}

fn counts(pool_id: i64, admitted: i64, completed: i64) -> BTreeMap<i64, PhaseCWindowCountsV1> {
    BTreeMap::from([(
        pool_id,
        PhaseCWindowCountsV1 {
            admitted_turns: admitted,
            completed_turns: completed,
        },
    )])
}

/// A real aligned-window completion reaches persistence with exact bounds and
/// counts, through the authoritative projection, the fail-closed qualifier, and
/// the fenced typed upsert.
#[tokio::test]
async fn a_completed_window_reaches_persistence_with_exact_bounds_and_counts() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed(&db, "wnrd-complete", "shadow").await;
    let (_, fence) = live_fence(&db).await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let catalog = catalog_service();
    let path = path(pool_id);
    let projection = projection(vec![path.clone()]);
    let evidence = [covered(&path)];
    let attempts = [complete_attempt(&path)];

    let outcome = run_completed_window_cycle_v1(
        &repository,
        &catalog,
        &fence,
        &completed(&projection, &evidence, &attempts, counts(pool_id, 11, 9)),
        7,
    )
    .await
    .expect("controller cycle");
    assert!(
        outcome.qualification.admitted,
        "{:?}",
        outcome.qualification
    );
    assert!(!outcome.fenced);
    assert_eq!(outcome.persisted_pools, vec![pool_id]);
    assert!(outcome.drained_pools.is_empty());

    assert_eq!(
        learner_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            pool_id,
            2,
            WINDOW_START,
            WINDOW_END,
        )
        .await
        .expect("learner read"),
        Some(PhaseCLearnerWindowV1 {
            pool_id,
            window_sequence: 2,
            started_at: WINDOW_START.into(),
            ended_at: WINDOW_END.into(),
            admitted_turns: 11,
            completed_turns: 9,
        })
    );
}

/// Leadership loss and succession: a generation that is draining, that has
/// stopped renewing, or that never existed cannot commit, and the last
/// persisted window stands exactly as its owner left it.
#[tokio::test]
async fn a_stale_controller_generation_cannot_commit_after_succession() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed(&db, "wnrd-fence", "shadow").await;
    let (incarnation_id, fence) = live_fence(&db).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let catalog = catalog_service();
    let path = path(pool_id);
    let projection = projection(vec![path.clone()]);
    let evidence = [covered(&path)];
    let attempts = [complete_attempt(&path)];
    let cycle = async |fence: &ModelTurnControllerFence, admitted: i64| {
        run_completed_window_cycle_v1(
            &repository,
            &catalog,
            fence,
            &completed(
                &projection,
                &evidence,
                &attempts,
                counts(pool_id, admitted, admitted),
            ),
            7,
        )
        .await
        .expect("controller cycle")
    };

    let first = cycle(&fence, 11).await;
    assert!(!first.fenced);
    assert_eq!(first.persisted_pools, vec![pool_id]);

    // Succession: the incarnation begins draining, so its controller work stops
    // committing even though the process is still running.
    assert!(
        djinn_db::CoordinatorIncarnationRepository::new(db.clone())
            .mark_draining(&incarnation_id)
            .await
            .expect("mark draining")
    );
    let drained = cycle(&fence, 99).await;
    assert!(drained.fenced, "a draining generation must not commit");
    assert!(drained.persisted_pools.is_empty());

    // An incarnation that stopped renewing before the liveness floor is equally
    // fenced, as is one that never registered at all.
    for stale in [
        ModelTurnControllerFence {
            incarnation_id: incarnation_id.clone(),
            // Renewal floor in the far future: nothing has renewed since.
            live_since_at: "3000-01-01T00:00:00Z".into(),
        },
        ModelTurnControllerFence {
            incarnation_id: uuid::Uuid::now_v7().to_string(),
            live_since_at: "1970-01-01T00:00:00Z".into(),
        },
    ] {
        let outcome = cycle(&stale, 99).await;
        assert!(outcome.fenced, "{stale:?} must not commit");
        assert!(outcome.persisted_pools.is_empty());
    }

    // The last committed window is untouched by every fenced attempt.
    assert_eq!(
        learner_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            pool_id,
            2,
            WINDOW_START,
            WINDOW_END,
        )
        .await
        .expect("learner read")
        .map(|window| (window.admitted_turns, window.completed_turns)),
        Some((11, 11)),
        "a fenced cycle must hold the last persisted window"
    );

    // A successor with a fresh live incarnation commits again.
    let (_, successor) = live_fence(&db).await;
    let resumed = cycle(&successor, 21).await;
    assert!(!resumed.fenced);
    assert_eq!(resumed.persisted_pools, vec![pool_id]);
}

/// Coverage loss persists a diagnostic-only window and moves the selected
/// enforcing pools to draining before any later acquisition can commit.
#[tokio::test]
async fn coverage_loss_drains_enforcing_pools_before_the_next_acquisition() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_admitting(&db, "wnrd-coverage").await;
    let sibling = seed_admitting(&db, "wnrd-sibling").await;
    let (_, fence) = live_fence(&db).await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let catalog = catalog_service();
    let path = path(pool_id);
    let projection = projection(vec![path.clone()]);
    let attempts = [complete_attempt(&path)];

    let acquire = async |pool_id: i64, request_id: &str| {
        repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id,
                request_id: request_id.to_owned(),
                owner_pod_uid: Some("wnrd-owner".into()),
                generation: 1,
                debits: vec![ModelTurnBucketDebit {
                    bucket_kind: ModelTurnBucketKind::Request,
                    units: 1,
                }],
            })
            .await
            .expect("acquire turn")
    };
    let before = acquire(pool_id, "before-drain").await;
    assert!(
        matches!(before, ModelTurnAcquireOutcome::Admitted { .. }),
        "the pool must be admitting before coverage is lost: {before:?}"
    );
    let control_before = repository
        .pool_control_state_for_test(pool_id)
        .await
        .expect("pool state")
        .expect("pool state");

    // No capability evidence at all: complete coverage did not hold.
    let outcome = run_completed_window_cycle_v1(
        &repository,
        &catalog,
        &fence,
        &completed(&projection, &[], &attempts, counts(pool_id, 4, 4)),
        7,
    )
    .await
    .expect("controller cycle");
    assert!(!outcome.qualification.admitted);
    assert!(
        outcome
            .qualification
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == PhaseCWindowDiagnosticCodeV1::MissingCapability)
    );
    assert_eq!(outcome.persisted_pools, vec![pool_id]);
    assert_eq!(outcome.drained_pools, vec![pool_id]);

    // The window is durable and diagnostic-only, so it cannot train.
    let summary = repository
        .controller_window_summary_for_test(pool_id, 2)
        .await
        .expect("summary read")
        .expect("summary");
    assert!(!summary.trainable);
    assert!(!summary.diagnostics.is_empty());
    assert!(
        learner_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            pool_id,
            2,
            WINDOW_START,
            WINDOW_END,
        )
        .await
        .expect("learner read")
        .is_none()
    );

    // No later acquisition can commit against the old phase.
    assert!(
        matches!(
            acquire(pool_id, "after-drain").await,
            ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::Draining)
        ),
        "a drained pool must refuse the next acquisition"
    );
    // The sibling pool, which was not selected, is untouched and still admits.
    assert!(matches!(
        acquire(sibling, "sibling").await,
        ModelTurnAcquireOutcome::Admitted { .. }
    ));

    // Breaker/identity/capability state and the learned target are unchanged;
    // only the phase moved.
    let control_after = repository
        .pool_control_state_for_test(pool_id)
        .await
        .expect("pool state")
        .expect("pool state");
    assert_eq!(control_before.0, "enforce");
    assert_eq!(control_after.0, "draining");
    assert_eq!(control_after.1, control_before.1, "identity state");
    assert_eq!(control_after.2, control_before.2, "capability state");
    assert_eq!(control_after.3, control_before.3, "learned concurrency");

    // Draining is idempotent: a second coverage-loss cycle selects nothing new.
    let again = run_completed_window_cycle_v1(
        &repository,
        &catalog,
        &fence,
        &completed(&projection, &[], &attempts, counts(pool_id, 4, 4)),
        7,
    )
    .await
    .expect("controller cycle");
    assert!(again.drained_pools.is_empty());
}

/// Succession resumes from persisted lease timestamps alone, expires only what
/// is stale at the 90-second boundary, preserves healthy siblings, and reclaims
/// accounting at most once across a duplicate run.
#[tokio::test]
async fn the_reaper_resumes_from_persisted_timestamps_and_reclaims_at_most_once() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_admitting(&db, "wnrd-reaper").await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());

    let acquire = async |request_id: &str| match repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id,
            request_id: request_id.to_owned(),
            owner_pod_uid: Some("wnrd-owner".into()),
            generation: 1,
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 1,
            }],
        })
        .await
        .expect("acquire turn")
    {
        ModelTurnAcquireOutcome::Admitted { lease, .. } => lease.identity.clone(),
        other => panic!("expected admission, got {other:?}"),
    };

    let stale = acquire("stale-attempt").await;
    let healthy = acquire("healthy-attempt").await;
    // Injected time: move the only clock the reaper reads. One lease is durably
    // ancient, its sibling durably recent.
    repository
        .backdate_lease_for_test(&stale, "1970-01-01T00:00:00Z", None)
        .await
        .expect("backdate stale lease");
    repository
        .backdate_lease_for_test(&healthy, "2999-01-01T00:00:00Z", None)
        .await
        .expect("forward-date healthy lease");

    let boundary = "2000-01-01T00:00:00Z";
    // A brand-new handle built from nothing but a connection string: abrupt
    // succession has no in-memory record of what the previous owner saw.
    let successor = ModelTurnAdmissionRepository::new(
        Database::reopen_test(&db.test_dsn().expect("test dsn")).expect("reopen"),
    );
    let observed = successor
        .list_stale_lease_observations(boundary, 64)
        .await
        .expect("stale observations");
    assert_eq!(
        observed
            .iter()
            .map(|(_, observation)| observation.identity.request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["stale-attempt"],
        "only the durably stale observation is listed"
    );

    let first = reap_stale_model_turn_leases_v1(&successor, boundary, 64, None)
        .await
        .expect("reaper pass");
    assert_eq!(
        first,
        PhaseCReaperOutcomeV1 {
            expired: 1,
            fenced: 0
        }
    );
    let after_first = repository
        .pool_control_state_for_test(pool_id)
        .await
        .expect("pool state")
        .expect("pool state");

    // A duplicate pass — the classic double-reaper across handoff — expires
    // nothing and reclaims nothing a second time.
    let second = reap_stale_model_turn_leases_v1(&successor, boundary, 64, None)
        .await
        .expect("duplicate reaper pass");
    assert_eq!(second, PhaseCReaperOutcomeV1::default());
    assert_eq!(
        repository
            .pool_control_state_for_test(pool_id)
            .await
            .expect("pool state")
            .expect("pool state")
            .4,
        after_first.4,
        "in-flight accounting is reclaimed at most once"
    );

    // The healthy sibling is still in flight and can still reconcile normally.
    assert!(
        successor
            .list_stale_lease_observations(boundary, 64)
            .await
            .expect("stale observations")
            .is_empty()
    );
    assert_eq!(
        repository
            .heartbeat(&healthy)
            .await
            .expect("heartbeat healthy sibling"),
        ModelTurnLeaseMutationOutcome::Fenced,
        "a reserved lease has not yet been dispatched"
    );

    // The boundary is a real 90-second boundary, not a label: a boundary before
    // the stale lease's own timestamp selects nothing.
    assert!(
        successor
            .list_stale_lease_observations("1970-01-01T00:00:30Z", 64)
            .await
            .expect("stale observations")
            .is_empty()
    );
}

/// Cancellation — leadership loss in progress — stops the reaper before it can
/// mutate anything, with a genuinely reapable lease sitting right there.
#[tokio::test]
async fn a_cancelled_leader_reaps_nothing() {
    let db = Database::ephemeral().await.expect("db");
    let pool_id = seed_admitting(&db, "wnrd-cancelled").await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let ModelTurnAcquireOutcome::Admitted { lease, .. } = repository
        .acquire_turn(ModelTurnAcquireInput {
            pool_id,
            request_id: "cancelled-attempt".into(),
            owner_pod_uid: Some("wnrd-owner".into()),
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
    repository
        .backdate_lease_for_test(&lease.identity, "1970-01-01T00:00:00Z", None)
        .await
        .expect("backdate lease");
    let boundary = "2000-01-01T00:00:00Z";

    // The lease really is reapable: a live leader would expire it.
    assert_eq!(
        repository
            .list_stale_lease_observations(boundary, 64)
            .await
            .expect("stale observations")
            .len(),
        1
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        reap_stale_model_turn_leases_while_leading_v1(&repository, &cancel, boundary, 64, None)
            .await
            .expect("cancelled reaper pass"),
        PhaseCReaperOutcomeV1::default(),
        "a cancelled leader must not expire anything"
    );
    assert_eq!(
        repository
            .list_stale_lease_observations(boundary, 64)
            .await
            .expect("stale observations")
            .len(),
        1,
        "the lease is untouched, so this is a refusal and not a no-op fixture"
    );

    // The same call under a live token does expire it, so the guard is the
    // only difference between the two outcomes.
    assert_eq!(
        reap_stale_model_turn_leases_while_leading_v1(
            &repository,
            &tokio_util::sync::CancellationToken::new(),
            boundary,
            64,
            None,
        )
        .await
        .expect("live reaper pass"),
        PhaseCReaperOutcomeV1 {
            expired: 1,
            fenced: 0
        }
    );
}

/// The controller and reaper introduce no second admission mechanism and emit
/// nothing identifying: counts and opaque pool IDs only.
#[test]
fn the_controller_and_reaper_introduce_no_new_mechanism_or_identifier() {
    let source = include_str!("model_turn_admission_controller.rs");
    // Comment lines are stripped: the point is what the module *does*, and the
    // prose deliberately names the mechanisms it refuses to use.
    let production: String = source
        .split("\n#[cfg(test)]")
        .next()
        .expect("production part")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        // No process-local admission semaphore, resident scheduler, emergency
        // cap, or breaker reset.
        "Semaphore",
        "semaphore",
        "resident",
        "emergency_cap",
        "emergency cap",
        "reset_breaker",
        "breaker_reset",
        "breaker_state",
        // No competing leadership mechanism: the fence is the incarnation lease.
        "advisory_lock",
        "pg_try_advisory",
        "leader_election",
        // No raw sensitive identifier in telemetry or state.
        "credential_id",
        "user_id",
        "account_id",
        "project_id",
        "request_body",
        // No raw SQL and no second learner query.
        concat!("sqlx", "::query"),
        concat!("model_turn_", "controller_windows"),
    ] {
        assert!(
            !production.contains(forbidden),
            "the controller must not contain {forbidden}"
        );
    }
    // Every tracing event carries counts and flags only. The check reads the
    // macro bodies themselves rather than the whole module, so an ordinary
    // local binding named `pool_id` is not mistaken for an emitted field.
    let events: Vec<&str> = production
        .split("tracing::")
        .skip(1)
        .map(|event| event.split(");").next().unwrap_or(event))
        .collect();
    assert!(
        events.len() >= 3,
        "expected the controller and reaper events, found {}",
        events.len()
    );
    for event in events {
        for field in [
            "pool_id",
            "lease_id",
            "request_id",
            "slot_pod_uid",
            "deployment_revision",
            "attempt_fingerprint",
            "provider_id",
            "model_id",
            "credential_id",
            "user_id",
        ] {
            // Tracing fields are `name = value`, `%name` or `?name`. Match the
            // field forms only, so a field name occurring inside the human
            // message is not a false positive.
            for form in [
                format!("{field} ="),
                format!("%{field}"),
                format!("?{field}"),
            ] {
                assert!(
                    !event.contains(&form),
                    "telemetry must not emit {field}: {event}"
                );
            }
        }
    }
    assert!(production.contains("expired = outcome.expired"));
    assert!(production.contains("persisted = outcome.persisted_pools.len()"));
    // The fence is the coordinator incarnation the actor already owns.
    assert!(production.contains("self.coordinator_incarnation_id.clone()"));
}

#[test]
fn only_capability_coverage_codes_select_a_pool_for_draining() {
    for coverage in [
        PhaseCWindowDiagnosticCodeV1::EmptyExpectedDenominator,
        PhaseCWindowDiagnosticCodeV1::MissingCapability,
        PhaseCWindowDiagnosticCodeV1::UnexpectedCapability,
        PhaseCWindowDiagnosticCodeV1::DuplicateCapability,
        PhaseCWindowDiagnosticCodeV1::UncoveredCapability,
        PhaseCWindowDiagnosticCodeV1::PartialCapabilityCoverage,
        PhaseCWindowDiagnosticCodeV1::StaleHeartbeat,
    ] {
        assert!(is_capability_coverage_loss_v1(coverage), "{coverage:?}");
    }
    for chain in [
        PhaseCWindowDiagnosticCodeV1::UnknownAttemptPath,
        PhaseCWindowDiagnosticCodeV1::MissingUsage,
        PhaseCWindowDiagnosticCodeV1::ExpiredLease,
        PhaseCWindowDiagnosticCodeV1::OpenBreaker,
        PhaseCWindowDiagnosticCodeV1::MissingStage,
        PhaseCWindowDiagnosticCodeV1::DuplicateStage,
        PhaseCWindowDiagnosticCodeV1::MissingStageOutcome,
        PhaseCWindowDiagnosticCodeV1::StageOutsideWindow,
        PhaseCWindowDiagnosticCodeV1::ReversedStages,
        PhaseCWindowDiagnosticCodeV1::InvalidStageOutcome,
    ] {
        assert!(!is_capability_coverage_loss_v1(chain), "{chain:?}");
    }
}
