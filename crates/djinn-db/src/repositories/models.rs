use djinn_core::models::{Credential, CustomProvider, Epic, GitSettings, Note, Project};

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelsRepository;

impl ModelsRepository {
    pub fn parse_project(json: &str) -> crate::error::DbResult<Project> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_epic(json: &str) -> crate::error::DbResult<Epic> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_note(json: &str) -> crate::error::DbResult<Note> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_git_settings(json: &str) -> crate::error::DbResult<GitSettings> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_custom_provider(json: &str) -> crate::error::DbResult<CustomProvider> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_credential(json: &str) -> crate::error::DbResult<Credential> {
        serde_json::from_str(json).map_err(Into::into)
    }
}
