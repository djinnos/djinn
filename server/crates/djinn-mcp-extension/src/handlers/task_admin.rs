//! Task admin handlers that can operate through [`crate::ExtensionContext`].
//!
//! Handlers that require djinn-agent internals (task_merge, knowledge_promotion)
//! remain in the `djinn-agent` façade.

use djinn_db::TaskRepository;

use crate::context::ExtensionContext;
use crate::helpers::*;
use crate::types::*;

pub(crate) async fn call_task_archive_activity(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskArchiveActivityParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let count = repo
        .archive_activity_for_task(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "ok": true, "task_id": task.short_id, "archived_count": count }))
}

pub(crate) async fn call_task_reset_counters(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskResetCountersParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let updated = repo
        .reset_intervention_counters(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    let _ = &updated;
    Ok(
        serde_json::json!({ "ok": true, "task_id": task.short_id, "reopen_count": 0, "continuation_count": 0 }),
    )
}

pub(crate) async fn call_task_blocked_list(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(ctx.db(), ctx.event_bus());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let blocked = repo
        .list_blocked_by(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    let tasks: Vec<serde_json::Value> = blocked
        .iter()
        .map(|b| {
            serde_json::json!({
                "task_id": b.task_id,
                "short_id": b.short_id,
                "title": b.title,
                "status": b.status,
            })
        })
        .collect();
    Ok(serde_json::json!({ "tasks": tasks }))
}
