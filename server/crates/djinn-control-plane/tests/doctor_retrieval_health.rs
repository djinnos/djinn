//! Integration coverage for the production retrieval zero-result Doctor check.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::doctor::{RETRIEVAL_ZERO_RESULT_NAME, checks::retrieval::RetrievalHealthConfig};
use djinn_db::repositories::retrieval_trace::{
    CreateRetrievalTraceParams, DEFAULT_CANDIDATE_CAP, RetrievalTraceEntryPoint,
    RetrievalTraceRepository,
};
use djinn_db::{DoctorFindingRepository, RecentDoctorFindings};
use serde_json::json;
use time::{Duration, OffsetDateTime, format_description::well_known::Iso8601};

async fn harness() -> McpTestHarness {
    let harness = McpTestHarness::new().await;
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    harness
}

async fn insert_traces(
    repo: &RetrievalTraceRepository,
    project_id: &str,
    zero_results: usize,
    non_zero_results: usize,
) -> Vec<String> {
    let zero_candidates = json!([]);
    let candidates = json!([{"note_id":"note-1","outcome":"injected","rank":1}]);
    let mut trace_ids = Vec::with_capacity(zero_results + non_zero_results);
    for (count, candidates) in [
        (zero_results, &zero_candidates),
        (non_zero_results, &candidates),
    ] {
        for _ in 0..count {
            trace_ids.push(
                repo.insert(CreateRetrievalTraceParams {
                    project_id,
                    session_id: None,
                    task_run_id: None,
                    task_id: None,
                    entry_point: RetrievalTraceEntryPoint::Dispatch,
                    trigger: None,
                    candidates,
                    candidate_cap: DEFAULT_CANDIDATE_CAP,
                    candidate_cap_exceeded: false,
                    sampling_metadata: None,
                    durations_ms: &json!({}),
                    estimated_injected_tokens: 0,
                })
                .await
                .expect("insert retrieval trace")
                .id,
            );
        }
    }
    trace_ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_run_retrieval_check_persists_only_strictly_above_threshold() {
    let harness = harness().await;
    let db = harness.db().clone();
    let above = common::create_test_project(&db).await;
    let equal = common::create_test_project(&db).await;
    let traces = RetrievalTraceRepository::new(db.clone());

    // Both projects meet the default floor (20). 12/22 is above 0.50; 10/20
    // equals it and therefore must be suppressed.
    insert_traces(&traces, &above.id, 12, 10).await;
    insert_traces(&traces, &equal.id, 10, 10).await;

    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names": [RETRIEVAL_ZERO_RESULT_NAME]}),
        )
        .await
        .expect("dispatch doctor_run");
    assert_eq!(response["ok"], true);
    assert!(
        response["registered_checks"]
            .as_array()
            .expect("registered checks")
            .iter()
            .any(|check| check["name"] == RETRIEVAL_ZERO_RESULT_NAME)
    );

    // An explicit retrieval-only request must not accidentally execute every
    // ordinary globally registered check.
    let results = response["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["check"]["name"], RETRIEVAL_ZERO_RESULT_NAME);
    assert_eq!(results[0]["ran"], true);
    let entries = results[0]["findings"].as_array().expect("finding entries");
    assert_eq!(entries.len(), 1);

    let finding_id = entries[0]["finding_id"].as_str().expect("persisted id");
    let findings = DoctorFindingRepository::new(db);
    let persisted = findings
        .get(finding_id)
        .await
        .expect("read persisted finding")
        .expect("finding exists");
    assert_eq!(persisted.check_name, RETRIEVAL_ZERO_RESULT_NAME);
    assert_eq!(
        persisted.entity_ids["project_id"].as_str(),
        Some(above.id.as_str())
    );

    // The persisted evidence is the complete, immutable shared-source snapshot.
    let evidence = &persisted.evidence;
    assert_eq!(evidence["project_id"].as_str(), Some(above.id.as_str()));
    assert!(evidence["window"]["start"].as_str().is_some());
    assert!(evidence["window"]["end"].as_str().is_some());
    assert_eq!(evidence["threshold"].as_f64(), Some(0.5));
    assert_eq!(evidence["floor"].as_i64(), Some(20));
    assert_eq!(evidence["numerator"].as_i64(), Some(12));
    assert_eq!(evidence["denominator"].as_i64(), Some(22));
    assert_eq!(evidence["rate"].as_f64(), Some(12.0 / 22.0));
    assert_eq!(
        evidence["per_entry_point_counts"]["dispatch"]["total_queries"],
        22
    );
    assert_eq!(
        evidence["per_entry_point_counts"]["dispatch"]["zero_result_queries"],
        12
    );

    let retrieval_findings = findings
        .list_recent(RecentDoctorFindings {
            check_name: Some(RETRIEVAL_ZERO_RESULT_NAME.to_owned()),
            ..Default::default()
        })
        .await
        .expect("list retrieval findings");
    assert!(
        retrieval_findings.iter().all(|finding| {
            finding.entity_ids["project_id"].as_str() != Some(equal.id.as_str())
        }),
        "equality at the threshold must not emit a finding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_retrieval_config_drives_memory_health_and_doctor_prefetch() {
    // This deliberately differs from every default. The one config is injected
    // through McpState::with_enrichment by the harness and both production MCP
    // paths below must read that state-held value.
    let config = RetrievalHealthConfig::new(72, 0.75, 7).expect("valid non-default config");
    let db = djinn_db::Database::open_in_memory().expect("open test database");
    let harness = McpTestHarness::from_db_with_retrieval_config(db, config);
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    let project = common::create_test_project(harness.db()).await;
    let at_threshold = common::create_test_project(harness.db()).await;
    let traces = RetrievalTraceRepository::new(harness.db().clone());

    // Eight queries clears the injected floor of seven but would be suppressed
    // by the default floor. Its 7/8 zero-result rate clears the injected 0.75
    // threshold. The second project's 6/8 rate equals 0.75 and is suppressed;
    // it would be a finding under the default 0.50 threshold.
    let project_trace_ids = insert_traces(&traces, &project.id, 7, 1).await;
    insert_traces(&traces, &at_threshold.id, 6, 2).await;

    // The final trace is the only non-zero result. Backdate it to 48 hours:
    // it is included by the injected 72-hour rollup but excluded by a
    // fixed/default 24-hour prefetch window. The Doctor evidence below must
    // therefore retain all eight traces, not merely describe a 72-hour check.
    let inside_72_outside_24 = OffsetDateTime::now_utc() - Duration::hours(48);
    let inside_72_outside_24 = inside_72_outside_24
        .format(&Iso8601::DEFAULT)
        .expect("format backdated trace timestamp");
    traces
        .update_created_at(&project_trace_ids[7], &inside_72_outside_24)
        .await
        .expect("backdate window-sensitive retrieval trace");

    let health = harness
        .call_tool("memory_health", json!({"project": project.slug()}))
        .await
        .expect("dispatch memory_health");
    assert_eq!(health["retrieval"]["config_window_hours"], 72);
    assert!(health["retrieval"]["persisted"]["window_start"].is_string());
    assert!(health["retrieval"]["persisted"]["window_end"].is_string());
    // The legacy test helper deliberately writes pre-taxonomy traces. They are
    // excluded from authoritative taxonomy-v1 groups rather than inferred from
    // candidate JSON by memory_health.
    assert!(
        health["retrieval"]["persisted"]["groups"]
            .as_array()
            .expect("taxonomy-v1 groups")
            .is_empty()
    );

    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names": [RETRIEVAL_ZERO_RESULT_NAME]}),
        )
        .await
        .expect("dispatch doctor_run");
    let findings = response["results"][0]["findings"]
        .as_array()
        .expect("retrieval finding entries");
    assert_eq!(findings.len(), 1, "injected floor must permit the finding");

    let finding_id = findings[0]["finding_id"]
        .as_str()
        .expect("persisted finding id");
    let finding = DoctorFindingRepository::new(harness.db().clone())
        .get(finding_id)
        .await
        .expect("read persisted finding")
        .expect("finding exists");
    let evidence = &finding.evidence;
    assert_eq!(evidence["threshold"].as_f64(), Some(0.75));
    assert_eq!(evidence["floor"].as_i64(), Some(7));
    assert_eq!(evidence["numerator"].as_i64(), Some(7));
    // Includes the 48-hour non-zero trace; a 24-hour prefetch would produce
    // denominator 7 instead, even if the check retained this config's text.
    assert_eq!(evidence["denominator"].as_i64(), Some(8));
    assert!(evidence["window"]["start"].is_string());
    assert!(evidence["window"]["end"].is_string());
    assert!(
        finding
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("last 72 hours")),
        "Doctor must build the check with the injected window: {:?}",
        finding.detail
    );
}
