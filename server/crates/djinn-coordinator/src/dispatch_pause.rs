//! Compatibility shim: dispatch pause utilities.
//!
//! These functions mirror `djinn-agent::dispatch_pause` so coordinator
//! dispatch logic can check administrative pause state.

use djinn_core::events::EventBus;
use djinn_core::models::{DispatchPause, DispatchPauseState, Task};
use djinn_db::{Database, DispatchPauseRepository};

use djinn_orchestration_types::coordinator::DispatchPauseView;

pub fn parse_dispatch_pause_wall_clock_ts(raw: &str) -> Option<::time::OffsetDateTime> {
    use ::time::format_description::well_known::{Iso8601, Rfc3339};

    ::time::OffsetDateTime::parse(raw, &Iso8601::DEFAULT)
        .or_else(|_| ::time::OffsetDateTime::parse(raw, &Rfc3339))
        .ok()
}

pub fn dispatch_pause_is_active(pause: &DispatchPause) -> bool {
    let Some(expires_at) = pause.expires_at.as_deref() else {
        return true;
    };

    parse_dispatch_pause_wall_clock_ts(expires_at)
        .map(|deadline| deadline > ::time::OffsetDateTime::now_utc())
        .unwrap_or(true)
}

pub fn active_global_dispatch_pause(pause_state: &DispatchPauseState) -> Option<&DispatchPause> {
    pause_state
        .global
        .as_ref()
        .filter(|pause| dispatch_pause_is_active(pause))
}

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

type PauseMatch<'a> = (&'static str, Option<String>, &'a DispatchPause);

pub fn matching_task_dispatch_pause<'a>(
    pause_state: &'a DispatchPauseState,
    task: &Task,
) -> Option<PauseMatch<'a>> {
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

    let creator = task.created_by_user_id.as_deref()?;
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
