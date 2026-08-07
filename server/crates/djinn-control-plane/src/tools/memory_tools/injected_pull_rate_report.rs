//! `memory_injected_pull_rate_report` — the operator surface for
//! `P(memory_read | Injected)` (proposal u46i AC6).
//!
//! Shaped after [`super::retrieval_outcomes_report`]: same project/interval
//! params, same `{ report, error }` response, same reject-don't-clip behaviour
//! for out-of-retention intervals.
//!
//! This tool is registered on the **direct control-plane MCP surface only**. It
//! is deliberately absent from the agent role surfaces (worker, reviewer,
//! planner, architect): it is an operator diagnostic, and putting it in front of
//! an agent would spend context without changing any agent decision.

use super::{
    InjectedPullRateReportParams, MemoryInjectedPullRateReportResponse,
    MemoryInjectedPullRateReportSchemaResponse,
};
use crate::server::DjinnMcpServer;
use djinn_db::repositories::retrieval_trace::RetrievalTraceRepository;
use djinn_db::repositories::retrieval_trace::injected_pull_rate::InjectedPullRateRequest;
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

#[tool_router(router = memory_injected_pull_rate_report_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(
        description = "Read P(memory_read | Injected) — how often a knowledge note that was injected into a prompt was then explicitly pulled back by the agent via memory_read — for one project over an explicit timezone-aware RFC-3339 [start,end) interval. Read-only and observational: it makes no causal claim. Injected candidates are joined to the note_access_events ledger on stable note_id (never permalink, which move_note mutates), correlated by task_run_id or session_id, and counted only when the read strictly follows the injection. memory_search result touches are recorded but are NEVER counted as pulls. ALWAYS read the `evidence` field before `pull_rate`; it is exactly one of: `no_injected_candidates` (the window contained nothing to measure — this is NOT a zero pull rate), `no_attributable_candidates` (injected candidates exist but none carry a task run or session, so nothing can be correlated — an attribution-coverage gap, NOT a zero pull rate), `no_access_ledger_coverage` (attributable candidates exist but the window holds zero memory_read ledger rows, so the rate reflects missing instrumentation rather than agent behaviour), and `measured` (the only value under which pull_rate is a real measurement). pull_rate is null whenever the denominator is zero, and its denominator is attributable_candidate_count, not injected_candidate_count; unattributable and identityless candidates are reported in diagnostics instead of being silently counted as not-pulled. Requests outside the protected 30-day retention window are rejected without clipping."
    )]
    pub async fn memory_injected_pull_rate_report(
        &self,
        Parameters(p): Parameters<InjectedPullRateReportParams>,
    ) -> Json<MemoryInjectedPullRateReportSchemaResponse> {
        Json(report(self, p).await.into())
    }
}

fn err(error: impl Into<String>) -> MemoryInjectedPullRateReportResponse {
    MemoryInjectedPullRateReportResponse {
        report: None,
        error: Some(error.into()),
    }
}

pub(super) async fn report(
    server: &DjinnMcpServer,
    p: InjectedPullRateReportParams,
) -> MemoryInjectedPullRateReportResponse {
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
    match RetrievalTraceRepository::new(server.state.db().clone())
        .injected_pull_rate_report(InjectedPullRateRequest {
            project_id,
            start: p.start,
            end: p.end,
            timezone: p.timezone,
        })
        .await
    {
        Ok(report) => MemoryInjectedPullRateReportResponse {
            report: Some(report),
            error: None,
        },
        Err(error) => err(error.to_string()),
    }
}
