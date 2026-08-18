//! Phase-C controller-window conformance over migrated Postgres (epic ai6g, hb3s).
//!
//! Every window here comes out of the real qualifier and goes through gscv's
//! catalog-qualified persistence seam, then comes back — or does not — through
//! the exact-bound learner seam. Nothing hand-builds a summary and calls it
//! evidence, and nothing asks the database to decide catalog membership.

use super::*;
use djinn_core::models::{Model, Pricing, Provider};
use djinn_db::{Database, repositories::test_support::seed_scoped_model_turn_admission_fixture};
use djinn_provider::{ProviderAttemptAbortResultV1, ProviderAttemptTerminalV1, ProviderOutcomeV1};

const PROVIDER: &str = "hb3s-provider";
const MODEL: &str = "namespace/hb3s-model";
const WINDOW_START: &str = "1970-01-01T00:02:00Z";
const WINDOW_END: &str = "1970-01-01T00:03:00Z";
const SEQUENCE: i64 = 2;

fn catalog() -> CatalogService {
    let catalog = CatalogService::new();
    catalog.add_custom_provider(
        Provider {
            id: PROVIDER.into(),
            name: "hb3s Provider".into(),
            npm: String::new(),
            env_vars: vec!["HB3S_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![Model {
            id: MODEL.into(),
            provider_id: PROVIDER.into(),
            name: "hb3s Model".into(),
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

fn path(pool_id: i64, revision: &str) -> ExpectedAttemptPathV1 {
    ExpectedAttemptPathV1 {
        slot_pod_uid: "slot-uid".into(),
        deployment_revision: revision.into(),
        provider: PROVIDER.into(),
        model_scope: MODEL.into(),
        pool_id,
    }
}

fn window() -> AlignedPhaseCWindowV1 {
    AlignedPhaseCWindowV1::new(120).expect("aligned window")
}

fn accounting() -> PhaseCWindowAccountingV1 {
    PhaseCWindowAccountingV1 {
        window_sequence: SEQUENCE,
        started_at: WINDOW_START.into(),
        ended_at: WINDOW_END.into(),
        admitted_turns: 8,
        completed_turns: 8,
    }
}

fn covered(path: ExpectedAttemptPathV1) -> PhaseCCapabilityEvidenceV1 {
    PhaseCCapabilityEvidenceV1 {
        path,
        coverage_start_second: 120,
        coverage_end_second: 180,
        observed_at_second: 150,
        covered: true,
    }
}

fn complete_attempt(path: ExpectedAttemptPathV1) -> PhaseCAdmittedAttemptV1 {
    let provider = ProviderOutcomeV1 {
        terminal: ProviderAttemptTerminalV1::Completed,
        authoritative_usage: None,
        observation: None,
        abort: ProviderAttemptAbortResultV1::NotRequested,
        token_emission: Default::default(),
    };
    let stages = [
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
    .collect();
    PhaseCAdmittedAttemptV1 {
        path,
        admitted_at_second: 120,
        has_authoritative_usage: true,
        lease_expired: false,
        breaker_open: false,
        stages,
    }
}

/// Register a real coordinator-incarnation lease and fence writes on it.
async fn live_fence(db: &Database) -> ModelTurnControllerFence {
    let incarnation_id = uuid::Uuid::now_v7().to_string();
    djinn_db::CoordinatorIncarnationRepository::new(db.clone())
        .register(&incarnation_id)
        .await
        .expect("register coordinator incarnation");
    ModelTurnControllerFence {
        incarnation_id,
        live_since_at: "1970-01-01T00:00:00Z".into(),
    }
}

async fn seed(db: &Database, credential: &str) -> i64 {
    seed_scoped_model_turn_admission_fixture(
        db,
        credential,
        PROVIDER,
        MODEL,
        "shadow",
        "supported",
        1,
    )
    .await
}

async fn learner(
    repository: &ModelTurnAdmissionRepository,
    catalog: &CatalogService,
    pool_id: i64,
) -> Option<PhaseCLearnerWindowV1> {
    learner_catalog_qualified_phase_c_window_v1(
        repository,
        catalog,
        pool_id,
        SEQUENCE,
        WINDOW_START,
        WINDOW_END,
    )
    .await
    .expect("learner read")
}

/// A complete window is the only thing the qualifier admits, and the only thing
/// that survives the round trip through migrated Postgres.
#[tokio::test]
async fn a_complete_window_round_trips_through_the_migrated_ledger() {
    let db = Database::ephemeral().await.expect("db");
    let fence = live_fence(&db).await;
    let pool_id = seed(&db, "hb3s-complete").await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let catalog = catalog();
    let path = path(pool_id, "revision-1");

    let qualification = qualify_aligned_phase_c_window_v1(
        window(),
        std::slice::from_ref(&path),
        &[covered(path.clone())],
        &[complete_attempt(path.clone())],
    );
    assert!(
        qualification.admitted && qualification.diagnostics.is_empty(),
        "a complete chain under complete coverage must qualify: {qualification:?}"
    );

    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &path,
        accounting(),
        &qualification,
        &fence,
    )
    .await
    .expect("persist the qualified window");

    assert_eq!(
        learner(&repository, &catalog, pool_id).await,
        Some(PhaseCLearnerWindowV1 {
            pool_id,
            window_sequence: SEQUENCE,
            started_at: WINDOW_START.into(),
            ended_at: WINDOW_END.into(),
            admitted_turns: 8,
            completed_turns: 8,
        })
    );

    // Nothing but the exact bounds is visible, and the label pair still has to
    // resolve in the *current* catalog.
    assert!(
        learner_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            pool_id,
            SEQUENCE,
            WINDOW_START,
            "1970-01-01T00:04:00Z",
        )
        .await
        .expect("boundary-mismatched learner read")
        .is_none()
    );
    catalog.remove_custom_provider(PROVIDER);
    assert!(
        learner(&repository, &catalog, pool_id).await.is_none(),
        "a catalog-removed route must stop training an already-durable window"
    );
}

/// Every incomplete-coverage and incomplete-chain shape the qualifier can
/// report persists as a real diagnostic window, and none of them can train.
#[tokio::test]
async fn every_diagnostic_window_persists_and_stays_learner_invisible() {
    let db = Database::ephemeral().await.expect("db");
    let fence = live_fence(&db).await;
    let repository = ModelTurnAdmissionRepository::new(db.clone());
    let catalog = catalog();
    let base = path(0, "revision-1");

    // (label, expected diagnostic, capability evidence, attempt mutation)
    type Case = (
        &'static str,
        PhaseCWindowDiagnosticCodeV1,
        fn(&ExpectedAttemptPathV1) -> Vec<PhaseCCapabilityEvidenceV1>,
        fn(&mut PhaseCAdmittedAttemptV1),
    );
    let noop: fn(&mut PhaseCAdmittedAttemptV1) = |_| {};
    let one_covered: fn(&ExpectedAttemptPathV1) -> Vec<PhaseCCapabilityEvidenceV1> =
        |path| vec![covered(path.clone())];

    let cases: Vec<Case> = vec![
        (
            "missing capability report",
            PhaseCWindowDiagnosticCodeV1::MissingCapability,
            |_| Vec::new(),
            noop,
        ),
        (
            "silent (uncovered) capability report",
            PhaseCWindowDiagnosticCodeV1::UncoveredCapability,
            |path| {
                vec![PhaseCCapabilityEvidenceV1 {
                    covered: false,
                    ..covered(path.clone())
                }]
            },
            noop,
        ),
        (
            "duplicate capability report",
            PhaseCWindowDiagnosticCodeV1::DuplicateCapability,
            |path| vec![covered(path.clone()), covered(path.clone())],
            noop,
        ),
        (
            "partial capability coverage",
            PhaseCWindowDiagnosticCodeV1::PartialCapabilityCoverage,
            |path| {
                vec![PhaseCCapabilityEvidenceV1 {
                    coverage_start_second: 130,
                    ..covered(path.clone())
                }]
            },
            noop,
        ),
        (
            "heartbeat observed outside the window",
            PhaseCWindowDiagnosticCodeV1::StaleHeartbeat,
            |path| {
                vec![PhaseCCapabilityEvidenceV1 {
                    observed_at_second: 60,
                    ..covered(path.clone())
                }]
            },
            noop,
        ),
        (
            "revision-skewed capability report",
            PhaseCWindowDiagnosticCodeV1::UnexpectedCapability,
            |path| {
                vec![
                    covered(path.clone()),
                    covered(ExpectedAttemptPathV1 {
                        deployment_revision: "revision-2".into(),
                        ..path.clone()
                    }),
                ]
            },
            noop,
        ),
        (
            "missing authoritative usage",
            PhaseCWindowDiagnosticCodeV1::MissingUsage,
            one_covered,
            |attempt| attempt.has_authoritative_usage = false,
        ),
        (
            "expired lease",
            PhaseCWindowDiagnosticCodeV1::ExpiredLease,
            one_covered,
            |attempt| attempt.lease_expired = true,
        ),
        (
            "open breaker",
            PhaseCWindowDiagnosticCodeV1::OpenBreaker,
            one_covered,
            |attempt| attempt.breaker_open = true,
        ),
        (
            "incomplete chain",
            PhaseCWindowDiagnosticCodeV1::MissingStage,
            one_covered,
            |attempt| {
                attempt
                    .stages
                    .retain(|item| item.stage != PhaseCAttemptStageV1::Reconcile)
            },
        ),
        (
            "duplicate chain stage",
            PhaseCWindowDiagnosticCodeV1::DuplicateStage,
            one_covered,
            |attempt| {
                let first = attempt.stages[0].clone();
                attempt.stages.push(first);
            },
        ),
        (
            "missing stage outcome",
            PhaseCWindowDiagnosticCodeV1::MissingStageOutcome,
            one_covered,
            |attempt| {
                for item in attempt.stages.iter_mut() {
                    if item.stage == PhaseCAttemptStageV1::Heartbeat {
                        item.outcome = PhaseCAttemptEvidenceOutcomeV1::Missing;
                    }
                }
            },
        ),
        (
            "provider stage without a provider outcome",
            PhaseCWindowDiagnosticCodeV1::InvalidStageOutcome,
            one_covered,
            |attempt| {
                for item in attempt.stages.iter_mut() {
                    if item.stage == PhaseCAttemptStageV1::ProviderOutcome {
                        item.outcome = PhaseCAttemptEvidenceOutcomeV1::Recorded;
                    }
                }
            },
        ),
        (
            "stage recorded outside the window",
            PhaseCWindowDiagnosticCodeV1::StageOutsideWindow,
            one_covered,
            |attempt| attempt.stages[0].timestamp_second = 60,
        ),
        (
            "reordered chain",
            PhaseCWindowDiagnosticCodeV1::ReversedStages,
            one_covered,
            |attempt| {
                attempt.stages[0].timestamp_second = 179;
            },
        ),
    ];

    for (index, (label, expected, evidence, mutate)) in cases.into_iter().enumerate() {
        let pool_id = seed(&db, &format!("hb3s-diag-{index}")).await;
        let path = ExpectedAttemptPathV1 {
            pool_id,
            ..base.clone()
        };
        let mut attempt = complete_attempt(path.clone());
        mutate(&mut attempt);
        let qualification = qualify_aligned_phase_c_window_v1(
            window(),
            std::slice::from_ref(&path),
            &evidence(&path),
            &[attempt],
        );
        assert!(
            !qualification.admitted,
            "{label} must not qualify: {qualification:?}"
        );
        assert!(
            qualification
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == expected),
            "{label} must report {expected:?}, got {:?}",
            qualification.diagnostics
        );

        persist_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            &path,
            accounting(),
            &qualification,
            &fence,
        )
        .await
        .unwrap_or_else(|error| panic!("{label} must persist as a diagnostic window: {error}"));

        // The row is really there — this is not a silently skipped write.
        let stored = repository
            .controller_window_summary_for_test(pool_id, SEQUENCE)
            .await
            .expect("read persisted summary")
            .unwrap_or_else(|| panic!("{label} left no durable row"));
        assert!(!stored.trainable, "{label} must be stored as untrainable");
        assert!(
            !stored.diagnostics.is_empty(),
            "{label} must store its reason codes"
        );
        assert!(
            stored
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.pool_id == 0 || diagnostic.pool_id == pool_id),
            "{label} stored a foreign pool identity: {:?}",
            stored.diagnostics
        );

        assert!(
            learner(&repository, &catalog, pool_id).await.is_none(),
            "{label} must remain learner-invisible"
        );
    }

    // An attempt on a path outside the coordinator-owned denominator is an
    // unknown path, and is reported against the zero sentinel.
    let pool_id = seed(&db, "hb3s-unknown-path").await;
    let expected = ExpectedAttemptPathV1 {
        pool_id,
        ..base.clone()
    };
    let foreign = ExpectedAttemptPathV1 {
        slot_pod_uid: "other-slot".into(),
        ..expected.clone()
    };
    let qualification = qualify_aligned_phase_c_window_v1(
        window(),
        std::slice::from_ref(&expected),
        &[covered(expected.clone())],
        &[complete_attempt(foreign)],
    );
    assert!(
        qualification
            .diagnostics
            .iter()
            .any(
                |diagnostic| diagnostic.code == PhaseCWindowDiagnosticCodeV1::UnknownAttemptPath
                    && diagnostic.pool_id == 0
            ),
        "an unknown attempt path must be reported against the zero sentinel: {qualification:?}"
    );
    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &expected,
        accounting(),
        &qualification,
        &fence,
    )
    .await
    .expect("persist the unknown-path diagnostic window");
    assert!(learner(&repository, &catalog, pool_id).await.is_none());

    // An empty denominator is itself a diagnostic, reported against the sentinel.
    let empty = qualify_aligned_phase_c_window_v1(window(), &[], &[], &[]);
    assert!(
        empty.diagnostics.iter().any(|diagnostic| diagnostic.code
            == PhaseCWindowDiagnosticCodeV1::EmptyExpectedDenominator
            && diagnostic.pool_id == 0),
        "{empty:?}"
    );
}

/// Each pool owns its own diagnostics. A sibling's positive identity is neither
/// serialised into another pool's summary nor accepted by the typed boundary.
#[tokio::test]
async fn multi_pool_qualification_keeps_every_diagnostic_pool_local() {
    let db = Database::ephemeral().await.expect("db");
    let fence = live_fence(&db).await;
    let first = seed(&db, "hb3s-pool-a").await;
    let second = seed(&db, "hb3s-pool-b").await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let catalog = catalog();

    let first_path = path(first, "revision-1");
    let second_path = ExpectedAttemptPathV1 {
        slot_pod_uid: "slot-uid-b".into(),
        ..path(second, "revision-1")
    };
    let mut first_attempt = complete_attempt(first_path.clone());
    first_attempt.has_authoritative_usage = false;
    let mut second_attempt = complete_attempt(second_path.clone());
    second_attempt.breaker_open = true;

    let qualification = qualify_aligned_phase_c_window_v1(
        window(),
        &[first_path.clone(), second_path.clone()],
        &[covered(first_path.clone()), covered(second_path.clone())],
        &[first_attempt, second_attempt],
    );
    assert!(!qualification.admitted);
    // The shared qualification really does mention both pools; the per-pool
    // filtering below is therefore doing work.
    assert!(
        qualification
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.pool_id == first)
            && qualification
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.pool_id == second)
    );

    for path in [&first_path, &second_path] {
        persist_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            path,
            accounting(),
            &qualification,
            &fence,
        )
        .await
        .expect("persist per-pool diagnostics");
    }

    for (pool_id, sibling, own_code, sibling_code) in [
        (
            first,
            second,
            PhaseCWindowDiagnosticCodeV1::MissingUsage,
            PhaseCWindowDiagnosticCodeV1::OpenBreaker,
        ),
        (
            second,
            first,
            PhaseCWindowDiagnosticCodeV1::OpenBreaker,
            PhaseCWindowDiagnosticCodeV1::MissingUsage,
        ),
    ] {
        let stored = repository
            .controller_window_summary_for_test(pool_id, SEQUENCE)
            .await
            .expect("read persisted summary")
            .expect("persisted summary");
        let json = serde_json::to_value(&stored.diagnostics).expect("diagnostics json");
        let json = json.as_array().expect("diagnostics array");
        assert!(
            json.iter()
                .all(|diagnostic| diagnostic["pool_id"] == 0 || diagnostic["pool_id"] == pool_id),
            "pool {pool_id} stored a foreign identity: {json:?}"
        );
        assert!(
            !json
                .iter()
                .any(|diagnostic| diagnostic["pool_id"] == sibling),
            "pool {pool_id} must not carry pool {sibling}'s identity"
        );
        let own = serde_json::to_value(own_code).expect("own code");
        let other = serde_json::to_value(sibling_code).expect("sibling code");
        assert!(json.iter().any(|diagnostic| diagnostic["code"] == own));
        assert!(!json.iter().any(|diagnostic| diagnostic["code"] == other));
        assert!(learner(&repository, &catalog, pool_id).await.is_none());
    }

    // The typed storage boundary refuses a sibling's positive identity outright,
    // so the filtering above is a contract and not merely a convention.
    let forged = PhaseCWindowQualificationV1 {
        admitted: false,
        diagnostics: qualification.diagnostics.clone(),
    };
    let smuggled = ExpectedAttemptPathV1 {
        pool_id: first,
        ..second_path.clone()
    };
    assert!(
        persist_catalog_qualified_phase_c_window_v1(
            &repository,
            &catalog,
            &smuggled,
            PhaseCWindowAccountingV1 {
                window_sequence: 3,
                started_at: "1970-01-01T00:03:00Z".into(),
                ended_at: "1970-01-01T00:04:00Z".into(),
                ..accounting()
            },
            &forged,
            &fence,
        )
        .await
        .is_ok(),
        "the wrapper filters foreign identities rather than failing the write"
    );
    let stored = repository
        .controller_window_summary_for_test(first, 3)
        .await
        .expect("read persisted summary")
        .expect("persisted summary");
    assert!(
        stored
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.pool_id == 0 || diagnostic.pool_id == first),
        "{:?}",
        stored.diagnostics
    );
}
