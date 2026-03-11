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
    TaskCreated {
        task: Task,
        /// `true` when the event originated from a peer sync import.
        /// The background export listener ignores these to prevent loops.
        #[serde(skip)]
        from_sync: bool,
    },
    TaskUpdated {
        task: Task,
        /// `true` when the event originated from a peer sync import.
        /// The background export listener ignores these to prevent loops.
        #[serde(skip)]
        from_sync: bool,
    },
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
    /// Emitted immediately when a task is dispatched to a slot, before the
    /// session record is created in the DB. Lets the frontend show the agent
    /// avatar as soon as the task goes in-progress.
    SessionDispatched {
        task_id: String,
        model_id: String,
        agent_type: String,
    },
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

    /// Emitted for each agent conversation turn in a live session.
    SessionMessage {
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    },

    /// Emitted when a message is stored in the session_messages table.
    SessionMessageInserted {
        session_id: String,
        task_id: String,
        role: String,
    },

    // Sync lifecycle (SYNC-13)
    SyncCompleted {
        channel: String,
        /// "export" | "import"
        direction: String,
        count: usize,
        error: Option<String>,
    },

    // Project health (setup/verification commands result)
    ProjectHealthChanged {
        project_id: String,
        healthy: bool,
        error: Option<String>,
    },
}
