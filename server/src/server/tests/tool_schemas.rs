use insta::assert_json_snapshot;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::helpers::{canonicalize_json, mcp_jsonrpc};
use crate::server::AppState;
use crate::test_helpers;

const ENVIRONMENT_CONFIG_SCHEMA_SURFACES: [(&str, &str, &str); 6] = [
    ("image_create", "inputSchema", "input_schema"),
    ("image_list", "outputSchema", "output_schema"),
    ("image_update", "inputSchema", "input_schema"),
    (
        "project_environment_config_get",
        "outputSchema",
        "output_schema",
    ),
    (
        "project_environment_config_reset",
        "outputSchema",
        "output_schema",
    ),
    (
        "project_environment_config_set",
        "inputSchema",
        "input_schema",
    ),
];

fn assert_environment_config_workspace_metadata_schema(
    schema: &Value,
    context: impl std::fmt::Display,
) {
    let environment_config = &schema["$defs"]["EnvironmentConfig"];
    assert_eq!(
        environment_config["properties"]["workspaces"]["items"]["$ref"],
        json!("#/$defs/Workspace"),
        "{context} EnvironmentConfig.workspaces no longer exposes Workspace"
    );

    let workspace = &schema["$defs"]["Workspace"];
    assert_eq!(
        workspace["properties"]["slug"],
        json!({"default": null, "nullable": true, "type": "string"}),
        "{context} Workspace.slug schema drifted"
    );
    assert_eq!(
        workspace["properties"]["name"],
        json!({"default": null, "nullable": true, "type": "string"}),
        "{context} Workspace.name schema drifted"
    );
    assert_eq!(
        workspace["properties"]["tags"],
        json!({"default": [], "items": {"type": "string"}, "type": "array"}),
        "{context} Workspace.tags schema drifted"
    );

    let rendered = serde_json::to_string(schema).expect("schema serializes");
    assert!(
        !rendered.to_ascii_lowercase().contains("duplicate slug"),
        "{context} still advertises duplicate-slug rejection"
    );
}

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

#[tokio::test]
async fn code_graph_schema_advertises_route_shape_impact_flow_ops() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();
    let code_graph = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("code_graph"))
        .expect("code_graph schema");
    let description = code_graph
        .get("description")
        .and_then(Value::as_str)
        .expect("code_graph description");

    for op in ["route_map", "shape_check", "api_impact", "flow"] {
        assert!(
            description.contains(op),
            "code_graph schema description does not advertise {op}"
        );
    }
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

    for (tool_name, live_schema_key, _) in ENVIRONMENT_CONFIG_SCHEMA_SURFACES {
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            .unwrap_or_else(|| panic!("missing {tool_name} schema"));
        let schema = tool
            .get(live_schema_key)
            .unwrap_or_else(|| panic!("{tool_name} missing {live_schema_key}"));
        assert_environment_config_workspace_metadata_schema(
            schema,
            format_args!("{tool_name} {live_schema_key}"),
        );
    }
}

#[test]
fn checked_in_mcp_snapshot_exposes_environment_config_workspace_metadata() {
    let snapshot =
        include_str!("snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap");
    let json_body = snapshot
        .splitn(3, "---\n")
        .nth(2)
        .expect("insta snapshot body after metadata header");
    let tools: Vec<Value> = serde_json::from_str(json_body).expect("MCP schema snapshot is JSON");

    assert!(
        !json_body.to_ascii_lowercase().contains("duplicate slug"),
        "checked-in MCP tool schema snapshot still advertises duplicate-slug rejection"
    );

    for (tool_name, _, snapshot_schema_key) in ENVIRONMENT_CONFIG_SCHEMA_SURFACES {
        let tool = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
            .unwrap_or_else(|| panic!("checked-in snapshot is missing {tool_name}"));
        let schema = tool.get(snapshot_schema_key).unwrap_or_else(|| {
            panic!("checked-in snapshot {tool_name} missing {snapshot_schema_key}")
        });
        assert_environment_config_workspace_metadata_schema(
            schema,
            format_args!("checked-in snapshot {tool_name} {snapshot_schema_key}"),
        );
    }

    let mut workspace_schema_count = 0;
    for tool in &tools {
        let tool_name = tool
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        for schema_key in ["input_schema", "output_schema"] {
            let Some(workspace) = tool
                .get(schema_key)
                .and_then(|schema| schema.get("$defs"))
                .and_then(|defs| defs.get("Workspace"))
            else {
                continue;
            };

            workspace_schema_count += 1;
            let properties = &workspace["properties"];
            for field in ["slug", "name", "tags"] {
                assert!(
                    properties.get(field).is_some(),
                    "checked-in snapshot {tool_name} {schema_key} Workspace missing {field}"
                );
            }
        }
    }
    assert!(
        workspace_schema_count >= ENVIRONMENT_CONFIG_SCHEMA_SURFACES.len(),
        "checked-in snapshot should include EnvironmentConfig Workspace schemas"
    );
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
