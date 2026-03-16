use crate::db::ProjectConfig;
use crate::models::Credential;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Note;
use crate::models::Project;
use crate::models::Task;
use crate::verification::StepEvent;
use serde::de::DeserializeOwned;

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
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
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
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
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
                .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
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
            .expect("serializing DjinnEventEnvelope payload to Value should not fail"),
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

#[cfg(test)]
mod tests {
    use super::DjinnEventEnvelope;
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

        let envelope = DjinnEventEnvelope::task_created(&task, true);
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
        let envelope = DjinnEventEnvelope::project_deleted("proj-123");

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
        let envelope = DjinnEventEnvelope::session_message("s1", "t1", "worker", &msg);

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
        let step = StepEvent::Started {
            index: 1,
            total: 3,
            name: "clippy".into(),
            command: "cargo clippy".into(),
        };
        let envelope = DjinnEventEnvelope::verification_step("p1", Some("t1"), "verification", &step);

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
        let detail = json!({ "path": "/tmp/worktree" });
        let envelope = DjinnEventEnvelope::task_lifecycle_step("t1", "worktree_creating", &detail);

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
        let envelope = DjinnEventEnvelope::project_created(&project);
        let value = serde_json::to_value(envelope).expect("envelope serializes");

        assert!(value.get("entity_type").is_some());
        assert!(value.get("action").is_some());
        assert!(value.get("payload").is_some());
        assert!(value.get("from_sync").is_none());
    }
}
