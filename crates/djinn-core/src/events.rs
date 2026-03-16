use crate::models::Credential;
use crate::models::CustomProvider;
use crate::models::Epic;
use crate::models::GitSettings;
use crate::models::Note;
use crate::models::Project;
use serde::de::DeserializeOwned;

#[derive(Clone, Debug, serde::Serialize)]
pub enum DjinnEvent {
    ProjectCreated(Project),
    ProjectUpdated(Project),
    ProjectDeleted { id: String },

    EpicCreated(Epic),
    EpicUpdated(Epic),
    EpicDeleted { id: String },

    NoteCreated(Note),
    NoteUpdated(Note),
    NoteDeleted { id: String },

    GitSettingsUpdated { project_id: String, settings: GitSettings },

    CustomProviderUpserted(CustomProvider),
    CustomProviderDeleted { id: String },

    CredentialCreated(Credential),
    CredentialUpdated(Credential),
    CredentialDeleted { id: String },
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
}

impl DjinnEventEnvelope {
    pub fn entity_type(&self) -> &'static str { self.entity_type }
    pub fn action(&self) -> &'static str { self.action }
    pub fn payload(&self) -> &serde_json::Value { &self.payload }
    pub fn parse_payload<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(self.payload.clone()).ok()
    }
}

impl From<DjinnEvent> for DjinnEventEnvelope {
    fn from(event: DjinnEvent) -> Self {
        let (entity_type, action, payload, id, project_id) = match event {
            DjinnEvent::ProjectCreated(project) => {
                ("project", "created", serde_json::to_value(project), None, None)
            }
            DjinnEvent::ProjectUpdated(project) => {
                ("project", "updated", serde_json::to_value(project), None, None)
            }
            DjinnEvent::ProjectDeleted { id } => (
                "project",
                "deleted",
                serde_json::to_value(serde_json::json!({ "id": id.clone() })),
                Some(id),
                None,
            ),
            DjinnEvent::EpicCreated(epic) => ("epic", "created", serde_json::to_value(epic), None, None),
            DjinnEvent::EpicUpdated(epic) => ("epic", "updated", serde_json::to_value(epic), None, None),
            DjinnEvent::EpicDeleted { id } => (
                "epic",
                "deleted",
                serde_json::to_value(serde_json::json!({ "id": id.clone() })),
                Some(id),
                None,
            ),
            DjinnEvent::NoteCreated(note) => ("note", "created", serde_json::to_value(note), None, None),
            DjinnEvent::NoteUpdated(note) => ("note", "updated", serde_json::to_value(note), None, None),
            DjinnEvent::NoteDeleted { id } => (
                "note",
                "deleted",
                serde_json::to_value(serde_json::json!({ "id": id.clone() })),
                Some(id),
                None,
            ),
            DjinnEvent::GitSettingsUpdated {
                project_id,
                settings,
            } => (
                "git_settings",
                "updated",
                serde_json::to_value(
                    serde_json::json!({ "project_id": project_id.clone(), "settings": settings }),
                ),
                None,
                Some(project_id),
            ),
            DjinnEvent::CustomProviderUpserted(provider) => (
                "custom_provider",
                "upserted",
                serde_json::to_value(provider),
                None,
                None,
            ),
            DjinnEvent::CustomProviderDeleted { id } => (
                "custom_provider",
                "deleted",
                serde_json::to_value(serde_json::json!({ "id": id.clone() })),
                Some(id),
                None,
            ),
            DjinnEvent::CredentialCreated(credential) => (
                "credential",
                "created",
                serde_json::to_value(credential),
                None,
                None,
            ),
            DjinnEvent::CredentialUpdated(credential) => (
                "credential",
                "updated",
                serde_json::to_value(credential),
                None,
                None,
            ),
            DjinnEvent::CredentialDeleted { id } => (
                "credential",
                "deleted",
                serde_json::to_value(serde_json::json!({ "id": id.clone() })),
                Some(id),
                None,
            ),
        };

        Self {
            entity_type,
            action,
            payload: payload.unwrap_or(serde_json::Value::Null),
            id,
            project_id,
        }
    }
}
