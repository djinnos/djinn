use crate::db::ProjectConfig;
use crate::models::Credential;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Note;
use crate::models::Project;
use crate::models::SessionRecord;
use crate::models::Setting;
use crate::models::Task;

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


    // Custom providers
    CustomProviderUpserted(crate::models::CustomProvider),
    CustomProviderDeleted {
        id: String,
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
        project_id: String,
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

    // Task activity stream
    ActivityLogged {
        task_id: Option<String>,
        action: String,
        actor: String,
        actor_role: String,
        payload: serde_json::Value,
    },
}


impl DjinnEvent {
    pub fn entity_type(&self) -> &'static str {
        match self {
            DjinnEvent::SettingUpdated(_) => "setting",
            DjinnEvent::ProjectCreated(_) | DjinnEvent::ProjectUpdated(_) | DjinnEvent::ProjectDeleted { .. } => "project",
            DjinnEvent::ProjectConfigUpdated { .. } => "project_config",
            DjinnEvent::EpicCreated(_) | DjinnEvent::EpicUpdated(_) | DjinnEvent::EpicDeleted { .. } => "epic",
            DjinnEvent::TaskCreated { .. } | DjinnEvent::TaskUpdated { .. } | DjinnEvent::TaskDeleted { .. } => "task",
            DjinnEvent::NoteCreated(_) | DjinnEvent::NoteUpdated(_) | DjinnEvent::NoteDeleted { .. } => "note",
            DjinnEvent::GitSettingsUpdated { .. } => "git_settings",
            DjinnEvent::CustomProviderUpserted(_) | DjinnEvent::CustomProviderDeleted { .. } => "custom_provider",
            DjinnEvent::CredentialCreated(_) | DjinnEvent::CredentialUpdated(_) | DjinnEvent::CredentialDeleted { .. } => "credential",
            DjinnEvent::SessionDispatched { .. } | DjinnEvent::SessionCreated(_) | DjinnEvent::SessionUpdated(_) | DjinnEvent::SessionTokenUpdate { .. } | DjinnEvent::SessionMessage { .. } => "session",
            DjinnEvent::SessionMessageInserted { .. } => "session_message",
            DjinnEvent::SyncCompleted { .. } => "sync",
            DjinnEvent::ProjectHealthChanged { .. } => "project",
            DjinnEvent::ActivityLogged { .. } => "activity",
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            DjinnEvent::SettingUpdated(_) => "updated",
            DjinnEvent::ProjectCreated(_) => "created",
            DjinnEvent::ProjectUpdated(_) => "updated",
            DjinnEvent::ProjectDeleted { .. } => "deleted",
            DjinnEvent::ProjectConfigUpdated { .. } => "updated",
            DjinnEvent::EpicCreated(_) => "created",
            DjinnEvent::EpicUpdated(_) => "updated",
            DjinnEvent::EpicDeleted { .. } => "deleted",
            DjinnEvent::TaskCreated { .. } => "created",
            DjinnEvent::TaskUpdated { .. } => "updated",
            DjinnEvent::TaskDeleted { .. } => "deleted",
            DjinnEvent::NoteCreated(_) => "created",
            DjinnEvent::NoteUpdated(_) => "updated",
            DjinnEvent::NoteDeleted { .. } => "deleted",
            DjinnEvent::GitSettingsUpdated { .. } => "updated",
            DjinnEvent::CustomProviderUpserted(_) => "updated",
            DjinnEvent::CustomProviderDeleted { .. } => "deleted",
            DjinnEvent::CredentialCreated(_) => "created",
            DjinnEvent::CredentialUpdated(_) => "updated",
            DjinnEvent::CredentialDeleted { .. } => "deleted",
            DjinnEvent::SessionDispatched { .. } => "dispatched",
            DjinnEvent::SessionCreated(_) => "started",
            DjinnEvent::SessionUpdated(v) => match v.status.as_str() {
                "completed" => "completed",
                "interrupted" => "interrupted",
                "failed" => "failed",
                _ => "updated",
            },
            DjinnEvent::SessionTokenUpdate { .. } => "token_update",
            DjinnEvent::SessionMessage { .. } => "message",
            DjinnEvent::SessionMessageInserted { .. } => "inserted",
            DjinnEvent::SyncCompleted { .. } => "completed",
            DjinnEvent::ProjectHealthChanged { healthy: true, .. } => "health_ok",
            DjinnEvent::ProjectHealthChanged { healthy: false, .. } => "health_error",
            DjinnEvent::ActivityLogged { .. } => "logged",
        }
    }

    pub fn from_sync(&self) -> bool {
        match self {
            DjinnEvent::TaskCreated { from_sync, .. } | DjinnEvent::TaskUpdated { from_sync, .. } => *from_sync,
            _ => false,
        }
    }
}
