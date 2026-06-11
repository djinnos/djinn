use super::*;

pub(super) fn planner_patrol_knowledge_task_budget() -> usize {
    std::env::var(PATROL_KNOWLEDGE_TASK_BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PATROL_KNOWLEDGE_TASK_BUDGET)
}

pub(super) fn normalize_text_for_matching(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn is_hygiene_knowledge_task(task: &Task) -> bool {
    if task.status == "closed" {
        return false;
    }

    let searchable = normalize_text_for_matching(&[&task.title, &task.description, &task.design]);
    let has_hygiene_keyword = [
        "orphan",
        "broken link",
        "duplicate cluster",
        "duplicate note",
        "consolidat",
        "stale note",
        "low-confidence",
        "low confidence",
        "memory hygiene",
        "extraction",
        "review_needed",
        "review needed",
    ]
    .iter()
    .any(|keyword| searchable.contains(keyword));

    has_hygiene_keyword && matches!(task.issue_type.as_str(), "planning" | "task" | "research")
}

pub(super) fn is_exploration_knowledge_task(task: &Task) -> bool {
    if task.status == "closed" {
        return false;
    }

    let searchable = normalize_text_for_matching(&[&task.title, &task.description, &task.design]);
    let has_exploration_keyword = [
        "explore and document",
        "explore",
        "document",
        "subsystem overview",
        "overview",
        "undocumented",
        "knowledge gap",
        "architectural",
        "structural change",
        "new module",
    ]
    .iter()
    .any(|keyword| searchable.contains(keyword));

    has_exploration_keyword && matches!(task.issue_type.as_str(), "spike" | "research" | "planning")
}

pub(super) fn format_open_knowledge_tasks(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "none".to_string();
    }

    tasks
        .iter()
        .take(4)
        .map(|task| format!("`{}` ({})", task.short_id, task.title))
        .collect::<Vec<_>>()
        .join(", ")
}
