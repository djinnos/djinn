use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::settings::McpServerConfig;

const DISCOVERY_PATHS: [&str; 3] = ["mcp.json", ".cursor/mcp.json", ".opencode/mcp.json"];
const ROOT_MCP_JSON_PATH: &str = "mcp.json";
const LEGACY_SETTINGS_PATH: &str = ".djinn/settings.json";

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
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
    migrate_legacy_settings_mcp_servers(worktree_path);
    load_discovered_mcp_server_registry(worktree_path)
}

fn load_discovered_mcp_server_registry(worktree_path: &Path) -> HashMap<String, McpServerConfig> {
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

fn migrate_legacy_settings_mcp_servers(worktree_path: &Path) {
    let settings_path = worktree_path.join(LEGACY_SETTINGS_PATH);
    let Some(legacy_servers) = read_legacy_settings_registry(&settings_path) else {
        return;
    };
    if legacy_servers.is_empty() {
        return;
    }

    let discovered_registry = load_discovered_mcp_server_registry(worktree_path);
    let root_path = worktree_path.join(ROOT_MCP_JSON_PATH);
    let mut root_document = match read_or_initialize_root_document(&root_path) {
        Some(document) => document,
        None => return,
    };

    let Some(root_object) = root_document.as_object_mut() else {
        tracing::warn!(
            path = %root_path.display(),
            "Root mcp.json is not a JSON object; skipping legacy MCP migration"
        );
        return;
    };

    let mcp_servers_value = root_object
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(root_servers) = mcp_servers_value.as_object_mut() else {
        tracing::warn!(
            path = %root_path.display(),
            "Root mcp.json has a non-object mcpServers field; skipping legacy MCP migration"
        );
        return;
    };

    let mut migrated_count = 0usize;
    for (name, entry) in legacy_servers {
        if discovered_registry.contains_key(&name) || root_servers.contains_key(&name) {
            continue;
        }

        let Ok(entry_value) = serde_json::to_value(entry) else {
            tracing::warn!(server_name = %name, "Failed to serialize legacy MCP server entry; skipping");
            continue;
        };
        root_servers.insert(name, entry_value);
        migrated_count += 1;
    }

    if migrated_count == 0 {
        return;
    }

    let serialized = match serde_json::to_string_pretty(&root_document) {
        Ok(serialized) => serialized,
        Err(error) => {
            tracing::warn!(
                path = %root_path.display(),
                error = %error,
                "Failed to serialize migrated mcp.json; skipping write"
            );
            return;
        }
    };

    if let Err(error) = std::fs::write(&root_path, format!("{serialized}\n")) {
        tracing::warn!(
            path = %root_path.display(),
            error = %error,
            "Failed to write migrated root mcp.json"
        );
        return;
    }

    tracing::info!(
        path = %root_path.display(),
        migrated_count,
        "Migrated legacy MCP servers from .djinn/settings.json into root mcp.json"
    );
}

#[derive(Debug, Deserialize, Default)]
struct LegacyDjinnSettings {
    #[serde(default)]
    mcp_servers: HashMap<String, McpServerEntry>,
}

fn read_legacy_settings_registry(path: &Path) -> Option<HashMap<String, McpServerEntry>> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<LegacyDjinnSettings>(&content) {
            Ok(settings) => Some(settings.mcp_servers),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Failed to parse legacy .djinn/settings.json for MCP migration; skipping"
                );
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to read legacy .djinn/settings.json for MCP migration; skipping"
            );
            None
        }
    }
}

fn read_or_initialize_root_document(path: &Path) -> Option<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(document) => Some(document),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "Failed to parse root mcp.json during legacy migration; leaving file unchanged"
                );
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(serde_json::json!({ "mcpServers": {} }))
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "Failed to read root mcp.json during legacy migration; skipping"
            );
            None
        }
    }
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
    fn ignores_legacy_djinn_settings_registry_when_migration_cannot_write() {
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
        write_file(&dir, "mcp.json", "[");

        let registry = load_mcp_server_registry(dir.path());

        assert!(registry.is_empty());
    }

    #[test]
    fn migrates_legacy_settings_registry_into_root_mcp_json() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            ".djinn/settings.json",
            r#"{
                "mcp_servers": {
                    "legacy": {
                        "url": "https://legacy.example/mcp",
                        "headers": {"Authorization": "Bearer legacy-token"}
                    }
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(
            registry["legacy"].url.as_deref(),
            Some("https://legacy.example/mcp")
        );
        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        let root_json: serde_json::Value =
            serde_json::from_str(&root).expect("valid root mcp.json");
        assert_eq!(
            root_json["mcpServers"]["legacy"]["headers"]["Authorization"],
            "Bearer legacy-token"
        );
    }

    #[test]
    fn migration_preserves_first_found_wins_across_discovery_sources() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            ".cursor/mcp.json",
            r#"{
                "mcpServers": {
                    "shared": {"url": "https://cursor.example/mcp"}
                }
            }"#,
        );
        write_file(
            &dir,
            ".djinn/settings.json",
            r#"{
                "mcp_servers": {
                    "shared": {"url": "https://legacy.example/shared"},
                    "legacy-only": {"url": "https://legacy.example/only"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(
            registry["shared"].url.as_deref(),
            Some("https://cursor.example/mcp")
        );
        assert_eq!(
            registry["legacy-only"].url.as_deref(),
            Some("https://legacy.example/only")
        );

        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        let root_json: serde_json::Value =
            serde_json::from_str(&root).expect("valid root mcp.json");
        assert!(root_json["mcpServers"].get("shared").is_none());
        assert_eq!(
            root_json["mcpServers"]["legacy-only"]["url"],
            "https://legacy.example/only"
        );
    }

    #[test]
    fn migration_preserves_existing_root_fields_and_servers() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{
                "projectName": "demo-project",
                "mcpServers": {
                    "root-existing": {"url": "https://root.example/mcp"}
                }
            }"#,
        );
        write_file(
            &dir,
            ".djinn/settings.json",
            r#"{
                "mcp_servers": {
                    "root-existing": {"url": "https://legacy.example/should-not-win"},
                    "legacy-added": {"url": "https://legacy.example/added"}
                }
            }"#,
        );

        let registry = load_mcp_server_registry(dir.path());

        assert_eq!(
            registry["root-existing"].url.as_deref(),
            Some("https://root.example/mcp")
        );
        assert_eq!(
            registry["legacy-added"].url.as_deref(),
            Some("https://legacy.example/added")
        );

        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        let root_json: serde_json::Value =
            serde_json::from_str(&root).expect("valid root mcp.json");
        assert_eq!(root_json["projectName"], "demo-project");
        assert_eq!(
            root_json["mcpServers"]["root-existing"]["url"],
            "https://root.example/mcp"
        );
        assert_eq!(
            root_json["mcpServers"]["legacy-added"]["url"],
            "https://legacy.example/added"
        );
    }

    #[test]
    fn migration_skips_non_object_root_mcp_servers_field() {
        let dir = tempdir_in_tmp();
        write_file(
            &dir,
            "mcp.json",
            r#"{
                "projectName": "demo-project",
                "mcpServers": []
            }"#,
        );
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
        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        let root_json: serde_json::Value =
            serde_json::from_str(&root).expect("valid root mcp.json");
        assert_eq!(root_json["projectName"], "demo-project");
        assert_eq!(root_json["mcpServers"], serde_json::json!([]));
    }

    #[test]
    fn migration_skips_non_object_root_document_without_clobbering_file() {
        let dir = tempdir_in_tmp();
        write_file(&dir, "mcp.json", "[]");
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
        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        assert_eq!(root, "[]");
    }

    #[test]
    fn migration_skips_malformed_root_mcp_json_without_clobbering_file() {
        let dir = tempdir_in_tmp();
        write_file(&dir, "mcp.json", "{not valid json");
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
        let root = std::fs::read_to_string(dir.path().join("mcp.json")).expect("root mcp.json");
        assert_eq!(root, "{not valid json");
    }

    #[test]
    fn migration_does_nothing_when_settings_file_missing() {
        let dir = tempdir_in_tmp();

        let registry = load_mcp_server_registry(dir.path());

        assert!(registry.is_empty());
        assert!(!dir.path().join("mcp.json").exists());
    }

    #[test]
    fn migration_skips_malformed_settings_file() {
        let dir = tempdir_in_tmp();
        write_file(&dir, ".djinn/settings.json", "{not valid json");

        let registry = load_mcp_server_registry(dir.path());

        assert!(registry.is_empty());
        assert!(!dir.path().join("mcp.json").exists());
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
