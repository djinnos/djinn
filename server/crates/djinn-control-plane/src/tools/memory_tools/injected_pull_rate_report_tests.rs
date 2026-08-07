//! Importer: `memory_tools/mod.rs`; exercises direct production operation dispatch.
//! Public APIs under test: `McpTestHarness::call_tool` and the injected-pull-rate
//! request/response DTOs.
//! Data shapes: `{project,start,end,timezone}` JSON and report/error envelopes.

use super::InjectedPullRateReportParams;

#[test]
fn injected_pull_rate_report_dto_requires_explicit_interval() {
    assert!(
        serde_json::from_value::<InjectedPullRateReportParams>(
            serde_json::json!({"project":"p","start":"2026-08-01T00:00:00Z","end":"2026-08-02T00:00:00Z","timezone":"UTC"})
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<InjectedPullRateReportParams>(
            serde_json::json!({"project":"p","start":"x"})
        )
        .is_err()
    );
}

#[test]
fn injected_pull_rate_report_dispatch_contract_rejects_unsupported_arguments() {
    let request = serde_json::json!({
        "project": "p",
        "start": "2026-08-01T00:00:00Z",
        "end": "2026-08-02T00:00:00Z",
        "timezone": "UTC",
        "note_id": "must-not-narrow-the-window"
    });
    let error = serde_json::from_value::<InjectedPullRateReportParams>(request)
        .err()
        .expect("report DTO must reject unknown filters rather than ignoring them");
    assert!(error.to_string().contains("unknown field `note_id`"));
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
            .create("pull-rate-boundary", "pull-owner", "pull-repo")
            .await
            .expect("create report project");
        (McpTestHarness::from_db(db), project.slug())
    }

    /// The tool must be reachable by name — a `#[tool_router]` registration that
    /// is never added to the name dispatch match is a tool nobody can call, and
    /// AC6 says the metric is *reported*.
    #[tokio::test]
    async fn direct_dispatch_routes_the_report_and_reports_an_empty_window_honestly() {
        let (harness, project) = harness_with_project().await;
        let end = OffsetDateTime::now_utc() - Duration::minutes(1);
        let start = end - Duration::hours(2);
        let start = timestamp(start);
        let end = timestamp(end);

        let response = harness
            .call_tool(
                "memory_injected_pull_rate_report",
                serde_json::json!({
                    "project": project,
                    "start": start,
                    "end": end,
                    "timezone": "America/New_York"
                }),
            )
            .await
            .expect("operation-name dispatch reaches the injected-pull-rate handler");

        assert_eq!(response["error"], serde_json::Value::Null);
        assert_eq!(response["report"]["timezone"], "America/New_York");
        assert_eq!(response["report"]["injected_candidate_count"], 0);
        // The whole point of `evidence`: an empty window must never present as
        // a proven 0% pull rate.
        assert_eq!(response["report"]["pull_rate"], serde_json::Value::Null);
        assert_eq!(response["report"]["evidence"], "no_injected_candidates");
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
                    "memory_injected_pull_rate_report",
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
        }
    }

    #[tokio::test]
    async fn direct_dispatch_requires_a_project_and_a_timezone() {
        let (harness, project) = harness_with_project().await;
        let end = OffsetDateTime::now_utc() - Duration::minutes(1);
        let start = timestamp(end - Duration::hours(2));
        let end = timestamp(end);

        let response = harness
            .call_tool(
                "memory_injected_pull_rate_report",
                serde_json::json!({
                    "project": project,
                    "start": start,
                    "end": end,
                    "timezone": "   "
                }),
            )
            .await
            .expect("public dispatch returns report error envelope");
        assert_eq!(response["error"], "timezone parameter required");

        let response = harness
            .call_tool(
                "memory_injected_pull_rate_report",
                serde_json::json!({
                    "start": start,
                    "end": end,
                    "timezone": "UTC"
                }),
            )
            .await
            .expect("public dispatch returns report error envelope");
        assert_eq!(
            response["error"],
            "project or project_id parameter required"
        );
    }
}
