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
        json!({"default": null, "type": ["string", "null"]}),
        "{context} Workspace.slug schema drifted"
    );
    assert_eq!(
        workspace["properties"]["name"],
        json!({"default": null, "type": ["string", "null"]}),
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

/// The file-era diff/history tools and ADR-057 mount settings were removed as
/// one public-schema cutover. Keep this assertion alongside the positive
/// registration check so clients cannot rediscover either compatibility
/// surface through serialized MCP schemas.
#[tokio::test]
async fn all_tool_schemas_excludes_retired_file_era_memory_surfaces() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    for retired in ["memory_diff", "memory_history"] {
        assert!(
            !names.contains(retired),
            "retired file-era tool remains registered: {retired}"
        );
    }

    for retained in ["memory_read", "memory_search", "memory_write"] {
        assert!(
            names.contains(retained),
            "DB-backed memory tool should remain registered: {retained}"
        );
    }

    let serialized = serde_json::to_string(&tools).expect("tool schemas serialize");
    for retired in ["memory_mount_enabled", "memory_mount_path"] {
        assert!(
            !serialized.contains(retired),
            "retired ADR-057 mount setting remains serialized: {retired}"
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

#[tokio::test]
async fn code_graph_schema_advertises_uid_resolver_pagination_and_partial_semantics() {
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

    for resolver_keyword in [
        "uid",
        "stable exact node input",
        "fall back",
        "name",
        "file_path",
        "kind",
        "ranked candidates",
    ] {
        assert!(
            description.contains(resolver_keyword),
            "code_graph schema description does not advertise resolver keyword {resolver_keyword}"
        );
    }

    for triage_control in [
        "limit",
        "offset",
        "pageLimit",
        "summaryOnly",
        "byDepthCounts",
    ] {
        assert!(
            description.contains(triage_control),
            "code_graph schema description does not advertise traversal triage control {triage_control}"
        );
    }

    for warning_fragment in [
        "Partial pages and capped summaries are triage views",
        "absence from a page or summary is NOT evidence",
        "absent from the full graph",
    ] {
        assert!(
            description.contains(warning_fragment),
            "code_graph schema description does not advertise partial-result warning fragment {warning_fragment}"
        );
    }
}

/// vxmw / 5ice: registry-driven dispatch is a *runtime* change — the MCP
/// tool wire shape (operation names, parameter surface, response envelope
/// variants) must stay bit-identical through the vxmw vertical slice.
///
/// This test guards the contract that the checked-in
/// `djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap`
/// snapshot does **not** drift on the `code_graph` tool's `input_schema`
/// as a result of the registry-driven dispatch refactor. The runtime
/// dispatcher (`DjinnMcpServer::dispatch_code_graph`) is now
/// registry-derived for `neighbors` / `impact` / `context` /
/// `coupling_hotspots`, but the externally visible tool surface — the
/// `code_graph` MCP tool's `inputSchema` — is unchanged.
///
/// If the snapshot diff is purely description-only (e.g. the tool's
/// `description` block was rewritten but no field was added/removed),
/// accept the snapshot and document the diff in the task report.
#[tokio::test]
async fn code_graph_schema_unchanged_by_registry_dispatch_refactor() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let code_graph = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("code_graph"))
        .expect("code_graph schema");
    let live_input_schema = code_graph
        .get("inputSchema")
        .expect("code_graph inputSchema");
    let live_output_schema = code_graph
        .get("outputSchema")
        .expect("code_graph outputSchema");
    let live_description = code_graph
        .get("description")
        .and_then(Value::as_str)
        .expect("code_graph description");

    // ── Wire-shape invariants that the registry vertical slice must
    // not perturb. If any of these fail, the refactor leaked an
    // implementation detail into the MCP contract — a wire-shape
    // break for every chat / UI host that consumes the schema.
    //
    // The check here is the **field / variant set** (additive types),
    // not byte-equality of the snapshot. The full byte-equality lives
    // in the `mcp_tools_schema` insta snapshot; this test catches the
    // specific class of regression where the vxmw refactor accidentally
    // rewrites the `code_graph` schema.
    let input = live_input_schema
        .as_object()
        .expect("input_schema is a JSON object");
    let input_props = input
        .get("properties")
        .and_then(Value::as_object)
        .expect("input_schema has properties");
    // `operation` is the routing key the dispatcher branches on. The
    // registry refactor MUST NOT widen the field type (e.g. from
    // string to enum) because chat / UI hosts string-match the
    // operation name.
    assert!(
        input_props.contains_key("operation"),
        "code_graph input_schema lost `operation` field — registry refactor leak"
    );
    let operation_schema = &input_props["operation"];
    let operation_type = operation_schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        operation_type, "string",
        "code_graph `operation` field type changed from `string` to `{operation_type}` — \
         registry refactor must preserve wire-shape"
    );

    // The `outputSchema` for `code_graph` is the `ErrorOr<CodeGraphResponse>`
    // envelope — a thin tagged union that carries the variant envelope
    // through `additionalProperties: true`. The registry refactor is
    // forbidden from changing the envelope's wire shape (it must remain
    // permissive enough for every variant to pass through unchanged).
    let output = live_output_schema
        .as_object()
        .expect("output_schema is a JSON object");
    assert_eq!(
        output.get("type").and_then(Value::as_str),
        Some("object"),
        "code_graph outputSchema envelope type must stay `object`"
    );
    assert_eq!(
        output.get("additionalProperties").and_then(Value::as_bool),
        Some(true),
        "code_graph outputSchema must stay `additionalProperties: true` so every \
         `CodeGraphResponse` variant passes through the `ErrorOr` envelope unchanged"
    );
    // schemars 1.x (via rmcp 1.x) no longer emits a `title` for this derived
    // envelope wrapper. The functional wire-shape (object + additionalProperties)
    // asserted above is what variant pass-through depends on; the title was a
    // cosmetic annotation, so its absence under the new schema generator is fine.
    assert!(
        output.get("title").is_none(),
        "code_graph outputSchema envelope no longer carries a title under schemars 1.x; \
         got {:?}",
        output.get("title")
    );

    // The description advertises the full operation surface. The
    // vxmw registry refactor is allowed to make this richer but
    // never thinner — every operation that the pre-vxmw handwritten
    // dispatch handled must still be advertised in the description.
    for required_op in [
        "neighbors",
        "impact",
        "context",
        "coupling_hotspots",
        "coupling",
        "ranked",
        "search",
        "cycles",
        "orphans",
        "path",
        "edges",
        "describe",
        "status",
        "snapshot",
        "crate_graph",
        "impact_check",
    ] {
        assert!(
            live_description.contains(required_op),
            "code_graph description lost `{required_op}` — registry refactor \
             must not drop pre-vxmw operations from the advertised surface"
        );
    }
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_surfaces_advertise_nullable_parked_reason() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let find_tool = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("missing {name} schema"))
    };

    let list = find_tool("session_list");
    let show = find_tool("session_show");
    let timeline = find_tool("task_timeline");
    // schemars 1.x (via rmcp 1.x) renders nullable Option<T> as a JSON Schema
    // 2020-12 type array rather than the `nullable: true` extension.
    let expected = json!({
        "description": "Optional deliberate-park reason for terminal sessions, e.g. `budget`.",
        "type": ["string", "null"]
    });

    assert_eq!(
        list["outputSchema"]["$defs"]["SessionToolSession"]["properties"]["parked_reason"],
        expected,
        "session_list output schema must expose nullable parked_reason"
    );
    assert_eq!(
        show["outputSchema"]["properties"]["parked_reason"], expected,
        "session_show output schema must expose nullable parked_reason"
    );
    assert_eq!(
        timeline["outputSchema"]["$defs"]["SessionToolSession"]["properties"]["parked_reason"],
        expected,
        "task_timeline sessions schema must expose nullable parked_reason"
    );
}

// ── Dispatch-pause public MCP contract ─────────────────────────────────────────
//
// The dispatch-pause epic introduces three new MCP tools — `dispatch_pause`,
// `dispatch_resume`, and `dispatch_pause_status` — that must advertise a stable
// input/output shape so external clients can build against them. The
// generic `mcp_tools_schema` snapshot above already pins the full surface
// (line-for-line), but broad snapshot diffs are noisy. The tests below are
// focused contract assertions: they target only the dispatch-pause tools and
// the specific contract details that must NOT drift silently — input field
// names, required fields, output response shape, and the safety annotations
// that drive host agent UI (read-only, destructive, idempotent, open-world,
// concurrent-safe). If a future change adds a new field, removes a required
// input, or flips a safety hint, these tests fail with a clear, local
// message instead of forcing a reviewer to scan 14k lines of snapshot text.

/// Locate a tool schema by `name`. Mirrors the helper pattern used elsewhere
/// in this module but kept private to the dispatch-pause block so a renamed
/// tool only touches this section.
fn dispatch_pause_tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("missing dispatch-pause tool schema: {name}"))
}

#[tokio::test]
async fn all_tool_schemas_advertises_dispatch_pause_pause_resume_and_status() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    for name in ["dispatch_pause", "dispatch_resume", "dispatch_pause_status"] {
        let tool = dispatch_pause_tool(&tools, name);
        // Every dispatch-pause tool must surface a typed input AND output
        // schema so clients can both invoke and parse the result without
        // guessing at field names. A null inputSchema/outputSchema would
        // mean the router dropped the typed contract for that surface.
        assert!(
            tool.get("inputSchema")
                .map(|s| s.get("properties").is_some())
                .unwrap_or(false),
            "{name} inputSchema is missing `properties`"
        );
        assert_eq!(
            tool["inputSchema"]["additionalProperties"],
            json!(false),
            "{name} inputSchema must reject unknown fields"
        );
        assert!(
            tool.get("outputSchema")
                .map(|s| s.get("properties").is_some())
                .unwrap_or(false),
            "{name} outputSchema is missing `properties`"
        );
    }

    // dispatch_pause — required inputs (scope + reason) and output (mutation envelope).
    let pause = dispatch_pause_tool(&tools, "dispatch_pause");
    let pause_input_required = pause["inputSchema"]["required"]
        .as_array()
        .expect("dispatch_pause inputSchema.required array");
    for field in ["scope", "reason"] {
        assert!(
            pause_input_required
                .iter()
                .any(|v| v.as_str() == Some(field)),
            "dispatch_pause inputSchema.required missing `{field}`"
        );
    }
    assert!(
        pause["inputSchema"]["properties"]["target_id"].is_object(),
        "dispatch_pause inputSchema.properties.target_id must be present (optional but typed)"
    );
    let pause_output_required = pause["outputSchema"]["required"]
        .as_array()
        .expect("dispatch_pause outputSchema.required array");
    for field in ["ok", "changed", "scope"] {
        assert!(
            pause_output_required
                .iter()
                .any(|v| v.as_str() == Some(field)),
            "dispatch_pause outputSchema.required missing `{field}`"
        );
    }

    // dispatch_resume — only `scope` is required; same mutation envelope output.
    let resume = dispatch_pause_tool(&tools, "dispatch_resume");
    let resume_input_required = resume["inputSchema"]["required"]
        .as_array()
        .expect("dispatch_resume inputSchema.required array");
    assert_eq!(
        resume_input_required
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>(),
        vec!["scope"],
        "dispatch_resume inputSchema.required must be exactly [\"scope\"]"
    );
    assert!(
        resume["inputSchema"]["properties"]["target_id"].is_object(),
        "dispatch_resume inputSchema.properties.target_id must be present (optional but typed)"
    );
    let resume_output_required = resume["outputSchema"]["required"]
        .as_array()
        .expect("dispatch_resume outputSchema.required array");
    for field in ["ok", "changed", "scope"] {
        assert!(
            resume_output_required
                .iter()
                .any(|v| v.as_str() == Some(field)),
            "dispatch_resume outputSchema.required missing `{field}`"
        );
    }

    // dispatch_pause_status — neither scope nor target_id is required at the
    // type level (the handler infers "all scopes" when both are absent and
    // the runtime catches the global+target_id mismatch). The response
    // contract requires only `ok` — both `state` and `current` are
    // `anyOf`/`nullable` envelopes that are absent in different branches.
    let status = dispatch_pause_tool(&tools, "dispatch_pause_status");
    let status_input_required = status["inputSchema"]
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        status_input_required.is_empty(),
        "dispatch_pause_status inputSchema.required must be empty (no required fields), got {status_input_required:?}"
    );
    for field in ["scope", "target_id"] {
        assert!(
            status["inputSchema"]["properties"][field].is_object(),
            "dispatch_pause_status inputSchema.properties.{field} must be present (optional but typed)"
        );
    }
    let status_output_required = status["outputSchema"]["required"]
        .as_array()
        .expect("dispatch_pause_status outputSchema.required array");
    assert_eq!(
        status_output_required
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>(),
        vec!["ok"],
        "dispatch_pause_status outputSchema.required must be exactly [\"ok\"]"
    );
    for field in ["state", "current", "scope", "target_id", "error"] {
        assert!(
            status["outputSchema"]["properties"][field].is_object(),
            "dispatch_pause_status outputSchema.properties.{field} must be present (optional but typed)"
        );
    }
}

#[tokio::test]
async fn dispatch_pause_safety_annotations_match_intent() {
    // Host agents (e.g. OpenAI Codex strict schema validators, Anthropic's
    // tool-use approval UI, in-app agent UIs) consult the MCP tool's safety
    // annotations to decide whether to prompt the user for confirmation.
    // The dispatch-pause epic relies on those hints to be correct:
    //
    // * dispatch_pause / dispatch_resume — write/admin operations. They
    //   mutate persisted state, fail closed (require admin), and should be
    //   presented to users as "destructive: true". `concurrent_safe: false`
    //   because two overlapping pause calls can race on the mutation record
    //   and the second call's "changed: false" branch is the only thing that
    //   papers over the race.
    //
    // * dispatch_pause_status — strictly read-only / non-destructive. It
    //   must never mutate pause state and must never emit a pause-state
    //   change event. The test here pins the schema hint; the runtime
    //   guarantee (no event emission, no state change) is covered by
    //   `dispatch_pause_status_does_not_mutate_state_or_emit_events` in
    //   `server_tests.rs`.
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    for name in ["dispatch_pause", "dispatch_resume"] {
        let tool = dispatch_pause_tool(&tools, name);
        assert_eq!(
            tool["readOnly"], false,
            "{name} must advertise readOnly=false"
        );
        assert_eq!(
            tool["destructive"], true,
            "{name} must advertise destructive=true (pause/resume mutate state)"
        );
        // `idempotent: false` is the conservative default — the repository
        // layer will short-circuit no-op pauses/resumes, but the tool
        // itself is not contractually idempotent (each call still records
        // an actor/time). Hosts MUST treat repeated calls as distinct
        // mutations and prompt accordingly.
        assert_eq!(
            tool["idempotent"], false,
            "{name} must advertise idempotent=false (each call is a recorded mutation)"
        );
        assert_eq!(
            tool["openWorld"], false,
            "{name} must advertise openWorld=false (no external system call)"
        );
        assert_eq!(
            tool["concurrent_safe"], false,
            "{name} must advertise concurrent_safe=false (pause/resume race on mutation record)"
        );
    }

    let status = dispatch_pause_tool(&tools, "dispatch_pause_status");
    assert_eq!(
        status["readOnly"], true,
        "dispatch_pause_status must advertise readOnly=true (it is a pure read)"
    );
    assert_eq!(
        status["destructive"], false,
        "dispatch_pause_status must advertise destructive=false (no state mutation)"
    );
    assert_eq!(
        status["idempotent"], true,
        "dispatch_pause_status must advertise idempotent=true (pure read, safe to retry)"
    );
    assert_eq!(
        status["openWorld"], false,
        "dispatch_pause_status must advertise openWorld=false (in-process DB read only)"
    );
    assert_eq!(
        status["concurrent_safe"], false,
        "dispatch_pause_status must advertise concurrent_safe=false (the underlying \
         connection is not pooled for concurrent reads in this revision; the host UI \
         can still treat the call as read-only)"
    );
}

#[test]
fn checked_in_mcp_snapshot_includes_dispatch_pause_tools_with_strict_schemas() {
    // The checked-in insta snapshot of every tool schema is the contract
    // clients rely on at runtime. This test guards two specific properties
    // of the dispatch-pause entry points inside that snapshot:
    //
    // 1. All three tool names appear in the snapshot.
    // 2. Each tool's input schema carries `additionalProperties: false` —
    //    the JSON-Schema manifestation of `#[serde(deny_unknown_fields)]`.
    //    A future migration that drops `deny_unknown_fields` on any of the
    //    three param structs would silently weaken the contract; this test
    //    fails loudly before that change ships.
    let snapshot =
        include_str!("snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap");
    let json_body = snapshot
        .splitn(3, "---\n")
        .nth(2)
        .expect("insta snapshot body after metadata header");
    let tools: Vec<Value> = serde_json::from_str(json_body).expect("MCP schema snapshot is JSON");

    for name in ["dispatch_pause", "dispatch_resume", "dispatch_pause_status"] {
        let tool = tools
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("checked-in MCP snapshot is missing `{name}`"));

        let input_schema = tool
            .get("input_schema")
            .unwrap_or_else(|| panic!("{name} snapshot entry missing input_schema"));
        assert_eq!(
            input_schema["additionalProperties"],
            json!(false),
            "{name} input_schema must declare additionalProperties=false \
             (i.e. #[serde(deny_unknown_fields)] on the params struct)"
        );
    }
}

// ── Evidence lifecycle payload regression (hoh3 / tlon) ─────────────────────
//
// Focused schema-surface assertions that the checked-in MCP snapshot AND the
// live `all_tool_schemas()` include the distinct AwaitingEvidence /
// EvidenceFailed payload shape on `proposal_refinement_status`.  The broad
// byte-equality `mcp_tools_schema` insta snapshot already pins the full surface
// line-for-line; these focused tests give a clear, local failure message if a
// future change silently drops the evidence discriminator fields, narrows the
// enum, or removes the structured-claim sub-fields.

/// Collect all string `const` values from a JSON-Schema `oneOf`/`anyOf` enum
/// node.  Returns an empty set when the node is not an enum-style schema.
fn collect_enum_consts(schema: &Value) -> std::collections::BTreeSet<String> {
    let mut consts = std::collections::BTreeSet::new();
    let arrays = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array);
    if let Some(arr) = arrays {
        for variant in arr {
            if let Some(c) = variant.get("const").and_then(Value::as_str) {
                consts.insert(c.to_string());
            }
        }
    }
    consts
}

/// Assert the evidence-lifecycle $defs shape on a schema's output_schema.
/// Used for both the live schema and the checked-in snapshot so a drift in
/// either direction is caught.
fn assert_evidence_lifecycle_payload_shape(schema: &Value, context: &str) {
    let defs = schema
        .get("$defs")
        .unwrap_or_else(|| panic!("{context}: output_schema missing $defs"));

    // EvidenceLifecycleState enum must include all six discriminators.
    let state_defs = defs
        .get("EvidenceLifecycleState")
        .unwrap_or_else(|| panic!("{context}: $defs missing EvidenceLifecycleState definition"));
    let state_consts = collect_enum_consts(state_defs);
    for variant in [
        "active",
        "awaiting_evidence",
        "evidence_received",
        "evidence_failed",
        "paused_or_frozen",
        "terminal",
    ] {
        assert!(
            state_consts.contains(variant),
            "{context}: EvidenceLifecycleState must include `{variant}` variant, \
             got {state_consts:?}"
        );
    }

    // EvidenceLifecyclePhase enum must include the three inner phases.
    let phase_defs = defs
        .get("EvidenceLifecyclePhase")
        .unwrap_or_else(|| panic!("{context}: $defs missing EvidenceLifecyclePhase definition"));
    let phase_consts = collect_enum_consts(phase_defs);
    for variant in ["awaiting_evidence", "evidence_received", "evidence_failed"] {
        assert!(
            phase_consts.contains(variant),
            "{context}: EvidenceLifecyclePhase must include `{variant}` variant, \
             got {phase_consts:?}"
        );
    }

    // NeedsEvidenceStatus must carry the structured-claim + spike-id fields.
    let ne_defs = defs
        .get("NeedsEvidenceStatus")
        .unwrap_or_else(|| panic!("{context}: $defs missing NeedsEvidenceStatus definition"));
    let ne_props = ne_defs
        .get("properties")
        .unwrap_or_else(|| panic!("{context}: NeedsEvidenceStatus missing properties"));
    for field in [
        "claim",
        "spike_task_id",
        "spike_short_id",
        "spike_status",
        "evidence_phase",
        "failure_reason",
    ] {
        assert!(
            ne_props.get(field).is_some(),
            "{context}: NeedsEvidenceStatus missing required field `{field}`"
        );
    }

    // Structured-claim fields (added by h2az) must be present.
    for field in [
        "question",
        "target_subsystem",
        "spec_unknown_anchor",
        "insufficient_in_session_research",
        "expected_findings",
    ] {
        assert!(
            ne_props.get(field).is_some(),
            "{context}: NeedsEvidenceStatus missing structured-claim field `{field}`"
        );
    }

    // The status model itself must surface evidence_lifecycle_state and
    // needs_evidence so consumers can use the discriminator without inspecting
    // sub-fields.  These live inside the ProposalRefinementStatusModel $def.
    let status_model = defs
        .get("ProposalRefinementStatusModel")
        .unwrap_or_else(|| {
            panic!("{context}: $defs missing ProposalRefinementStatusModel definition")
        });
    let status_props = status_model
        .get("properties")
        .unwrap_or_else(|| panic!("{context}: ProposalRefinementStatusModel missing properties"));
    assert!(
        status_props.get("evidence_lifecycle_state").is_some(),
        "{context}: status model missing evidence_lifecycle_state discriminator"
    );
    assert!(
        status_props.get("needs_evidence").is_some(),
        "{context}: status model missing needs_evidence field"
    );
}

/// Live schema assertion: `proposal_refinement_status` output schema carries
/// the distinct AwaitingEvidence / EvidenceFailed payload shape.
#[tokio::test]
async fn proposal_refinement_status_schema_includes_evidence_lifecycle_payload() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let mcp = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state());
    let tools = mcp.all_tool_schemas();

    let status_tool = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("proposal_refinement_status"))
        .expect("proposal_refinement_status schema");
    let output = status_tool
        .get("outputSchema")
        .expect("proposal_refinement_status outputSchema");

    assert_evidence_lifecycle_payload_shape(output, "live proposal_refinement_status");
}

/// Checked-in snapshot assertion: the committed MCP snapshot carries the same
/// payload shape, so a regeneration or hand-edit cannot silently drop the
/// evidence discriminator fields.
#[test]
fn checked_in_mcp_snapshot_includes_evidence_lifecycle_payload() {
    let snapshot =
        include_str!("snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap");
    let json_body = snapshot
        .splitn(3, "---\n")
        .nth(2)
        .expect("insta snapshot body after metadata header");
    let tools: Vec<Value> = serde_json::from_str(json_body).expect("MCP schema snapshot is JSON");

    let status_tool = tools
        .iter()
        .find(|t| t.get("name").and_then(Value::as_str) == Some("proposal_refinement_status"))
        .unwrap_or_else(|| panic!("checked-in snapshot is missing proposal_refinement_status"));
    let output = status_tool
        .get("output_schema")
        .expect("snapshot proposal_refinement_status output_schema");

    assert_evidence_lifecycle_payload_shape(output, "snapshot proposal_refinement_status");
}
