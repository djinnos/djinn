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
    #[error(transparent)]
    Residue(#[from] crate::project_residue::ProjectResidueError),
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let report = crate::project_residue::cleanup_project_local_djinn(checkout)?;
            if !report.is_clean() {
                tracing::warn!(checkout = %checkout.display(), residue = ?report, "project-local .djinn cleanup blocked; retained residue");
            }
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    let result = match LegacySettingsImport::new(db)
        .import(project_id, &source)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            // Keep malformed input in place, but make its local cleanup gate
            // observable before startup proceeds to the next project.
            let report = crate::project_residue::cleanup_project_local_djinn(checkout)?;
            tracing::warn!(checkout = %checkout.display(), residue = ?report, "project-local .djinn cleanup blocked; retained residue");
            return Err(error.into());
        }
    };
    tokio::fs::remove_file(source_path).await?;
    let report = crate::project_residue::cleanup_project_local_djinn(checkout)?;
    if !report.is_clean() {
        tracing::warn!(checkout = %checkout.display(), residue = ?report, "project-local .djinn cleanup blocked; retained residue");
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::test_support::seed_project;
    use tempfile::TempDir;

    async fn database_with_project(project_id: &str) -> Database {
        let db = Database::ephemeral()
            .await
            .expect("open isolated Postgres database");
        seed_project(&db, project_id, project_id).await;
        db
    }

    async fn settings_file(checkout: &TempDir, source: &[u8]) -> std::path::PathBuf {
        let path = checkout.path().join(LEGACY_SETTINGS_RELATIVE_PATH);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, source).await.unwrap();
        path
    }

    #[tokio::test]
    async fn failure_retains_the_exact_source_file_without_creating_replacements() {
        let checkout = TempDir::new().unwrap();
        let source = settings_file(&checkout, br#"{"global_skills":["retired"]}"#).await;
        let db = database_with_project("legacy-file-failure").await;
        assert!(
            import_legacy_settings_file(db, "legacy-file-failure", checkout.path())
                .await
                .is_err()
        );
        assert!(source.exists());
        assert!(!checkout.path().join(".agents").exists());
        assert!(!checkout.path().join("settings.json").exists());
    }

    #[tokio::test]
    async fn successful_file_handoff_deletes_only_source_and_restart_is_idempotent() {
        let checkout = TempDir::new().unwrap();
        let source = settings_file(&checkout, br#"{"setup":["make setup"]}"#).await;
        let sibling = checkout.path().join(".djinn/keep.txt");
        tokio::fs::write(&sibling, "keep").await.unwrap();
        let db = database_with_project("legacy-file-success").await;
        assert_eq!(
            import_legacy_settings_file(db.clone(), "legacy-file-success", checkout.path())
                .await
                .unwrap(),
            Some(LegacySettingsImportResult::Imported)
        );
        assert!(!source.exists());
        assert!(sibling.exists());
        assert_eq!(
            import_legacy_settings_file(db, "legacy-file-success", checkout.path())
                .await
                .unwrap(),
            None
        );
        assert!(!checkout.path().join(".agents").exists());
        assert!(!checkout.path().join("settings.json").exists());
    }
}
