use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use super::settings::McpServerConfig;

const DISCOVERY_PATHS: [&str; 3] = ["mcp.json", ".cursor/mcp.json", ".opencode/mcp.json"];

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize, Default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-mcp-json-")
    }

    fn write_file(dir: &tempfile::TempDir, relative_path: &str, content: &str) {
        let path = dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn loads_root_mcp_json() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{
                "mcpServers": {
                    "root-server": {"url": "https://example.com/mcp"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        let server = registry.get("root-server").expect("server in registry");
        assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
        assert!(server.command.is_none());
    }

    #[test]
    fn falls_back_to_cursor_mcp_json() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            ".cursor/mcp.json",
            r#"{
                "mcpServers": {
                    "cursor-server": {"url": "https://cursor.example/mcp"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry["cursor-server"].url.as_deref(),
            Some("https://cursor.example/mcp")
        );
    }

    #[test]
    fn falls_back_to_opencode_mcp_json() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            ".opencode/mcp.json",
            r#"{
                "mcpServers": {
                    "opencode-server": {"command": "npx"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(registry.len(), 1);
        assert_eq!(registry["opencode-server"].command.as_deref(), Some("npx"));
    }

    #[test]
    fn earlier_discovery_paths_win_on_duplicate_server_names() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{"mcpServers": {"shared": {"url": "https://root.example/mcp"}}}"#,
        );
        write_file(
            &dir,
            ".cursor/mcp.json",
            r#"{"mcpServers": {"shared": {"url": "https://cursor.example/mcp"}, "cursor-only": {"url": "https://cursor-only.example/mcp"}}}"#,
        );
        write_file(
            &dir,
            ".opencode/mcp.json",
            r#"{"mcpServers": {"shared": {"url": "https://opencode.example/mcp"}, "opencode-only": {"url": "https://opencode-only.example/mcp"}}}"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(
            registry["shared"].url.as_deref(),
            Some("https://root.example/mcp")
        );
        assert!(registry.contains_key("cursor-only"));
        assert!(registry.contains_key("opencode-only"));
    }

    #[test]
    fn parses_standard_fields() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{
                "mcpServers": {
                    "full": {
                        "url": "https://example.com/mcp",
                        "command": "npx",
                        "args": ["-y", "example-server"],
                        "env": {"API_KEY": "secret"},
                        "headers": {"Authorization": "Bearer token"}
                    }
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());
        let server = registry.get("full").expect("server in registry");

        assert_eq!(server.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y", "example-server"]);
        assert_eq!(
            server.env.get("API_KEY").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            server.headers.get("Authorization").map(String::as_str),
            Some("Bearer token")
        );
    }

    #[test]
    fn skips_invalid_file_and_continues_to_fallbacks() {
        let dir = tempdir_in_tmp();
        write_file(&dir, "mcp.json", "{not valid json");
        write_file(
            &dir,
            ".cursor/mcp.json",
            r#"{"mcpServers": {"cursor": {"url": "https://cursor.example/mcp"}}}"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry["cursor"].url.as_deref(),
            Some("https://cursor.example/mcp")
        );
    }

    #[test]
    fn ignores_legacy_djinn_settings_registry() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            ".djinn/settings.json",
            r#"{
                "mcp_servers": {
                    "legacy": {"url": "https://legacy.example/mcp"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert!(registry.is_empty());
    }

    #[test]
    fn reads_single_discovery_file_directly() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{"mcpServers": {"direct": {"url": "https://direct.example/mcp"}}}"#,
        );

        let config = read_mcp_json_file(&dir.path().join("mcp.json")).expect("config loads");

        assert!(config.mcp_servers.contains_key("direct"));
    }
}
