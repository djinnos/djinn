use super::*;
use crate::repositories::retrieval_trace::{
    CreateRetrievalTraceTerminalParams, KNOWLEDGE_TRACE_TAXONOMY_VERSION_V1,
    KnowledgeTraceDispositionCounts, KnowledgeTraceTerminalState, TaxonomyV1RetrievalHealthCounts,
};

fn terminal_params<'a>(
    project_id: &'a str,
    candidates: &'a serde_json::Value,
    durations_ms: &'a serde_json::Value,
) -> CreateRetrievalTraceTerminalParams<'a> {
    CreateRetrievalTraceTerminalParams {
        trace: CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
            trigger: None,
            candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms,
            estimated_injected_tokens: 10,
        },
        rollout_label: "cohort:terminal-test",
        outcome: RetrievalTraceOutcome::Injected,
        terminal_state: KnowledgeTraceTerminalState::Success,
        terminal_at: "2026-07-20T12:00:00.000Z",
        candidate_count: Some(5),
        injected_count: Some(1),
        dispositions: Some(KnowledgeTraceDispositionCounts {
            confidence_filtered: 1,
            not_top_k: 1,
            oversized_skipped: 1,
            injected: 1,
            budget_pruned: 1,
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taxonomy_v1_rollup_groups_terminal_windows_and_excludes_invalid_rows() {
    let db = test_db();
    let project_a = "019f4900-0000-7000-8000-000000000139";
    let project_b = "019f4900-0000-7000-8000-000000000140";
    seed_project(&db, project_a).await;
    seed_project(&db, project_b).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let candidates = json!([]);
    let legacy_candidates = json!([
        {"note_id":"legacy-candidate-a","rank":1,"confidence":0.99,"outcome":"injected"},
        {"note_id":"legacy-candidate-b","rank":2,"confidence":0.98,"outcome":"skipped","skipped_reason":"budget_pruned"}
    ]);
    let durations = json!({});

    repo.insert_terminal(terminal_params(project_a, &candidates, &durations))
        .await
        .unwrap();
    let mut starved = terminal_params(project_a, &candidates, &durations);
    starved.candidate_count = Some(2);
    starved.injected_count = Some(0);
    starved.outcome = RetrievalTraceOutcome::Empty;
    starved.dispositions = Some(KnowledgeTraceDispositionCounts {
        confidence_filtered: 0,
        not_top_k: 1,
        oversized_skipped: 0,
        injected: 0,
        budget_pruned: 1,
    });
    repo.insert_terminal(starved).await.unwrap();
    let mut zero = terminal_params(project_a, &candidates, &durations);
    zero.outcome = RetrievalTraceOutcome::Empty;
    zero.candidate_count = Some(0);
    zero.injected_count = Some(0);
    zero.dispositions = Some(KnowledgeTraceDispositionCounts {
        confidence_filtered: 0,
        not_top_k: 0,
        oversized_skipped: 0,
        injected: 0,
        budget_pruned: 0,
    });
    repo.insert_terminal(zero).await.unwrap();
    for state in [
        KnowledgeTraceTerminalState::Error,
        KnowledgeTraceTerminalState::Cancelled,
    ] {
        let mut exceptional = terminal_params(project_a, &candidates, &durations);
        exceptional.terminal_state = state;
        exceptional.outcome = RetrievalTraceOutcome::Error;
        exceptional.candidate_count = None;
        exceptional.injected_count = None;
        exceptional.dispositions = None;
        repo.insert_terminal(exceptional).await.unwrap();
    }

    let malformed = repo
        .insert_terminal(terminal_params(project_a, &candidates, &durations))
        .await
        .unwrap();
    sqlx::query("UPDATE retrieval_traces SET injected_count = 4 WHERE id = $1")
        .bind(&malformed.id)
        .execute(db.pool())
        .await
        .unwrap();
    let legacy = repo
        .insert(CreateRetrievalTraceParams {
            project_id: project_a,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
            trigger: None,
            candidates: &legacy_candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &durations,
            estimated_injected_tokens: 9_999,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE retrieval_traces SET terminal_at = $1 WHERE id = $2")
        .bind("2026-07-20T12:00:00.000Z")
        .bind(&legacy.id)
        .execute(db.pool())
        .await
        .unwrap();
    let mut other = terminal_params(project_b, &candidates, &durations);
    other.trace.entry_point = RetrievalTraceEntryPoint::Dispatch;
    repo.insert_terminal(other).await.unwrap();
    let mut outside = terminal_params(project_a, &candidates, &durations);
    // This is the exclusive endpoint with a noncanonical fractional spelling.
    // A lexical VARCHAR comparison would incorrectly include it.
    outside.terminal_at = "2026-07-20T13:00:00.0000Z";
    repo.insert_terminal(outside).await.unwrap();

    let groups = repo
        .taxonomy_v1_health_rollup(
            "2026-07-20T12:00:00.000Z",
            "2026-07-20T13:00:00.000Z",
            "2026-07-20T13:05:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(groups.len(), 2);
    let load = groups
        .iter()
        .find(|g| {
            g.project_id == project_a
                && g.entry_point == RetrievalTraceEntryPoint::LoadKnowledgeContext
        })
        .unwrap();
    assert!(load.invalid);
    assert_eq!(load.taxonomy_version, 1);
    assert_eq!(load.window_start, "2026-07-20T12:00:00.000Z");
    assert_eq!(load.window_end, "2026-07-20T13:00:00.000Z");
    assert_eq!(load.refreshed_at, "2026-07-20T13:05:00.000Z");
    assert_eq!(load.counts.total_queries, 5);
    assert_eq!(load.counts.successful_queries, 3);
    assert_eq!(load.counts.errored_queries, 2);
    assert_eq!(load.counts.zero_candidate_queries, 1);
    assert_eq!(load.counts.candidate_bearing_queries, 2);
    assert_eq!(load.counts.starved_queries, 1);
    assert_eq!(load.counts.injected_queries, 1);
    assert_eq!(load.counts.candidate_total, 7);
    assert_eq!(load.counts.injected_total, 1);
    assert_eq!(
        load.counts.confidence_filtered_total
            + load.counts.not_top_k_total
            + load.counts.oversized_skipped_total
            + load.counts.injected_disposition_total
            + load.counts.budget_pruned_total,
        load.counts.candidate_total
    );
    assert_eq!(load.counts.legacy_unclassified_queries, 1);
    assert_eq!(load.counts.invalid_taxonomy_queries, 1);
    assert_eq!(load.validation_errors[0].reason, "injected_count_mismatch");
    let dispatch = groups.iter().find(|g| g.project_id == project_b).unwrap();
    assert_eq!(dispatch.entry_point, RetrievalTraceEntryPoint::Dispatch);
    assert!(!dispatch.invalid);
    assert_eq!(dispatch.counts.total_queries, 1);
    assert_eq!(dispatch.counts.legacy_unclassified_queries, 0);
    assert_eq!(dispatch.counts.invalid_taxonomy_queries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taxonomy_v1_rollup_classifies_int_overflow_shape_without_losing_healthy_groups() {
    let db = test_db();
    let malformed_project = "019f4900-0000-7000-8000-000000000141";
    let healthy_project = "019f4900-0000-7000-8000-000000000142";
    seed_project(&db, malformed_project).await;
    seed_project(&db, healthy_project).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let candidates = json!([]);
    let durations = json!({});

    let malformed = repo
        .insert_terminal(terminal_params(malformed_project, &candidates, &durations))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE retrieval_traces SET candidate_count = 0, injected_count = 0, \
         confidence_filtered_count = 2147483647, not_top_k_count = 2147483647, \
         oversized_skipped_count = 0, budget_pruned_count = 0 WHERE id = $1",
    )
    .bind(&malformed.id)
    .execute(db.pool())
    .await
    .unwrap();

    let mut healthy = terminal_params(healthy_project, &candidates, &durations);
    healthy.trace.entry_point = RetrievalTraceEntryPoint::Dispatch;
    repo.insert_terminal(healthy).await.unwrap();

    let groups = repo
        .taxonomy_v1_health_rollup(
            "2026-07-20T11:00:00.000Z",
            "2026-07-20T13:00:00.000Z",
            "2026-07-20T13:05:00.000Z",
        )
        .await
        .unwrap();

    assert_eq!(groups.len(), 2);
    let malformed_group = groups
        .iter()
        .find(|group| group.project_id == malformed_project)
        .unwrap();
    assert!(malformed_group.invalid);
    assert_eq!(
        malformed_group.counts,
        TaxonomyV1RetrievalHealthCounts {
            invalid_taxonomy_queries: 1,
            ..Default::default()
        }
    );
    assert_eq!(malformed_group.validation_errors.len(), 1);
    assert_eq!(
        malformed_group.validation_errors[0].reason,
        "injected_count_mismatch"
    );

    let healthy_group = groups
        .iter()
        .find(|group| group.project_id == healthy_project)
        .unwrap();
    assert!(!healthy_group.invalid);
    assert_eq!(
        healthy_group.entry_point,
        RetrievalTraceEntryPoint::Dispatch
    );
    assert_eq!(healthy_group.counts.total_queries, 1);
    assert_eq!(healthy_group.counts.successful_queries, 1);
    assert_eq!(healthy_group.counts.candidate_total, 5);
    assert_eq!(healthy_group.counts.injected_total, 1);
    assert_eq!(healthy_group.counts.confidence_filtered_total, 1);
    assert_eq!(healthy_group.counts.not_top_k_total, 1);
    assert_eq!(healthy_group.counts.oversized_skipped_total, 1);
    assert_eq!(healthy_group.counts.injected_disposition_total, 1);
    assert_eq!(healthy_group.counts.budget_pruned_total, 1);
    assert_eq!(healthy_group.counts.invalid_taxonomy_queries, 0);
    assert!(healthy_group.validation_errors.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_write_is_atomic_and_legacy_writes_are_null_taxonomy() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000136";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let candidates = json!([]);
    let durations = json!({});

    let row = repo
        .insert_terminal(terminal_params(project_id, &candidates, &durations))
        .await
        .unwrap();
    assert_eq!(
        row.knowledge_trace_taxonomy_version,
        Some(KNOWLEDGE_TRACE_TAXONOMY_VERSION_V1)
    );
    assert_eq!(row.terminal_state.as_deref(), Some("success"));
    assert_eq!(row.candidate_count, Some(5));
    assert_eq!(row.budget_pruned_count, Some(1));

    let mut invalid = terminal_params(project_id, &candidates, &durations);
    invalid.dispositions.as_mut().unwrap().budget_pruned = 2;
    assert!(
        format!("{}", repo.insert_terminal(invalid).await.unwrap_err()).contains("sum exactly")
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retrieval_traces WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 1);

    let legacy = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &durations,
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();
    assert_eq!(legacy.knowledge_trace_taxonomy_version, None);
    assert_eq!(legacy.terminal_state, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_terminal_without_injections_has_empty_outcome() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000138";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);
    let candidates = json!([]);
    let durations = json!({});
    let mut params = terminal_params(project_id, &candidates, &durations);
    params.trace.estimated_injected_tokens = 0;
    params.outcome = RetrievalTraceOutcome::Empty;
    params.candidate_count = Some(0);
    params.injected_count = Some(0);
    params.dispositions = Some(KnowledgeTraceDispositionCounts {
        confidence_filtered: 0,
        not_top_k: 0,
        oversized_skipped: 0,
        injected: 0,
        budget_pruned: 0,
    });

    let row = repo.insert_terminal(params).await.unwrap();

    assert_eq!(row.terminal_state.as_deref(), Some("success"));
    assert_eq!(row.outcome, RetrievalTraceOutcome::Empty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_and_cancelled_terminals_do_not_fabricate_dispositions() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000137";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);
    let candidates = json!([]);
    let durations = json!({});

    for state in [
        KnowledgeTraceTerminalState::Error,
        KnowledgeTraceTerminalState::Cancelled,
    ] {
        let mut params = terminal_params(project_id, &candidates, &durations);
        params.terminal_state = state;
        params.outcome = RetrievalTraceOutcome::Error;
        params.candidate_count = None;
        params.injected_count = None;
        params.dispositions = None;
        let row = repo.insert_terminal(params).await.unwrap();
        assert_eq!(row.terminal_state.as_deref(), Some(state.as_str()));
        assert_eq!(row.candidate_count, None);
        assert_eq!(row.confidence_filtered_count, None);
    }
}
