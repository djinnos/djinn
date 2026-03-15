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
