use insta::assert_json_snapshot;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::helpers::{canonicalize_json, mcp_jsonrpc};
use crate::server::AppState;
use crate::test_helpers;

#[tokio::test]
async fn all_tool_schemas_includes_cross_domain_tools() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_mcp::server::DjinnMcpServer::new(state.mcp_state());
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
async fn chat_uses_router_derived_tool_schemas() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_mcp::server::DjinnMcpServer::new(state.mcp_state());

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
    assert!(names.contains("task_sync_enable"));
    assert!(names.contains("project_list"));
    assert!(names.contains("execution_start"));
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
                {
                    bad_nullable.push(format!(
                        "{tool_name} {schema_kind} {path} has nullable=true without a type"
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
async fn mcp_tools_schema_snapshot() {
    let app = test_helpers::create_test_app();
    let session_id = test_helpers::initialize_mcp_session(&app).await;
    let list_event = mcp_jsonrpc(&app, &session_id, 2, "tools/list", json!({})).await;
    let tools = list_event["result"]["tools"]
        .as_array()
        .expect("tools array");

    let mut signatures: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool["name"],
                    "input_schema": canonicalize_json(tool.get("inputSchema").unwrap_or(&Value::Null)),
                    "output_schema": canonicalize_json(tool.get("outputSchema").unwrap_or(&Value::Null)),
                })
            })
            .collect();
    signatures.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    assert_json_snapshot!("mcp_tools_schema", signatures);
}
