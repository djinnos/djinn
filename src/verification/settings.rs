use std::path::Path;

use serde::Deserialize;

use crate::commands::CommandSpec;
use crate::models::Project;

#[derive(Debug, Deserialize, Default)]
pub struct DjinnSettings {
    #[serde(default)]
    pub setup: Vec<CommandSpec>,
    #[serde(default)]
    pub verification: Vec<CommandSpec>,
}

fn parse_db_commands(project: &Project) -> (Vec<CommandSpec>, Vec<CommandSpec>) {
    let setup = serde_json::from_str(&project.setup_commands).unwrap_or_default();
    let verification = serde_json::from_str(&project.verification_commands).unwrap_or_default();
    tracing::info!("Loaded commands from DB project config");
    (setup, verification)
}

/// Load commands from .djinn/settings.json in worktree, falling back to project DB config.
pub fn load_commands(worktree_path: &Path, project: &Project) -> (Vec<CommandSpec>, Vec<CommandSpec>) {
    let settings_path = worktree_path.join(".djinn/settings.json");

    match std::fs::read_to_string(&settings_path) {
        Ok(content) => match serde_json::from_str::<DjinnSettings>(&content) {
            Ok(settings) => {
                tracing::info!(path = %settings_path.display(), "Loaded commands from .djinn/settings.json");
                (settings.setup, settings.verification)
            }
            Err(e) => {
                tracing::warn!(path = %settings_path.display(), error = %e, "Failed to parse .djinn/settings.json; falling back to DB");
                parse_db_commands(project)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %settings_path.display(), "No .djinn/settings.json found; falling back to DB");
            parse_db_commands(project)
        }
        Err(e) => {
            tracing::warn!(path = %settings_path.display(), error = %e, "Failed to read .djinn/settings.json; falling back to DB");
            parse_db_commands(project)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn project_with_db_commands(setup_json: &str, verification_json: &str) -> Project {
        Project {
            id: "p1".into(),
            name: "proj".into(),
            path: "/tmp/proj".into(),
            created_at: "now".into(),
            setup_commands: setup_json.into(),
            verification_commands: verification_json.into(),
            target_branch: "main".into(),
            auto_merge: false,
            sync_enabled: false,
            sync_remote: None,
        }
    }

    #[test]
    fn load_commands_uses_settings_file_when_present() {
        let dir = tempdir().unwrap();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(
            djinn_dir.join("settings.json"),
            r#"{
                "setup": [{"name": "build", "command": "cargo build", "timeout_secs": 300}],
                "verification": [{"name": "test", "command": "cargo test", "timeout_secs": 300}]
            }"#,
        )
        .unwrap();

        let project = project_with_db_commands("[]", "[]");
        let (setup, verification) = load_commands(dir.path(), &project);

        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0].name, "build");
        assert_eq!(verification.len(), 1);
        assert_eq!(verification[0].name, "test");
    }

    #[test]
    fn load_commands_falls_back_to_db_when_file_missing() {
        let dir = tempdir().unwrap();
        let project = project_with_db_commands(
            r#"[{"name":"db-setup","command":"echo setup","timeout_secs":10}]"#,
            r#"[{"name":"db-verify","command":"echo verify","timeout_secs":20}]"#,
        );

        let (setup, verification) = load_commands(dir.path(), &project);

        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0].name, "db-setup");
        assert_eq!(verification.len(), 1);
        assert_eq!(verification[0].name, "db-verify");
    }

    #[test]
    fn load_commands_falls_back_to_db_when_file_malformed() {
        let dir = tempdir().unwrap();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join("settings.json"), "{not valid json").unwrap();

        let project = project_with_db_commands(
            r#"[{"name":"db-setup","command":"echo setup","timeout_secs":10}]"#,
            r#"[{"name":"db-verify","command":"echo verify","timeout_secs":20}]"#,
        );

        let (setup, verification) = load_commands(dir.path(), &project);

        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0].name, "db-setup");
        assert_eq!(verification.len(), 1);
        assert_eq!(verification[0].name, "db-verify");
    }

    #[test]
    fn djinn_settings_defaults_missing_fields_to_empty() {
        let dir = tempdir().unwrap();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(
            djinn_dir.join("settings.json"),
            r#"{
                "setup": [{"name": "build", "command": "cargo build", "timeout_secs": 300}]
            }"#,
        )
        .unwrap();

        let project = project_with_db_commands("[]", "[]");
        let (setup, verification) = load_commands(dir.path(), &project);

        assert_eq!(setup.len(), 1);
        assert!(verification.is_empty());
    }
}
