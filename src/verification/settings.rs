use std::path::Path;

use serde::Deserialize;

use djinn_core::commands::CommandSpec;

#[derive(Debug, Deserialize, Default)]
pub struct DjinnSettings {
    #[serde(default)]
    pub setup: Vec<CommandSpec>,
    #[serde(default)]
    pub verification: Vec<CommandSpec>,
}

/// Load commands from `.djinn/settings.json` in the worktree.
///
/// Returns empty vecs when the file is absent. Errors on malformed JSON.
pub fn load_commands(
    worktree_path: &Path,
) -> Result<(Vec<CommandSpec>, Vec<CommandSpec>), String> {
    let settings_path = worktree_path.join(".djinn/settings.json");

    match std::fs::read_to_string(&settings_path) {
        Ok(content) => match serde_json::from_str::<DjinnSettings>(&content) {
            Ok(settings) => {
                tracing::info!(path = %settings_path.display(), "Loaded commands from .djinn/settings.json");
                Ok((settings.setup, settings.verification))
            }
            Err(e) => Err(format!(
                "invalid .djinn/settings.json at {}: {e}",
                settings_path.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %settings_path.display(), "No .djinn/settings.json found; using empty commands");
            Ok((Vec::new(), Vec::new()))
        }
        Err(e) => Err(format!(
            "failed to read .djinn/settings.json at {}: {e}",
            settings_path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

        let (setup, verification) = load_commands(dir.path()).expect("load commands");

        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0].name, "build");
        assert_eq!(verification.len(), 1);
        assert_eq!(verification[0].name, "test");
    }

    #[test]
    fn load_commands_returns_empty_when_file_missing() {
        let dir = tempdir().unwrap();

        let (setup, verification) = load_commands(dir.path()).expect("load commands");

        assert!(setup.is_empty());
        assert!(verification.is_empty());
    }

    #[test]
    fn load_commands_errors_when_file_malformed() {
        let dir = tempdir().unwrap();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join("settings.json"), "{not valid json").unwrap();

        let err = load_commands(dir.path()).expect_err("malformed settings should error");
        assert!(err.contains("invalid .djinn/settings.json"));
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

        let (setup, verification) = load_commands(dir.path()).expect("load commands");

        assert_eq!(setup.len(), 1);
        assert!(verification.is_empty());
    }
}
