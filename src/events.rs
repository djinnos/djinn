use crate::models::project::Project;
use crate::models::settings::Setting;

/// Domain events emitted by repositories after every write.
///
/// Sent over a `tokio::sync::broadcast` channel. SSE subscribers and
/// other internal consumers receive full entities — no follow-up reads needed.
#[derive(Clone, Debug)]
pub enum DjinnEvent {
    SettingUpdated(Setting),
    ProjectCreated(Project),
    ProjectUpdated(Project),
    ProjectDeleted { id: String },
}
