use crate::db::ProjectConfig;
use crate::models::Credential;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Note;
use crate::models::Project;
use crate::models::Task;
use serde::de::DeserializeOwned;

/// Domain events emitted by repositories after every write.
///
/// Sent over a `tokio::sync::broadcast` channel. SSE subscribers and
/// other internal consumers receive full entities — no follow-up reads needed.
///
/// Conventions:
///   - `Created` / `Updated` variants carry the full entity clone.
///   - `Deleted` variants carry only the `id` string.
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

    // Task activity stream
    ActivityLogged {
        task_id: Option<String>,
        action: String,
        actor: String,
        actor_role: String,
        payload: serde_json::Value,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
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
    pub fn entity_type(&self) -> &'static str {
        self.entity_type
    }

    pub fn action(&self) -> &'static str {
        self.action
    }

    pub fn from_sync(&self) -> bool {
        self.from_sync
    }

    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    pub fn parse_payload<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.payload.clone()).ok()
    }
}

impl From<DjinnEvent> for DjinnEventEnvelope {
    fn from(event: DjinnEvent) -> Self {
        let entity_type = event.entity_type();
        let action = event.action();
        let from_sync = event.is_from_sync();

        let id = match &event {
            DjinnEvent::ProjectDeleted { id }
            | DjinnEvent::EpicDeleted { id }
            | DjinnEvent::TaskDeleted { id }
            | DjinnEvent::NoteDeleted { id }
            | DjinnEvent::CustomProviderDeleted { id }
            | DjinnEvent::CredentialDeleted { id } => Some(id.clone()),
            _ => None,
        };

        let project_id = match &event {
            DjinnEvent::ProjectConfigUpdated { project_id, .. }
            | DjinnEvent::GitSettingsUpdated { project_id, .. }
            | DjinnEvent::SessionDispatched { project_id, .. }
            | DjinnEvent::ProjectHealthChanged { project_id, .. } => Some(project_id.clone()),
            _ => None,
        };

        let payload = match event {
            DjinnEvent::ProjectCreated(project) => serde_json::to_value(project),
            DjinnEvent::ProjectUpdated(project) => serde_json::to_value(project),
            DjinnEvent::ProjectDeleted { id } => {
                serde_json::to_value(serde_json::json!({ "id": id }))
            }
            DjinnEvent::ProjectConfigUpdated { project_id, config } => serde_json::to_value(
                serde_json::json!({ "project_id": project_id, "config": config }),
            ),
            DjinnEvent::EpicCreated(epic) => serde_json::to_value(epic),
            DjinnEvent::EpicUpdated(epic) => serde_json::to_value(epic),
            DjinnEvent::EpicDeleted { id } => serde_json::to_value(serde_json::json!({ "id": id })),
            DjinnEvent::TaskCreated { task, from_sync } => {
                serde_json::to_value(serde_json::json!({ "task": task, "from_sync": from_sync }))
            }
            DjinnEvent::TaskUpdated { task, from_sync } => {
                serde_json::to_value(serde_json::json!({ "task": task, "from_sync": from_sync }))
            }
            DjinnEvent::TaskDeleted { id } => serde_json::to_value(serde_json::json!({ "id": id })),
            DjinnEvent::NoteCreated(note) => serde_json::to_value(note),
            DjinnEvent::NoteUpdated(note) => serde_json::to_value(note),
            DjinnEvent::NoteDeleted { id } => serde_json::to_value(serde_json::json!({ "id": id })),
            DjinnEvent::GitSettingsUpdated {
                project_id,
                settings,
            } => serde_json::to_value(
                serde_json::json!({ "project_id": project_id, "settings": settings }),
            ),
            DjinnEvent::CustomProviderUpserted(provider) => serde_json::to_value(provider),
            DjinnEvent::CustomProviderDeleted { id } => {
                serde_json::to_value(serde_json::json!({ "id": id }))
            }
            DjinnEvent::CredentialCreated(credential) => serde_json::to_value(credential),
            DjinnEvent::CredentialUpdated(credential) => serde_json::to_value(credential),
            DjinnEvent::CredentialDeleted { id } => {
                serde_json::to_value(serde_json::json!({ "id": id }))
            }
            DjinnEvent::SessionDispatched {
                project_id,
                task_id,
                model_id,
                agent_type,
            } => serde_json::to_value(serde_json::json!({
                "project_id": project_id,
                "task_id": task_id,
                "model_id": model_id,
                "agent_type": agent_type,
            })),
            DjinnEvent::SessionTokenUpdate {
                session_id,
                task_id,
                tokens_in,
                tokens_out,
                context_window,
                usage_pct,
            } => serde_json::to_value(serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "context_window": context_window,
                "usage_pct": usage_pct,
            })),
            DjinnEvent::SessionMessage {
                session_id,
                task_id,
                agent_type,
                message,
            } => serde_json::to_value(serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "agent_type": agent_type,
                "message": message,
            })),
            DjinnEvent::SyncCompleted {
                channel,
                direction,
                count,
                error,
            } => serde_json::to_value(serde_json::json!({
                "channel": channel,
                "direction": direction,
                "count": count,
                "error": error,
            })),
            DjinnEvent::ProjectHealthChanged {
                project_id,
                healthy,
                error,
            } => serde_json::to_value(serde_json::json!({
                "project_id": project_id,
                "healthy": healthy,
                "error": error,
            })),
            DjinnEvent::ActivityLogged {
                task_id,
                action,
                actor,
                actor_role,
                payload,
            } => serde_json::to_value(serde_json::json!({
                "task_id": task_id,
                "action": action,
                "actor": actor,
                "actor_role": actor_role,
                "payload": payload,
            })),
        }
        .expect("serializing DjinnEvent payload to Value should not fail");

        Self {
            entity_type,
            action,
            payload,
            id,
            project_id,
            from_sync,
        }
    }
}

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
            setup_commands: "[]".into(),
            verification_commands: "[]".into(),
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
