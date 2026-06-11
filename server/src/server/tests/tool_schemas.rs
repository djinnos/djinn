use insta::assert_json_snapshot;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::helpers::{canonicalize_json, mcp_jsonrpc};
use crate::server::AppState;
use crate::test_helpers;

#[tokio::test]
async fn all_tool_schemas_includes_cross_domain_tools() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();
    assert!(!tools.is_empty(), "all_tool_schemas should not be empty");

    let names = tools
        .iter()
        .filter_map(|v| v.get("name").and_then(serde_json::Value::as_str))
        .collect::<std::collections::HashSet<_>>();

    for required in [
        "task_list",
        "epic_list",
        "memory_search",
        "project_list",
        "provider_catalog",
        "session_list",
        "settings_get",
        "system_ping",
    ] {
        assert!(
            names.contains(required),
            "missing required tool schema: {required}"
        );
    }
}

#[tokio::test]
async fn all_tool_schemas_default_safety_annotations_fail_closed() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    for field in [
        "readOnly",
        "destructive",
        "idempotent",
        "openWorld",
        "concurrent_safe",
    ] {
        assert!(
            tools.iter().all(|tool| tool.get(field).is_some()),
            "server-wide all_tool_schemas omitted safety field {field}"
        );
    }

    let task_list = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("task_list"))
        .expect("task_list schema");
    assert_eq!(task_list["readOnly"], false);
    assert_eq!(task_list["destructive"], true);
    assert_eq!(task_list["idempotent"], false);
    assert_eq!(task_list["openWorld"], false);
    assert_eq!(task_list["concurrent_safe"], false);
}

#[tokio::test]
async fn chat_uses_router_derived_tool_schemas() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());

    let names = mcp
        .all_tool_schemas()
        .into_iter()
        .filter_map(|v| {
            v.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect::<std::collections::HashSet<_>>();

    assert!(names.contains("credential_set"));
    assert!(names.contains("project_list"));
    assert!(names.contains("execution_kill_task"));
}

/// The curated chat surface (`filter_chat_allowed_mcp_schemas`) is what the
/// chat completions handler hands to the provider. Every `object`-typed
/// (sub)schema in it must carry a `properties` field — OpenAI/Codex's strict
/// validator 400s on object schemas without one (the original bug that
/// motivated the allowlist). This guards the ADR-050 board-management writes
/// (`task_create`/`task_update`/`task_transition`/`task_comment_add`/
/// `task_claim`, `epic_create`/`epic_update`/`epic_close`/`epic_reopen`)
/// against silently shipping a non-compliant schema.
#[tokio::test]
async fn chat_allowed_mcp_schemas_are_strict_validator_safe() {
    fn object_schemas_missing_properties(path: &str, value: &Value, missing: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                // An object-typed subschema that is not a pure reference
                // (`$ref`/`anyOf`/`oneOf`/`allOf` describe shape elsewhere)
                // must declare `properties`.
                let is_object = matches!(map.get("type"), Some(Value::String(t)) if t == "object");
                let defers_elsewhere = map.contains_key("$ref")
                    || map.contains_key("anyOf")
                    || map.contains_key("oneOf")
                    || map.contains_key("allOf");
                if is_object && !defers_elsewhere && !map.contains_key("properties") {
                    missing.push(format!("{path} is type=object without `properties`"));
                }
                for (k, v) in map {
                    object_schemas_missing_properties(&format!("{path}/{k}"), v, missing);
                }
            }
            Value::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    object_schemas_missing_properties(&format!("{path}[{idx}]"), item, missing);
                }
            }
            _ => {}
        }
    }

    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let chat_schemas =
        djinn_agent::chat_tools::filter_chat_allowed_mcp_schemas(mcp.all_tool_schemas());

    let names: std::collections::HashSet<String> = chat_schemas
        .iter()
        .filter_map(|v| {
            v.get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    for required in [
        "task_create",
        "task_update",
        "task_transition",
        "task_comment_add",
        "task_claim",
        "epic_create",
        "epic_update",
        "epic_close",
        "epic_reopen",
        // ADR-050 §2 amendment 2026-05-26 — chat curates the code-graph
        // noise filter. Pinned here so the pair also gets strict-validator
        // coverage (their `Option<Vec<String>>` params must not regress
        // into an object-without-properties schema).
        "project_graph_exclusions_get",
        "project_graph_exclusions_set",
    ] {
        assert!(
            names.contains(required),
            "chat board-management write `{required}` missing from filtered chat surface"
        );
    }

    let mut missing = Vec::new();
    for schema in &chat_schemas {
        let name = schema
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        if let Some(input) = schema.get("inputSchema") {
            object_schemas_missing_properties(&format!("{name}.inputSchema"), input, &mut missing);
        }
    }
    assert!(
        missing.is_empty(),
        "chat tool schemas with object params lacking `properties` (would 400 the strict validator):\n  {}",
        missing.join("\n  ")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tools_list_schemas_do_not_use_nonstandard_uint_or_nullable_without_type() {
    fn collect_bad_formats(
        tool_name: &str,
        schema_kind: &str,
        path: &str,
        value: &Value,
        bad: &mut Vec<String>,
        bad_nullable: &mut Vec<String>,
    ) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(format)) = map.get("format")
                    && (format == "uint" || format.starts_with("uint"))
                {
                    bad.push(format!(
                        "{tool_name} {schema_kind} {path}/format = {format}"
                    ));
                }

                if matches!(map.get("nullable"), Some(Value::Bool(true)))
                    && !matches!(map.get("type"), Some(Value::String(_)))
                    && !matches!(map.get("const"), Some(Value::Null))
                {
                    bad_nullable.push(format!(
                        "{tool_name} {schema_kind} {path} has nullable=true without a type or const=null"
                    ));
                }

                for (k, v) in map {
                    let next_path = format!("{path}/{k}");
                    collect_bad_formats(tool_name, schema_kind, &next_path, v, bad, bad_nullable);
                }
            }
            Value::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    let next_path = format!("{path}[{idx}]");
                    collect_bad_formats(
                        tool_name,
                        schema_kind,
                        &next_path,
                        item,
                        bad,
                        bad_nullable,
                    );
                }
            }
            _ => {}
        }
    }

    let app = test_helpers::create_test_app();
    let session_id = test_helpers::initialize_mcp_session(&app).await;
    let list_event = mcp_jsonrpc(&app, &session_id, 2, "tools/list", serde_json::json!({})).await;
    let result = list_event.get("result").expect("tools/list result missing");

    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools/list result missing tools array");

    let mut bad_formats = Vec::new();
    let mut bad_nullable = Vec::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");

        for (schema_kind, key) in &[("input", "inputSchema"), ("output", "outputSchema")] {
            if let Some(schema) = tool.get(*key) {
                collect_bad_formats(
                    name,
                    schema_kind,
                    "$",
                    schema,
                    &mut bad_formats,
                    &mut bad_nullable,
                );
            }
        }
    }

    assert!(
        bad_formats.is_empty(),
        "Found nonstandard uint schema formats (prefer i64-compatible fields):\n  {}",
        bad_formats.join("\n  ")
    );

    assert!(
        bad_nullable.is_empty(),
        "Found nullable schema branches without explicit type (breaks strict clients):\n  {}",
        bad_nullable.join("\n  ")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_config_tool_schemas_expose_workspace_metadata() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let schema_surfaces = [
        ("project_environment_config_get", "outputSchema"),
        ("project_environment_config_reset", "outputSchema"),
        ("project_environment_config_set", "inputSchema"),
    ];

    for (tool_name, schema_key) in schema_surfaces {
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            .unwrap_or_else(|| panic!("missing {tool_name} schema"));
        let schema = tool
            .get(schema_key)
            .unwrap_or_else(|| panic!("{tool_name} missing {schema_key}"));
        let environment_config = &schema["$defs"]["EnvironmentConfig"];
        assert_eq!(
            environment_config["properties"]["workspaces"]["items"]["$ref"],
            json!("#/$defs/Workspace"),
            "{tool_name} {schema_key} EnvironmentConfig.workspaces no longer exposes Workspace"
        );

        let workspace = &schema["$defs"]["Workspace"];

        assert_eq!(
            workspace["properties"]["slug"],
            json!({"default": null, "nullable": true, "type": "string"}),
            "{tool_name} {schema_key} Workspace.slug schema drifted"
        );
        assert_eq!(
            workspace["properties"]["name"],
            json!({"default": null, "nullable": true, "type": "string"}),
            "{tool_name} {schema_key} Workspace.name schema drifted"
        );
        assert_eq!(
            workspace["properties"]["tags"],
            json!({"default": [], "items": {"type": "string"}, "type": "array"}),
            "{tool_name} {schema_key} Workspace.tags schema drifted"
        );

        let rendered = serde_json::to_string(schema).expect("schema serializes");
        assert!(
            !rendered.to_ascii_lowercase().contains("duplicate slug"),
            "{tool_name} {schema_key} still advertises duplicate-slug rejection"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tools_schema_snapshot() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let mut signatures: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool["name"],
                    "read_only": tool["readOnly"],
                    "destructive": tool["destructive"],
                    "idempotent": tool["idempotent"],
                    "open_world": tool["openWorld"],
                    "concurrent_safe": tool["concurrent_safe"],
                    "input_schema": canonicalize_json(tool.get("inputSchema").unwrap_or(&Value::Null)),
                    "output_schema": canonicalize_json(tool.get("outputSchema").unwrap_or(&Value::Null)),
                })
            })
            .collect();
    signatures.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    assert_json_snapshot!("mcp_tools_schema", signatures);
}
