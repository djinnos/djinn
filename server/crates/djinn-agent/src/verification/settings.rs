use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use djinn_core::commands::CommandSpec;

/// Configuration for a single named MCP server, as declared in `.djinn/settings.json`.
///
/// Either `url` (for HTTP/SSE transports) or `command` (for stdio transports) should be
/// provided. Both may be present; `url` takes precedence.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    /// HTTP or SSE endpoint URL for this MCP server (e.g. `http://localhost:9000/mcp`).
    pub url: Option<String>,
    /// Command to launch the MCP server over stdio (e.g. `my-mcp-server --flag`).
    pub command: Option<String>,
}

/// A single verification rule: a glob pattern matched against changed file paths,
/// and the commands to run when any changed file matches the pattern.
///
/// Example entry in `.djinn/settings.json`:
/// ```json
/// {"match": "crates/djinn-mcp/**", "commands": ["cargo test -p djinn-mcp", "cargo clippy -p djinn-mcp -- -D warnings"]}
/// ```
#[derive(Debug, Clone, Deserialize, Default)]
pub struct VerificationRule {
    /// Glob pattern matched against changed file paths (e.g. `"crates/djinn-mcp/**"`).
    #[serde(rename = "match")]
    pub pattern: String,
    /// Commands to run when a changed file matches `pattern`.
    pub commands: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DjinnSettings {
    #[serde(default)]
    pub setup: Vec<CommandSpec>,
    /// File-pattern-to-command rules for scoped verification.
    ///
    /// Each rule maps a glob pattern to the commands that should run when any
    /// changed file matches that pattern.
    #[serde(default)]
    pub verification_rules: Vec<VerificationRule>,
    /// Named MCP server registry for this project.
    ///
    /// Keys are server names (referenced by `agent_roles.mcp_servers`).
    /// Values describe how to connect to each server.
    ///
    /// Example:
    /// ```json
    /// {
    ///   "mcp_servers": {
    ///     "my-db-tool": { "url": "http://localhost:9000/mcp" },
    ///     "github": { "command": "github-mcp-server stdio" }
    ///   }
    /// }
    /// ```
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

/// Load the full project settings from `.djinn/settings.json` in the worktree.
///
/// Returns a default (empty) `DjinnSettings` when the file is absent. Errors on malformed JSON.
pub fn load_settings(worktree_path: &Path) -> Result<DjinnSettings, String> {
    let settings_path = worktree_path.join(".djinn/settings.json");

    match std::fs::read_to_string(&settings_path) {
        Ok(content) => match serde_json::from_str::<DjinnSettings>(&content) {
            Ok(settings) => {
                tracing::info!(path = %settings_path.display(), "Loaded .djinn/settings.json");
                Ok(settings)
            }
            Err(e) => Err(format!(
                "invalid .djinn/settings.json at {}: {e}",
                settings_path.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %settings_path.display(), "No .djinn/settings.json found; using defaults");
            Ok(DjinnSettings::default())
        }
        Err(e) => Err(format!(
            "failed to read .djinn/settings.json at {}: {e}",
            settings_path.display()
        )),
    }
}

/// Load setup commands from `.djinn/settings.json` in the worktree.
///
/// Returns an empty vec when the file is absent. Errors on malformed JSON.
pub fn load_setup_commands(worktree_path: &Path) -> Result<Vec<CommandSpec>, String> {
    load_settings(worktree_path).map(|s| s.setup)
}

/// Load the MCP server registry from `.djinn/settings.json` in the worktree.
///
/// Returns an empty map when the file is absent or when no `mcp_servers` key exists.
/// Logs a warning on parse failures but returns an empty map (non-fatal).
pub fn load_mcp_server_registry(worktree_path: &Path) -> HashMap<String, McpServerConfig> {
    let settings_path = worktree_path.join(".djinn/settings.json");
    match std::fs::read_to_string(&settings_path) {
        Ok(content) => match serde_json::from_str::<DjinnSettings>(&content) {
            Ok(settings) => settings.mcp_servers,
            Err(e) => {
                tracing::warn!(
                    path = %settings_path.display(),
                    error = %e,
                    "failed to parse .djinn/settings.json for MCP server registry; using empty"
                );
                HashMap::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            tracing::warn!(
                path = %settings_path.display(),
                error = %e,
                "failed to read .djinn/settings.json for MCP server registry; using empty"
            );
            HashMap::new()
        }
    }
}

/// Resolve a list of role-level MCP server names against the project's registry.
///
/// For each name in `role_mcp_servers`:
/// - If found in `registry`, include it in the result.
/// - If not found, log a warning and skip it (don't fail).
///
/// Returns the resolved `(name, config)` pairs. Empty when the role has no servers
/// (default roles) or when all names are unknown.
pub fn resolve_mcp_servers<'a>(
    task_short_id: &str,
    role_name: &str,
    role_mcp_servers: &[String],
    registry: &'a HashMap<String, McpServerConfig>,
) -> Vec<(String, &'a McpServerConfig)> {
    if role_mcp_servers.is_empty() {
        return Vec::new();
    }

    let mut resolved = Vec::new();
    for name in role_mcp_servers {
        match registry.get(name.as_str()) {
            Some(config) => {
                tracing::debug!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server_name = %name,
                    has_url = config.url.is_some(),
                    has_command = config.command.is_some(),
                    "Lifecycle: resolved MCP server config for role"
                );
                resolved.push((name.clone(), config));
            }
            None => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server_name = %name,
                    "Lifecycle: role references unknown MCP server name; skipping (not in .djinn/settings.json)"
                );
            }
        }
    }
    resolved
}

/// Compute the verification cache key from a commit SHA and the resolved scoped command set.
///
/// The key encodes both the commit identity and the exact commands run so that different
/// scoped command sets for the same commit produce different cache entries.
pub fn verification_cache_key(commit_sha: &str, scoped_commands: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(commit_sha.as_bytes());
    for cmd in scoped_commands {
        hasher.update(b"\x00");
        hasher.update(cmd.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("djinn-settings-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn write_settings(dir: &tempfile::TempDir, content: &str) {
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join("settings.json"), content).unwrap();
    }

    #[test]
    fn load_setup_commands_uses_settings_file_when_present() {
        let dir = tempdir_in_tmp();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(
            djinn_dir.join("settings.json"),
            r#"{
                "setup": [{"name": "build", "command": "cargo build", "timeout_secs": 300}]
            }"#,
        )
        .unwrap();

        let setup = load_setup_commands(dir.path()).expect("load commands");

        assert_eq!(setup.len(), 1);
        assert_eq!(setup[0].name, "build");
    }

    #[test]
    fn load_setup_commands_returns_empty_when_file_missing() {
        let dir = tempdir_in_tmp();

        let setup = load_setup_commands(dir.path()).expect("load commands");

        assert!(setup.is_empty());
    }

    #[test]
    fn load_setup_commands_errors_when_file_malformed() {
        let dir = tempdir_in_tmp();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join("settings.json"), "{not valid json").unwrap();

        let err = load_setup_commands(dir.path()).expect_err("malformed settings should error");
        assert!(err.contains("invalid .djinn/settings.json"));
    }

    #[test]
    fn djinn_settings_defaults_missing_fields_to_empty() {
        let dir = tempdir_in_tmp();
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(
            djinn_dir.join("settings.json"),
            r#"{
                "setup": [{"name": "build", "command": "cargo build", "timeout_secs": 300}]
            }"#,
        )
        .unwrap();

        let setup = load_setup_commands(dir.path()).expect("load commands");

        assert_eq!(setup.len(), 1);
    }

    // ── MCP server registry tests ─────────────────────────────────────────────

    #[test]
    fn load_mcp_server_registry_returns_empty_when_no_file() {
        let dir = tempdir_in_tmp();
        let registry = load_mcp_server_registry(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_mcp_server_registry_returns_empty_when_no_mcp_servers_key() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{"setup": [], "verification": []}"#,
        );
        let registry = load_mcp_server_registry(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_mcp_server_registry_parses_url_server() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "mcp_servers": {
                    "my-db-tool": {"url": "http://localhost:9000/mcp"}
                }
            }"#,
        );
        let registry = load_mcp_server_registry(dir.path());
        assert_eq!(registry.len(), 1);
        let cfg = registry.get("my-db-tool").expect("my-db-tool should be in registry");
        assert_eq!(cfg.url.as_deref(), Some("http://localhost:9000/mcp"));
        assert!(cfg.command.is_none());
    }

    #[test]
    fn load_mcp_server_registry_parses_command_server() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "mcp_servers": {
                    "github": {"command": "github-mcp-server stdio"}
                }
            }"#,
        );
        let registry = load_mcp_server_registry(dir.path());
        let cfg = registry.get("github").expect("github should be in registry");
        assert!(cfg.url.is_none());
        assert_eq!(cfg.command.as_deref(), Some("github-mcp-server stdio"));
    }

    #[test]
    fn load_mcp_server_registry_parses_multiple_servers() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "mcp_servers": {
                    "server-a": {"url": "http://localhost:9001/mcp"},
                    "server-b": {"command": "mcp-b --stdio"},
                    "server-c": {"url": "http://localhost:9003/mcp", "command": "mcp-c"}
                }
            }"#,
        );
        let registry = load_mcp_server_registry(dir.path());
        assert_eq!(registry.len(), 3);
        assert!(registry.contains_key("server-a"));
        assert!(registry.contains_key("server-b"));
        assert!(registry.contains_key("server-c"));
    }

    #[test]
    fn resolve_mcp_servers_returns_empty_for_empty_role_list() {
        let registry = HashMap::new();
        let resolved = resolve_mcp_servers("abc", "worker", &[], &registry);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_resolves_known_servers() {
        let mut registry = HashMap::new();
        registry.insert(
            "my-tool".to_string(),
            McpServerConfig {
                url: Some("http://localhost:9000/mcp".to_string()),
                command: None,
            },
        );

        let names = vec!["my-tool".to_string()];
        let resolved = resolve_mcp_servers("t1", "worker", &names, &registry);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "my-tool");
        assert_eq!(resolved[0].1.url.as_deref(), Some("http://localhost:9000/mcp"));
    }

    #[test]
    fn resolve_mcp_servers_skips_unknown_without_panic() {
        let registry: HashMap<String, McpServerConfig> = HashMap::new();

        let names = vec!["missing-server".to_string()];
        let resolved = resolve_mcp_servers("t1", "worker", &names, &registry);

        // Unknown name is skipped silently (warning logged, not an error).
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_merges_known_and_skips_unknown() {
        let mut registry = HashMap::new();
        registry.insert(
            "known-server".to_string(),
            McpServerConfig {
                url: Some("http://localhost:9000/mcp".to_string()),
                command: None,
            },
        );

        let names = vec!["known-server".to_string(), "unknown-server".to_string()];
        let resolved = resolve_mcp_servers("t2", "specialist", &names, &registry);

        // Only the known server is returned; unknown is silently skipped.
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "known-server");
    }

    #[test]
    fn resolve_mcp_servers_default_role_empty_list_no_resolution() {
        // Default roles have empty mcp_servers — verify this case is a no-op.
        let mut registry = HashMap::new();
        registry.insert(
            "some-server".to_string(),
            McpServerConfig {
                url: Some("http://localhost:9000/mcp".to_string()),
                command: None,
            },
        );

        // Empty role server list → nothing resolved even when registry is populated.
        let resolved = resolve_mcp_servers("t3", "worker", &[], &registry);
        assert!(resolved.is_empty());
    }

    // ── verification_rules tests ──────────────────────────────────────────────

    #[test]
    fn load_settings_parses_verification_rules() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "verification_rules": [
                    {"match": "crates/djinn-mcp/**", "commands": ["cargo test -p djinn-mcp"]},
                    {"match": "crates/djinn-core/**", "commands": ["cargo test -p djinn-core", "cargo clippy -p djinn-core -- -D warnings"]}
                ]
            }"#,
        );

        let settings = load_settings(dir.path()).expect("load settings");
        assert_eq!(settings.verification_rules.len(), 2);
        assert_eq!(settings.verification_rules[0].pattern, "crates/djinn-mcp/**");
        assert_eq!(settings.verification_rules[0].commands, vec!["cargo test -p djinn-mcp"]);
        assert_eq!(settings.verification_rules[1].pattern, "crates/djinn-core/**");
        assert_eq!(settings.verification_rules[1].commands.len(), 2);
    }

    #[test]
    fn load_settings_defaults_verification_rules_to_empty() {
        let dir = tempdir_in_tmp();
        write_settings(&dir, r#"{"setup": [], "verification": []}"#);

        let settings = load_settings(dir.path()).expect("load settings");
        assert!(settings.verification_rules.is_empty());
    }

    #[test]
    fn load_settings_returns_default_when_file_missing() {
        let dir = tempdir_in_tmp();
        let settings = load_settings(dir.path()).expect("load settings on missing file");
        assert!(settings.setup.is_empty());
        assert!(settings.verification_rules.is_empty());
        assert!(settings.mcp_servers.is_empty());
    }
}
