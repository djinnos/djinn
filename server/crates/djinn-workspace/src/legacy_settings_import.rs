//! Filesystem handoff for the one-time legacy settings import.
//!
//! Parsing and database mutation deliberately live in `djinn-db`; this module
//! only owns the source-file boundary so a failed import cannot delete input.

use std::path::Path;

use djinn_db::{
    Database, LegacySettingsImport, LegacySettingsImportError, LegacySettingsImportResult,
};
use thiserror::Error;

const LEGACY_SETTINGS_RELATIVE_PATH: &str = ".djinn/settings.json";

#[derive(Debug, Error)]
pub enum LegacySettingsFileImportError {
    #[error("unable to read legacy settings: {0}")]
    Read(#[from] std::io::Error),
    #[error(transparent)]
    Import(#[from] LegacySettingsImportError),
}

/// Import a single checkout's legacy settings and delete exactly that source
/// after the database transaction succeeds. Missing files are intentionally a
/// no-op: a restarted process observes the durable successful outcome instead
/// of creating any fallback or replacement configuration file.
pub async fn import_legacy_settings_file(
    db: Database,
    project_id: &str,
    checkout: &Path,
) -> Result<Option<LegacySettingsImportResult>, LegacySettingsFileImportError> {
    let source_path = checkout.join(LEGACY_SETTINGS_RELATIVE_PATH);
    let source = match tokio::fs::read(&source_path).await {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let result = LegacySettingsImport::new(db)
        .import(project_id, &source)
        .await?;
    tokio::fs::remove_file(source_path).await?;
    Ok(Some(result))
}
