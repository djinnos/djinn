//! Tool-schema projection corpus invariant integration test.
//!
//! Loads every builtin and regression fixture listed in
//! `tests/fixtures/tool_schema_projection/manifest.json` and runs each
//! tool's `inputSchema` through the shared
//! [`djinn_provider::provider::format::tool_projection::project`] entry
//! point for every relevant `(ToolSchemaCompat, FormatFamily)` combination.
//!
//! Rather than asserting full JSON equality, the test asserts the
//! strict-validator *invariants* that each compatibility projection must
//! uphold:
//!
//! - **`compat = None`** — identity: the projected schema equals the input.
//! - **OpenAI-family (`OpenAi`)** — every object-typed subschema has a
//!   `properties` key (required for OpenAI strict function-calling).
//! - **Moonshot** — no invalid `$ref` sibling keywords, no `prefixItems`
//!   remnants, no `unevaluatedItems` remnants.
//! - **Gemini** — only whitelisted `functionDeclaration` keywords remain,
//!   and no Djinn safety-annotation keys leak into the schema.
//!
//! Failure messages identify the fixture file, tool name, compat, family,
//! and the JSON pointer of the offending node.
//!
//! See `tests/fixtures/tool_schema_projection/README.md` for the corpus
//! layout and refresh path.  This is the proposal `mpen` acceptance gap:
//! it proves the corpus — including known-bad regressions — is projected
//! safely across provider families.

use std::fs;

use djinn_provider::provider::format::tool_projection::project;
use djinn_provider::provider::{FormatFamily, ToolSchemaCompat};
use serde_json::{Map, Value};

/// Directory containing the corpus fixtures, relative to the crate root
/// (which is the CWD for integration tests).
const FIXTURE_DIR: &str = "tests/fixtures/tool_schema_projection";

/// Keywords that may remain as siblings of `$ref` after Moonshot projection.
/// Must mirror the allowlist in `tool_projection::rewrite_moonshot_recursive`.
const MOONSHOT_REF_ALLOWLIST: &[&str] = &[
    "$ref",
    "title",
    "description",
    "$comment",
    "default",
    "examples",
];

/// Keywords allowed in Gemini function-declaration schemas.  Must mirror the
/// `WHITELIST` constant in `tool_projection::rewrite_gemini_recursive`.
const GEMINI_WHITELIST: &[&str] = &[
    "type",
    "description",
    "nullable",
    "enum",
    "properties",
    "required",
    "items",
    // Numeric validation
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    // String validation
    "minLength",
    "maxLength",
    "pattern",
    // Array validation
    "minItems",
    "maxItems",
    "uniqueItems",
    // Object validation
    "minProperties",
    "maxProperties",
    "additionalProperties",
    // Combinators
    "anyOf",
    // References and local definitions
    "$ref",
    "$defs",
    "definitions",
    // Metadata
    "title",
    "$comment",
    "default",
    "examples",
    "format",
];

/// Djinn tool-level safety-annotation keys that must never appear inside a
/// projected tool-parameter schema.
const SAFETY_ANNOTATION_KEYS: &[&str] = &[
    "readOnly",
    "destructive",
    "idempotent",
    "openWorld",
    "concurrent_safe",
];

/// Every `FormatFamily` supported by the projection layer.
const ALL_FAMILIES: &[FormatFamily] = &[
    FormatFamily::OpenAI,
    FormatFamily::OpenAIResponses,
    FormatFamily::Anthropic,
    FormatFamily::Google,
];

// ─── Fixture loading ──────────────────────────────────────────────────────────

/// A single tool extracted from the corpus, ready for projection.
struct Fixture {
    group: &'static str,
    file: String,
    name: String,
    input_schema: Value,
}

/// Load and parse a JSON fixture relative to `FIXTURE_DIR`.
fn load_fixture_file(rel: &str) -> Value {
    let full = format!("{FIXTURE_DIR}/{rel}");
    let contents =
        fs::read_to_string(&full).unwrap_or_else(|e| panic!("failed to read fixture {full}: {e}"));
    serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("failed to parse fixture {full} as JSON: {e}"))
}

/// DjinnMcpServer fixture path and the minimum number of tools it must
/// contribute. The fixture is generated from `DjinnMcpServer::all_tool_schemas()`
/// and committed as JSON to avoid a provider -> control-plane dependency cycle.
const DJINN_MCP_SERVER_FIXTURE: &str = "builtin/djinn_mcp_server.json";
const DJINN_MCP_SERVER_MIN_TOOLS: usize = 140;

/// Load the full corpus (builtin + regression) into a flat list of fixtures.
fn load_corpus() -> Vec<Fixture> {
    let manifest = load_fixture_file("manifest.json");
    let mut out = Vec::new();

    // (group key, whether files in the group are JSON arrays of tools)
    for (group, is_array) in [("builtin", true), ("regression", false)] {
        let files = manifest["groups"][group]["files"]
            .as_array()
            .unwrap_or_else(|| panic!("manifest group '{group}' has no 'files' array"));
        for entry in files {
            let rel = entry.as_str().unwrap_or_else(|| {
                panic!("manifest file entry in group '{group}' is not a string")
            });
            let val = load_fixture_file(rel);
            let tools: Vec<Value> = if is_array {
                val.as_array()
                    .unwrap_or_else(|| panic!("builtin fixture {rel} must be a JSON array"))
                    .clone()
            } else {
                vec![val]
            };
            for tool in tools {
                let name = tool
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| panic!("tool in {rel} missing 'name'"))
                    .to_string();
                let input_schema = tool
                    .get("inputSchema")
                    .unwrap_or_else(|| panic!("tool {name} in {rel} missing 'inputSchema'"))
                    .clone();
                out.push(Fixture {
                    group,
                    file: rel.to_string(),
                    name,
                    input_schema,
                });
            }
        }
    }
    out
}

/// Assert that the DjinnMcpServer full-server fixture is present in the builtin
/// manifest and contributes a substantial corpus. This protects against
/// refactors that silently drop the expanded corpus from the projection test.
fn assert_djinn_mcp_server_fixture_included(manifest: &Value) {
    let files = manifest["groups"]["builtin"]["files"]
        .as_array()
        .expect("manifest builtin group has no 'files' array");
    let file_strings: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        file_strings.contains(&DJINN_MCP_SERVER_FIXTURE),
        "builtin manifest must include '{DJINN_MCP_SERVER_FIXTURE}'"
    );
}

#[test]
fn djinn_mcp_server_fixture_is_present_and_substantial() {
    let manifest = load_fixture_file("manifest.json");
    assert_djinn_mcp_server_fixture_included(&manifest);

    let tools = load_fixture_file(DJINN_MCP_SERVER_FIXTURE)
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
        let name = tool
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("tool in {DJINN_MCP_SERVER_FIXTURE} missing 'name'"));
        assert!(
            seen.insert(name),
            "duplicate tool name '{name}' in {DJINN_MCP_SERVER_FIXTURE}"
        );
        let schema = tool.get("inputSchema").unwrap_or_else(|| {
            panic!("tool '{name}' in {DJINN_MCP_SERVER_FIXTURE} missing 'inputSchema'")
        });
        assert!(
            schema.is_object(),
            "tool '{name}' in {DJINN_MCP_SERVER_FIXTURE} has a non-object 'inputSchema'"
        );
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Human-readable label for a (compat, family) combination.
fn label(compat: Option<ToolSchemaCompat>, family: FormatFamily) -> &'static str {
    match (compat, family) {
        (None, FormatFamily::OpenAI) => "compat=None family=OpenAI",
        (None, FormatFamily::OpenAIResponses) => "compat=None family=OpenAIResponses",
        (None, FormatFamily::Anthropic) => "compat=None family=Anthropic",
        (None, FormatFamily::Google) => "compat=None family=Google",
        (Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI) => "compat=OpenAi family=OpenAI",
        (Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAIResponses) => {
            "compat=OpenAi family=OpenAIResponses"
        }
        (Some(ToolSchemaCompat::OpenAi), FormatFamily::Anthropic) => {
            "compat=OpenAi family=Anthropic"
        }
        (Some(ToolSchemaCompat::OpenAi), FormatFamily::Google) => "compat=OpenAi family=Google",
        (Some(ToolSchemaCompat::Moonshot), FormatFamily::OpenAI) => "compat=Moonshot family=OpenAI",
        (Some(ToolSchemaCompat::Moonshot), FormatFamily::OpenAIResponses) => {
            "compat=Moonshot family=OpenAIResponses"
        }
        (Some(ToolSchemaCompat::Moonshot), FormatFamily::Anthropic) => {
            "compat=Moonshot family=Anthropic"
        }
        (Some(ToolSchemaCompat::Moonshot), FormatFamily::Google) => "compat=Moonshot family=Google",
        (Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI) => "compat=Gemini family=OpenAI",
        (Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAIResponses) => {
            "compat=Gemini family=OpenAIResponses"
        }
        (Some(ToolSchemaCompat::Gemini), FormatFamily::Anthropic) => {
            "compat=Gemini family=Anthropic"
        }
        (Some(ToolSchemaCompat::Gemini), FormatFamily::Google) => "compat=Gemini family=Google",
    }
}

/// Escape a JSON pointer reference-token (~ → ~0, / → ~1).
fn escape_ptr_segment(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// JSON Schema keywords whose value is a **name → schema** map.  The *keys*
/// of such maps are arbitrary identifiers (property names, definition names,
/// regex patterns), **not** schema keywords, so the container map itself must
/// never be checked against a keyword whitelist — only its child schemas.
const NAME_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// JSON Schema keywords whose value is a single subschema.
const SINGLE_SCHEMA_KEYWORDS: &[&str] = &[
    "additionalProperties",
    "contains",
    "propertyNames",
    "unevaluatedItems",
    "unevaluatedProperties",
    "if",
    "then",
    "else",
    "not",
];

/// JSON Schema keywords whose value is an array of subschemas.
const ARRAY_SCHEMA_KEYWORDS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];

/// Recursively walk every **schema** node of `value`, invoking `visit` with
/// each schema object and its accumulated JSON-pointer path.
///
/// Unlike a naive JSON-object walker, this understands JSON Schema structure:
/// `name → schema` container maps (e.g. `properties`, `$defs`) are descended
/// into but never themselves treated as schema nodes, so their keys (arbitrary
/// property/definition names) are not checked against keyword whitelists.
fn walk<F: FnMut(&Map<String, Value>, &str)>(value: &Value, path: &mut String, visit: &mut F) {
    let Some(obj) = value.as_object() else {
        return;
    };

    // This node is a schema — visit it.
    visit(obj, path.as_str());

    // Descend into container maps: each *value* is a schema; the container
    // itself is never visited.
    for kw in NAME_MAP_KEYWORDS {
        if let Some(Value::Object(map)) = obj.get(*kw) {
            for (name, child) in map.iter() {
                let mark = path.len();
                path.push('/');
                path.push_str(&escape_ptr_segment(kw));
                path.push('/');
                path.push_str(&escape_ptr_segment(name));
                walk(child, path, visit);
                path.truncate(mark);
            }
        }
    }

    // Descend into single-schema keywords.
    for kw in SINGLE_SCHEMA_KEYWORDS {
        if let Some(child) = obj.get(*kw) {
            let mark = path.len();
            path.push('/');
            path.push_str(&escape_ptr_segment(kw));
            walk(child, path, visit);
            path.truncate(mark);
        }
    }

    // Descend into array-of-schemas keywords.
    for kw in ARRAY_SCHEMA_KEYWORDS {
        if let Some(Value::Array(arr)) = obj.get(*kw) {
            for (i, child) in arr.iter().enumerate() {
                let mark = path.len();
                path.push('/');
                path.push_str(&escape_ptr_segment(kw));
                path.push('/');
                path.push_str(&i.to_string());
                walk(child, path, visit);
                path.truncate(mark);
            }
        }
    }

    // `items` can be a single schema or an array of schemas.
    if let Some(items) = obj.get("items") {
        let mark = path.len();
        path.push_str("/items");
        match items {
            Value::Array(arr) => {
                for (i, child) in arr.iter().enumerate() {
                    let mark2 = path.len();
                    path.push('/');
                    path.push_str(&i.to_string());
                    walk(child, path, visit);
                    path.truncate(mark2);
                }
            }
            other => walk(other, path, visit),
        }
        path.truncate(mark);
    }
}

/// Build the `[group/file:name]` prefix used in failure messages.
fn ctx_prefix(fx: &Fixture, combo: &'static str) -> String {
    format!(
        "[{}/{fx_file}:{fx_name} {combo}]",
        fx.group,
        fx_file = fx.file,
        fx_name = fx.name
    )
}

// ─── Invariant tests ──────────────────────────────────────────────────────────

/// Proposal `mpen` known-bad regression shapes that must all be present in
/// the corpus and therefore exercised by the invariant checks below.
const REQUIRED_REGRESSION_SHAPES: &[&str] = &[
    "regression_empty_items_object",
    "regression_items_object_no_properties",
    "regression_allof_if_then_conditionals",
    "regression_schemars_untagged_enum_anyof",
    "regression_ref_siblings",
    "regression_tuple_prefix_items",
    "regression_unevaluated_items",
    "regression_gemini_forbidden_keywords",
];

#[test]
fn corpus_covers_all_known_regression_shapes() {
    let corpus = load_corpus();
    let names: Vec<&str> = corpus
        .iter()
        .filter(|f| f.group == "regression")
        .map(|f| f.name.as_str())
        .collect();
    for required in REQUIRED_REGRESSION_SHAPES {
        assert!(
            names.contains(required),
            "regression corpus is missing required shape '{required}' — the invariant tests below \
             would not exercise it",
        );
    }
}

#[test]
fn none_compat_is_identity_for_every_fixture_and_family() {
    let corpus = load_corpus();
    assert!(!corpus.is_empty(), "corpus must not be empty");

    for fx in &corpus {
        for &family in ALL_FAMILIES {
            let combo = label(None, family);
            let out = project(fx.input_schema.clone(), None, family);
            assert_eq!(
                fx.input_schema,
                out,
                "[{}/{fx_file}:{fx_name} {combo}] compat=None must be identity \
                 (projected schema differs from input)",
                fx.group,
                fx_file = fx.file,
                fx_name = fx.name,
            );
        }
    }
}

#[test]
fn openai_compat_enforces_object_properties_on_every_subschema() {
    let corpus = load_corpus();

    for fx in &corpus {
        for &family in ALL_FAMILIES {
            let combo = label(Some(ToolSchemaCompat::OpenAi), family);
            let ctx = ctx_prefix(fx, combo);
            let out = project(
                fx.input_schema.clone(),
                Some(ToolSchemaCompat::OpenAi),
                family,
            );
            let mut path = String::new();
            walk(&out, &mut path, &mut |obj, p| {
                if obj.get("type").and_then(|v| v.as_str()) == Some("object")
                    && !obj.contains_key("properties")
                {
                    panic!(
                        "{ctx} OpenAI strict object-shape invariant violated: \
                         object subschema missing 'properties' at {p}"
                    );
                }
            });
        }
    }
}

#[test]
fn moonshot_compat_strips_ref_siblings_and_tuple_unevaluated_items() {
    let corpus = load_corpus();

    for fx in &corpus {
        for &family in ALL_FAMILIES {
            let combo = label(Some(ToolSchemaCompat::Moonshot), family);
            let ctx = ctx_prefix(fx, combo);
            let out = project(
                fx.input_schema.clone(),
                Some(ToolSchemaCompat::Moonshot),
                family,
            );
            let mut path = String::new();
            walk(&out, &mut path, &mut |obj, p| {
                if obj.contains_key("prefixItems") {
                    panic!(
                        "{ctx} Moonshot invariant violated: 'prefixItems' remnant at \
                         {p}/prefixItems (should have collapsed into 'items')"
                    );
                }
                if obj.contains_key("unevaluatedItems") {
                    panic!(
                        "{ctx} Moonshot invariant violated: 'unevaluatedItems' remnant at \
                         {p}/unevaluatedItems"
                    );
                }
                if obj.contains_key("$ref") {
                    for k in obj.keys() {
                        if !MOONSHOT_REF_ALLOWLIST.contains(&k.as_str()) {
                            panic!(
                                "{ctx} Moonshot invariant violated: invalid '$ref' sibling \
                                 '{k}' at {p}/{k}"
                            );
                        }
                    }
                }
            });
        }
    }
}

#[test]
fn gemini_compat_keeps_only_whitelisted_keywords_and_no_safety_keys() {
    let corpus = load_corpus();

    for fx in &corpus {
        for &family in ALL_FAMILIES {
            let combo = label(Some(ToolSchemaCompat::Gemini), family);
            let ctx = ctx_prefix(fx, combo);
            let out = project(
                fx.input_schema.clone(),
                Some(ToolSchemaCompat::Gemini),
                family,
            );
            let mut path = String::new();
            walk(&out, &mut path, &mut |obj, p| {
                for k in obj.keys() {
                    if SAFETY_ANNOTATION_KEYS.contains(&k.as_str()) {
                        panic!(
                            "{ctx} Gemini invariant violated: Djinn safety-annotation key \
                             '{k}' leaked into function parameters at {p}/{k}"
                        );
                    }
                    if !GEMINI_WHITELIST.contains(&k.as_str()) {
                        panic!(
                            "{ctx} Gemini invariant violated: unsupported functionDeclaration \
                             keyword '{k}' at {p}/{k}"
                        );
                    }
                }
            });
        }
    }
}
