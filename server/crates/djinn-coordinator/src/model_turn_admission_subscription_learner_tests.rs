//! Golden traces for aggregate-throughput learning and the concurrency
//! controller (task yh4d).
//!
//! Every number here is the epic's contract, not a snapshot of whatever the
//! implementation happened to produce: 100 aggregate tokens/s for one
//! 6,000-token stream and for two overlapping 3,000-token streams, 140 for two
//! overlapping 4,200-token streams, and a controller that walks 1 → 9 on eight
//! qualifying probes and back to 8 on one deduplicated loss.

use super::*;
use djinn_core::models::{Model, Pricing, Provider};
use djinn_db::{
    Database, ModelTurnControllerFence,
    repositories::test_support::seed_scoped_model_turn_admission_fixture,
};

use crate::model_turn_admission::{
    ExpectedAttemptPathV1, PhaseCWindowAccountingV1, PhaseCWindowDiagnosticCodeV1,
    PhaseCWindowDiagnosticV1, PhaseCWindowQualificationV1,
    persist_catalog_qualified_phase_c_window_v1,
};

const WINDOW_START_SECOND: i64 = 120;
const WINDOW_START: &str = "1970-01-01T00:02:00Z";
const WINDOW_END: &str = "1970-01-01T00:03:00Z";
const PROVIDER: &str = "yh4d-provider";
const MODEL: &str = "namespace/yh4d-model";

fn window() -> AlignedPhaseCWindowV1 {
    AlignedPhaseCWindowV1::new(WINDOW_START_SECOND).expect("aligned window")
}

/// A stream active over `[start, end)` emitting `tokens` in equal parts, one
/// emission per second, so tokens really are assigned by emission timestamp.
fn stream(start: i64, end: i64, tokens: i64) -> ActiveStreamV1 {
    let seconds = end - start;
    assert!(
        seconds > 0 && tokens % seconds == 0,
        "fixture must divide evenly"
    );
    ActiveStreamV1 {
        started_at_second: start,
        ended_at_second: end,
        emissions: (start..end)
            .map(|emitted_at_second| OutputTokenEmissionV1 {
                emitted_at_second,
                output_tokens: tokens / seconds,
            })
            .collect(),
    }
}

// ── Aggregate rate goldens ──────────────────────────────────────────────────

#[test]
fn one_six_thousand_token_stream_is_one_hundred_aggregate_tokens_per_second() {
    let throughput = aggregate_output_throughput_v1(window(), &[stream(120, 180, 6_000)]);
    assert_eq!(throughput.output_tokens, 6_000);
    assert_eq!(throughput.active_union_seconds, 60);
    assert_eq!(throughput.tokens_per_second, 100.0);
}

#[test]
fn two_overlapping_three_thousand_token_streams_are_also_one_hundred() {
    let streams = [stream(120, 180, 3_000), stream(120, 180, 3_000)];
    let throughput = aggregate_output_throughput_v1(window(), &streams);
    assert_eq!(throughput.output_tokens, 6_000);
    assert_eq!(
        throughput.active_union_seconds, 60,
        "two fully overlapping streams occupy 60 seconds of wall clock, not 120"
    );
    assert_eq!(throughput.tokens_per_second, 100.0);

    // Summed stream-seconds would have produced 50 tokens/s. The union is
    // therefore load-bearing and not an accidental agreement.
    let summed_stream_seconds: i64 = streams
        .iter()
        .map(|stream| stream.ended_at_second - stream.started_at_second)
        .sum();
    assert_eq!(summed_stream_seconds, 120);
    assert_ne!(
        throughput.tokens_per_second,
        throughput.output_tokens as f64 / summed_stream_seconds as f64
    );
}

#[test]
fn two_overlapping_forty_two_hundred_token_streams_are_one_hundred_forty() {
    let throughput = aggregate_output_throughput_v1(
        window(),
        &[stream(120, 180, 4_200), stream(120, 180, 4_200)],
    );
    assert_eq!(throughput.output_tokens, 8_400);
    assert_eq!(throughput.active_union_seconds, 60);
    assert_eq!(throughput.tokens_per_second, 140.0);
}

#[test]
fn crossing_streams_clip_to_the_half_open_window() {
    // Each stream straddles a boundary; only the in-window halves count, and
    // together they tile the window exactly once.
    let leading = stream(90, 150, 6_000); // 100/s, half inside
    let trailing = stream(150, 210, 6_000); // 100/s, half inside
    let throughput = aggregate_output_throughput_v1(window(), &[leading, trailing]);
    assert_eq!(throughput.active_union_seconds, 60);
    assert_eq!(throughput.output_tokens, 6_000);
    assert_eq!(throughput.tokens_per_second, 100.0);

    // The window is half-open: a stream that ends exactly at the start, or
    // begins exactly at the end, contributes nothing.
    for outside in [stream(60, 120, 6_000), stream(180, 240, 6_000)] {
        let throughput = aggregate_output_throughput_v1(window(), &[outside]);
        assert_eq!(throughput.active_union_seconds, 0);
        assert_eq!(throughput.output_tokens, 0);
        assert_eq!(throughput.tokens_per_second, 0.0);
    }

    // An emission timestamped at the exact end second belongs to the next
    // window, even when its stream is still active.
    let boundary = ActiveStreamV1 {
        started_at_second: 120,
        ended_at_second: 240,
        emissions: vec![
            OutputTokenEmissionV1 {
                emitted_at_second: 179,
                output_tokens: 6_000,
            },
            OutputTokenEmissionV1 {
                emitted_at_second: 180,
                output_tokens: 999_999,
            },
        ],
    };
    let throughput = aggregate_output_throughput_v1(window(), &[boundary]);
    assert_eq!(throughput.output_tokens, 6_000);
    assert_eq!(throughput.active_union_seconds, 60);
    assert_eq!(throughput.tokens_per_second, 100.0);
}

#[test]
fn partial_overlap_unions_rather_than_sums() {
    // [120,180) and [150,180): the union is the whole window, while summed
    // stream-seconds would be 90 and would report 66.67 tokens/s.
    let streams = [stream(120, 180, 3_000), stream(150, 180, 3_000)];
    let throughput = aggregate_output_throughput_v1(window(), &streams);
    assert_eq!(throughput.active_union_seconds, 60);
    assert_eq!(throughput.output_tokens, 6_000);
    assert_eq!(throughput.tokens_per_second, 100.0);
    let summed_stream_seconds: i64 = streams
        .iter()
        .map(|stream| stream.ended_at_second - stream.started_at_second)
        .sum();
    assert_eq!(summed_stream_seconds, 90);
    assert_ne!(
        throughput.tokens_per_second,
        throughput.output_tokens as f64 / summed_stream_seconds as f64
    );
}

// ── Eligibility, baseline, bootstrap ────────────────────────────────────────

#[test]
fn eligibility_needs_eight_turns_and_thirty_union_seconds() {
    let long = aggregate_output_throughput_v1(window(), &[stream(120, 180, 6_000)]);
    let exactly_thirty = aggregate_output_throughput_v1(window(), &[stream(120, 150, 3_000)]);
    let just_short = aggregate_output_throughput_v1(window(), &[stream(120, 149, 2_900)]);

    assert_eq!(exactly_thirty.active_union_seconds, 30);
    assert_eq!(just_short.active_union_seconds, 29);

    assert!(window_is_eligible_v1(8, &long));
    assert!(window_is_eligible_v1(8, &exactly_thirty));
    assert!(!window_is_eligible_v1(7, &long));
    assert!(!window_is_eligible_v1(8, &just_short));
    // Two overlapping half-windows still only buy 30 union seconds.
    let overlapping = aggregate_output_throughput_v1(
        window(),
        &[stream(120, 150, 1_500), stream(120, 150, 1_500)],
    );
    assert_eq!(overlapping.active_union_seconds, 30);
}

#[test]
fn baseline_is_an_ewma_with_alpha_zero_point_two() {
    assert_eq!(update_baseline_v1(None, 100.0), 100.0);
    // 0.2 * 200 + 0.8 * 100
    assert_eq!(update_baseline_v1(Some(100.0), 200.0), 120.0);
    assert_eq!(update_baseline_v1(Some(120.0), 120.0), 120.0);
}

#[test]
fn the_bootstrap_is_deterministic_and_confidence_bounded() {
    let samples = vec![100.0, 104.0, 96.0, 102.0, 98.0];
    let first = bootstrap_lower_bound_v1(&samples, 7).expect("bound");
    let again = bootstrap_lower_bound_v1(&samples, 7).expect("bound");
    assert_eq!(first, again, "the same seed must replay the same bound");
    assert!(
        first < 100.0,
        "a 95% lower bound must sit below the sample mean, got {first}"
    );
    assert!(bootstrap_lower_bound_v1(&[], 7).is_none());

    // A noisy sample set whose mean beats the baseline still fails when the
    // lower bound does not clear the 5% threshold.
    assert!(!growth_qualifies_v1(&[40.0, 170.0], 100.0, 7));
    // A tight sample set well above the threshold passes.
    assert!(growth_qualifies_v1(&[150.0, 152.0, 149.0], 100.0, 7));
    // Exactly at the threshold is not growth.
    assert!(!growth_qualifies_v1(&[105.0, 105.0, 105.0], 100.0, 7));
}

// ── Controller golden trace ─────────────────────────────────────────────────

fn qualified(completed_turns: i64) -> PhaseCLearnerWindowV1 {
    PhaseCLearnerWindowV1 {
        pool_id: 1,
        window_sequence: 2,
        started_at: WINDOW_START.into(),
        ended_at: WINDOW_END.into(),
        admitted_turns: completed_turns,
        completed_turns,
    }
}

/// An eligible window whose per-stream samples all sit at `rate`.
fn eligible_at(rate: f64) -> SubscriptionWindowObservationV1 {
    SubscriptionWindowObservationV1 {
        qualified: Some(qualified(8)),
        throughput: AggregateThroughputV1 {
            output_tokens: (rate * 60.0) as i64,
            active_union_seconds: 60,
            tokens_per_second: rate,
        },
        rate_samples: vec![rate, rate, rate],
        terminals: Vec::new(),
        bootstrap_seed: 42,
    }
}

fn loss(attempt: &str) -> AttemptTerminalObservationV1 {
    AttemptTerminalObservationV1 {
        attempt: attempt.to_owned(),
        terminal: ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::RateLimited),
    }
}

#[test]
fn the_controller_walks_one_to_nine_then_backs_off_to_eight() {
    let mut state = SubscriptionControllerStateV1::new();
    assert_eq!(state.target(), 1, "targets start at 1");

    // The first eligible window only establishes the baseline.
    let mut rate = 100.0;
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(rate)),
        ControllerTransitionV1::ProbeDidNotGrow
    );
    assert_eq!(state.target(), 1);
    assert_eq!(state.baseline(), Some(100.0));

    // Eight qualifying growth probes, each comfortably clearing the EWMA
    // baseline by more than 5%, take the target from 1 to 9.
    for probe in 1..=8 {
        rate *= 1.5;
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(rate)),
            ControllerTransitionV1::Grew,
            "probe {probe} must qualify"
        );
        assert_eq!(state.target(), 1 + probe);
    }
    assert_eq!(state.target(), 9);

    // One deduplicated loss: floor(0.9 * 9) = 8.
    let mut window = eligible_at(rate * 1.5);
    window.terminals = vec![loss("attempt-a")];
    assert_eq!(
        observe_window_v1(&mut state, &window),
        ControllerTransitionV1::BackedOff,
        "a loss takes precedence over the growth this same window would show"
    );
    assert_eq!(state.target(), 8);

    // The same loss again is a duplicate and holds.
    let mut duplicate = eligible_at(rate * 4.0);
    duplicate.terminals = vec![loss("attempt-a")];
    assert_eq!(
        observe_window_v1(&mut state, &duplicate),
        ControllerTransitionV1::HeldDuplicateLoss
    );
    assert_eq!(state.target(), 8);

    // An unqualified window holds and moves nothing, including the baseline.
    let baseline = state.baseline();
    let mut unqualified = eligible_at(rate * 4.0);
    unqualified.qualified = None;
    assert_eq!(
        observe_window_v1(&mut state, &unqualified),
        ControllerTransitionV1::HeldUnqualified
    );
    assert_eq!(state.target(), 8);
    assert_eq!(state.baseline(), baseline);

    // So does an eligible-looking window that is under the thresholds.
    let mut ineligible = eligible_at(rate * 4.0);
    ineligible.qualified = Some(qualified(7));
    assert_eq!(
        observe_window_v1(&mut state, &ineligible),
        ControllerTransitionV1::HeldIneligible
    );
    assert_eq!(state.target(), 8);
    assert_eq!(state.baseline(), baseline);
}

#[test]
fn three_non_growing_probes_are_rejected_and_five_plateau_windows_hold() {
    let mut state = SubscriptionControllerStateV1::new();
    // Establish a baseline, then feed a flat plateau.
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(100.0)),
        ControllerTransitionV1::ProbeDidNotGrow
    );
    for probe in 1..=2 {
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(100.0)),
            ControllerTransitionV1::ProbeDidNotGrow,
            "plateau probe {probe}"
        );
        assert_eq!(state.non_growing_probes(), probe);
    }
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(100.0)),
        ControllerTransitionV1::ProbeRejected,
        "the third consecutive non-growing probe suspends probing"
    );
    assert_eq!(state.remaining_hold_windows(), 5);
    assert_eq!(state.target(), 1);

    // Five held windows follow, and growth cannot slip through any of them.
    for held in 1..=5 {
        assert_eq!(
            observe_window_v1(&mut state, &eligible_at(1_000.0)),
            ControllerTransitionV1::HeldPlateau,
            "held window {held}"
        );
        assert_eq!(state.target(), 1);
        assert_eq!(state.remaining_hold_windows(), 5 - held);
    }
    // Probing resumes on the sixth.
    assert_eq!(
        observe_window_v1(&mut state, &eligible_at(10_000.0)),
        ControllerTransitionV1::Grew
    );
    assert_eq!(state.target(), 2);
}

#[test]
fn a_loss_can_never_also_count_as_growth() {
    let mut state = SubscriptionControllerStateV1::new();
    observe_window_v1(&mut state, &eligible_at(100.0));
    for _ in 0..4 {
        let mut growing = eligible_at(state.baseline().expect("baseline") * 4.0);
        growing.terminals = Vec::new();
        assert_eq!(
            observe_window_v1(&mut state, &growing),
            ControllerTransitionV1::Grew
        );
    }
    assert_eq!(state.target(), 5);

    // The very same window shape, plus one typed B1 failure, backs off instead.
    let mut with_loss = eligible_at(state.baseline().expect("baseline") * 4.0);
    with_loss.terminals = vec![loss("attempt-b")];
    assert_eq!(
        observe_window_v1(&mut state, &with_loss),
        ControllerTransitionV1::BackedOff
    );
    assert_eq!(state.target(), 4);

    // Completed and aborted terminals are not losses.
    for terminal in [
        ProviderAttemptTerminalV1::Completed,
        ProviderAttemptTerminalV1::Aborted,
    ] {
        let mut window = eligible_at(state.baseline().expect("baseline") * 4.0);
        window.terminals = vec![AttemptTerminalObservationV1 {
            attempt: "attempt-c".to_owned(),
            terminal,
        }];
        assert_eq!(
            observe_window_v1(&mut state, &window),
            ControllerTransitionV1::Grew,
            "{terminal:?} must not read as a loss"
        );
    }
}

#[test]
fn the_target_never_leaves_one_through_thirty_two() {
    let mut state = SubscriptionControllerStateV1::new();
    let mut rate = 100.0;
    observe_window_v1(&mut state, &eligible_at(rate));
    // Far more qualifying probes than the ceiling allows.
    for _ in 0..64 {
        rate *= 1.5;
        observe_window_v1(&mut state, &eligible_at(rate));
        assert!((MIN_TARGET..=MAX_TARGET).contains(&state.target()));
    }
    assert_eq!(state.target(), MAX_TARGET);

    // And far more distinct losses than the floor allows.
    for index in 0..64 {
        let mut window = eligible_at(rate);
        window.terminals = vec![loss(&format!("attempt-{index}"))];
        observe_window_v1(&mut state, &window);
        assert!((MIN_TARGET..=MAX_TARGET).contains(&state.target()));
    }
    assert_eq!(state.target(), MIN_TARGET);
}

// ── Production ingestion ────────────────────────────────────────────────────

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

fn yh4d_catalog() -> CatalogService {
    let catalog = CatalogService::new();
    catalog.add_custom_provider(
        Provider {
            id: PROVIDER.into(),
            name: "yh4d Provider".into(),
            npm: String::new(),
            env_vars: vec!["YH4D_API_KEY".into()],
            base_url: "https://example.invalid/v1".into(),
            docs_url: String::new(),
            is_openai_compatible: true,
        },
        vec![Model {
            id: MODEL.into(),
            provider_id: PROVIDER.into(),
            name: "yh4d Model".into(),
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

fn activity() -> WindowActivityV1 {
    WindowActivityV1 {
        streams: vec![stream(120, 180, 3_000), stream(120, 180, 3_000)],
        terminals: Vec::new(),
    }
}

/// Ingestion reads the verdict off the durable ledger through gscv's seam. A
/// caller cannot assert that a diagnostic window is trainable, and a window that
/// stops resolving in the active catalog stops feeding the learner.
#[tokio::test]
async fn production_ingestion_only_learns_from_a_catalog_qualified_durable_window() {
    let db = Database::ephemeral().await.expect("db");
    let fence = live_fence(&db).await;
    let pool_id = seed_scoped_model_turn_admission_fixture(
        &db,
        "yh4d-ingest",
        PROVIDER,
        MODEL,
        "shadow",
        "supported",
        1,
    )
    .await;
    let repository = ModelTurnAdmissionRepository::new(db);
    let catalog = yh4d_catalog();
    // A descendant module of `model_turn_admission`, so the deliberately
    // private route fields are constructible here exactly as production does.
    let path = ExpectedAttemptPathV1 {
        slot_pod_uid: "slot-uid".into(),
        deployment_revision: "revision-1".into(),
        provider: PROVIDER.into(),
        model_scope: MODEL.into(),
        pool_id,
    };
    let accounting = |completed: i64| PhaseCWindowAccountingV1 {
        window_sequence: 2,
        started_at: WINDOW_START.into(),
        ended_at: WINDOW_END.into(),
        admitted_turns: completed,
        completed_turns: completed,
    };
    let mut state = SubscriptionControllerStateV1::new();
    let request = QualifiedWindowRequestV1 {
        pool_id,
        window: window(),
        started_at: WINDOW_START.into(),
        ended_at: WINDOW_END.into(),
    };
    let ingest = async |state: &mut SubscriptionControllerStateV1| {
        ingest_qualified_window_v1(&repository, &catalog, state, &request, &activity())
            .await
            .expect("ingest")
    };

    // Absent: nothing persisted yet.
    assert_eq!(
        ingest(&mut state).await,
        ControllerTransitionV1::HeldUnqualified
    );
    assert_eq!(state.baseline(), None);

    // A diagnostic window is durable but never trainable, so it cannot reach a
    // rate or state transition either.
    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &path,
        accounting(8),
        &PhaseCWindowQualificationV1 {
            admitted: false,
            diagnostics: vec![PhaseCWindowDiagnosticV1 {
                pool_id,
                code: PhaseCWindowDiagnosticCodeV1::MissingUsage,
            }],
        },
        &fence,
    )
    .await
    .expect("persist diagnostic window");
    assert_eq!(
        ingest(&mut state).await,
        ControllerTransitionV1::HeldUnqualified
    );
    assert_eq!(state.baseline(), None);

    // A trainable window over the same bounds does reach the learner, and it
    // arrives with the aggregate rate — 100, not 50.
    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &path,
        accounting(8),
        &PhaseCWindowQualificationV1 {
            admitted: true,
            diagnostics: Vec::new(),
        },
        &fence,
    )
    .await
    .expect("persist trainable window");
    assert_eq!(
        ingest(&mut state).await,
        ControllerTransitionV1::ProbeDidNotGrow
    );
    assert_eq!(state.baseline(), Some(100.0));

    // A window under the completed-turn threshold is eligible for nothing.
    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &path,
        accounting(7),
        &PhaseCWindowQualificationV1 {
            admitted: true,
            diagnostics: Vec::new(),
        },
        &fence,
    )
    .await
    .expect("persist short window");
    assert_eq!(
        ingest(&mut state).await,
        ControllerTransitionV1::HeldIneligible
    );

    // Losing the route from the active catalog stops the learner cold.
    persist_catalog_qualified_phase_c_window_v1(
        &repository,
        &catalog,
        &path,
        accounting(8),
        &PhaseCWindowQualificationV1 {
            admitted: true,
            diagnostics: Vec::new(),
        },
        &fence,
    )
    .await
    .expect("persist trainable window");
    catalog.remove_custom_provider(PROVIDER);
    assert_eq!(
        ingest(&mut state).await,
        ControllerTransitionV1::HeldUnqualified
    );

    // Boundary-mismatched bounds are invisible even while the row is present.
    let restored = yh4d_catalog();
    let mut other = SubscriptionControllerStateV1::new();
    assert_eq!(
        ingest_qualified_window_v1(
            &repository,
            &restored,
            &mut other,
            &QualifiedWindowRequestV1 {
                pool_id,
                window: AlignedPhaseCWindowV1::new(180).expect("aligned window"),
                started_at: "1970-01-01T00:03:00Z".into(),
                ended_at: "1970-01-01T00:04:00Z".into(),
            },
            &activity(),
        )
        .await
        .expect("ingest"),
        ControllerTransitionV1::HeldUnqualified
    );
    assert_eq!(other.baseline(), None);
}

/// The learner has exactly one ingestion seam, and it is gscv's catalog-
/// qualified one. No raw controller-window query and no caller-supplied verdict
/// may appear in this module's production source.
#[test]
fn learner_ingestion_has_no_raw_or_in_memory_bypass() {
    let source = include_str!("model_turn_admission_subscription_learner.rs");
    let production = source
        .split("\n#[cfg(test)]")
        .next()
        .expect("production part");
    // Split so this assertion is not itself a match.
    let seam = concat!("learner_catalog_qualified_", "phase_c_window_v1(");
    assert_eq!(
        production.matches(seam).count(),
        1,
        "exactly one call to the catalog-qualified learner seam"
    );
    for forbidden in [
        concat!("model_turn_", "controller_windows"),
        concat!("sqlx", "::query"),
        concat!(".learner_", "window("),
        // No emergency cap, no breaker reset, no process-local admission
        // semaphore, no resident scheduler, and no raw sensitive identifier.
        "emergency_cap",
        "emergency cap",
        "reset_breaker",
        "breaker_reset",
        "Semaphore",
        "resident",
        "credential_id",
        "user_id",
        "account_id",
        "project_id",
        "request_id",
        "lease_id",
        "slot_pod_uid",
        "snapshot.json",
    ] {
        assert!(
            !production.contains(forbidden),
            "the learner must not contain {forbidden}"
        );
    }
    // The qualified verdict is read off the durable ledger, never taken from
    // the activity the caller supplies.
    assert!(!production.contains("activity.qualified"));
    assert!(production.contains("qualified: Option<PhaseCLearnerWindowV1>"));
}
