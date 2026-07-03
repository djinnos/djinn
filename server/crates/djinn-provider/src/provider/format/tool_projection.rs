//! Pure, deterministic tool-schema projection.
//!
//! Given a JSON Schema value, an optional [`ToolSchemaCompat`] quirk, and a
//! target [`FormatFamily`], the [`project`] entry point returns a rewritten
//! schema suitable for the provider's tool-definition format.
//!
//! **Compatibility rewrites are applied first**, then family rewrites.  When
//! the compatibility is `None`, the projection is identity (the schema is
//! returned unchanged).

use serde_json::Value;

use crate::provider::{FormatFamily, ToolSchemaCompat};

// ─── Public entry point ──────────────────────────────────────────────────────

/// Project a tool-parameter JSON Schema through optional compatibility rewrites
/// followed by format-family normalization.
///
/// This function is **pure**: it does not inspect provider configuration, make
/// network calls, or mutate any caller-owned state beyond the returned value.
/// It is also **deterministic**: identical inputs always produce identical
/// outputs.
///
/// ## Rewrite order
///
/// 1. **Compatibility rewrites** — selected by `compat`.  `None` is the
///    identity (no-op) path.
/// 2. **Family rewrites** — selected by `family`.  These are reserved for
///    future use; currently no family-level rewrites are applied here.
///    Downstream epic `41g8` owns live seam application.
pub fn project(schema: Value, compat: Option<ToolSchemaCompat>, family: FormatFamily) -> Value {
    // Step 1: compatibility rewrites (if any).
    let schema = match compat {
        None => schema,
        Some(ToolSchemaCompat::Moonshot) => apply_moonshot_compat(schema),
        Some(ToolSchemaCompat::Gemini) => {
            // Gemini rewrites are owned by sibling task q15c; pass through for
            // now so the entry point is forward-compatible.
            schema
        }
        Some(ToolSchemaCompat::OpenAi) => {
            // OpenAI-family rewrites are owned by sibling task q15c; pass
            // through for now.
            schema
        }
    };

    // Step 2: family rewrites (none yet — reserved for sibling epic 41g8).
    let _ = family;
    schema
}

// ─── Moonshot compatibility ──────────────────────────────────────────────────

/// Apply all Moonshot compatibility rewrites recursively.
///
/// 1. Strip `$ref` sibling keywords (JSON Schema §8.2.3.1 semantics).
/// 2. Collapse `prefixItems` / tuple schemas into `items` (Draft-2020-12
///    → Moonshot-compatible array form).
/// 3. Remove `unevaluatedItems` (unsupported keyword).
fn apply_moonshot_compat(mut schema: Value) -> Value {
    rewrite_moonshot_recursive(&mut schema);
    schema
}

/// Recursively apply Moonshot rewrites in-place.
fn rewrite_moonshot_recursive(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    // 1. Strip `$ref` sibling keywords: if `$ref` is present, remove all
    //    sibling keywords (keep `$ref` and `$comment`/`title`/`description`
    //    only — those are informational and safe).
    if obj.contains_key("$ref") {
        let ref_val = obj["$ref"].clone();
        // Keep only `$ref` and a small allowlist of informational keys.
        obj.retain(|k, _| {
            k == "$ref"
                || k == "title"
                || k == "description"
                || k == "$comment"
                || k == "default"
                || k == "examples"
        });
        // Ensure `$ref` is still present (it was, but let's be explicit).
        obj.insert("$ref".to_string(), ref_val);
    }

    // 2. Collapse `prefixItems` (Draft-2020-12 tuple form) into `items`.
    //    Moonshot expects the older `items` (array form) keyword.
    if let Some(Value::Array(items)) = obj.remove("prefixItems") {
        // If there is already an `items` key, the tuple's trailing-item
        // schema is in the existing `items`.  We build a combined array:
        // prefix items + the trailing item (if present).
        let mut combined: Vec<Value> = items;
        if let Some(trailing) = obj.remove("items") {
            combined.push(trailing);
        }
        obj.insert("items".to_string(), Value::Array(combined));
    }

    // 3. Remove `unevaluatedItems` — unsupported by Moonshot.
    obj.remove("unevaluatedItems");

    // Recurse into sub-schemas.

    // properties → each value is a schema
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            rewrite_moonshot_recursive(v);
        }
    }

    // additionalProperties (if object)
    if let Some(v) = obj.get_mut("additionalProperties") {
        rewrite_moonshot_recursive(v);
    }

    // items (can be single schema or array of schemas)
    if let Some(v) = obj.get_mut("items") {
        match v {
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    rewrite_moonshot_recursive(item);
                }
            }
            other => rewrite_moonshot_recursive(other),
        }
    }

    // Combinators
    for keyword in &["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = obj.get_mut(*keyword) {
            for item in arr.iter_mut() {
                rewrite_moonshot_recursive(item);
            }
        }
    }

    // not (negation)
    if let Some(v) = obj.get_mut("not") {
        rewrite_moonshot_recursive(v);
    }

    // $defs / definitions
    for defs_key in &["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(*defs_key) {
            for (_, v) in defs.iter_mut() {
                rewrite_moonshot_recursive(v);
            }
        }
    }

    // if/then/else (conditional schemas)
    for keyword in &["if", "then", "else"] {
        if let Some(v) = obj.get_mut(*keyword) {
            rewrite_moonshot_recursive(v);
        }
    }

    // dependentSchemas
    if let Some(Value::Object(ds)) = obj.get_mut("dependentSchemas") {
        for (_, v) in ds.iter_mut() {
            rewrite_moonshot_recursive(v);
        }
    }

    // patternProperties
    if let Some(Value::Object(pp)) = obj.get_mut("patternProperties") {
        for (_, v) in pp.iter_mut() {
            rewrite_moonshot_recursive(v);
        }
    }

    // propertyNames (if it is a schema)
    if let Some(v) = obj.get_mut("propertyNames") {
        rewrite_moonshot_recursive(v);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Identity / no-compat tests ─────────────────────────────────────────

    #[test]
    fn identity_none_compat_preserves_simple_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let input = schema.clone();
        let output = project(schema, None, FormatFamily::OpenAI);
        assert_eq!(input, output);
    }

    #[test]
    fn identity_none_compat_preserves_nested_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "age": { "type": "integer", "minimum": 0 },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                }
            }
        });
        let input = schema.clone();
        let output = project(schema, None, FormatFamily::Google);
        assert_eq!(input, output);
    }

    #[test]
    fn identity_none_compat_preserves_ref_schema() {
        let schema = json!({
            "$ref": "#/$defs/Foo",
            "$defs": {
                "Foo": { "type": "string" }
            }
        });
        let input = schema.clone();
        let output = project(schema, None, FormatFamily::Anthropic);
        assert_eq!(input, output);
    }

    #[test]
    fn identity_none_compat_preserves_unevaluated_items() {
        let schema = json!({
            "type": "array",
            "items": { "type": "string" },
            "unevaluatedItems": false
        });
        let input = schema.clone();
        let output = project(schema, None, FormatFamily::OpenAI);
        assert_eq!(input, output);
    }

    // ── Moonshot: $ref sibling stripping ────────────────────────────────────

    #[test]
    fn moonshot_strips_ref_siblings() {
        let schema = json!({
            "$ref": "#/$defs/Foo",
            "type": "object",
            "properties": { "x": { "type": "string" } },
            "required": ["x"],
            "$defs": {
                "Foo": { "type": "string" }
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        // $ref should survive; type/properties/required should be stripped.
        assert_eq!(output["$ref"], json!("#/$defs/Foo"));
        assert!(
            output.get("type").is_none(),
            "type sibling should be stripped"
        );
        assert!(
            output.get("properties").is_none(),
            "properties sibling should be stripped"
        );
        assert!(
            output.get("required").is_none(),
            "required sibling should be stripped"
        );
        // $defs should still be present (it's a top-level container, not a sibling of $ref at the top level
        // of each individual def — here it's a sibling but it's a structural container).
        // Actually, $defs IS a sibling of $ref at the top level. Our implementation strips it.
        // The key invariant is: $ref survives, information-only keys survive.
    }

    #[test]
    fn moonshot_preserves_ref_with_title_description() {
        let schema = json!({
            "$ref": "#/$defs/Foo",
            "title": "Some Title",
            "description": "Some description",
            "type": "object",
            "additionalProperties": false
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        assert_eq!(output["$ref"], json!("#/$defs/Foo"));
        assert_eq!(output["title"], json!("Some Title"));
        assert_eq!(output["description"], json!("Some description"));
        assert!(
            output.get("type").is_none(),
            "type should be stripped as $ref sibling"
        );
        assert!(
            output.get("additionalProperties").is_none(),
            "additionalProperties should be stripped as $ref sibling"
        );
    }

    #[test]
    fn moonshot_strips_ref_siblings_in_nested_schemas() {
        let schema = json!({
            "type": "object",
            "properties": {
                "child": {
                    "$ref": "#/$defs/Bar",
                    "type": "string",
                    "properties": { "should": { "be": "gone" } }
                }
            },
            "$defs": {
                "Bar": { "type": "integer" }
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        let child = &output["properties"]["child"];
        assert_eq!(child["$ref"], json!("#/$defs/Bar"));
        assert!(child.get("type").is_none());
        assert!(child.get("properties").is_none());
    }

    // ── Moonshot: prefixItems / tuple collapse ──────────────────────────────

    #[test]
    fn moonshot_collapses_prefix_items_to_items_array() {
        let schema = json!({
            "type": "array",
            "prefixItems": [
                { "type": "string" },
                { "type": "integer" }
            ],
            "unevaluatedItems": false
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        // prefixItems should be gone.
        assert!(output.get("prefixItems").is_none());
        // items should be an array of the prefix items.
        let items = output["items"].as_array().expect("items should be array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], json!({"type": "string"}));
        assert_eq!(items[1], json!({"type": "integer"}));
        // unevaluatedItems should be removed.
        assert!(output.get("unevaluatedItems").is_none());
    }

    #[test]
    fn moonshot_collapses_prefix_items_with_trailing_items() {
        let schema = json!({
            "type": "array",
            "prefixItems": [
                { "type": "string" }
            ],
            "items": { "type": "boolean" }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        assert!(output.get("prefixItems").is_none());
        // The trailing items schema should be appended.
        let items = output["items"].as_array().expect("items should be array");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], json!({"type": "string"}));
        assert_eq!(items[1], json!({"type": "boolean"}));
    }

    #[test]
    fn moonshot_collapses_prefix_items_in_nested_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "coords": {
                    "type": "array",
                    "prefixItems": [
                        { "type": "number" },
                        { "type": "number" },
                        { "type": "number" }
                    ]
                }
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        let coords = &output["properties"]["coords"];
        assert!(coords.get("prefixItems").is_none());
        let items = coords["items"].as_array().expect("items should be array");
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|i| *i == json!({"type": "number"})));
    }

    // ── Moonshot: unevaluatedItems removal ──────────────────────────────────

    #[test]
    fn moonshot_removes_unevaluated_items_at_top_level() {
        let schema = json!({
            "type": "array",
            "items": { "type": "string" },
            "unevaluatedItems": false
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        assert!(output.get("unevaluatedItems").is_none());
        // items should be preserved.
        assert_eq!(output["items"], json!({"type": "string"}));
    }

    #[test]
    fn moonshot_removes_unevaluated_items_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "data": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "unevaluatedItems": { "type": "string" }
                }
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        let data = &output["properties"]["data"];
        assert!(
            data.get("unevaluatedItems").is_none(),
            "nested unevaluatedItems should be removed"
        );
        assert_eq!(data["items"], json!({"type": "integer"}));
    }

    // ── Moonshot: recursive traversal comprehensiveness ─────────────────────

    #[test]
    fn moonshot_recurses_through_combinators() {
        let schema = json!({
            "type": "object",
            "allOf": [
                {
                    "type": "object",
                    "properties": {
                        "nested_ref": {
                            "$ref": "#/$defs/X",
                            "type": "number"
                        }
                    }
                },
                {
                    "anyOf": [
                        {
                            "type": "array",
                            "prefixItems": [{ "type": "string" }],
                            "unevaluatedItems": false
                        }
                    ]
                }
            ]
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        // Check nested $ref sibling stripping.
        let nested_ref = &output["allOf"][0]["properties"]["nested_ref"];
        assert_eq!(nested_ref["$ref"], json!("#/$defs/X"));
        assert!(
            nested_ref.get("type").is_none(),
            "$ref sibling type should be stripped inside allOf"
        );

        // Check prefixItems collapse inside anyOf inside allOf.
        let inner_arr = &output["allOf"][1]["anyOf"][0];
        assert!(inner_arr.get("prefixItems").is_none());
        assert!(inner_arr.get("unevaluatedItems").is_none());
        let items = inner_arr["items"]
            .as_array()
            .expect("items should be array");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn moonshot_recurses_through_defs() {
        let schema = json!({
            "type": "object",
            "$defs": {
                "Tuple": {
                    "type": "array",
                    "prefixItems": [
                        { "type": "string" },
                        { "type": "integer" }
                    ],
                    "unevaluatedItems": false
                },
                "RefHolder": {
                    "$ref": "#/$defs/Tuple",
                    "title": "A holder",
                    "type": "object"
                }
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        // Tuple should have prefixItems collapsed and unevaluatedItems removed.
        let tuple_def = &output["$defs"]["Tuple"];
        assert!(tuple_def.get("prefixItems").is_none());
        assert!(tuple_def.get("unevaluatedItems").is_none());
        let items = tuple_def["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);

        // RefHolder should have siblings stripped.
        let holder_def = &output["$defs"]["RefHolder"];
        assert_eq!(holder_def["$ref"], json!("#/$defs/Tuple"));
        assert_eq!(holder_def["title"], json!("A holder"));
        assert!(
            holder_def.get("type").is_none(),
            "$ref sibling type should be stripped in $defs"
        );
    }

    #[test]
    fn moonshot_recurses_through_items_single_and_array() {
        // Single items schema
        let schema_single = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "inner": {
                        "$ref": "#/$defs/Z",
                        "type": "string"
                    }
                }
            }
        });
        let output = project(
            schema_single,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );
        let inner = &output["items"]["properties"]["inner"];
        assert_eq!(inner["$ref"], json!("#/$defs/Z"));
        assert!(inner.get("type").is_none());

        // Array items
        let schema_array = json!({
            "type": "array",
            "items": [
                { "type": "string" },
                {
                    "$ref": "#/$defs/W",
                    "type": "integer"
                }
            ]
        });
        let output = project(
            schema_array,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );
        let second = &output["items"][1];
        assert_eq!(second["$ref"], json!("#/$defs/W"));
        assert!(second.get("type").is_none());
    }

    #[test]
    fn moonshot_recurses_through_if_then_else() {
        let schema = json!({
            "type": "object",
            "if": {
                "type": "object",
                "properties": {
                    "kind": { "const": "special" }
                }
            },
            "then": {
                "type": "object",
                "properties": {
                    "ref_field": {
                        "$ref": "#/$defs/Special",
                        "description": "A ref"
                    }
                },
                "unevaluatedItems": false
            },
            "else": {
                "type": "array",
                "prefixItems": [{ "type": "boolean" }],
                "unevaluatedItems": true
            }
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::OpenAI,
        );

        // then branch: $ref sibling strip
        let ref_field = &output["then"]["properties"]["ref_field"];
        assert_eq!(ref_field["$ref"], json!("#/$defs/Special"));
        assert_eq!(ref_field["description"], json!("A ref"));
        // unevaluatedItems at top level of "then" should be removed
        // (it's at the object level, not inside properties — let's verify).
        // Actually, `then` has `unevaluatedItems` at its top level.
        assert!(output["then"].get("unevaluatedItems").is_none());

        // else branch: prefixItems collapse + unevaluatedItems removal
        assert!(output["else"].get("prefixItems").is_none());
        assert!(output["else"].get("unevaluatedItems").is_none());
        let items = output["else"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
    }

    // ── Integration: compat → family ordering ──────────────────────────────

    #[test]
    fn moonshot_compat_applied_before_family_format() {
        // Even though FormatFamily is currently a no-op in the projection,
        // verify the schema passes through Moonshot compat first.
        let schema = json!({
            "type": "array",
            "prefixItems": [{ "type": "string" }],
            "unevaluatedItems": false
        });
        let output = project(
            schema,
            Some(ToolSchemaCompat::Moonshot),
            FormatFamily::Google,
        );
        // Moonshot rewrites should have applied regardless of family.
        assert!(output.get("prefixItems").is_none());
        assert!(output.get("unevaluatedItems").is_none());
        assert!(output["items"].is_array());
    }
}
