use djinn_core::models::{Credential, CustomProvider, Epic, GitSettings, Project};
use djinn_memory::Note;

use crate::Result;

#[derive(Debug, Clone, Copy, Default)]
pub struct ModelsRepository;

impl ModelsRepository {
    pub fn parse_project(json: &str) -> Result<Project> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_epic(json: &str) -> Result<Epic> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_note(json: &str) -> Result<Note> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_git_settings(json: &str) -> Result<GitSettings> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_custom_provider(json: &str) -> Result<CustomProvider> {
        serde_json::from_str(json).map_err(Into::into)
    }

    pub fn parse_credential(json: &str) -> Result<Credential> {
        serde_json::from_str(json).map_err(Into::into)
    }
}
