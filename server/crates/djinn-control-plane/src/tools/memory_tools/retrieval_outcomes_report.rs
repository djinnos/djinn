use super::{MemoryRetrievalOutcomesReportResponse, RetrievalOutcomesReportParams};
use crate::server::DjinnMcpServer;
use djinn_db::repositories::task_run_outcome::{
    TaskRunOutcomeReportRequest, TaskRunOutcomeRepository,
};
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};
#[tool_router(router = memory_retrieval_outcomes_report_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(
        description = "Read an observational retrieval-injection outcomes report for an explicit project-scoped, timezone-aware RFC-3339 [start,end) interval. This operation is read-only and observational only: it makes no causal or randomized-experiment claim. A task run is deduplicated within each entry_point/rollout_label/outcome cell; cells with different cohort keys can overlap and are non-additive. The report includes all denominators, counts, rates, not-applicable states, attempt distributions, and unattributed/unrecorded diagnostics. Requests outside the protected 30-day retention window are rejected without clipping."
    )]
    pub async fn memory_retrieval_outcomes_report(
        &self,
        Parameters(p): Parameters<RetrievalOutcomesReportParams>,
    ) -> Json<serde_json::Value> {
        Json(serde_json::to_value(report(self, p).await).unwrap_or_else(
            |_| serde_json::json!({ "error": "failed to serialize retrieval outcomes report" }),
        ))
    }
}
fn err(error: impl Into<String>) -> MemoryRetrievalOutcomesReportResponse {
    MemoryRetrievalOutcomesReportResponse {
        report: None,
        error: Some(error.into()),
    }
}
pub(super) async fn report(
    server: &DjinnMcpServer,
    p: RetrievalOutcomesReportParams,
) -> MemoryRetrievalOutcomesReportResponse {
    let Some(project) = p.project_id.as_deref().or(p.project.as_deref()) else {
        return err("project or project_id parameter required");
    };
    if p.timezone.trim().is_empty() {
        return err("timezone parameter required");
    }
    let project_id = match super::ops::resolve_project_id(server, project).await {
        Ok(id) => id,
        Err(error) => return err(error),
    };
    match TaskRunOutcomeRepository::new(server.state.db().clone())
        .retrieval_outcomes_report(TaskRunOutcomeReportRequest {
            project_id,
            start: p.start,
            end: p.end,
            timezone: p.timezone,
        })
        .await
    {
        Ok(report) => MemoryRetrievalOutcomesReportResponse {
            report: Some(report),
            error: None,
        },
        Err(error) => err(error.to_string()),
    }
}
