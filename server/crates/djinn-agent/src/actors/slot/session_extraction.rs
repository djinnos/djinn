// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// ─── hfhw cutover: session extraction delegated to djinn-slot ────────────
//
// The full session extraction implementation (structural taxonomy, LLM
// distillation, co-access flush, `run_extraction_backfill`) now lives in
// `djinn_slot::session_extraction`.
//
// This module is a thin adapter.  The public `run_extraction_backfill` entry
// point converts `AgentContext` → `SlotContext` and delegates to
// `djinn_slot::run_extraction_backfill`.
//
// The old production implementation (1700+ lines of direct `sqlx` queries,
// structural extraction, LLM extraction callout, and helper functions) has
// been removed from `djinn-agent`.  It is no longer production-reachable.
//
// Public types (`ExtractionQuality`, `SessionTaxonomy`, etc.) are re-exported
// from `djinn-slot`.  Test-only types (`SessionSignals`, `extract_session_signals`)
// are gated behind `#[cfg(test)]`.

use crate::context::AgentContext;

// ─── Re-exports from djinn-slot ──────────────────────────────────────────
// Types used by `llm_extraction.rs` and its tests.  The implementations
// now live in `djinn-slot`; these re-exports keep `crate::actors::slot::session_extraction::*`
// paths resolving for agent-internal callers.

pub use djinn_slot::{ExtractionQuality, SessionTaxonomy, derive_scope_paths};
// Only needed by llm_extraction_tests; suppressed in non-test builds to avoid
// unused-import warnings from clippy -D warnings.
#[cfg(test)]
pub use djinn_slot::{SessionSignals, extract_session_signals};

// `run_structural_extraction` is re-exported from djinn-slot but takes
// `SlotContext`.  We provide a thin adapter that accepts `AgentContext`
// for tests that still construct the agent-side context.
#[cfg(test)]
pub(crate) async fn run_structural_extraction(
    session_id: String,
    messages: Vec<djinn_core::message::Message>,
    app_state: AgentContext,
) -> Option<SessionTaxonomy> {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_structural_extraction(session_id, messages, slot_ctx).await
}

/// No-op host callbacks for the extraction-backfill adapter.
///
/// `run_extraction_backfill` only needs `db` and `event_bus` from the
/// context.  The callbacks trait methods are never invoked during
/// backfill, so a no-op implementation is safe.
struct ExtractionCallbacks;

impl djinn_slot::host::SlotHostCallbacks for ExtractionCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not available in extraction backfill".into()) })
    }

    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &djinn_core::models::Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }

    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        // Not invoked during extraction backfill.  Only `db` and `event_bus`
        // are used by the extraction path.
        unreachable!("build_mcp_state not available in extraction backfill adapter")
    }

    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not available in extraction backfill".into(),
            })
        })
    }

    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<djinn_slot::helpers::ProviderCredential, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not available in extraction backfill".into()) })
    }

    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: djinn_slot::host::SlotContext,
        _kill: tokio_util::sync::CancellationToken,
        _pause: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

/// Convert `AgentContext` → `SlotContext` for extraction backfill.
///
/// Maps the shared service-handle fields and provides a no-op
/// `SlotHostCallbacks` implementation (extraction backfill never
/// invokes host callbacks).
fn agent_to_slot_context(agent: &AgentContext) -> djinn_slot::host::SlotContext {
    djinn_slot::host::SlotContext {
        db: agent.db.clone(),
        event_bus: agent.event_bus.clone(),
        catalog: agent.catalog.clone(),
        health_tracker: agent.health_tracker.clone(),
        background_work_tasks: agent.background_work_tasks.clone(),
        active_tasks: agent.active_tasks.clone(),
        default_project_id: agent.default_project_id.clone(),
        working_root: agent.working_root.clone(),
        coordinator_trigger: None,
        runtime_ops: agent.runtime_ops.clone(),
        repo_graph_ops: agent.repo_graph_ops.clone(),
        callbacks: std::sync::Arc::new(ExtractionCallbacks),
    }
}

/// One-shot recovery sweep that backfills post-session knowledge extraction
/// over completed-but-unextracted task-runs.
///
/// This is the public entry point called from the server boot path.
/// It delegates to `djinn_slot::run_extraction_backfill` after converting
/// the `AgentContext` into a `SlotContext`.
pub async fn run_extraction_backfill(app_state: AgentContext) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_extraction_backfill(slot_ctx).await;
}

/// Server-side post-task-run knowledge extraction (thin adapter).
///
/// Converts `AgentContext` → `SlotContext` and delegates to
/// `djinn_slot::run_post_session_extraction`.
pub(crate) async fn run_post_session_extraction(
    task_id: String,
    task_run_id: String,
    app_state: AgentContext,
) {
    let slot_ctx = agent_to_slot_context(&app_state);
    djinn_slot::run_post_session_extraction(task_id, task_run_id, slot_ctx).await;
}
