// `memory_run_enrichment(project)` — admin/operator trigger for the
// best-effort LLM enrichment pass.
//
// The trigger wires the core `djinn-agent` enrichment module behind a small
// `MemoryEnrichmentOps` bridge trait (see `bridge/memory_enrichment_bridge.rs`)
// so the control plane never depends on `djinn-agent` directly. The bridge
// is documented in the concurrency regression test
// (`run_enrichment_tests::memory_graph_concurrent_with_enrichment_does_not_block`).
//
// Logging contract: the pass itself emits `INFO` start/finish lines (see
// `djinn_slot::memory_enrichment` via the agent facade); the trigger logs a
// high-level `INFO` line on entry so operators can correlate admin-tool
// invocations with the pass's own telemetry.

use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

use crate::bridge::{EnrichmentStatus, MemoryEnrichmentOps};
use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;
use crate::tools::memory_tools::{MemoryRunEnrichmentResponse, RunEnrichmentParams};

#[tool_router(router = memory_run_enrichment_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Trigger the best-effort LLM memory enrichment pass for a project.
    ///
    /// The pass walks every note in the project, batches them through the
    /// configured LLM provider, and persists claim / entity nodes plus
    /// typed implicit edges (`builds_on` / `contradicts` / `supersedes` /
    /// `exemplifies`) into the widened `note_associations` substrate. All
    /// persistence is best-effort: provider or parse failures are returned
    /// in `report.warnings` rather than propagated as blocking errors, so
    /// retrieval and the memory-graph UI never gate on this pass.
    ///
    /// Proposal-aware: the pass also fetches proposals targeting the project
    /// and includes them as first-class graph entities in each batch prompt.
    /// Edges involving proposals (note↔proposal, proposal↔proposal) are
    /// persisted through the heterogeneous `memory_entity_associations`
    /// substrate via `upsert_typed_entity_association`, so proposals
    /// participate in the same association graph as notes. Only edges with
    /// textual evidence are emitted for proposal endpoints.
    ///
    /// Defaults to synchronous execution (`background=false`). Set
    /// `background=true` to fire-and-forget the pass on a tokio task; the
    /// tool returns immediately with `status="queued"` and the structured
    /// report is logged at `INFO` on finish.
    ///
    /// Concurrency: the pass yields cooperatively to the runtime between
    /// batches, so `memory_graph` and the rest of the MCP surface keep
    /// serving while enrichment runs. The
    /// `run_enrichment_tests::memory_graph_concurrent_with_enrichment_does_not_block`
    /// regression asserts this contract.
    #[tool(
        description = "Run the best-effort LLM memory enrichment pass for a project. Persists claim/entity nodes and typed implicit edges (builds_on / contradicts / supersedes / exemplifies) through the widened note_associations substrate. Proposal-aware: also includes proposals targeting the project as first-class graph entities and persists note↔proposal / proposal↔proposal edges through the heterogeneous memory_entity_associations substrate. All persistence is best-effort: provider or parse failures are surfaced as warnings, never as blocking errors. Set background=true to fire-and-forget on a tokio task; the response then returns immediately with status=queued. Concurrency: the pass yields cooperatively so memory_graph stays responsive while enrichment runs."
    )]
    pub async fn memory_run_enrichment(
        &self,
        Parameters(p): Parameters<RunEnrichmentParams>,
    ) -> Json<MemoryRunEnrichmentResponse> {
        Json(run_enrichment(self, p).await)
    }
}

async fn run_enrichment(
    server: &DjinnMcpServer,
    params: RunEnrichmentParams,
) -> MemoryRunEnrichmentResponse {
    // Administrative maintenance trigger: trusted no-user/internal callers are
    // allowed by `require_admin`, but an authenticated non-admin must not be
    // able to start an LLM write pass over project memory.
    if let Err(error) = require_admin(server.state.db()).await {
        tracing::warn!(
            project_ref = %params.project,
            error = %error,
            "memory_run_enrichment: admin check failed"
        );
        return MemoryRunEnrichmentResponse {
            status: EnrichmentStatus::Completed.as_str().to_string(),
            project_id: None,
            report: None,
            error: Some(error),
        };
    }

    // Resolve the project through the same path other memory tools use —
    // slug / path / id all collapse to the same DB id before the bridge is
    // invoked. This keeps the trigger consistent with `memory_graph` and
    // `memory_repair_embeddings`.
    let project_id = match server.resolve_project_id(&params.project).await {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(
                project_ref = %params.project,
                error = %error,
                "memory_run_enrichment: project resolution failed"
            );
            return MemoryRunEnrichmentResponse {
                status: EnrichmentStatus::Completed.as_str().to_string(),
                project_id: None,
                report: None,
                error: Some(error),
            };
        }
    };

    let Some(ops) = server.state.enrichment_ops() else {
        // Off-server contexts (test harnesses, dev boxes that haven't wired
        // the bridge) surface a clean "not configured" error rather than a
        // panic or a silent no-op.
        let message = "memory enrichment subsystem is not configured on this server".to_string();
        tracing::warn!(
            project_id = %project_id,
            "memory_run_enrichment: enrichment bridge not wired"
        );
        return MemoryRunEnrichmentResponse {
            status: EnrichmentStatus::Completed.as_str().to_string(),
            project_id: Some(project_id),
            report: None,
            error: Some(message),
        };
    };

    let background = params.background.unwrap_or(false);
    if background {
        // Fire-and-forget: spawn the pass on a tokio task so the MCP
        // caller sees a snappy response. The pass itself never panics —
        // `run_memory_enrichment_with_db` catches every error path and
        // returns it in the report.
        tracing::info!(
            project_id = %project_id,
            "memory_run_enrichment: scheduling enrichment on background task"
        );
        let project_id_for_task = project_id.clone();
        let ops_for_task = ops.clone();
        tokio::spawn(async move {
            // Fire-and-forget: any error is logged inside the helper, not
            // surfaced to the spawn caller (which is `tokio::spawn` itself).
            let _ = run_enrichment_inner(&*ops_for_task, &project_id_for_task).await;
        });
        return MemoryRunEnrichmentResponse {
            status: EnrichmentStatus::Queued.as_str().to_string(),
            project_id: Some(project_id),
            report: None,
            error: None,
        };
    }

    match run_enrichment_inner(&*ops, &project_id).await {
        Ok(report) => MemoryRunEnrichmentResponse {
            status: EnrichmentStatus::Completed.as_str().to_string(),
            project_id: Some(project_id),
            report: Some(report),
            error: None,
        },
        Err(error) => MemoryRunEnrichmentResponse {
            status: EnrichmentStatus::Completed.as_str().to_string(),
            project_id: Some(project_id),
            report: None,
            error: Some(error),
        },
    }
}

/// Drive the bridge impl with the standard `INFO` start/finish logging that
/// the housekeeping pattern uses (see `djinn-db/src/background/housekeeping.rs`).
/// The bridge impl itself also logs at start/finish; the wrapping here keeps
/// the tool-level trigger visible in the operator's audit trail.
async fn run_enrichment_inner(
    ops: &dyn MemoryEnrichmentOps,
    project_id: &str,
) -> Result<crate::bridge::EnrichmentReport, String> {
    tracing::info!(
        project_id = %project_id,
        "memory_run_enrichment: starting enrichment pass"
    );
    let report = ops.run_enrichment(project_id).await;
    match &report {
        Ok(r) => tracing::info!(
            project_id = %project_id,
            status = "completed",
            entities = r.entities.len(),
            claims = r.claims.len(),
            edges = r.edges.len(),
            notes_processed = r.notes_processed,
            batches_sent = r.batches_sent,
            warnings = r.warnings.len(),
            "memory_run_enrichment: enrichment pass finished"
        ),
        Err(e) => {
            // The admin trigger's logging contract requires an INFO finish
            // line for every attempted run. Keep the failure detail on that
            // line for operator correlation, and emit a WARN as well so
            // alerting that keys off warnings still sees the failure.
            tracing::info!(
                project_id = %project_id,
                status = "failed",
                error = %e,
                "memory_run_enrichment: enrichment pass finished"
            );
            tracing::warn!(
                project_id = %project_id,
                error = %e,
                "memory_run_enrichment: enrichment pass failed"
            );
        }
    }
    report
}

impl EnrichmentStatus {
    /// Stable wire string for the `status` field. Kept here so the test
    /// surface doesn't depend on the serde rename rule.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Queued => "queued",
        }
    }
}
