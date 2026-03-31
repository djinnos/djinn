mod kb;
mod repo_map;

pub use kb::spawn_kb_watchers;
pub use repo_map::spawn_repo_map_refresh_watchers;

/// Test-only: re-export the refresh-scheduled channel so contract tests can
/// observe that `project.created` triggers initial repo-map refresh scheduling.
#[cfg(test)]
pub(crate) use repo_map::REFRESH_SCHEDULED_TX;
