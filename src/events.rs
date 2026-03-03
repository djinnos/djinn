use crate::models::credential::Credential;
use crate::models::epic::Epic;
use crate::models::git_settings::GitSettings;
use crate::models::note::Note;
use crate::models::project::Project;
use crate::models::settings::Setting;
use crate::models::task::Task;

/// Domain events emitted by repositories after every write.
///
/// Sent over a `tokio::sync::broadcast` channel. SSE subscribers and
/// other internal consumers receive full entities — no follow-up reads needed.
///
/// Conventions:
///   - `Created` / `Updated` variants carry the full entity clone.
///   - `Deleted` variants carry only the `id` string.
#[derive(Clone, Debug, serde::Serialize)]
pub enum DjinnEvent {
    // Settings
    SettingUpdated(Setting),

    // Projects
    ProjectCreated(Project),
    ProjectUpdated(Project),
    ProjectDeleted { id: String },

    // Epics
    EpicCreated(Epic),
    EpicUpdated(Epic),
    EpicDeleted { id: String },

    // Tasks
    TaskCreated(Task),
    TaskUpdated(Task),
    TaskDeleted { id: String },

    // Knowledge-base notes
    NoteCreated(Note),
    NoteUpdated(Note),
    NoteDeleted { id: String },

    // Git settings
    GitSettingsUpdated {
        project_id: String,
        settings: GitSettings,
    },

    // Credential vault (encrypted_value never included in event payload)
    CredentialCreated(Credential),
    CredentialUpdated(Credential),
    CredentialDeleted { id: String },
}
