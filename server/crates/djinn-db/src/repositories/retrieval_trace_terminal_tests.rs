use super::*;
use crate::repositories::retrieval_trace::{
    CreateRetrievalTraceTerminalParams, KNOWLEDGE_TRACE_TAXONOMY_VERSION_V1,
    KnowledgeTraceDispositionCounts, KnowledgeTraceTerminalState,
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
