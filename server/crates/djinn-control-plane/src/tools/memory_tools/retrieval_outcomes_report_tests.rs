//! Importer: `memory_tools/mod.rs`; exercises direct production operation dispatch.
//! Public APIs under test: `McpTestHarness::call_tool` and report request/response DTOs.
//! Data shapes: `{project,start,end,timezone}` JSON and report/error envelopes.
//! Task: add cross-surface dispatch and retention-boundary tests for
//! `memory_retrieval_outcomes_report`.

use super::RetrievalOutcomesReportParams;

#[test]
fn retrieval_outcomes_report_dto_requires_explicit_interval() {
    assert!(serde_json::from_value::<RetrievalOutcomesReportParams>(serde_json::json!({"project":"p","start":"2026-07-01T00:00:00Z","end":"2026-07-02T00:00:00Z","timezone":"UTC"})).is_ok());
    assert!(
        serde_json::from_value::<RetrievalOutcomesReportParams>(
            serde_json::json!({"project":"p","start":"x","end":"y","task_id":"no"})
        )
        .is_err()
    );
}

#[test]
fn retrieval_outcomes_report_dispatch_contract_rejects_unsupported_arguments() {
    let request = serde_json::json!({
        "project": "p",
        "start": "2026-07-01T00:00:00Z",
        "end": "2026-07-02T00:00:00Z",
        "timezone": "UTC",
        "task_id": "must-not-be-used-as-a-fallback"
    });
    let error = serde_json::from_value::<RetrievalOutcomesReportParams>(request)
        .err()
        .expect("report DTO must reject task_id rather than silently join through it");
    assert!(error.to_string().contains("unknown field `task_id`"));
}

mod dispatch_boundary {
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProjectRepository};
    use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

    use crate::test_support::McpTestHarness;

    fn timestamp(value: OffsetDateTime) -> String {
        value.format(&Rfc3339).expect("format RFC-3339 timestamp")
    }

    async fn harness_with_project() -> (McpTestHarness, String) {
        let db = Database::ephemeral().await.expect("ephemeral database");
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create("report-boundary", "boundary-owner", "boundary-repo")
            .await
            .expect("create report project");
        (McpTestHarness::from_db(db), project.slug())
    }

    #[tokio::test]
    async fn direct_dispatch_routes_valid_report_and_preserves_explicit_interval() {
        let (harness, project) = harness_with_project().await;
        let end = OffsetDateTime::now_utc() - Duration::minutes(1);
        let start = end - Duration::hours(2);
        let start = timestamp(start);
        let end = timestamp(end);

        let response = harness
            .call_tool(
                "memory_retrieval_outcomes_report",
                serde_json::json!({
                    "project": project,
                    "start": start,
                    "end": end,
                    "timezone": "America/New_York"
                }),
            )
            .await
            .expect("operation-name dispatch reaches report handler");

        assert_eq!(response["error"], serde_json::Value::Null);
        assert_eq!(response["report"]["start"], start);
        assert_eq!(response["report"]["end"], end);
        assert_eq!(response["report"]["timezone"], "America/New_York");
    }

    #[tokio::test]
    async fn direct_dispatch_rejects_invalid_and_outside_retention_without_clipping() {
        let (harness, project) = harness_with_project().await;
        let now = OffsetDateTime::now_utc() - Duration::minutes(1);
        let invalid_start = timestamp(now);
        let invalid_end = timestamp(now - Duration::hours(1));
        let old_start = timestamp(now - Duration::days(32));
        let old_end = timestamp(now - Duration::days(31));

        for (start, end) in [
            (invalid_start.as_str(), invalid_end.as_str()),
            (old_start.as_str(), old_end.as_str()),
        ] {
            let response = harness
                .call_tool(
                    "memory_retrieval_outcomes_report",
                    serde_json::json!({
                        "project": project,
                        "start": start,
                        "end": end,
                        "timezone": "UTC"
                    }),
                )
                .await
                .expect("public dispatch returns report error envelope");

            assert_eq!(response["report"], serde_json::Value::Null);
            assert_eq!(
                response["error"],
                "invalid data: unsupported report interval"
            );
            assert!(
                response.get("start").is_none() && response.get("end").is_none(),
                "rejected intervals must not be clipped into a successful response"
            );
        }
    }
}
