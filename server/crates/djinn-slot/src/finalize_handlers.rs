//! Finalize tool handling.
use crate::host::SlotContext;

pub(crate) async fn process_finalize_payload(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    _ctx: &SlotContext,
) {
    let Some(_payload) = payload else { return };
    // Delegate to host for actual processing
    tracing::debug!(
        finalize_tool = %finalize_tool_name,
        task_id = %task_id,
        "finalize_handlers: processing finalize payload"
    );
}

pub(crate) async fn handle_budget_park(
    summary: &str,
    details: &str,
    task_id: &str,
    ctx: &SlotContext,
) {
    let summary = summary.trim();
    if summary.is_empty() {
        return;
    }
    let task_repo = djinn_db::TaskRepository::new(ctx.db.clone(), ctx.event_bus.clone());
    let payload = serde_json::json!({
        "summary": summary,
        "details": details,
    });
    let _ = task_repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "system",
            "budget_park",
            &payload.to_string(),
        )
        .await;
}
