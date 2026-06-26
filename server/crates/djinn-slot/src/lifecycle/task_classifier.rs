//! Task-based classifier for native-skill loading triggers.
use djinn_core::models::Task;

/// Classify a task to determine which native skills to load.
pub(crate) fn classify_task(task: &Task) -> &'static str {
    if task.issue_type == "epic_breakdown" {
        "decomposition"
    } else {
        "default"
    }
}
