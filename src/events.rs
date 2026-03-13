use serde::Serialize;
use serde_json::Value;

/// Flat event envelope emitted by repositories after writes.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DjinnEvent {
    pub entity_type: String,
    pub action: String,
    pub payload: Value,
    pub id: Option<String>,
    pub project_id: Option<String>,
    #[serde(skip)]
    pub from_sync: bool,
}

impl DjinnEvent {
    pub fn entity(entity_type: impl Into<String>, action: impl Into<String>, payload: &impl Serialize) -> Self {
        Self {
            entity_type: entity_type.into(),
            action: action.into(),
            payload: serde_json::to_value(payload).unwrap_or(Value::Null),
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn deleted(entity_type: impl Into<String>, id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            entity_type: entity_type.into(),
            action: "deleted".to_string(),
            payload: serde_json::json!({ "id": id }),
            id: Some(id),
            project_id: None,
            from_sync: false,
        }
    }

    pub fn scoped(
        entity_type: impl Into<String>,
        action: impl Into<String>,
        project_id: impl Into<String>,
        payload: &impl Serialize,
    ) -> Self {
        let mut evt = Self::entity(entity_type, action, payload);
        evt.project_id = Some(project_id.into());
        evt
    }

    pub fn signal(entity_type: impl Into<String>, action: impl Into<String>, payload: Value) -> Self {
        Self {
            entity_type: entity_type.into(),
            action: action.into(),
            payload,
            id: None,
            project_id: None,
            from_sync: false,
        }
    }

    pub fn with_from_sync(mut self) -> Self {
        self.from_sync = true;
        self
    }
}
