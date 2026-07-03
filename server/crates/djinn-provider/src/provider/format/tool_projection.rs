//! Pure, deterministic tool-schema projection.
//!
//! Given a JSON Schema value, an optional [`ToolSchemaCompat`] quirk, and a
//! target [`FormatFamily`], the [`project`] entry point returns a rewritten
//! schema suitable for the provider's tool-definition format.
//!
//! **Compatibility rewrites are applied first**, then family rewrites.  When
//! the compatibility is `None`, the projection is identity (the schema is
//! returned unchanged).

use serde_json::{Value, json};

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
        Some(ToolSchemaCompat::Gemini) => apply_gemini_compat(schema),
        Some(ToolSchemaCompat::OpenAi) => apply_openai_compat(schema),
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

// ─── Gemini compatibility ───────────────────────────────────────────────────

/// Apply all Gemini compatibility rewrites recursively.
///
/// 1. Whitelist supported JSON Schema keywords; remove unsupported keys.
/// 2. Coerce `enum` arrays into the provider-compatible single string type.
/// 3. Filter `required` so it only lists keys that actually exist in `properties`.
/// 4. Strip `null` variants from `anyOf`/`oneOf` nullable unions and coerce them
///    to the Gemini representation.
fn apply_gemini_compat(mut schema: Value) -> Value {
    rewrite_gemini_recursive(&mut schema);
    schema
}

/// Recursively apply Gemini rewrites in-place.
fn rewrite_gemini_recursive(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    // 1. Keyword whitelist: keep only the keywords Gemini's function-declaration
    //    schema format supports.  Structural/value keywords that are harmless and
    //    widely used are retained; anything that is unsupported or rejected
    //    is removed.
    const WHITELIST: &[&str] = &[
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
        // Combinators (allowed, but nullable null variants are handled below)
        "anyOf",
        // References and local definitions (kept so `$ref` schemas resolve; the
        // unsupported-key stripping is about validation keywords, not structure)
        "$ref",
        "$defs",
        "definitions",
        // Gemini/JSON Schema title metadata
        "title",
        "$comment",
        "default",
        "examples",
        "format",
    ];
    obj.retain(|k, _| WHITELIST.contains(&k.as_str()));

    // 2. Enum-to-string coercion: Gemini expects a single string type, not a
    //    JSON Schema `enum` array.  Convert `{"type": "string", "enum": [...]}`
    //    into `{"type": "string", "description": "... (one of: ...)", ...}` so the
    //    values are preserved as human-readable guidance without the unsupported
    //    enum keyword.
    if let Some("string") = obj.get("type").and_then(|v| v.as_str())
        && let Some(Value::Array(values)) = obj.remove("enum")
    {
        let joined = values
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            let existing = obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let desc = if existing.is_empty() {
                format!("Must be one of: {joined}.")
            } else {
                format!("{existing} Must be one of: {joined}.")
            };
            obj.insert("description".to_string(), Value::String(desc));
        }
    }

    // 3. Required filtering: only keep keys that exist in `properties`.
    let properties_keys: std::collections::HashSet<String> = obj
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(Value::Array(required)) = obj.get_mut("required") {
        required.retain(|v| {
            v.as_str()
                .map(|name| properties_keys.contains(name))
                .unwrap_or(false)
        });
        // Drop empty required arrays.
        if required.is_empty() {
            obj.remove("required");
        }
    }

    // 4. Nullable handling: for `anyOf` schemas that contain a null variant,
    //    strip the null variant, set `nullable: true`, and if the remaining
    //    schema is a single branch, flatten it.  This is the most compatible
    //    representation for Gemini.
    if let Some(Value::Array(branches)) = obj.get_mut("anyOf") {
        // Remove null variants first.
        branches.retain(|branch| !is_null_schema(branch));

        // Flatten nested single-branch anyOf schemas.
        if branches.len() == 1 {
            let mut single = branches.remove(0).clone();
            if let Some(inner) = single.as_object_mut() {
                // Merge nullable and non-conflicting keys into the outer schema.
                inner.insert("nullable".to_string(), Value::Bool(true));
                // Replace outer with inner but keep a description/title if outer
                // had one and inner doesn't.
                let preserved_title = obj.get("title").cloned();
                let preserved_desc = obj.get("description").cloned();
                *obj = inner.clone();
                if obj.get("title").is_none()
                    && let Some(t) = preserved_title
                {
                    obj.insert("title".to_string(), t);
                }
                if obj.get("description").is_none()
                    && let Some(d) = preserved_desc
                {
                    obj.insert("description".to_string(), d);
                }
            }
        } else if !branches.is_empty() {
            // Multi-branch non-null anyOf remains anyOf; mark nullable.
            obj.insert("nullable".to_string(), Value::Bool(true));
        }
    }

    // Recurse into sub-schemas.
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            rewrite_gemini_recursive(v);
        }
    }

    if let Some(v) = obj.get_mut("additionalProperties") {
        rewrite_gemini_recursive(v);
    }

    if let Some(v) = obj.get_mut("items") {
        match v {
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    rewrite_gemini_recursive(item);
                }
            }
            other => rewrite_gemini_recursive(other),
        }
    }

    for keyword in &["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = obj.get_mut(*keyword) {
            for item in arr.iter_mut() {
                rewrite_gemini_recursive(item);
            }
        }
    }

    if let Some(v) = obj.get_mut("not") {
        rewrite_gemini_recursive(v);
    }

    for defs_key in &["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(*defs_key) {
            for (_, v) in defs.iter_mut() {
                rewrite_gemini_recursive(v);
            }
        }
    }

    for keyword in &["if", "then", "else"] {
        if let Some(v) = obj.get_mut(*keyword) {
            rewrite_gemini_recursive(v);
        }
    }

    if let Some(Value::Object(ds)) = obj.get_mut("dependentSchemas") {
        for (_, v) in ds.iter_mut() {
            rewrite_gemini_recursive(v);
        }
    }

    if let Some(Value::Object(pp)) = obj.get_mut("patternProperties") {
        for (_, v) in pp.iter_mut() {
            rewrite_gemini_recursive(v);
        }
    }

    if let Some(v) = obj.get_mut("propertyNames") {
        rewrite_gemini_recursive(v);
    }
}

/// Returns true when the schema is the null type schema `{ "type": "null" }`.
fn is_null_schema(schema: &Value) -> bool {
    schema
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| t == "null")
        .unwrap_or(false)
}

// ─── OpenAI-family compatibility ──────────────────────────────────────────────

/// Apply OpenAI-family compatibility rewrites recursively.
///
/// 1. Deeply enforce object `properties`: every object schema that lacks
///    `properties` gains an empty `properties` object.
/// 2. Top-level `anyOf` flattening: if the top-level schema is an `anyOf`
///    containing a null variant, remove the null variant and set `nullable`.
///    For a single remaining branch, flatten it into the top-level schema.
fn apply_openai_compat(mut schema: Value) -> Value {
    rewrite_openai_recursive(&mut schema);
    // Flatten top-level anyOf/null-variant schemas into a single object schema
    // when possible, matching the prior `ensure_object_properties` behavior but
    // generalized.
    flatten_top_level_anyof(&mut schema);
    schema
}

/// Recursively apply OpenAI object-shape enforcement in-place.
fn rewrite_openai_recursive(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    if obj.get("type").and_then(|v| v.as_str()) == Some("object") {
        obj.entry("properties").or_insert_with(|| json!({}));
    }

    // Recurse into sub-schemas.
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            rewrite_openai_recursive(v);
        }
    }

    if let Some(v) = obj.get_mut("additionalProperties") {
        rewrite_openai_recursive(v);
    }

    if let Some(v) = obj.get_mut("items") {
        match v {
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    rewrite_openai_recursive(item);
                }
            }
            other => rewrite_openai_recursive(other),
        }
    }

    for keyword in &["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(arr)) = obj.get_mut(*keyword) {
            for item in arr.iter_mut() {
                rewrite_openai_recursive(item);
            }
        }
    }

    if let Some(v) = obj.get_mut("not") {
        rewrite_openai_recursive(v);
    }

    for defs_key in &["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get_mut(*defs_key) {
            for (_, v) in defs.iter_mut() {
                rewrite_openai_recursive(v);
            }
        }
    }

    for keyword in &["if", "then", "else"] {
        if let Some(v) = obj.get_mut(*keyword) {
            rewrite_openai_recursive(v);
        }
    }

    if let Some(Value::Object(ds)) = obj.get_mut("dependentSchemas") {
        for (_, v) in ds.iter_mut() {
            rewrite_openai_recursive(v);
        }
    }

    if let Some(Value::Object(pp)) = obj.get_mut("patternProperties") {
        for (_, v) in pp.iter_mut() {
            rewrite_openai_recursive(v);
        }
    }

    if let Some(v) = obj.get_mut("propertyNames") {
        rewrite_openai_recursive(v);
    }
}

/// Flatten top-level `anyOf` schemas that contain a null variant.
///
/// If the top-level schema is an `anyOf` containing a null variant, remove the
/// null variant and mark the schema as `nullable`.  When the remaining schema
/// consists of a single non-null branch, flatten that branch into the top-level
/// schema so object properties enforcement still applies.
fn flatten_top_level_anyof(schema: &mut Value) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };

    let Some(Value::Array(branches)) = obj.get_mut("anyOf") else {
        return;
    };

    let non_null: Vec<Value> = branches
        .iter()
        .filter(|b| !is_null_schema(b))
        .cloned()
        .collect();

    let had_null = branches.len() != non_null.len();

    match non_null.len() {
        0 => {
            // All branches were null: keep as null type schema.
            obj.clear();
            obj.insert("type".to_string(), Value::String("null".to_string()));
            if had_null {
                obj.insert("nullable".to_string(), Value::Bool(true));
            }
        }
        1 => {
            // Single non-null branch: flatten into the top-level schema.
            let mut inner = non_null.into_iter().next().unwrap();
            // Recurse on the inner schema first so deep enforcement applies.
            rewrite_openai_recursive(&mut inner);
            if let Some(inner_obj) = inner.as_object_mut() {
                // Preserve outer keys not in the inner schema, except anyOf.
                let preserved_title = obj.get("title").cloned();
                let preserved_desc = obj.get("description").cloned();
                *obj = inner_obj.clone();
                if obj.get("title").is_none()
                    && let Some(t) = preserved_title
                {
                    obj.insert("title".to_string(), t);
                }
                if obj.get("description").is_none()
                    && let Some(d) = preserved_desc
                {
                    obj.insert("description".to_string(), d);
                }
            }
            if had_null {
                obj.insert("nullable".to_string(), Value::Bool(true));
            }
        }
        _ => {
            // Multiple non-null branches: keep anyOf, mark nullable if needed.
            *branches = non_null;
            if had_null {
                obj.insert("nullable".to_string(), Value::Bool(true));
            }
        }
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

    // ── Moonshot: prefixItems / tuple collapse ─────────────────────────────

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

    // ── Moonshot: unevaluatedItems removal ─────────────────────────────────

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
        assert!(output["then"].get("unevaluatedItems").is_none());

        // else branch: prefixItems collapse + unevaluatedItems removal
        assert!(output["else"].get("prefixItems").is_none());
        assert!(output["else"].get("unevaluatedItems").is_none());
        let items = output["else"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
    }

    // ── Gemini: keyword whitelist and unsupported-key removal ───────────────

    #[test]
    fn gemini_whitelist_keeps_supported_keywords() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 1 }
            },
            "required": ["name"],
            "additionalProperties": false
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert_eq!(output["type"], "object");
        assert!(output["properties"]["name"].get("type").is_some());
        assert!(output["properties"]["name"].get("minLength").is_some());
        assert!(output.get("additionalProperties").is_some());
    }

    #[test]
    fn gemini_removes_unsupported_keywords() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "unevaluatedProperties": false,
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "const": "nope"
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert!(output.get("unevaluatedProperties").is_none());
        assert!(output.get("$schema").is_none());
        assert!(output.get("const").is_none());
    }

    #[test]
    fn gemini_removes_unsupported_keywords_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "child": {
                    "type": "object",
                    "properties": { "x": { "type": "integer" } },
                    "unevaluatedProperties": false
                }
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let child = &output["properties"]["child"];
        assert!(child.get("unevaluatedProperties").is_none());
        assert!(child["properties"]["x"].get("type").is_some());
    }

    // ── Gemini: enum-to-string coercion ────────────────────────────────────

    #[test]
    fn gemini_coerces_enum_to_description() {
        let schema = json!({
            "type": "string",
            "enum": ["red", "green", "blue"]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert!(output.get("enum").is_none());
        assert_eq!(output["type"], "string");
        let desc = output["description"].as_str().unwrap();
        assert!(desc.contains("red"));
        assert!(desc.contains("green"));
        assert!(desc.contains("blue"));
    }

    #[test]
    fn gemini_appends_enum_to_existing_description() {
        let schema = json!({
            "type": "string",
            "description": "Pick a color.",
            "enum": ["red", "green"]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let desc = output["description"].as_str().unwrap();
        assert!(desc.starts_with("Pick a color."));
        assert!(desc.contains("red"));
        assert!(desc.contains("green"));
    }

    #[test]
    fn gemini_coerces_nested_enum() {
        let schema = json!({
            "type": "object",
            "properties": {
                "color": { "type": "string", "enum": ["a", "b"] }
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let color = &output["properties"]["color"];
        assert!(color.get("enum").is_none());
        assert!(color["description"].as_str().unwrap().contains("a"));
    }

    // ── Gemini: required filtering ─────────────────────────────────────────

    #[test]
    fn gemini_filters_required_to_existing_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" }
            },
            "required": ["a", "b", "c"]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let required = output["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "a");
    }

    #[test]
    fn gemini_drops_required_when_empty() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" }
            },
            "required": ["b"]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert!(output.get("required").is_none());
    }

    #[test]
    fn gemini_filters_required_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "child": {
                    "type": "object",
                    "properties": { "x": { "type": "integer" } },
                    "required": ["x", "y"]
                }
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let child = &output["properties"]["child"];
        let required = child["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "x");
    }

    // ── Gemini: nullable handling ──────────────────────────────────────────

    #[test]
    fn gemini_strips_null_anyof_variant_and_sets_nullable() {
        let schema = json!({
            "anyOf": [
                { "type": "string" },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert!(output.get("anyOf").is_none());
        assert_eq!(output["type"], "string");
        assert_eq!(output["nullable"], true);
    }

    #[test]
    fn gemini_keeps_multi_branch_anyof_after_null_removal() {
        let schema = json!({
            "anyOf": [
                { "type": "string" },
                { "type": "integer" },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        let branches = output["anyOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert!(branches.iter().all(|b| b.get("type").is_some()));
        assert_eq!(output["nullable"], true);
    }

    #[test]
    fn gemini_nullable_handling_preserves_description() {
        let schema = json!({
            "description": "Maybe a string",
            "anyOf": [
                { "type": "string" },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::Gemini), FormatFamily::OpenAI);

        assert_eq!(output["description"], "Maybe a string");
        assert_eq!(output["type"], "string");
        assert_eq!(output["nullable"], true);
    }

    // ── OpenAI: deep object properties enforcement ───────────────────────────

    #[test]
    fn openai_enforces_object_properties_top_level() {
        let schema = json!({
            "type": "object",
            "required": ["name"]
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert!(output.get("properties").is_some());
        assert!(output["properties"].is_object());
        assert_eq!(output["required"], json!(["name"]));
    }

    #[test]
    fn openai_enforces_object_properties_deeply() {
        let schema = json!({
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "required": ["id"]
                }
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        let user = &output["properties"]["user"];
        assert!(user.get("properties").is_some());
        assert!(user["properties"].is_object());
    }

    #[test]
    fn openai_enforces_object_properties_in_items() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["x"]
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert!(output["items"]["properties"].is_object());
    }

    #[test]
    fn openai_preserves_existing_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert_eq!(output["properties"]["name"]["type"], "string");
    }

    // ── OpenAI: top-level anyOf flattening / null-variant stripping ──────────

    #[test]
    fn openai_flattens_top_level_nullable_anyof() {
        let schema = json!({
            "anyOf": [
                { "type": "object", "properties": { "x": { "type": "string" } } },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert!(output.get("anyOf").is_none());
        assert_eq!(output["type"], "object");
        assert!(output["properties"].is_object());
        assert!(output["properties"]["x"].get("type").is_some());
        assert_eq!(output["nullable"], true);
    }

    #[test]
    fn openai_keeps_multi_branch_top_level_anyof() {
        let schema = json!({
            "anyOf": [
                { "type": "string" },
                { "type": "integer" },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        let branches = output["anyOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(output["nullable"], true);
    }

    #[test]
    fn openai_all_null_anyof_becomes_null_type() {
        let schema = json!({
            "anyOf": [
                { "type": "null" },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert_eq!(output["type"], "null");
        assert_eq!(output["nullable"], true);
    }

    #[test]
    fn openai_flattened_object_properties_enforced() {
        // Top-level anyOf flattened to object, then properties enforced.
        let schema = json!({
            "anyOf": [
                { "type": "object", "required": ["x"] },
                { "type": "null" }
            ]
        });
        let output = project(schema, Some(ToolSchemaCompat::OpenAi), FormatFamily::OpenAI);

        assert_eq!(output["type"], "object");
        assert!(output["properties"].is_object());
        assert_eq!(output["nullable"], true);
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

    #[test]
    fn compat_family_composition_is_pure() {
        // Verify that applying a compat and then another compat yields the same
        // result as applying only the last compat (projection is idempotent in
        // the no-op family step).
        let schema = json!({
            "type": "object",
            "properties": {
                "color": { "type": "string", "enum": ["red", "green"] }
            }
        });
        let first = project(
            schema.clone(),
            Some(ToolSchemaCompat::Gemini),
            FormatFamily::OpenAI,
        );
        let second = project(first.clone(), None, FormatFamily::OpenAI);
        assert_eq!(first, second);
    }
}
