//! Gated planner enrichment for the production prompt-assembly path.

use std::collections::HashSet;

use djinn_core::message::{Conversation, Message};
use djinn_core::models::Task;
use djinn_db::NoteRepository;

use crate::actors::slot::lifecycle::memory_intent_planner::{
    MEMORY_INTENT_PLANNER_PROMPT_ID, PlannedNoteType, PlannerInput, parse_planned_queries,
    prepare_planner_request,
};

use super::KNOWLEDGE_BUDGET_CHARS;
use super::types::{MemoryIntentPlannerInvocation, PlannedNoteSearch};

/// Gated attributed planner enrichment for the real prompt-assembly path.
pub(super) async fn merge_planned_knowledge(
    scope: Option<String>,
    scope_notes: &[djinn_memory::Note],
    note_repo: &NoteRepository,
    task: &Task,
    planner: Option<&MemoryIntentPlannerInvocation<'_>>,
) -> Option<String> {
    let Some(invocation) = planner.filter(|p| p.config.is_enabled()) else {
        return scope;
    };
    let Some(creator_id) = invocation.creator_id.filter(|id| !id.is_empty()) else {
        return scope;
    };
    let input = PlannerInput {
        title: task.title.clone(),
        description: task.description.clone(),
        acceptance_criteria: invocation.acceptance_criteria.clone(),
        resume_compaction_summary: invocation.resume_compaction_summary.map(str::to_owned),
    };
    let Some(prepared) = prepare_planner_request(invocation.config, input) else {
        return scope;
    };
    let mut conversation = Conversation::new();
    conversation.push(Message::user(prepared.prompt));
    let request = djinn_supervisor::services::wire::AttributedPlannerRequest {
        project_id: task.project_id.clone(),
        task_id: task.id.clone(),
        task_run_id: invocation.task_run_id.into(),
        session_id: invocation.session_id.into(),
        created_by_user_id: creator_id.into(),
        operation: "memory_intent_planner".into(),
        prompt_id: MEMORY_INTENT_PLANNER_PROMPT_ID.into(),
        conversation: match serde_json::to_string(&conversation) {
            Ok(v) => v,
            Err(_) => return scope,
        },
        tools: "[]".into(),
        tool_choice: None,
        max_tokens: invocation.config.max_output.min(u32::MAX as usize) as u32,
        timeout_ms: invocation.config.timeout.as_millis().min(u64::MAX as u128) as u64,
    };
    let Ok(attempt) = invocation.host.plan_memory_intents(request).await else {
        return scope;
    };
    let Some(payload) = attempt.content else {
        return scope;
    };
    let Ok(queries) = parse_planned_queries(&payload) else {
        return scope;
    };
    let search: &dyn PlannedNoteSearch = invocation.planned_note_search.unwrap_or(note_repo);
    let searches = queries.iter().map(|q| {
        let note_type = match q.note_type {
            PlannedNoteType::Pitfall => "pitfall",
            PlannedNoteType::Pattern => "pattern",
            PlannedNoteType::Case => "case",
            PlannedNoteType::Reference => "reference",
        };
        search.search_planned_notes(&task.project_id, &task.id, &q.query, note_type)
    });
    let buckets = futures::future::join_all(searches).await;
    if buckets.iter().any(Result::is_err) {
        return scope;
    }
    let mut output = scope.unwrap_or_default();
    let mut ids: HashSet<String> = scope_notes.iter().map(|n| n.id.clone()).collect();
    let mut links: HashSet<String> = scope_notes.iter().map(|n| n.permalink.clone()).collect();
    let mut count = 0;
    for bucket in buckets {
        // The per-query cap is on successfully rendered *unique* notes, not
        // the first two repository rows. A duplicate first row must not
        // suppress the next unique, ranked row.
        let mut rendered_for_query = 0;
        for row in bucket.expect("checked") {
            if rendered_for_query == 2 {
                break;
            }
            if count == 6 || !ids.insert(row.id.clone()) || !links.insert(row.permalink.clone()) {
                continue;
            }
            let line = format!(
                "- **[Note] {}**: {} (permalink: {})",
                row.title, row.snippet, row.permalink
            );
            if output.len() + usize::from(!output.is_empty()) + line.len() > KNOWLEDGE_BUDGET_CHARS
            {
                return (!output.is_empty()).then_some(output);
            }
            if !output.is_empty() {
                output.push('\n')
            }
            output.push_str(&line);
            count += 1;
            rendered_for_query += 1;
        }
    }
    (!output.is_empty()).then_some(output)
}
