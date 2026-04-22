use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Configuration for a single named MCP server, as discovered from `mcp.json`-style files
/// or migrated from legacy `.djinn/settings.json` entries.
///
/// Either `url` (for HTTP/SSE transports) or `command` (for stdio transports) should be
/// provided. Both may be present; `url` takes precedence.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    /// HTTP or SSE endpoint URL for this MCP server (e.g. `http://localhost:9000/mcp`).
    pub url: Option<String>,
    /// Command to launch the MCP server over stdio (e.g. `my-mcp-server --flag`).
    pub command: Option<String>,
    /// Positional arguments for `command` when using stdio transport.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to provide to the launched MCP server.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// HTTP headers to attach when connecting to a remote MCP server.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// The non-verification subset of `.djinn/settings.json` that the agent still
/// reads from the worktree.
///
/// The verification-related fields — `setup`, `verification_rules`,
/// `verification_timeout_secs` — were moved to Dolt's
/// `projects.environment_config.verification` column as part of the P8 cut-
/// over. What remains here is:
///
/// * `agent_mcp_defaults` — per-role MCP server defaults.
/// * `global_skills` — skills injected into every agent prompt.
///
/// Both are still file-based pending a follow-up migration; the
/// environment-config schema does not model them yet. When that lands, this
/// whole struct goes away.
#[derive(Debug, Deserialize, Default)]
pub struct DjinnSettings {
    #[serde(default)]
    pub agent_mcp_defaults: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub global_skills: Vec<String>,
}

fn dedupe_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::new();
    for name in names {
        if seen.insert(name.clone()) {
            deduped.push(name);
        }
    }
    deduped
}

pub fn default_mcp_servers_for_agent(settings: &DjinnSettings, agent_name: &str) -> Vec<String> {
    settings
        .agent_mcp_defaults
        .get(agent_name)
        .or_else(|| settings.agent_mcp_defaults.get("*"))
        .cloned()
        .unwrap_or_default()
}

pub fn effective_mcp_server_names(
    settings: &DjinnSettings,
    agent_name: &str,
    role_mcp_servers: Option<&[String]>,
) -> Vec<String> {
    match role_mcp_servers {
        Some(names) => dedupe_names(names.iter().cloned()),
        None => dedupe_names(default_mcp_servers_for_agent(settings, agent_name)),
    }
}

pub fn effective_skill_names(settings: &DjinnSettings, role_skills: &[String]) -> Vec<String> {
    dedupe_names(
        settings
            .global_skills
            .iter()
            .cloned()
            .chain(role_skills.iter().cloned()),
    )
}

/// Load the non-verification project settings from `.djinn/settings.json` in
/// the worktree.
///
/// Returns a default (empty) `DjinnSettings` when the file is absent. Errors
/// on malformed JSON. Verification-specific fields (setup commands, glob
/// rules, pipeline timeout) are *not* parsed here — those live in Dolt's
/// `environment_config.verification` column and are fetched via
/// [`crate::verification::environment`].
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

/// Load the MCP server registry from standard discovery files in the worktree.
///
/// Discovery order is `mcp.json`, `.cursor/mcp.json`, `.opencode/mcp.json`, with
/// first-found-wins precedence by server name. Invalid files are logged and skipped.
/// Legacy `.djinn/settings.json` `mcp_servers` entries are migrated into root `mcp.json`
/// during registry load, but session resolution no longer reads the settings-based registry
/// directly.
pub fn load_mcp_server_registry(worktree_path: &Path) -> HashMap<String, McpServerConfig> {
    crate::verification::mcp_json::load_mcp_server_registry(worktree_path)
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
                    "Lifecycle: role references unknown MCP server name; skipping (not in discovered mcp.json registry)"
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
        crate::test_helpers::test_tempdir("djinn-settings-")
    }

    fn write_settings(dir: &tempfile::TempDir, content: &str) {
        let djinn_dir = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn_dir).unwrap();
        std::fs::write(djinn_dir.join("settings.json"), content).unwrap();
    }

    #[test]
    fn load_settings_parses_agent_defaults_and_global_skills() {
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "agent_mcp_defaults": {
                    "*": ["web", "filesystem"],
                    "worker": ["worker-only"],
                    "chat": ["chat-web"]
                },
                "global_skills": ["git", "rust"]
            }"#,
        );

        let settings = load_settings(dir.path()).expect("settings load");

        assert_eq!(settings.agent_mcp_defaults["*"], vec!["web", "filesystem"]);
        assert_eq!(settings.agent_mcp_defaults["worker"], vec!["worker-only"]);
        assert_eq!(settings.agent_mcp_defaults["chat"], vec!["chat-web"]);
        assert_eq!(settings.global_skills, vec!["git", "rust"]);
    }

    #[test]
    fn load_settings_ignores_unknown_fields_post_cutover() {
        // Verification-related fields from the pre-cut-over schema are
        // silently ignored (serde(deny_unknown_fields) is *not* set, so they
        // deserialize as a no-op). This keeps old settings.json files from
        // breaking the boot path while the verification config is owned by
        // Dolt's environment_config.verification block.
        let dir = tempdir_in_tmp();
        write_settings(
            &dir,
            r#"{
                "setup": [{"name": "legacy", "command": "true", "timeout_secs": 1}],
                "verification_rules": [{"match": "**", "commands": ["true"]}],
                "verification_timeout_secs": 999,
                "global_skills": ["git"]
            }"#,
        );

        let settings = load_settings(dir.path()).expect("settings load");
        assert_eq!(settings.global_skills, vec!["git"]);
    }

    #[test]
    fn effective_mcp_server_names_prefers_named_default_then_wildcard() {
        let settings = DjinnSettings {
            agent_mcp_defaults: HashMap::from([
                ("*".to_string(), vec!["web".to_string()]),
                (
                    "worker".to_string(),
                    vec!["worker-web".to_string(), "worker-web".to_string()],
                ),
                ("chat".to_string(), vec!["chat-web".to_string()]),
            ]),
            ..Default::default()
        };

        assert_eq!(
            effective_mcp_server_names(&settings, "worker", None),
            vec!["worker-web"]
        );
        assert_eq!(
            effective_mcp_server_names(&settings, "reviewer", None),
            vec!["web"]
        );
        assert_eq!(
            effective_mcp_server_names(&settings, "chat", None),
            vec!["chat-web"]
        );
    }

    #[test]
    fn effective_mcp_server_names_role_assignment_overrides_defaults_and_empty_opts_out() {
        let settings = DjinnSettings {
            agent_mcp_defaults: HashMap::from([(
                "*".to_string(),
                vec!["web".to_string(), "filesystem".to_string()],
            )]),
            ..Default::default()
        };

        let assigned = vec!["special".to_string(), "special".to_string()];
        let opt_out: Vec<String> = Vec::new();

        assert_eq!(
            effective_mcp_server_names(&settings, "worker", Some(&assigned)),
            vec!["special"]
        );
        assert!(effective_mcp_server_names(&settings, "worker", Some(&opt_out)).is_empty());
    }

    #[test]
    fn effective_skill_names_adds_global_skills_and_dedupes() {
        let settings = DjinnSettings {
            global_skills: vec!["git".to_string(), "rust".to_string(), "git".to_string()],
            ..Default::default()
        };

        let effective =
            effective_skill_names(&settings, &["rust".to_string(), "testing".to_string()]);

        assert_eq!(effective, vec!["git", "rust", "testing"]);
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
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
            },
        );

        let names = vec!["my-tool".to_string()];
        let resolved = resolve_mcp_servers("t1", "worker", &names, &registry);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "my-tool");
        assert_eq!(
            resolved[0].1.url.as_deref(),
            Some("http://localhost:9000/mcp")
        );
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
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
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
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
            },
        );

        // Empty role server list → nothing resolved even when registry is populated.
        let resolved = resolve_mcp_servers("t3", "worker", &[], &registry);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_mcp_servers_preserves_http_headers_env_and_args() {
        let mut registry = HashMap::new();
        registry.insert(
            "remote-server".to_string(),
            McpServerConfig {
                url: Some("https://example.com/mcp".to_string()),
                command: Some("ignored-when-http-present".to_string()),
                args: vec!["--flag".to_string(), "value".to_string()],
                env: HashMap::from([(
                    "API_TOKEN".to_string(),
                    "${DJINN_REMOTE_TOKEN}".to_string(),
                )]),
                headers: HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer ${DJINN_REMOTE_TOKEN}".to_string(),
                )]),
            },
        );

        let names = vec!["remote-server".to_string()];
        let resolved = resolve_mcp_servers("t4", "worker", &names, &registry);

        assert_eq!(resolved.len(), 1);
        let config = resolved[0].1;
        assert_eq!(config.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(config.command.as_deref(), Some("ignored-when-http-present"));
        assert_eq!(config.args, vec!["--flag", "value"]);
        assert_eq!(
            config.env.get("API_TOKEN").map(String::as_str),
            Some("${DJINN_REMOTE_TOKEN}")
        );
        assert_eq!(
            config.headers.get("Authorization").map(String::as_str),
            Some("Bearer ${DJINN_REMOTE_TOKEN}")
        );
    }
}
