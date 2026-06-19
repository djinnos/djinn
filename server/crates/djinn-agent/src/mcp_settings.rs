//! MCP server configuration and resolution helpers.
//!
//! Extracted from the former `verification::settings` and `verification::mcp_json`
//! modules when the verification pre-PR gate was removed.  These helpers remain
//! because MCP server discovery and resolution are used by the per-session
//! lifecycle (MCP + skills resolution).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Deserialize;

/// Configuration for a single named MCP server, as discovered from
/// `mcp.json`-style files at the project root.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    pub url: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
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

/// Resolve the per-agent MCP server default list from the project's
/// `environment_config.agent_mcp_defaults` map.
pub fn default_mcp_servers_for_agent(
    agent_mcp_defaults: &BTreeMap<String, Vec<String>>,
    agent_name: &str,
) -> Vec<String> {
    agent_mcp_defaults
        .get(agent_name)
        .or_else(|| agent_mcp_defaults.get("*"))
        .cloned()
        .unwrap_or_default()
}

/// Compute the effective MCP server name list for a role.
pub fn effective_mcp_server_names(
    agent_mcp_defaults: &BTreeMap<String, Vec<String>>,
    agent_name: &str,
    role_mcp_servers: Option<&[String]>,
) -> Vec<String> {
    match role_mcp_servers {
        Some(names) => dedupe_names(names.iter().cloned()),
        None => dedupe_names(default_mcp_servers_for_agent(
            agent_mcp_defaults,
            agent_name,
        )),
    }
}

/// Compute the effective skill name list for a role: project-level
/// `global_skills` followed by role-specific skills, de-duplicated.
pub fn effective_skill_names(global_skills: &[String], role_skills: &[String]) -> Vec<String> {
    dedupe_names(
        global_skills
            .iter()
            .cloned()
            .chain(role_skills.iter().cloned()),
    )
}

// ─── MCP server registry (from mcp_json) ────────────────────────────────────

const DISCOVERY_PATHS: [&str; 3] = ["mcp.json", ".cursor/mcp.json", ".opencode/mcp.json"];

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct McpServerEntry {
    url: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

impl From<McpServerEntry> for McpServerConfig {
    fn from(entry: McpServerEntry) -> Self {
        Self {
            url: entry.url,
            command: entry.command,
            args: entry.args,
            env: entry.env,
            headers: entry.headers,
        }
    }
}

/// Load the MCP server registry from standard discovery files at the project
/// root. Discovery order is `mcp.json`, `.cursor/mcp.json`, `.opencode/mcp.json`;
/// duplicate server names resolve first-found-wins.
pub fn load_mcp_server_registry(worktree_path: &Path) -> HashMap<String, McpServerConfig> {
    let mut registry = HashMap::new();

    for relative_path in DISCOVERY_PATHS {
        let path = worktree_path.join(relative_path);
        let Some(config) = read_mcp_json_file(&path) else {
            continue;
        };

        for (name, entry) in config.mcp_servers {
            registry.entry(name).or_insert_with(|| entry.into());
        }
    }

    registry
}

fn read_mcp_json_file(path: &Path) -> Option<McpJsonConfig> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<McpJsonConfig>(&content) {
            Ok(config) => {
                tracing::debug!(path = %path.display(), "Loaded MCP discovery file");
                Some(config)
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Failed to parse MCP discovery file; skipping"
                );
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to read MCP discovery file; skipping"
            );
            None
        }
    }
}

/// Resolve a list of role-level MCP server names against the project's registry.
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
                    "Lifecycle: role references unknown MCP server name; skipping"
                );
            }
        }
    }
    resolved
}
