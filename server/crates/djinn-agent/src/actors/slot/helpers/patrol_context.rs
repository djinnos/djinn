use super::code_context::note_scope_covers_path;
use super::knowledge_helpers::{
    format_open_knowledge_tasks, is_exploration_knowledge_task, is_hygiene_knowledge_task,
    planner_patrol_knowledge_task_budget,
};
use super::*;

pub(crate) async fn build_planner_patrol_context(
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
) -> Option<String> {
    if task.issue_type != "review" || !task.title.to_ascii_lowercase().contains("patrol") {
        return None;
    }

    let graph_ops = app_state.repo_graph_ops.clone()?;
    // `task.project_id` is the canonical project identifier; the
    // bridge requires `(id, clone_path)` so pack both into ctx.
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: task.project_id.clone(),
        clone_path: project_path.to_string(),
        workspace: None,
        sub_path: None,
    };
    let ranked = graph_ops
        .ranked(
            &ctx,
            ctx.workspace.as_deref(),
            Some("file"),
            Some("pagerank"),
            20,
        )
        .await
        .ok()
        .unwrap_or_default();

    let project_id = task.project_id.clone();
    let note_repo =
        djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let notes = note_repo
        .list(&project_id, None)
        .await
        .ok()
        .unwrap_or_default();
    let memory_health = note_repo.health(&project_id).await.ok();
    let open_tasks = task_repo
        .list_by_project(&project_id)
        .await
        .ok()
        .unwrap_or_default()
        .into_iter()
        .filter(|candidate| candidate.status != "closed" && candidate.id != task.id)
        .collect::<Vec<_>>();

    let mut documented_paths = Vec::new();
    let mut stale_scoped_areas = Vec::new();
    for note in &notes {
        let scopes = parse_json_array(&note.scope_paths);
        if !scopes.is_empty() {
            let note_tags = parse_json_array(&note.tags);
            let is_review_needed = note_tags.iter().any(|tag| tag == "review_needed");
            if is_review_needed || note.confidence <= djinn_db::STALE_CITATION {
                let scope_display = scopes
                    .iter()
                    .take(3)
                    .map(|scope| format!("`{scope}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                stale_scoped_areas.push(format!(
                    "{} scoped to {} (confidence {:.3}, review_needed: {})",
                    note.title,
                    scope_display,
                    note.confidence,
                    if is_review_needed {
                        "yes"
                    } else {
                        "pending decay"
                    }
                ));
            }
            documented_paths.extend(scopes);
        }
    }

    let mut lines = Vec::new();
    if let Some(health) = memory_health {
        lines.push("### Memory Health Signals".to_string());
        lines.push(format!(
            "- Notes: {} total, {} low-confidence, {} stale, {} duplicate clusters, {} broken links, {} orphans",
            health.total_notes,
            health.low_confidence_note_count,
            health.stale_note_count,
            health.duplicate_cluster_count,
            health.broken_link_count,
            health.orphan_note_count
        ));
        lines.push(format!(
            "- Stale-note folders: {}",
            if health.stale_notes_by_folder.is_empty() {
                "none".to_string()
            } else {
                health
                    .stale_notes_by_folder
                    .iter()
                    .take(4)
                    .map(|folder| format!("`{}` ({})", folder.folder, folder.count))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        lines.push(String::new());
    }
    let mut undocumented_hotspots = Vec::new();
    let mut weakly_documented_hotspots = Vec::new();
    for node in ranked.into_iter().take(12) {
        let Some(path) = node.key.strip_prefix("file:") else {
            continue;
        };
        let coverage_count = documented_paths
            .iter()
            .filter(|scope| note_scope_covers_path(std::slice::from_ref(scope), path))
            .count();
        let item = format!(
            "`{path}` (score {:.3}, coverage {coverage_count})",
            node.page_rank
        );
        if coverage_count == 0 {
            undocumented_hotspots.push(item);
        } else if coverage_count <= 1 {
            weakly_documented_hotspots.push(item);
        }
    }

    lines.push("\n### Knowledge Coverage Gaps".to_string());
    lines.push(format!(
        "- Undocumented hotspots: {}",
        if undocumented_hotspots.is_empty() {
            "none".to_string()
        } else {
            undocumented_hotspots
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    lines.push(format!(
        "- Weakly documented hotspots: {}",
        if weakly_documented_hotspots.is_empty() {
            "none".to_string()
        } else {
            weakly_documented_hotspots
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    lines.push(format!(
        "- Stale scoped-note areas affected by changed code: {}",
        if stale_scoped_areas.is_empty() {
            "none".to_string()
        } else {
            stale_scoped_areas
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    let budget = planner_patrol_knowledge_task_budget();
    let open_hygiene_tasks = open_tasks
        .iter()
        .filter(|task| is_hygiene_knowledge_task(task))
        .cloned()
        .collect::<Vec<_>>();
    let open_exploration_tasks = open_tasks
        .iter()
        .filter(|task| is_exploration_knowledge_task(task))
        .cloned()
        .collect::<Vec<_>>();

    lines.push("\n### Knowledge Task Guard Rails".to_string());
    lines.push(format!(
        "- Patrol knowledge-task budget: create at most {budget} new hygiene/exploration follow-up tasks this patrol (override with `{PATROL_KNOWLEDGE_TASK_BUDGET_ENV}`, default {DEFAULT_PATROL_KNOWLEDGE_TASK_BUDGET})."
    ));
    lines.push(format!(
        "- Open hygiene knowledge tasks already on the board: {}",
        format_open_knowledge_tasks(&open_hygiene_tasks)
    ));
    lines.push(format!(
        "- Open exploration knowledge tasks already on the board: {}",
        format_open_knowledge_tasks(&open_exploration_tasks)
    ));
    lines.push(
        "- If a relevant hygiene or exploration task is already open for the same area/problem, suppress creating another one and mention the existing task in your patrol summary instead.".to_string(),
    );
    lines.push(format!(
        "- If no similar open knowledge task exists, you may still create eligible follow-up work, but never exceed {budget} total new knowledge tasks in this patrol."
    ));

    Some(lines.join("\n"))
}
