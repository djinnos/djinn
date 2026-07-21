use djinn_core::events::EventBus;
use djinn_core::models::{DispatchPause, DispatchPauseState, Task};
use djinn_db::{Database, DispatchPauseRepository};

use crate::actors::coordinator::DispatchPauseView;

pub(crate) fn parse_dispatch_pause_wall_clock_ts(raw: &str) -> Option<::time::OffsetDateTime> {
    use ::time::format_description::well_known::{Iso8601, Rfc3339};

    ::time::OffsetDateTime::parse(raw, &Iso8601::DEFAULT)
        .or_else(|_| ::time::OffsetDateTime::parse(raw, &Rfc3339))
        .ok()
}

pub(crate) fn dispatch_pause_is_active(pause: &DispatchPause) -> bool {
    let Some(expires_at) = pause.expires_at.as_deref() else {
        return true;
    };

    parse_dispatch_pause_wall_clock_ts(expires_at)
        .map(|deadline| deadline > ::time::OffsetDateTime::now_utc())
        // A malformed persisted expiry should not accidentally bypass an
        // administrative pause. Treat it as active until an admin resumes it.
        .unwrap_or(true)
}

pub(crate) fn active_global_dispatch_pause(
    pause_state: &DispatchPauseState,
) -> Option<&DispatchPause> {
    pause_state
        .global
        .as_ref()
        .filter(|pause| dispatch_pause_is_active(pause))
}

#[allow(dead_code)] // Read by the forthcoming /debug/dispatch-state server handler.
pub fn debug_view(state: &DispatchPauseState) -> DispatchPauseView {
    let mut projects: Vec<_> = state.projects.keys().cloned().collect();
    projects.sort();
    let mut users: Vec<_> = state.users.keys().cloned().collect();
    users.sort();
    DispatchPauseView {
        global: active_global_dispatch_pause(state).is_some(),
        projects,
        users,
    }
}

#[allow(dead_code)]
pub(crate) fn matching_task_dispatch_pause<'a>(
    pause_state: &'a DispatchPauseState,
    task: &Task,
) -> Option<(&'static str, Option<String>, &'a DispatchPause)> {
    if let Some(pause) = active_global_dispatch_pause(pause_state) {
        return Some(("global", None, pause));
    }

    if let Some(pause) = pause_state
        .projects
        .get(&task.project_id)
        .filter(|pause| dispatch_pause_is_active(pause))
    {
        return Some(("project", Some(task.project_id.clone()), pause));
    }

    let creator = task.created_by_user_id.as_str();
    pause_state
        .users
        .get(creator)
        .filter(|pause| dispatch_pause_is_active(pause))
        .map(|pause| ("user", Some(creator.to_owned()), pause))
}

pub async fn load_dispatch_pause_state(
    db: Database,
    events: EventBus,
) -> djinn_db::Result<DispatchPauseState> {
    DispatchPauseRepository::new(db, events).get_status().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pause() -> DispatchPause {
        DispatchPause {
            paused_by: "admin".to_owned(),
            paused_at: "2026-06-12T00:00:00Z".to_owned(),
            reason: "maintenance".to_owned(),
            expires_at: None,
        }
    }

    fn task(project_id: &str, created_by_user_id: Option<&str>) -> Task {
        Task {
            id: "task-id".to_owned(),
            short_id: "task".to_owned(),
            epic_id: None,
            project_id: project_id.to_owned(),
            title: "Task".to_owned(),
            description: String::new(),
            design: String::new(),
            acceptance_criteria: "[]".to_owned(),
            issue_type: "task".to_owned(),
            status: "open".to_owned(),
            priority: 0,
            owner: String::new(),
            labels: "[]".to_owned(),
            memory_refs: "[]".to_owned(),
            created_by_user_id: created_by_user_id.unwrap_or("test-user").to_owned(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-06-12T00:00:00Z".to_owned(),
            updated_at: "2026-06-12T00:00:00Z".to_owned(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            agent_type: None,
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn matching_dispatch_pause_honors_global_project_and_user_scopes() {
        let mut state = DispatchPauseState {
            global: Some(pause()),
            ..Default::default()
        };
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-a", Some("user-a")))
                .map(|(scope, _, _)| scope),
            Some("global")
        );

        state.global = None;
        state.projects.insert("project-a".to_owned(), pause());
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-a", Some("user-b")))
                .map(|(scope, _, _)| scope),
            Some("project")
        );
        assert!(matching_task_dispatch_pause(&state, &task("project-b", Some("user-b"))).is_none());

        state.projects.clear();
        state.users.insert("user-a".to_owned(), pause());
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-b", Some("user-a")))
                .map(|(scope, _, _)| scope),
            Some("user")
        );
        assert!(matching_task_dispatch_pause(&state, &task("project-b", Some("user-b"))).is_none());
        assert!(matching_task_dispatch_pause(&state, &task("project-b", None)).is_none());
    }

    #[test]
    fn debug_view_honors_expired_global_pause() {
        let mut expired_global = pause();
        expired_global.expires_at = Some("2000-01-01T00:00:00Z".to_owned());
        let mut state = DispatchPauseState {
            global: Some(expired_global),
            ..Default::default()
        };
        state.projects.insert("project-b".to_owned(), pause());
        state.projects.insert("project-a".to_owned(), pause());
        state.users.insert("user-b".to_owned(), pause());
        state.users.insert("user-a".to_owned(), pause());

        let view = debug_view(&state);
        assert!(!view.global);
        assert_eq!(view.projects, vec!["project-a", "project-b"]);
        assert_eq!(view.users, vec!["user-a", "user-b"]);
    }
}
