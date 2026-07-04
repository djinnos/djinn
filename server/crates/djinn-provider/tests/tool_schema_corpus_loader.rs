//! Tool-schema projection corpus fixture loader and validity checks.
//!
//! This test file loads the committed JSON corpus under
//! `tests/fixtures/tool_schema_projection/` and validates that every
//! fixture is well-formed JSON in the expected RMCP tool shape.
//!
//! The companion invariant test
//! `tool_schema_projection_corpus.rs` loads the same fixtures and runs
//! every fixture's `inputSchema` through every relevant
//! `(ToolSchemaCompat, FormatFamily)` projection combination, asserting
//! strict-validator invariants.  This file supplies the fixture layout
//! and loading scaffolding; the invariant test owns the projection
//! assertions.
//!
//! See `tests/fixtures/tool_schema_projection/README.md` for the
//! dependency-cycle rationale (why we use committed snapshots instead of
//! direct crate imports) and the refresh path.

use std::fs;

use serde_json::Value;

/// Directory containing the corpus fixtures, relative to the crate root
/// (which is the CWD for integration tests).
const FIXTURE_DIR: &str = "tests/fixtures/tool_schema_projection";

/// Path to the committed full-server DjinnMcpServer fixture.
const DJINN_MCP_SERVER_FIXTURE: &str = "builtin/djinn_mcp_server.json";
/// Robust lower bound for the DjinnMcpServer corpus size. The exact count
/// drifts as tools are added or removed; this only asserts a full-server size.
const DJINN_MCP_SERVER_MIN_TOOLS: usize = 140;

// ─── Fixture loading helpers ──────────────────────────────────────────────────

/// Load and parse a JSON fixture relative to `FIXTURE_DIR`.
fn load_fixture(rel_path: &str) -> Value {
    let full = format!("{FIXTURE_DIR}/{rel_path}");
    let contents =
        fs::read_to_string(&full).unwrap_or_else(|e| panic!("failed to read fixture {full}: {e}"));
    serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("failed to parse fixture {full} as JSON: {e}"))
}

/// Load all fixtures listed in a manifest group.
fn load_group(group: &str) -> Vec<(String, Value)> {
    let manifest = load_fixture("manifest.json");
    let files = manifest["groups"][group]["files"]
        .as_array()
        .unwrap_or_else(|| panic!("manifest group '{group}' has no 'files' array"))
        .iter()
        .filter_map(|v| v.as_str().map(String::from));
    files
        .map(|f| {
            let val = load_fixture(&f);
            (f, val)
        })
        .collect()
}

/// Extract the tool name from a fixture value.
fn tool_name(val: &Value) -> &str {
    val.get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("fixture missing 'name' field: {val}"))
}

// ─── Validity checks ──────────────────────────────────────────────────────────

#[test]
fn manifest_json_loads_and_has_expected_groups() {
    let manifest = load_fixture("manifest.json");
    assert_eq!(manifest["version"], 1, "manifest version must be 1");
    let groups = manifest["groups"].as_object().expect("groups is an object");
    assert!(
        groups.contains_key("builtin"),
        "manifest must have a 'builtin' group"
    );
    assert!(
        groups.contains_key("regression"),
        "manifest must have a 'regression' group"
    );
}

#[test]
fn djinn_mcp_server_fixture_is_substantial_and_well_formed() {
    let tools = load_fixture(DJINN_MCP_SERVER_FIXTURE)
        .as_array()
        .unwrap_or_else(|| panic!("{DJINN_MCP_SERVER_FIXTURE} must be a JSON array of tools"))
        .clone();
    assert!(
        tools.len() >= DJINN_MCP_SERVER_MIN_TOOLS,
        "{DJINN_MCP_SERVER_FIXTURE} has only {} tools; expected at least {DJINN_MCP_SERVER_MIN_TOOLS}",
        tools.len()
    );

    let mut seen = std::collections::HashSet::new();
    for tool in &tools {
        let name = tool_name(tool);
        assert!(
            seen.insert(name),
            "duplicate tool name '{name}' in {DJINN_MCP_SERVER_FIXTURE}"
        );
        let schema = &tool["inputSchema"];
        assert!(
            schema.is_object(),
            "tool '{name}' in {DJINN_MCP_SERVER_FIXTURE} has a non-object 'inputSchema'"
        );
        assert!(
            schema.get("type").is_some(),
            "tool '{name}' in {DJINN_MCP_SERVER_FIXTURE} inputSchema should have a 'type' field"
        );
    }
}

#[test]
fn builtin_corpus_loads_and_every_tool_has_input_schema() {
    let corpus = load_group("builtin");
    assert!(
        !corpus.is_empty(),
        "builtin corpus must not be empty — check manifest.json"
    );

    // Every builtin file is an array of tool objects with inputSchema.
    let mut total_tools = 0usize;
    for (file, tools) in &corpus {
        let arr = tools.as_array().unwrap_or_else(|| {
            panic!("builtin fixture {file} must be a JSON array of tool objects")
        });
        assert!(
            !arr.is_empty(),
            "builtin fixture {file} must contain at least one tool"
        );
        for tool in arr {
            assert!(
                tool.get("inputSchema").is_some(),
                "tool {} in {file} is missing 'inputSchema'",
                tool_name(tool)
            );
            assert!(
                tool.get("name").is_some(),
                "tool in {file} is missing 'name'"
            );
        }
        total_tools += arr.len();
    }
    // Sanity: the built-in corpus should have a substantial number of tools.
    assert!(
        total_tools >= 20,
        "builtin corpus has only {total_tools} tools — expected at least 20"
    );
}

#[test]
fn regression_corpus_loads_and_covers_all_known_bad_shapes() {
    let corpus = load_group("regression");
    assert!(!corpus.is_empty(), "regression corpus must not be empty");

    // Every regression fixture is a single tool object (not an array).
    for (file, tool) in &corpus {
        assert!(
            tool.is_object() && !tool.is_array(),
            "regression fixture {file} must be a single JSON object, not an array"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "regression fixture {file} is missing 'inputSchema'"
        );
        assert!(
            tool.get("name").is_some(),
            "regression fixture {file} is missing 'name'"
        );
        assert!(
            tool.get("description").is_some(),
            "regression fixture {file} is missing 'description'"
        );
    }

    // Verify every known bad shape from proposal mpen is represented.
    let names: Vec<&str> = corpus.iter().map(|(_, v)| tool_name(v)).collect();

    let required_shapes = [
        "regression_empty_items_object",
        "regression_items_object_no_properties",
        "regression_allof_if_then_conditionals",
        "regression_schemars_untagged_enum_anyof",
        "regression_ref_siblings",
        "regression_tuple_prefix_items",
        "regression_unevaluated_items",
        "regression_gemini_forbidden_keywords",
    ];
    for required in &required_shapes {
        assert!(
            names.contains(required),
            "regression corpus is missing required shape '{required}'"
        );
    }
}

#[test]
fn regression_fixture_names_are_unique() {
    let corpus = load_group("regression");
    let mut seen = std::collections::HashSet::new();
    for (file, tool) in &corpus {
        let name = tool_name(tool);
        assert!(
            seen.insert(name),
            "duplicate regression tool name '{name}' in {file}"
        );
    }
}

/// Load only the `builtin/djinn_mcp_server.json` fixture and return its
/// parsed JSON array.
fn load_djinn_mcp_server_fixture() -> Vec<Value> {
    let val = load_fixture("builtin/djinn_mcp_server.json");
    val.as_array()
        .expect("djinn_mcp_server.json must be a JSON array of tool objects")
        .clone()
}

#[test]
fn manifest_includes_djinn_mcp_server_fixture() {
    let manifest = load_fixture("manifest.json");
    let files = manifest["groups"]["builtin"]["files"]
        .as_array()
        .expect("builtin group has files array");
    let file_strs: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        file_strs.contains(&"builtin/djinn_mcp_server.json"),
        "builtin manifest must include builtin/djinn_mcp_server.json — the \
         DjinnMcpServer::all_tool_schemas() snapshot should be listed alongside \
         role-based builtin fixtures"
    );
}

#[test]
fn djinn_mcp_server_fixture_is_full_size_corpus() {
    let tools = load_djinn_mcp_server_fixture();
    assert!(
        !tools.is_empty(),
        "djinn_mcp_server.json must contain at least one tool"
    );
    // The DjinnMcpServer corpus is expected to be a substantial full-server
    // tool set.  Use a robust lower bound (>= 140) rather than a brittle
    // exact count so minor tool additions/removals don't flake the test.
    assert!(
        tools.len() >= 140,
        "djinn_mcp_server.json has only {} tools — expected a full-size \
         DjinnMcpServer corpus (>= 140 tools). If tools were intentionally \
         removed, update this bound.",
        tools.len()
    );
}

#[test]
fn djinn_mcp_server_fixture_has_quality_input_schemas() {
    let tools = load_djinn_mcp_server_fixture();
    for tool in &tools {
        let name = tool_name(tool);
        // Every tool must have a non-empty object inputSchema.
        let schema = tool
            .get("inputSchema")
            .unwrap_or_else(|| panic!("DjinnMcpServer tool '{name}' is missing 'inputSchema'"));
        assert!(
            schema.is_object(),
            "DjinnMcpServer tool '{name}' inputSchema must be a JSON object, got {}",
            if schema.is_null() {
                "null"
            } else {
                schema.as_str().unwrap_or("non-object")
            }
        );
        assert!(
            !schema.as_object().unwrap().is_empty(),
            "DjinnMcpServer tool '{name}' inputSchema must not be an empty object"
        );
    }
}

#[test]
fn djinn_mcp_server_fixture_tool_names_are_unique() {
    let tools = load_djinn_mcp_server_fixture();
    let mut seen = std::collections::HashSet::new();
    for tool in &tools {
        let name = tool_name(tool);
        assert!(
            seen.insert(name),
            "duplicate DjinnMcpServer tool name '{name}' — \
             DjinnMcpServer::all_tool_schemas() should produce unique names"
        );
    }
}

#[test]
fn builtin_corpus_input_schemas_are_valid_json_schema_objects() {
    let corpus = load_group("builtin");
    for (file, tools) in &corpus {
        let arr = tools.as_array().expect("builtin fixtures are arrays");
        for tool in arr {
            let name = tool_name(tool);
            let schema = &tool["inputSchema"];
            assert!(
                schema.is_object(),
                "{file}:{name} inputSchema must be a JSON object"
            );
            // Every inputSchema should have a "type" at the top level
            // (RMCP tool schemas are object-typed).
            assert!(
                schema.get("type").is_some(),
                "{file}:{name} inputSchema should have a 'type' field"
            );
        }
    }
}
