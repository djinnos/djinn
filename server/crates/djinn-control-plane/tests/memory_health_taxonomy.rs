//! End-to-end taxonomy-v1 `memory_health` tool contract tests.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_db::repositories::retrieval_trace::{
    CreateRetrievalTraceParams, CreateRetrievalTraceTerminalParams, DEFAULT_CANDIDATE_CAP,
    KnowledgeTraceDispositionCounts, KnowledgeTraceTerminalState, RetrievalTraceEntryPoint,
    RetrievalTraceOutcome, RetrievalTraceRepository,
};
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Iso8601};

async fn insert_terminal(
    repo: &RetrievalTraceRepository,
    project_id: &str,
    entry_point: RetrievalTraceEntryPoint,
    terminal_state: KnowledgeTraceTerminalState,
    outcome: RetrievalTraceOutcome,
    terminal_at: &str,
    candidate_count: Option<i32>,
    injected_count: Option<i32>,
    dispositions: Option<KnowledgeTraceDispositionCounts>,
) -> String {
    let candidates = json!([]);
    let durations = json!({});
    repo.insert_terminal(CreateRetrievalTraceTerminalParams {
        trace: CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point,
            trigger: None,
            candidates: &candidates,
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &durations,
            estimated_injected_tokens: 1,
        },
        rollout_label: "cohort:memory-health-tool-test",
        outcome,
        terminal_state,
        terminal_at,
        candidate_count,
        injected_count,
        dispositions,
    })
    .await
    .expect("persist taxonomy-v1 retrieval terminal")
    .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_health_tool_assembles_persisted_taxonomy_groups_without_pooling() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();
    let requested = common::create_test_project(&db).await;
    let other = common::create_test_project(&db).await;
    let traces = RetrievalTraceRepository::new(db.clone());
    let terminal_at = OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .expect("format terminal timestamp");
    let injected = KnowledgeTraceDispositionCounts {
        confidence_filtered: 1,
        not_top_k: 1,
        oversized_skipped: 1,
        injected: 1,
        budget_pruned: 1,
    };

    // This requested-project group contains valid injected, starved, and
    // zero-candidate successes, plus error/cancelled terminals. A malformed
    // terminal makes only this group invalid; all authoritative valid-v1
    // counters still exclude it.
    insert_terminal(
        &traces,
        &requested.id,
        RetrievalTraceEntryPoint::LoadKnowledgeContext,
        KnowledgeTraceTerminalState::Success,
        RetrievalTraceOutcome::Injected,
        &terminal_at,
        Some(5),
        Some(1),
        Some(injected),
    )
    .await;
    insert_terminal(
        &traces,
        &requested.id,
        RetrievalTraceEntryPoint::LoadKnowledgeContext,
        KnowledgeTraceTerminalState::Success,
        RetrievalTraceOutcome::Empty,
        &terminal_at,
        Some(2),
        Some(0),
        Some(KnowledgeTraceDispositionCounts {
            confidence_filtered: 0,
            not_top_k: 1,
            oversized_skipped: 0,
            injected: 0,
            budget_pruned: 1,
        }),
    )
    .await;
    insert_terminal(
        &traces,
        &requested.id,
        RetrievalTraceEntryPoint::LoadKnowledgeContext,
        KnowledgeTraceTerminalState::Success,
        RetrievalTraceOutcome::Empty,
        &terminal_at,
        Some(0),
        Some(0),
        Some(KnowledgeTraceDispositionCounts {
            confidence_filtered: 0,
            not_top_k: 0,
            oversized_skipped: 0,
            injected: 0,
            budget_pruned: 0,
        }),
    )
    .await;
    for state in [
        KnowledgeTraceTerminalState::Error,
        KnowledgeTraceTerminalState::Cancelled,
    ] {
        insert_terminal(
            &traces,
            &requested.id,
            RetrievalTraceEntryPoint::LoadKnowledgeContext,
            state,
            RetrievalTraceOutcome::Error,
            &terminal_at,
            None,
            None,
            None,
        )
        .await;
    }
    let malformed_id = insert_terminal(
        &traces,
        &requested.id,
        RetrievalTraceEntryPoint::LoadKnowledgeContext,
        KnowledgeTraceTerminalState::Success,
        RetrievalTraceOutcome::Injected,
        &terminal_at,
        Some(5),
        Some(1),
        Some(injected),
    )
    .await;
    sqlx::query("UPDATE retrieval_traces SET injected_count = 4 WHERE id = $1")
        .bind(&malformed_id)
        .execute(db.pool())
        .await
        .expect("make taxonomy-v1 terminal malformed");

    // Legacy candidate JSON is deliberately unclassified, never inferred.
    let legacy = traces
        .insert(CreateRetrievalTraceParams {
            project_id: &requested.id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
            trigger: None,
            candidates: &json!([{ "outcome": "injected" }]),
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 99,
        })
        .await
        .expect("persist legacy trace");
    sqlx::query("UPDATE retrieval_traces SET terminal_at = $1 WHERE id = $2")
        .bind(&terminal_at)
        .bind(&legacy.id)
        .execute(db.pool())
        .await
        .expect("place legacy trace in health window");

    // A healthy entry point remains present next to the invalid group, while
    // another project's group proves the public operation applies its filter.
    for project_id in [&requested.id, &other.id] {
        insert_terminal(
            &traces,
            project_id,
            RetrievalTraceEntryPoint::Dispatch,
            KnowledgeTraceTerminalState::Success,
            RetrievalTraceOutcome::Injected,
            &terminal_at,
            Some(5),
            Some(1),
            Some(injected),
        )
        .await;
    }

    let response = harness
        .call_tool("memory_health", json!({ "project": requested.slug() }))
        .await
        .expect("dispatch memory_health");
    assert!(response["error"].is_null(), "{response}");
    let groups = response["retrieval"]["persisted"]["groups"]
        .as_array()
        .expect("serialized taxonomy-v1 groups");
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|group| group["project_id"] == requested.id));
    assert!(groups.iter().all(|group| group["project_id"] != other.id));

    let load = groups
        .iter()
        .find(|group| group["entry_point"] == "load_knowledge_context")
        .expect("requested load group");
    assert_eq!(load["taxonomy_version"], 1);
    assert!(load["window_start"].is_string());
    assert!(load["window_end"].is_string());
    assert!(load["refreshed_at"].is_string());
    assert_eq!(load["invalid"], true);
    assert_eq!(load["total_queries"], 5);
    assert_eq!(load["successful_queries"], 3);
    assert_eq!(load["errored_queries"], 2);
    assert_eq!(load["zero_candidate_queries"], 1);
    assert_eq!(load["zero_result_queries"], load["zero_candidate_queries"]);
    assert_eq!(load["candidate_bearing_queries"], 2);
    assert_eq!(load["starved_queries"], 1);
    assert_eq!(load["injected_queries"], 1);
    assert_eq!(load["candidate_total"], 7);
    assert_eq!(load["injected_total"], 1);
    assert_eq!(load["legacy_unclassified_queries"], 1);
    assert_eq!(load["invalid_taxonomy_queries"], 1);
    assert_eq!(load["validation_errors"][0]["trace_id"], malformed_id);
    assert_eq!(load["validation_errors"][0]["reason"], "injected_count_mismatch");
    let dispositions = &load["dispositions"];
    let histogram_total = [
        "confidence_filtered_total",
        "not_top_k_total",
        "oversized_skipped_total",
        "injected_total",
        "budget_pruned_total",
    ]
    .into_iter()
    .map(|field| dispositions[field].as_i64().expect("disposition total"))
    .sum::<i64>();
    assert_eq!(histogram_total, load["candidate_total"]);

    let dispatch = groups
        .iter()
        .find(|group| group["entry_point"] == "dispatch")
        .expect("healthy requested-project dispatch group");
    assert_eq!(dispatch["invalid"], false);
    assert_eq!(dispatch["total_queries"], 1);
    assert_eq!(dispatch["candidate_total"], 5);
    assert_eq!(dispatch["injected_total"], 1);
    assert_eq!(dispatch["invalid_taxonomy_queries"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_health_tool_reports_an_available_empty_taxonomy_window() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("memory_health", json!({ "project": project.slug() }))
        .await
        .expect("dispatch memory_health");
    assert_eq!(response["retrieval"]["persisted"]["status"], "available");
    assert!(response["retrieval"]["persisted"]["window_start"].is_string());
    assert!(response["retrieval"]["persisted"]["window_end"].is_string());
    assert!(response["retrieval"]["persisted"]["groups"]
        .as_array()
        .expect("empty taxonomy-v1 groups")
        .is_empty());
}
