use crate::db::ProjectConfig;
use crate::models::Credential;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Note;
use crate::models::Project;
use crate::models::Task;
use crate::verification::StepEvent;
use serde::de::DeserializeOwned;

/// Domain events emitted by repositories after every write.
///
/// Sent over a `tokio::sync::broadcast` channel. SSE subscribers and
/// other internal consumers receive full entities — no follow-up reads needed.
///
/// Conventions:
///   - `Created` / `Updated` variants carry the full entity clone.
///   - `Deleted` variants carry only the `id` string.
///
/// NOTE: Variants are only constructed inside this module (in the `From` impl
/// and tests). External callers use `DjinnEventEnvelope::*` constructors directly.
#[allow(dead_code)]
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) enum DjinnEvent {
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

    /// Step-level progress during verification command execution.
    #[allow(dead_code)]
    VerificationStep {
        project_id: String,
        task_id: Option<String>,
        phase: String,
        step: StepEvent,
    },

    /// Step-level progress during task lifecycle setup (before agent session).
    #[allow(dead_code)]
    TaskLifecycleStep {
        task_id: String,
        step: String,
        detail: serde_json::Value,
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

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DjinnEventEnvelope {
    pub entity_type: &'static str,
    pub action: &'static str,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip)]
    pub from_sync: bool,
}

impl DjinnEventEnvelope {
    pub(crate) fn project_created(project: &Project) -> Self {
        Self {
            entity_type: "project",
            action: "created",
            payload: serde_json::to_value(project)
                .expect("serializing DjinnEvent payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub(crate) fn project_updated(project: &Project) -> Self {
        Self {
            entity_type: "project",
            action: "updated",
            payload: serde_json::to_value(project)
                .expect("serializing DjinnEvent payload to Value should not fail"),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub(crate) fn project_deleted(id: &str) -> Self {
        Self {
            entity_type: "project",
            action: "deleted",
            payload: serde_json::to_value(serde_json::json!({ "id": id }))
                .expect("serializing DjinnEvent payload to Value should not fail"),
            id: Some(id.to_string()),
            project_id: None,
            from_sync: false,
        }
    }

    pub(crate) fn project_config_updated(project_id: &str, config: &ProjectConfig) -> Self {
        Self {
            entity_type: "project_config",
            action: "updated",
            payload: serde_json::to_value(
                serde_json::json!({ "project_id": project_id, "config": config }),
            )
            .expect("serializing DjinnEvent payload to Value should not fail"),
            id: None,
            project_id: Some(project_id.to_string()),
            from_sync: false,
        }
    }

    pub(crate) fn epic_created(epic: &Epic) -> Self {
        Self { entity_type: "epic", action: "created", payload: serde_json::to_value(epic).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn epic_updated(epic: &Epic) -> Self {
        Self { entity_type: "epic", action: "updated", payload: serde_json::to_value(epic).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn epic_deleted(id: &str) -> Self {
        Self { entity_type: "epic", action: "deleted", payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(), id: Some(id.to_string()), project_id: None, from_sync: false }
    }
    pub(crate) fn task_created(task: &Task, from_sync: bool) -> Self {
        Self { entity_type: "task", action: "created", payload: serde_json::to_value(serde_json::json!({"task": task, "from_sync": from_sync})).unwrap(), id: None, project_id: None, from_sync }
    }
    pub(crate) fn task_updated(task: &Task, from_sync: bool) -> Self {
        Self { entity_type: "task", action: "updated", payload: serde_json::to_value(serde_json::json!({"task": task, "from_sync": from_sync})).unwrap(), id: None, project_id: None, from_sync }
    }
    pub(crate) fn task_deleted(id: &str) -> Self {
        Self { entity_type: "task", action: "deleted", payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(), id: Some(id.to_string()), project_id: None, from_sync: false }
    }
    pub(crate) fn note_created(note: &Note) -> Self {
        Self { entity_type: "note", action: "created", payload: serde_json::to_value(note).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn note_updated(note: &Note) -> Self {
        Self { entity_type: "note", action: "updated", payload: serde_json::to_value(note).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn note_deleted(id: &str) -> Self {
        Self { entity_type: "note", action: "deleted", payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(), id: Some(id.to_string()), project_id: None, from_sync: false }
    }
    pub(crate) fn git_settings_updated(project_id: &str, settings: &GitSettings) -> Self {
        Self { entity_type: "git_settings", action: "updated", payload: serde_json::to_value(serde_json::json!({"project_id": project_id, "settings": settings})).unwrap(), id: None, project_id: Some(project_id.to_string()), from_sync: false }
    }
    pub(crate) fn custom_provider_upserted(provider: &crate::models::CustomProvider) -> Self {
        Self { entity_type: "custom_provider", action: "updated", payload: serde_json::to_value(provider).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn custom_provider_deleted(id: &str) -> Self {
        Self { entity_type: "custom_provider", action: "deleted", payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(), id: Some(id.to_string()), project_id: None, from_sync: false }
    }
    pub(crate) fn credential_created(credential: &Credential) -> Self {
        Self { entity_type: "credential", action: "created", payload: serde_json::to_value(credential).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn credential_updated(credential: &Credential) -> Self {
        Self { entity_type: "credential", action: "updated", payload: serde_json::to_value(credential).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn credential_deleted(id: &str) -> Self {
        Self { entity_type: "credential", action: "deleted", payload: serde_json::to_value(serde_json::json!({"id": id})).unwrap(), id: Some(id.to_string()), project_id: None, from_sync: false }
    }
    pub(crate) fn session_dispatched(project_id: &str, task_id: &str, model_id: &str, agent_type: &str) -> Self {
        Self { entity_type: "session", action: "dispatched", payload: serde_json::to_value(serde_json::json!({"project_id": project_id, "task_id": task_id, "model_id": model_id, "agent_type": agent_type})).unwrap(), id: None, project_id: Some(project_id.to_string()), from_sync: false }
    }
    pub(crate) fn session_token_update(session_id: &str, task_id: &str, tokens_in: i64, tokens_out: i64, context_window: i64, usage_pct: f64) -> Self {
        Self { entity_type: "session", action: "token_update", payload: serde_json::to_value(serde_json::json!({"session_id": session_id, "task_id": task_id, "tokens_in": tokens_in, "tokens_out": tokens_out, "context_window": context_window, "usage_pct": usage_pct})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn session_message(session_id: &str, task_id: &str, agent_type: &str, message: &serde_json::Value) -> Self {
        Self { entity_type: "session", action: "message", payload: serde_json::to_value(serde_json::json!({"session_id": session_id, "task_id": task_id, "agent_type": agent_type, "message": message})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn sync_completed(channel: &str, direction: &str, count: usize, error: Option<&str>) -> Self {
        Self { entity_type: "sync", action: "completed", payload: serde_json::to_value(serde_json::json!({"channel": channel, "direction": direction, "count": count, "error": error})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn project_health_changed(project_id: &str, healthy: bool, error: Option<&str>) -> Self {
        Self { entity_type: "project", action: if healthy { "health_ok" } else { "health_error" }, payload: serde_json::to_value(serde_json::json!({"project_id": project_id, "healthy": healthy, "error": error})).unwrap(), id: None, project_id: Some(project_id.to_string()), from_sync: false }
    }
    pub(crate) fn verification_step(project_id: &str, task_id: Option<&str>, phase: &str, step: &StepEvent) -> Self {
        Self { entity_type: "verification", action: "step", payload: serde_json::to_value(serde_json::json!({"project_id": project_id, "task_id": task_id, "phase": phase, "step": step})).unwrap(), id: None, project_id: Some(project_id.to_string()), from_sync: false }
    }
    pub(crate) fn task_lifecycle_step(task_id: &str, step: &str, detail: &serde_json::Value) -> Self {
        Self { entity_type: "lifecycle", action: "step", payload: serde_json::to_value(serde_json::json!({"task_id": task_id, "step": step, "detail": detail})).unwrap(), id: None, project_id: None, from_sync: false }
    }
    pub(crate) fn activity_logged(task_id: Option<&str>, action: &str, actor: &str, actor_role: &str, payload: &serde_json::Value) -> Self {
        Self { entity_type: "activity", action: "logged", payload: serde_json::to_value(serde_json::json!({"task_id": task_id, "action": action, "actor": actor, "actor_role": actor_role, "payload": payload})).unwrap(), id: None, project_id: None, from_sync: false }
    }

    pub fn entity_type(&self) -> &'static str { self.entity_type }
    pub fn action(&self) -> &'static str { self.action }
    pub fn from_sync(&self) -> bool { self.from_sync }
    pub fn payload(&self) -> &serde_json::Value { &self.payload }
    pub fn parse_payload<T: DeserializeOwned>(&self) -> Option<T> { serde_json::from_value(self.payload.clone()).ok() }
}

impl From<DjinnEvent> for DjinnEventEnvelope {
    fn from(event: DjinnEvent) -> Self {
        match event {
            DjinnEvent::ProjectCreated(project) => Self::project_created(&project),
            DjinnEvent::ProjectUpdated(project) => Self::project_updated(&project),
            DjinnEvent::ProjectDeleted { id } => Self::project_deleted(&id),
            DjinnEvent::ProjectConfigUpdated { project_id, config } => Self::project_config_updated(&project_id, &config),
            DjinnEvent::EpicCreated(epic) => Self::epic_created(&epic),
            DjinnEvent::EpicUpdated(epic) => Self::epic_updated(&epic),
            DjinnEvent::EpicDeleted { id } => Self::epic_deleted(&id),
            DjinnEvent::TaskCreated { task, from_sync } => Self::task_created(&task, from_sync),
            DjinnEvent::TaskUpdated { task, from_sync } => Self::task_updated(&task, from_sync),
            DjinnEvent::TaskDeleted { id } => Self::task_deleted(&id),
            DjinnEvent::NoteCreated(note) => Self::note_created(&note),
            DjinnEvent::NoteUpdated(note) => Self::note_updated(&note),
            DjinnEvent::NoteDeleted { id } => Self::note_deleted(&id),
            DjinnEvent::GitSettingsUpdated { project_id, settings } => Self::git_settings_updated(&project_id, &settings),
            DjinnEvent::CustomProviderUpserted(provider) => Self::custom_provider_upserted(&provider),
            DjinnEvent::CustomProviderDeleted { id } => Self::custom_provider_deleted(&id),
            DjinnEvent::CredentialCreated(credential) => Self::credential_created(&credential),
            DjinnEvent::CredentialUpdated(credential) => Self::credential_updated(&credential),
            DjinnEvent::CredentialDeleted { id } => Self::credential_deleted(&id),
            DjinnEvent::SessionDispatched { project_id, task_id, model_id, agent_type } => Self::session_dispatched(&project_id, &task_id, &model_id, &agent_type),
            DjinnEvent::SessionTokenUpdate { session_id, task_id, tokens_in, tokens_out, context_window, usage_pct } => Self::session_token_update(&session_id, &task_id, tokens_in, tokens_out, context_window, usage_pct),
            DjinnEvent::SessionMessage { session_id, task_id, agent_type, message } => Self::session_message(&session_id, &task_id, &agent_type, &message),
            DjinnEvent::SyncCompleted { channel, direction, count, error } => Self::sync_completed(&channel, &direction, count, error.as_deref()),
            DjinnEvent::ProjectHealthChanged { project_id, healthy, error } => Self::project_health_changed(&project_id, healthy, error.as_deref()),
            DjinnEvent::VerificationStep { project_id, task_id, phase, step } => Self::verification_step(&project_id, task_id.as_deref(), &phase, &step),
            DjinnEvent::TaskLifecycleStep { task_id, step, detail } => Self::task_lifecycle_step(&task_id, &step, &detail),
            DjinnEvent::ActivityLogged { task_id, action, actor, actor_role, payload } => Self::activity_logged(task_id.as_deref(), &action, &actor, &actor_role, &payload),
        }
    }
}

#[allow(dead_code)]
impl DjinnEvent {
    fn entity_type(&self) -> &'static str {
        match self {
            DjinnEvent::ProjectCreated(_)
            | DjinnEvent::ProjectUpdated(_)
            | DjinnEvent::ProjectDeleted { .. } => "project",
            DjinnEvent::ProjectConfigUpdated { .. } => "project_config",
            DjinnEvent::EpicCreated(_)
            | DjinnEvent::EpicUpdated(_)
            | DjinnEvent::EpicDeleted { .. } => "epic",
            DjinnEvent::TaskCreated { .. }
            | DjinnEvent::TaskUpdated { .. }
            | DjinnEvent::TaskDeleted { .. } => "task",
            DjinnEvent::NoteCreated(_)
            | DjinnEvent::NoteUpdated(_)
            | DjinnEvent::NoteDeleted { .. } => "note",
            DjinnEvent::GitSettingsUpdated { .. } => "git_settings",
            DjinnEvent::CustomProviderUpserted(_) | DjinnEvent::CustomProviderDeleted { .. } => {
                "custom_provider"
            }
            DjinnEvent::CredentialCreated(_)
            | DjinnEvent::CredentialUpdated(_)
            | DjinnEvent::CredentialDeleted { .. } => "credential",
            DjinnEvent::SessionDispatched { .. }
            | DjinnEvent::SessionTokenUpdate { .. }
            | DjinnEvent::SessionMessage { .. } => "session",
            DjinnEvent::SyncCompleted { .. } => "sync",
            DjinnEvent::ProjectHealthChanged { .. } => "project",
            DjinnEvent::VerificationStep { .. } => "verification",
            DjinnEvent::TaskLifecycleStep { .. } => "lifecycle",
            DjinnEvent::ActivityLogged { .. } => "activity",
        }
    }

    fn action(&self) -> &'static str {
        match self {
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
            DjinnEvent::SessionTokenUpdate { .. } => "token_update",
            DjinnEvent::SessionMessage { .. } => "message",
            DjinnEvent::SyncCompleted { .. } => "completed",
            DjinnEvent::ProjectHealthChanged { healthy: true, .. } => "health_ok",
            DjinnEvent::ProjectHealthChanged { healthy: false, .. } => "health_error",
            DjinnEvent::VerificationStep { .. } | DjinnEvent::TaskLifecycleStep { .. } => "step",
            DjinnEvent::ActivityLogged { .. } => "logged",
        }
    }

    fn is_from_sync(&self) -> bool {
        match self {
            DjinnEvent::TaskCreated { from_sync, .. }
            | DjinnEvent::TaskUpdated { from_sync, .. } => *from_sync,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DjinnEvent, DjinnEventEnvelope};
    use crate::models::{Project, Setting, Task};
    use crate::verification::StepEvent;
    use serde_json::json;

    #[test]
    fn envelope_task_created_round_trip_and_parse_payload() {
        let task = Task {
            id: "task-1".into(),
            project_id: "p1".into(),
            short_id: "T-1".into(),
            epic_id: None,
            title: "Title".into(),
            description: "".into(),
            design: "".into(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 1,
            owner: "".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            memory_refs: "[]".into(),
            unresolved_blocker_count: 0,
        };
        let event = DjinnEvent::TaskCreated {
            task: task.clone(),
            from_sync: true,
        };

        let envelope = DjinnEventEnvelope::from(event);
        assert_eq!(envelope.entity_type(), "task");
        assert_eq!(envelope.action(), "created");
        assert!(envelope.from_sync());
        assert_eq!(envelope.id, None);
        assert_eq!(envelope.project_id, None);

        let parsed: Option<serde_json::Value> = envelope.parse_payload();
        assert_eq!(parsed, Some(json!({ "task": task, "from_sync": true })));
    }

    #[test]
    fn envelope_project_deleted_has_id_only() {
        let event = DjinnEvent::ProjectDeleted {
            id: "proj-123".into(),
        };
        let envelope = DjinnEventEnvelope::from(event);

        assert_eq!(envelope.entity_type(), "project");
        assert_eq!(envelope.action(), "deleted");
        assert_eq!(envelope.id.as_deref(), Some("proj-123"));
        assert_eq!(envelope.project_id, None);
        assert!(!envelope.from_sync());
        assert_eq!(envelope.payload(), &json!({"id": "proj-123"}));
    }

    #[test]
    fn envelope_session_message_nested_payload() {
        let msg = json!({"content": [{"type":"text","text":"hello"}]});
        let event = DjinnEvent::SessionMessage {
            session_id: "s1".into(),
            task_id: "t1".into(),
            agent_type: "worker".into(),
            message: msg.clone(),
        };
        let envelope = DjinnEventEnvelope::from(event);

        assert_eq!(envelope.entity_type(), "session");
        assert_eq!(envelope.action(), "message");
        assert_eq!(
            envelope.payload(),
            &json!({
                "session_id": "s1",
                "task_id": "t1",
                "agent_type": "worker",
                "message": msg,
            })
        );
    }

    #[test]
    fn envelope_verification_step_maps_entity_action_and_payload() {
        let event = DjinnEvent::VerificationStep {
            project_id: "p1".into(),
            task_id: Some("t1".into()),
            phase: "verification".into(),
            step: StepEvent::Started {
                index: 1,
                total: 3,
                name: "clippy".into(),
                command: "cargo clippy".into(),
            },
        };
        let envelope = DjinnEventEnvelope::from(event);

        assert_eq!(envelope.entity_type(), "verification");
        assert_eq!(envelope.action(), "step");
        assert_eq!(envelope.project_id.as_deref(), Some("p1"));
        assert_eq!(
            envelope.payload(),
            &json!({
                "project_id": "p1",
                "task_id": "t1",
                "phase": "verification",
                "step": {
                    "Started": {
                        "index": 1,
                        "total": 3,
                        "name": "clippy",
                        "command": "cargo clippy"
                    }
                }
            })
        );
    }

    #[test]
    fn envelope_task_lifecycle_step_maps_entity_action_and_payload() {
        let event = DjinnEvent::TaskLifecycleStep {
            task_id: "t1".into(),
            step: "worktree_creating".into(),
            detail: json!({ "path": "/tmp/worktree" }),
        };
        let envelope = DjinnEventEnvelope::from(event);

        assert_eq!(envelope.entity_type(), "lifecycle");
        assert_eq!(envelope.action(), "step");
        assert_eq!(envelope.project_id, None);
        assert_eq!(
            envelope.payload(),
            &json!({
                "task_id": "t1",
                "step": "worktree_creating",
                "detail": { "path": "/tmp/worktree" }
            })
        );
    }

    #[test]
    fn envelope_setting_updated_parse_payload_typed() {
        let setting = Setting {
            key: "foo".into(),
            value: "bar".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        };
        let envelope = DjinnEventEnvelope {
            entity_type: "setting",
            action: "updated",
            payload: serde_json::to_value(&setting).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        };

        assert_eq!(envelope.entity_type(), "setting");
        assert_eq!(envelope.action(), "updated");

        let parsed: Option<Setting> = envelope.parse_payload();
        assert!(parsed.is_some());
        let parsed = parsed.expect("setting payload parses");
        assert_eq!(parsed.key, setting.key);
        assert_eq!(parsed.value, setting.value);
        assert_eq!(parsed.updated_at, setting.updated_at);
    }

    #[test]
    fn envelope_serializes_flat_json() {
        let project = Project {
            id: "proj-1".into(),
            name: "name".into(),
            path: "/tmp/proj".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            target_branch: "main".into(),
            auto_merge: false,
            sync_enabled: false,
            sync_remote: None,
        };
        let envelope = DjinnEventEnvelope::from(DjinnEvent::ProjectCreated(project));
        let value = serde_json::to_value(envelope).expect("envelope serializes");

        assert!(value.get("entity_type").is_some());
        assert!(value.get("action").is_some());
        assert!(value.get("payload").is_some());
        assert!(value.get("from_sync").is_none());
    }
}

#[cfg(test)]
mod constructor_parity_tests {
    use super::{DjinnEvent, DjinnEventEnvelope};
    use crate::db::ProjectConfig;
    use crate::models::{Credential, CustomProvider, Epic, GitSettings, Note, Project, SeedModel, Task};
    use crate::verification::StepEvent;
    use serde_json::json;

    fn fixtures() -> (
        Project,
        ProjectConfig,
        Epic,
        Task,
        Note,
        GitSettings,
        CustomProvider,
        Credential,
        StepEvent,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    ) {
        let project = Project { id: "p1".into(), name: "proj".into(), path: "/tmp/p".into(), created_at: "2025-01-01T00:00:00Z".into(), target_branch: "main".into(), auto_merge: false, sync_enabled: false, sync_remote: None };
        let config = ProjectConfig { target_branch: "main".into(), auto_merge: false, sync_enabled: false, sync_remote: None };
        let epic = Epic { id: "e1".into(), project_id: "p1".into(), short_id: "E-1".into(), title: "epic".into(), description: "".into(), emoji: "🔥".into(), color: "#ff0000".into(), status: "open".into(), owner: "".into(), created_at: "2025-01-01T00:00:00Z".into(), updated_at: "2025-01-01T00:00:00Z".into(), closed_at: None, memory_refs: "[]".into() };
        let task = Task { id: "t1".into(), project_id: "p1".into(), short_id: "T-1".into(), epic_id: None, title: "task".into(), description: "".into(), design: "".into(), issue_type: "task".into(), status: "open".into(), priority: 1, owner: "".into(), labels: "[]".into(), acceptance_criteria: "[]".into(), reopen_count: 0, continuation_count: 0, verification_failure_count: 0, created_at: "2025-01-01T00:00:00Z".into(), updated_at: "2025-01-01T00:00:00Z".into(), closed_at: None, close_reason: None, merge_commit_sha: None, memory_refs: "[]".into(), unresolved_blocker_count: 0 };
        let note = Note { id: "n1".into(), project_id: "p1".into(), permalink: "docs/n1".into(), title: "note".into(), file_path: "/tmp/p/docs/n1.md".into(), note_type: "reference".into(), folder: "docs".into(), tags: "[]".into(), content: "c".into(), created_at: "2025-01-01T00:00:00Z".into(), updated_at: "2025-01-01T00:00:00Z".into(), last_accessed: "2025-01-01T00:00:00Z".into() };
        let settings = GitSettings { target_branch: "main".into() };
        let provider = CustomProvider { id: "cp1".into(), name: "lab".into(), base_url: "http://x".into(), env_var: "KEY".into(), seed_models: vec![SeedModel { id: "m".into(), name: "model".into() }], created_at: "2025-01-01T00:00:00Z".into() };
        let credential = Credential { id: "c1".into(), provider_id: "openai".into(), key_name: "OPENAI_API_KEY".into(), created_at: "2025-01-01T00:00:00Z".into(), updated_at: "2025-01-01T00:00:00Z".into() };
        let step = StepEvent::Started { index: 1, total: 2, name: "test".into(), command: "cargo test".into() };
        let msg = json!({"k":"v"});
        let detail = json!({"d":1});
        let payload = json!({"p":1});
        (project, config, epic, task, note, settings, provider, credential, step, msg, detail, payload)
    }

    #[test]
    fn constructors_match_from_variants() {
        let (project, config, epic, task, note, settings, provider, credential, step, msg, detail, payload) = fixtures();
        assert_eq!(DjinnEventEnvelope::project_created(&project), DjinnEventEnvelope::from(DjinnEvent::ProjectCreated(project.clone())));
        assert_eq!(DjinnEventEnvelope::project_updated(&project), DjinnEventEnvelope::from(DjinnEvent::ProjectUpdated(project.clone())));
        assert_eq!(DjinnEventEnvelope::project_deleted("p1"), DjinnEventEnvelope::from(DjinnEvent::ProjectDeleted { id: "p1".into() }));
        assert_eq!(DjinnEventEnvelope::project_config_updated("p1", &config), DjinnEventEnvelope::from(DjinnEvent::ProjectConfigUpdated { project_id: "p1".into(), config: config.clone() }));
        assert_eq!(DjinnEventEnvelope::epic_created(&epic), DjinnEventEnvelope::from(DjinnEvent::EpicCreated(epic.clone())));
        assert_eq!(DjinnEventEnvelope::epic_updated(&epic), DjinnEventEnvelope::from(DjinnEvent::EpicUpdated(epic.clone())));
        assert_eq!(DjinnEventEnvelope::epic_deleted("e1"), DjinnEventEnvelope::from(DjinnEvent::EpicDeleted { id: "e1".into() }));
        assert_eq!(DjinnEventEnvelope::task_created(&task, true), DjinnEventEnvelope::from(DjinnEvent::TaskCreated { task: task.clone(), from_sync: true }));
        assert_eq!(DjinnEventEnvelope::task_updated(&task, false), DjinnEventEnvelope::from(DjinnEvent::TaskUpdated { task: task.clone(), from_sync: false }));
        assert_eq!(DjinnEventEnvelope::task_deleted("t1"), DjinnEventEnvelope::from(DjinnEvent::TaskDeleted { id: "t1".into() }));
        assert_eq!(DjinnEventEnvelope::note_created(&note), DjinnEventEnvelope::from(DjinnEvent::NoteCreated(note.clone())));
        assert_eq!(DjinnEventEnvelope::note_updated(&note), DjinnEventEnvelope::from(DjinnEvent::NoteUpdated(note.clone())));
        assert_eq!(DjinnEventEnvelope::note_deleted("n1"), DjinnEventEnvelope::from(DjinnEvent::NoteDeleted { id: "n1".into() }));
        assert_eq!(DjinnEventEnvelope::git_settings_updated("p1", &settings), DjinnEventEnvelope::from(DjinnEvent::GitSettingsUpdated { project_id: "p1".into(), settings: settings.clone() }));
        assert_eq!(DjinnEventEnvelope::custom_provider_upserted(&provider), DjinnEventEnvelope::from(DjinnEvent::CustomProviderUpserted(provider.clone())));
        assert_eq!(DjinnEventEnvelope::custom_provider_deleted("cp1"), DjinnEventEnvelope::from(DjinnEvent::CustomProviderDeleted { id: "cp1".into() }));
        assert_eq!(DjinnEventEnvelope::credential_created(&credential), DjinnEventEnvelope::from(DjinnEvent::CredentialCreated(credential.clone())));
        assert_eq!(DjinnEventEnvelope::credential_updated(&credential), DjinnEventEnvelope::from(DjinnEvent::CredentialUpdated(credential.clone())));
        assert_eq!(DjinnEventEnvelope::credential_deleted("c1"), DjinnEventEnvelope::from(DjinnEvent::CredentialDeleted { id: "c1".into() }));
        assert_eq!(DjinnEventEnvelope::session_dispatched("p1", "t1", "m1", "worker"), DjinnEventEnvelope::from(DjinnEvent::SessionDispatched { project_id: "p1".into(), task_id: "t1".into(), model_id: "m1".into(), agent_type: "worker".into() }));
        assert_eq!(DjinnEventEnvelope::session_token_update("s1", "t1", 1, 2, 3, 0.3), DjinnEventEnvelope::from(DjinnEvent::SessionTokenUpdate { session_id: "s1".into(), task_id: "t1".into(), tokens_in: 1, tokens_out: 2, context_window: 3, usage_pct: 0.3 }));
        assert_eq!(DjinnEventEnvelope::session_message("s1", "t1", "worker", &msg), DjinnEventEnvelope::from(DjinnEvent::SessionMessage { session_id: "s1".into(), task_id: "t1".into(), agent_type: "worker".into(), message: msg }));
        assert_eq!(DjinnEventEnvelope::sync_completed("ch", "export", 2, Some("err")), DjinnEventEnvelope::from(DjinnEvent::SyncCompleted { channel: "ch".into(), direction: "export".into(), count: 2, error: Some("err".into()) }));
        assert_eq!(DjinnEventEnvelope::project_health_changed("p1", true, None), DjinnEventEnvelope::from(DjinnEvent::ProjectHealthChanged { project_id: "p1".into(), healthy: true, error: None }));
        assert_eq!(DjinnEventEnvelope::verification_step("p1", Some("t1"), "verification", &step), DjinnEventEnvelope::from(DjinnEvent::VerificationStep { project_id: "p1".into(), task_id: Some("t1".into()), phase: "verification".into(), step: step.clone() }));
        assert_eq!(DjinnEventEnvelope::task_lifecycle_step("t1", "x", &detail), DjinnEventEnvelope::from(DjinnEvent::TaskLifecycleStep { task_id: "t1".into(), step: "x".into(), detail }));
        assert_eq!(DjinnEventEnvelope::activity_logged(Some("t1"), "a", "u", "worker", &payload), DjinnEventEnvelope::from(DjinnEvent::ActivityLogged { task_id: Some("t1".into()), action: "a".into(), actor: "u".into(), actor_role: "worker".into(), payload }));
    }
}

