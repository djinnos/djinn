use crate::db::repositories::project::ProjectConfig;
use crate::models::credential::Credential;
use crate::models::epic::Epic;
use crate::models::git_settings::GitSettings;
use crate::models::note::Note;
use crate::models::project::Project;
use crate::models::session::SessionRecord;
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
    ProjectDeleted {
        id: String,
    },
    ProjectConfigUpdated {
        project_id: String,
        config: ProjectConfig,
    },

    // Epics
    EpicCreated(Epic),
    EpicUpdated(Epic),
    EpicDeleted {
        id: String,
    },

    // Tasks
    TaskCreated(Task),
    TaskUpdated(Task),
    TaskDeleted {
        id: String,
    },

    // Knowledge-base notes
    NoteCreated(Note),
    NoteUpdated(Note),
    NoteDeleted {
        id: String,
    },

    // Git settings
    GitSettingsUpdated {
        project_id: String,
        settings: GitSettings,
    },

    // Credential vault (encrypted_value never included in event payload)
    CredentialCreated(Credential),
    CredentialUpdated(Credential),
    CredentialDeleted {
        id: String,
    },

    // Agent sessions
    SessionCreated(SessionRecord),
    SessionUpdated(SessionRecord),

    /// Periodic token usage snapshot emitted after each agent turn.
    /// `usage_pct` is `tokens_in / context_window` (0.0 when context_window unknown).
    SessionTokenUpdate {
        session_id: String,
        task_id: String,
        tokens_in: i64,
        tokens_out: i64,
        context_window: i64,
        usage_pct: f64,
    },

    /// Emitted when a session is compacted into a new continuation session.
    SessionCompacted {
        old_session_id: String,
        new_session_id: String,
        task_id: String,
        summary_tokens: i64,
        continuation_of: Option<String>,
    },

    // Project health (setup/verification commands result)
    ProjectHealthChanged {
        project_id: String,
        healthy: bool,
        error: Option<String>,
    },
}
