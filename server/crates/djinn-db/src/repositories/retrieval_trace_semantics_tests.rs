//! Trace rollout/outcome semantics tests for `retrieval_trace`.
//!
//! These remain a child of `retrieval_trace_tests` to reuse its in-memory DB and
//! candidate fixtures without expanding the parent test module past the size guard.

use super::*;

// ── Trace-level rollout/outcome semantics (u9hc) ────────────────────────────

#[test]
fn retrieval_trace_outcome_vocabulary_is_exact_and_rejects_unknown_values() {
    let mut actual: Vec<&str> = RetrievalTraceOutcome::ALL_VARIANTS
        .iter()
        .map(RetrievalTraceOutcome::as_str)
        .collect();
    actual.sort();
    let mut expected = RETRIEVAL_TRACE_OUTCOME_VALUES.to_vec();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(RetrievalTraceOutcome::parse("not_an_outcome"), None);
    assert!(serde_json::from_str::<RetrievalTraceOutcome>("\"not_an_outcome\"").is_err());
}

#[test]
fn legacy_outcome_classifier_is_conservative_for_malformed_and_contradictory_evidence() {
    let injected = json!([injected_candidate("n1", 1, 0.9)]);
    let skipped = json!([skipped_candidate("n2", 1, 0.2, SkippedReason::NotTopK)]);

    assert_eq!(
        classify_legacy_trace_outcome(&injected, 12),
        RetrievalTraceOutcome::Injected
    );
    assert_eq!(
        classify_legacy_trace_outcome(&skipped, 0),
        RetrievalTraceOutcome::Empty
    );
    assert_eq!(
        classify_legacy_trace_outcome(&json!([]), 0),
        RetrievalTraceOutcome::LegacyUnknown,
        "an empty legacy candidate list is not reliable empty evidence"
    );
    assert_eq!(
        classify_legacy_trace_outcome(&json!({"not": "an array"}), 100),
        RetrievalTraceOutcome::LegacyUnknown
    );
    assert_eq!(
        classify_legacy_trace_outcome(&injected, 0),
        RetrievalTraceOutcome::LegacyUnknown,
        "candidate evidence alone cannot claim injection without tokens"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_semantics_round_trip_and_project_filters_are_independent() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000109";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);
    let injected = json!([injected_candidate("n1", 1, 0.9)]);
    let skipped = json!([skipped_candidate("n2", 1, 0.2, SkippedReason::NotTopK)]);

    let injected_row = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &injected,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 42,
            },
            rollout_label: "cohort:canary",
            outcome: RetrievalTraceOutcome::Injected,
        })
        .await
        .unwrap();
    assert_eq!(injected_row.rollout_label, "cohort:canary");
    assert_eq!(injected_row.outcome, RetrievalTraceOutcome::Injected);

    let suppressed = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
                trigger: None,
                candidates: &skipped,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            },
            rollout_label: "off",
            outcome: RetrievalTraceOutcome::DisabledOff,
        })
        .await
        .unwrap();

    let detail = repo.get_by_id(&suppressed.id).await.unwrap().unwrap();
    assert_eq!(detail.rollout_label, "off");
    assert_eq!(detail.outcome, RetrievalTraceOutcome::DisabledOff);

    let by_rollout = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                rollout_label: Some("cohort:canary"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_rollout.len(), 1);
    assert_eq!(by_rollout[0].id, injected_row.id);

    let by_trace_outcome = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                trace_outcome: Some(RetrievalTraceOutcome::DisabledOff),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_trace_outcome.len(), 1);
    assert_eq!(by_trace_outcome[0].id, suppressed.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_semantics_reject_contradictions_and_returns_sql_write_errors() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000110";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let injected = json!([injected_candidate("n1", 1, 0.9)]);

    let invalid = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &injected,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 0,
            },
            rollout_label: "enabled",
            outcome: RetrievalTraceOutcome::Injected,
        })
        .await;
    assert!(invalid.is_err());
    assert!(format!("{}", invalid.unwrap_err()).contains("requires positive"));

    let invalid_disabled = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &injected,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 42,
            },
            rollout_label: "kill_switch",
            outcome: RetrievalTraceOutcome::DisabledKillSwitch,
        })
        .await;
    assert!(invalid_disabled.is_err());
    assert!(format!("{}", invalid_disabled.unwrap_err()).contains("disabled trace outcomes"));

    sqlx::query("DROP TABLE retrieval_traces")
        .execute(db.pool())
        .await
        .unwrap();
    let sql_error = repo
        .insert_with_semantics(CreateRetrievalTraceWithSemanticsParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point: RetrievalTraceEntryPoint::Dispatch,
                trigger: None,
                candidates: &injected,
                candidate_cap: 50,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &json!({}),
                estimated_injected_tokens: 42,
            },
            rollout_label: "enabled",
            outcome: RetrievalTraceOutcome::Injected,
        })
        .await;
    assert!(
        sql_error.is_err(),
        "write failures must be returned to callers"
    );
}
