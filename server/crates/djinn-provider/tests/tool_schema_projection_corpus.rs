//! Tool-schema projection corpus invariant integration test.
//!
//! Loads the committed JSON corpus under `tests/fixtures/tool_schema_projection/`
//! and runs every tool's `inputSchema` through every relevant
//! `(ToolSchemaCompat, FormatFamily)` projection combination, asserting the
//! strict-validator invariants required by OpenAI-family strict validators,
//! Moonshot/Kimi, and Google Gemini.
//!
//! See `tests/fixtures/tool_schema_projection/README.md` for the fixture
//! refresh path and the dependency-cycle rationale.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;

use serde_json::Value;

use djinn_provider::provider::format::tool_projection::project;
use djinn_provider::provider::{FormatFamily, ToolSchemaCompat};

const FIXTURE_DIR: &str = "tests/fixtures/tool_schema_projection";

#[derive(Debug, Clone)]
struct ToolEntry {
    source_file: String,
    group: String,
    name: String,
    schema: Value,
}

#[derive(Debug, Clone)]
struct Violation {
    tool_name: String,
    source_file: String,
    group: String,
    compat: String,
    family: String,
    pointer: String,
    message: String,
}

fn load_fixture(rel_path: &str) -> Value {
    let full = format!("{FIXTURE_DIR}/{rel_path}");
    let contents =
        fs::read_to_string(&full).unwrap_or_else(|e| panic!("failed to read fixture {full}: {e}"));
    serde_json::from_str(&contents)
        .unwrap_or_else(|e| panic!("failed to parse fixture {full} as JSON: {e}"))
}

fn load_all_tools() -> Vec<ToolEntry> {
    let manifest = load_fixture("manifest.json");
    let groups = manifest["groups"]
        .as_object()
        .expect("manifest groups must be an object");

    let mut entries = Vec::new();
    for (group, val) in groups {
        let files = val["files"]
            .as_array()
            .unwrap_or_else(|| panic!("group '{group}' has no files array"))
            .iter()
            .map(|v| {
                v.as_str()
                    .unwrap_or_else(|| panic!("manifest file entry must be a string"))
                    .to_string()
            });

        for rel in files {
            let parsed = load_fixture(&rel);
            match group.as_str() {
                "builtin" => {
                    let arr = parsed.as_array().unwrap_or_else(|| {
                        panic!("builtin fixture {rel} must be a JSON array of tool objects")
                    });
                    for tool in arr {
                        let name = tool["name"]
                            .as_str()
                            .unwrap_or_else(|| {
                                panic!("tool in builtin fixture {rel} is missing 'name'")
                            })
                            .to_string();
                        entries.push(ToolEntry {
                            source_file: rel.clone(),
                            group: group.clone(),
                            name,
                            schema: tool["inputSchema"].clone(),
                        });
                    }
                }
                "regression" => {
                    let name = parsed["name"]
                        .as_str()
                        .unwrap_or_else(|| panic!("regression fixture {rel} is missing 'name'"))
                        .to_string();
                    entries.push(ToolEntry {
                        source_file: rel.clone(),
                        group: group.clone(),
                        name,
                        schema: parsed["inputSchema"].clone(),
                    });
                }
                other => panic!("unknown manifest group '{other}'"),
            }
        }
    }
    entries
}

fn format_compat(c: Option<ToolSchemaCompat>) -> String {
    match c {
        None => "None".to_string(),
        Some(ToolSchemaCompat::OpenAi) => "OpenAi".to_string(),
        Some(ToolSchemaCompat::Moonshot) => "Moonshot".to_string(),
        Some(ToolSchemaCompat::Gemini) => "Gemini".to_string(),
    }
}

fn format_family(f: FormatFamily) -> String {
    match f {
        FormatFamily::OpenAI => "OpenAI",
        FormatFamily::OpenAIResponses => "OpenAIResponses",
        FormatFamily::Anthropic => "Anthropic",
        FormatFamily::Google => "Google",
    }
    .to_string()
}

fn encode_json_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Walk the schema nodes that can contain sub-schemas, skipping value-only
/// keywords (`default`, `examples`, `enum`) and `$ref` siblings (which are
/// ignored by JSON Schema reference semantics).
fn walk_schema_nodes<F: FnMut(&Value, &str)>(node: &Value, pointer: &str, visitor: &mut F) {
    visitor(node, pointer);

    let Some(obj) = node.as_object() else {
        return;
    };

    for (key, value) in obj {
        // $ref siblings are not evaluated as schemas; do not recurse into them.
        if key == "$ref" {
            continue;
        }

        let child_ptr = if pointer.is_empty() {
            format!("/{}", encode_json_pointer_token(key))
        } else {
            format!("{}/{}", pointer, encode_json_pointer_token(key))
        };

        match key.as_str() {
            "properties" | "$defs" | "definitions" | "dependentSchemas" | "patternProperties" => {
                if let Some(map) = value.as_object() {
                    for (k, v) in map {
                        let ptr = format!("{}/{}", child_ptr, encode_json_pointer_token(k));
                        walk_schema_nodes(v, &ptr, visitor);
                    }
                }
            }
            "additionalProperties" | "not" | "if" | "then" | "else" | "propertyNames" => {
                walk_schema_nodes(value, &child_ptr, visitor);
            }
            "items" => {
                if value.is_array() {
                    for (i, v) in value.as_array().unwrap().iter().enumerate() {
                        walk_schema_nodes(v, &format!("{child_ptr}/{i}"), visitor);
                    }
                } else {
                    walk_schema_nodes(value, &child_ptr, visitor);
                }
            }
            "allOf" | "anyOf" | "oneOf" => {
                if let Some(arr) = value.as_array() {
                    for (i, v) in arr.iter().enumerate() {
                        walk_schema_nodes(v, &format!("{child_ptr}/{i}"), visitor);
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_openai_family(f: FormatFamily) -> bool {
    matches!(f, FormatFamily::OpenAI | FormatFamily::OpenAIResponses)
}

const GEMINI_ALLOWED: &[&str] = &[
    "type",
    "description",
    "nullable",
    "enum",
    "properties",
    "required",
    "items",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "additionalProperties",
    "anyOf",
    "$ref",
    "$defs",
    "definitions",
    "title",
    "$comment",
    "default",
    "examples",
    "format",
];

const SAFETY_ANNOTATION_KEYS: &[&str] = &[
    "readOnly",
    "destructive",
    "idempotent",
    "openWorld",
    "concurrent_safe",
];

#[test]
fn tool_schema_projection_corpus_invariants() {
    let tools = load_all_tools();
    assert!(!tools.is_empty(), "corpus must contain at least one tool");

    let compatibilities: &[Option<ToolSchemaCompat>] = &[
        None,
        Some(ToolSchemaCompat::OpenAi),
        Some(ToolSchemaCompat::Moonshot),
        Some(ToolSchemaCompat::Gemini),
    ];
    let families = &[
        FormatFamily::OpenAI,
        FormatFamily::OpenAIResponses,
        FormatFamily::Anthropic,
        FormatFamily::Google,
    ];

    let mut violations: Vec<Violation> = Vec::new();

    for tool in &tools {
        for compat in compatibilities {
            for family in families {
                let projected = project(tool.schema.clone(), *compat, *family);
                check_projected_schema(&projected, tool, *compat, *family, &mut violations);
            }
        }
    }

    if !violations.is_empty() {
        let mut msg = format!(
            "tool-schema projection corpus invariant failures ({} total):\n",
            violations.len()
        );
        for (i, v) in violations.iter().enumerate() {
            let pointer = if v.pointer.is_empty() {
                "(root)".to_string()
            } else {
                v.pointer.clone()
            };
            let _ = writeln!(
                msg,
                "{}. {}/{} (tool: {}) compat={} family={} at {}: {}",
                i + 1,
                v.group,
                v.source_file,
                v.tool_name,
                v.compat,
                v.family,
                pointer,
                v.message
            );
        }
        panic!("{msg}");
    }
}

fn check_projected_schema(
    projected: &Value,
    tool: &ToolEntry,
    compat: Option<ToolSchemaCompat>,
    family: FormatFamily,
    violations: &mut Vec<Violation>,
) {
    let compat_label = format_compat(compat);
    let family_label = format_family(family);
    let allowed: HashSet<&str> = GEMINI_ALLOWED.iter().copied().collect();

    let mut push = |pointer: &str, message: String| {
        violations.push(Violation {
            tool_name: tool.name.clone(),
            source_file: tool.source_file.clone(),
            group: tool.group.clone(),
            compat: compat_label.clone(),
            family: family_label.clone(),
            pointer: pointer.to_string(),
            message,
        });
    };

    walk_schema_nodes(projected, "", &mut |node, pointer| {
        // OpenAI-family strict validator: every object schema must have properties.
        if compat == Some(ToolSchemaCompat::OpenAi) && is_openai_family(family) {
            if node.get("type").and_then(|v| v.as_str()) == Some("object") {
                if node.get("properties").is_none() {
                    push(
                        pointer,
                        "object schema is missing 'properties' required by OpenAI-family strict validators"
                            .to_string(),
                    );
                }
            }
        }

        // Moonshot/Kimi invariants.
        if compat == Some(ToolSchemaCompat::Moonshot) {
            if let Some(obj) = node.as_object() {
                if obj.contains_key("$ref") {
                    const ALLOWED_REF_SIBLINGS: &[&str] = &[
                        "$ref",
                        "title",
                        "description",
                        "$comment",
                        "default",
                        "examples",
                    ];
                    for key in obj.keys() {
                        if !ALLOWED_REF_SIBLINGS.contains(&key.as_str()) {
                            push(
                                pointer,
                                format!("$ref has forbidden sibling keyword '{key}'"),
                            );
                        }
                    }
                }
                if obj.contains_key("prefixItems") {
                    push(
                        pointer,
                        "Moonshot-projected schema still contains 'prefixItems'".to_string(),
                    );
                }
                if obj.contains_key("unevaluatedItems") {
                    push(
                        pointer,
                        "Moonshot-projected schema still contains 'unevaluatedItems'".to_string(),
                    );
                }
            }
        }

        // Gemini invariants.
        if compat == Some(ToolSchemaCompat::Gemini) {
            if let Some(obj) = node.as_object() {
                for key in obj.keys() {
                    if !allowed.contains(key.as_str()) {
                        push(
                            pointer,
                            format!("Gemini-projected schema contains unsupported keyword '{key}'"),
                        );
                    }
                }
                for key in SAFETY_ANNOTATION_KEYS {
                    if obj.contains_key(*key) {
                        push(
                            pointer,
                            format!(
                                "Gemini function parameters contain djinn safety annotation key '{key}'"
                            ),
                        );
                    }
                }
            }
        }
    });
}
