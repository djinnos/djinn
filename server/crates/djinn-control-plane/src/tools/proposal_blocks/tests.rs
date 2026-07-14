//! Tests for the proposal block registry, macro-expanded catalog, and parser.

use std::collections::HashMap;

use super::catalog::CANONICAL_BLOCK_TYPES;
use super::*;

#[test]
fn registry_contains_v1_blocks() {
    let registry = proposal_block_registry();
    assert_eq!(registry.len(), 14);
    assert_eq!(registry["rich-text"].tag, "RichText");
    assert_eq!(registry["diagram"].tag, "Diagram");
    assert_eq!(registry["annotated-code"].tag, "AnnotatedCode");
    assert_eq!(registry["api-endpoint"].tag, "ApiEndpoint");
    assert_eq!(registry["decisions"].tag, "Decisions");
    assert_eq!(registry["file-tree"].tag, "FileTree");
    assert_eq!(registry["diff"].tag, "Diff");
    assert_eq!(registry["callout"].tag, "Callout");
    assert_eq!(registry["checklist"].tag, "Checklist");
    assert_eq!(registry["json-explorer"].tag, "JsonExplorer");
    assert_eq!(registry["tabs"].tag, "Tabs");
    assert_eq!(registry["columns"].tag, "Columns");
    assert_eq!(registry["wireframe"].tag, "Wireframe");
    assert_eq!(registry["question-form"].tag, "QuestionForm");
    // The diff block ships authoring guidance for the LLM.
    assert!(
        registry["diff"]
            .description
            .is_some_and(|d| d.contains("unified diff")),
        "diff block must advertise unified-diff authoring guidance"
    );
    // The annotated-code block documents its `code`-attribute authoring format
    // (code with `<`/`{` cannot sit in children; the renderer reads the attr).
    assert!(
        registry["annotated-code"]
            .description
            .is_some_and(|d| d.contains("code={") && d.contains("annotations")),
        "annotated-code block must advertise its code-attribute authoring format"
    );
    // The tabs block documents its JSON-array `tabs` authoring format.
    assert_eq!(registry["tabs"].fields["tabs"].field_type, "string");
    assert!(
        registry["tabs"]
            .description
            .is_some_and(|d| d.contains("tabs={[") && d.contains("body")),
        "tabs block must advertise its JSON-array authoring format"
    );
    // The columns block documents its JSON-array `columns` authoring format.
    assert_eq!(registry["columns"].fields["columns"].field_type, "string");
    assert!(
        registry["columns"]
            .description
            .is_some_and(|d| d.contains("columns={[") && d.contains("body")),
        "columns block must advertise its JSON-array authoring format"
    );
    // The wireframe block documents its `surface` enum + the ASCII / box-drawing
    // authoring contract (HTML/`--wf-*`/`data-icon` are gone).
    assert_eq!(
        registry["wireframe"].fields["surface"]
            .enum_values
            .as_deref(),
        Some(["browser", "desktop", "mobile", "popover", "panel"].as_slice())
    );
    assert!(
        registry["wireframe"]
            .description
            .is_some_and(|d| d.contains("ASCII") && d.contains("box-drawing")),
        "wireframe block must advertise its ASCII / box-drawing authoring contract"
    );
}

#[test]
fn block_type_enum_covers_v1() {
    let types = vec![
        BlockType::RichText,
        BlockType::Diagram,
        BlockType::AnnotatedCode,
        BlockType::ApiEndpoint,
        BlockType::Decisions,
        BlockType::FileTree,
        BlockType::Diff,
        BlockType::Callout,
        BlockType::Checklist,
        BlockType::JsonExplorer,
        BlockType::Tabs,
        BlockType::Columns,
        BlockType::Wireframe,
        BlockType::QuestionForm,
    ];
    for bt in types {
        assert!(!bt.as_str().is_empty());
        assert!(!bt.tag().is_empty());
    }
    // Single-sourced coverage check: every canonical (type_str, tag) pair is
    // reachable from the enum's `as_str()`/`tag()` projection.
    assert_eq!(CANONICAL_BLOCK_TYPES.len(), 14);
}

#[test]
fn block_registry_new_has_all_definitions() {
    let reg = BlockRegistry::new();
    assert_eq!(reg.definitions().len(), 14);
    assert!(reg.definition_for_tag("RichText").is_some());
    assert!(reg.definition_for_tag("UnknownTag").is_none());
    assert!(reg.tags().contains("RichText"));
    assert!(!reg.tags().contains("UnknownTag"));
}

#[test]
fn registry_tags_match_canonical_v1_set() {
    // This test is the Rust-side parity guard for the TypeScript
    // CANONICAL_V1_TAGS array. If either side drifts, this assertion
    // (and the corresponding TS test) will fail. The expected set is now
    // single-sourced from the macro-emitted CANONICAL_BLOCK_TYPES.
    let expected: std::collections::HashSet<&str> =
        CANONICAL_BLOCK_TYPES.iter().map(|(_, tag)| *tag).collect();
    assert_eq!(
        expected.len(),
        14,
        "canonical block list must cover all 14 v1 tags"
    );
    let actual: std::collections::HashSet<&str> = proposal_block_tags().into_iter().collect();
    assert_eq!(
        actual, expected,
        "Rust registry tags do not match the canonical v1 set"
    );
}

#[test]
fn registry_type_strs_match_canonical_v1_set() {
    // The registry is keyed by the stable kebab-case type string; assert it
    // matches the macro-emitted canonical (type_str, _) list exactly.
    let expected: std::collections::HashSet<&str> =
        CANONICAL_BLOCK_TYPES.iter().map(|(ty, _)| *ty).collect();
    let actual: std::collections::HashSet<&str> =
        proposal_block_registry().keys().copied().collect();
    assert_eq!(
        actual, expected,
        "registry type-string keys do not match the canonical v1 set"
    );
}

#[test]
fn registry_contains_field_schemas() {
    let registry = proposal_block_registry();
    let diagram_type = registry["diagram"].fields["type"].clone();
    assert_eq!(diagram_type.field_type, "string");
    assert_eq!(
        diagram_type.enum_values.as_deref(),
        Some(["mermaid", "plantuml", "svg"].as_slice())
    );

    let question_kind = registry["question-form"].fields["questions"]
        .items
        .as_ref()
        .and_then(|items| items.fields.as_ref())
        .and_then(|fields| fields.get("kind"))
        .expect("question kind schema exists");
    assert_eq!(
        question_kind.enum_values.as_deref(),
        Some(["text", "single", "multi"].as_slice())
    );
}

/// The committed catalog JSON consumed by the TS side (a later PR) must stay in
/// sync with the macro-emitted `CANONICAL_BLOCK_TYPES`. This test re-serializes
/// the canonical list and diffs it against the committed file (regenerate-and-
/// diff style): set `UPDATE_PROPOSAL_BLOCK_CATALOG=1` to rewrite it.
#[test]
fn canonical_catalog_json_is_in_sync() {
    use std::path::Path;

    // Stable ordering: sort by type_str so the emitted JSON is deterministic
    // regardless of declaration order.
    // NB: keep the element type inferred (no explicit bare-Value annotation) —
    // `mcp_tools_do_not_use_untyped_json_output` greps the `tools/` tree for bare
    // serde Value type wrappers; `json!` + `collect::<Vec<_>>()` is byte-identical.
    let mut entries = CANONICAL_BLOCK_TYPES
        .iter()
        .map(|(ty, tag)| serde_json::json!({ "type": ty, "tag": tag }))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a["type"].as_str().unwrap().cmp(b["type"].as_str().unwrap()));
    let mut json = serde_json::to_string_pretty(&entries).expect("serialize catalog");
    json.push('\n');

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/proposal_blocks/proposal_block_catalog.json");

    if std::env::var("UPDATE_PROPOSAL_BLOCK_CATALOG").is_ok() {
        std::fs::write(&path, &json).expect("write catalog json");
        return;
    }

    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        on_disk, json,
        "proposal_block_catalog.json is out of sync with CANONICAL_BLOCK_TYPES; \
         regenerate with UPDATE_PROPOSAL_BLOCK_CATALOG=1"
    );
}

#[test]
fn parse_registered_mdx_blocks() {
    let body = r#"# Proposal

<RichText id="intro" content="Hello" />

<Diagram id='flow' type='mermaid'>
graph TD;
</Diagram>

<AnnotatedCode id="example" language="rust">
fn main() {}
</AnnotatedCode>"#;

    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].block_type, "rich-text");
    assert_eq!(blocks[0].tag, "RichText");
    assert_eq!(blocks[0].id, "intro");
    assert!(blocks[0].raw_content.is_empty());
    assert_eq!(blocks[1].block_type, "diagram");
    assert_eq!(blocks[1].tag, "Diagram");
    assert_eq!(blocks[1].id, "flow");
    assert_eq!(
        blocks[1].attributes.get("type").map(String::as_str),
        Some("mermaid")
    );
    assert!(blocks[1].raw_content.contains("graph TD"));
    assert_eq!(blocks[2].block_type, "annotated-code");
}

#[test]
fn parse_mdx_blocks_extracts_stable_ids() {
    let body = r#"
<FileTree id="repo-layout" name="repo" />
<ApiEndpoint id="create-user" method="POST" path="/users" />
"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].id, "repo-layout");
    assert_eq!(blocks[1].id, "create-user");
}

#[test]
fn parse_mdx_blocks_multiple_blocks_in_one_body() {
    let body = r#"# Proposal

<RichText id="intro" content="Hello" />
<Diagram id="flow" type="mermaid">
graph TD;
</Diagram>
<Decisions id="choices" />
<QuestionForm id="questions" title="Open questions" />
"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks[0].tag, "RichText");
    assert_eq!(blocks[1].tag, "Diagram");
    assert_eq!(blocks[2].tag, "Decisions");
    assert_eq!(blocks[3].tag, "QuestionForm");
}

#[test]
fn parse_mdx_blocks_empty_and_whitespace() {
    assert!(parse_mdx_blocks("").unwrap().is_empty());
    assert!(parse_mdx_blocks("   ").unwrap().is_empty());
    assert!(parse_mdx_blocks("\n\n  \n").unwrap().is_empty());
    assert!(
        parse_mdx_blocks("plain markdown with no blocks")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn validate_mdx_blocks_accepts_known_tags() {
    let body = r#"
<RichText id="intro" />
<Diagram id="flow" type="mermaid">
graph TD;
</Diagram>
"#;
    assert!(validate_mdx_blocks(body).is_ok());
}

#[test]
fn validate_mdx_blocks_rejects_empty_diagram() {
    // No source attribute and no children → broken "Empty mermaid diagram" box.
    let body = r#"<Diagram id="flow" type="mermaid" source="" />"#;
    let err = validate_mdx_blocks(body).unwrap_err();
    assert_eq!(err, BlockError::EmptyDiagram("flow".to_string()));
    assert!(err.to_string().contains("has no source"));
}

#[test]
fn validate_mdx_blocks_accepts_diagram_with_source_attribute() {
    // Source carried in the `source` attribute (the form agents author with).
    let body = r#"<Diagram id="flow" type="mermaid" source={`flowchart LR
  A[Start] --> B[End]`} />"#;
    assert!(validate_mdx_blocks(body).is_ok());
}

#[test]
fn validate_mdx_blocks_rejects_unknown_tag() {
    let body = r#"
<RichText id="intro" />
<FooBar id="bad" />
"#;
    let err = validate_mdx_blocks(body).unwrap_err();
    assert_eq!(err, BlockError::UnknownBlock("FooBar".to_string()));
    assert!(err.to_string().contains("Unknown MDX block tag: 'FooBar'"));
}

#[test]
fn validate_mdx_blocks_rejects_first_unknown_of_many() {
    let body = r#"
<RichText id="a" />
<BogusOne id="b" />
<AlsoBad id="c" />
"#;
    let err = validate_mdx_blocks(body).unwrap_err();
    assert_eq!(err, BlockError::UnknownBlock("BogusOne".to_string()));
}

#[test]
fn validate_mdx_blocks_empty_body() {
    assert!(validate_mdx_blocks("").is_ok());
    assert!(validate_mdx_blocks("   \n  ").is_ok());
}

#[test]
fn validate_mdx_blocks_ignores_lowercase_html() {
    let body = "<div>\n  <span>plain html</span>\n</div>";
    assert!(validate_mdx_blocks(body).is_ok());
}

#[test]
fn validate_mdx_blocks_nested_unknown_rejected() {
    let body = "<RichText>\n  <GhostBlock />\n</RichText>";
    let err = validate_mdx_blocks(body).unwrap_err();
    assert_eq!(err, BlockError::UnknownBlock("GhostBlock".to_string()));
}

#[test]
fn parse_mdx_blocks_returns_unclosed_error() {
    let body = "<Diagram id=\"flow\" type=\"mermaid\">\ngraph TD;";
    let err = parse_mdx_blocks(body).unwrap_err();
    assert_eq!(err, BlockError::UnclosedBlock("Diagram".to_string()));
}

#[test]
fn validate_ids_passes_with_unique_ids() {
    let blocks = vec![
        ParsedProposalBlock {
            block_type: "file-tree".to_string(),
            tag: "FileTree".to_string(),
            id: "schema-a".to_string(),
            attributes: HashMap::new(),
            raw_content: String::new(),
        },
        ParsedProposalBlock {
            block_type: "decisions".to_string(),
            tag: "Decisions".to_string(),
            id: "decisions-1".to_string(),
            attributes: HashMap::new(),
            raw_content: String::new(),
        },
    ];
    assert!(validate_block_ids(&blocks).is_ok());
}

#[test]
fn validate_ids_fails_on_empty() {
    let blocks = vec![ParsedProposalBlock {
        block_type: "file-tree".to_string(),
        tag: "FileTree".to_string(),
        id: String::new(),
        attributes: HashMap::new(),
        raw_content: String::new(),
    }];
    let err = validate_block_ids(&blocks).unwrap_err();
    assert!(err.contains("missing a required `id`"));
}

#[test]
fn validate_ids_fails_on_duplicate() {
    let blocks = vec![
        ParsedProposalBlock {
            block_type: "file-tree".to_string(),
            tag: "FileTree".to_string(),
            id: "same-id".to_string(),
            attributes: HashMap::new(),
            raw_content: String::new(),
        },
        ParsedProposalBlock {
            block_type: "decisions".to_string(),
            tag: "Decisions".to_string(),
            id: "same-id".to_string(),
            attributes: HashMap::new(),
            raw_content: String::new(),
        },
    ];
    let err = validate_block_ids(&blocks).unwrap_err();
    assert!(err.contains("duplicate block id"));
}

#[test]
fn test_validate_question_form_missing_ok() {
    // The question-form block is OPTIONAL: a proposal with zero of them (no open
    // questions) must be accepted.
    let body = r#"# Proposal

<FileTree id="schema" name="repo" />"#;

    assert!(validate_question_form_placement(body).is_ok());
}

#[test]
fn test_validate_question_form_none_at_all_ok() {
    // A body with no registered blocks at all is likewise accepted.
    let body = "# Proposal\n\nJust some prose, no blocks.";

    assert!(validate_question_form_placement(body).is_ok());
}

#[test]
fn test_validate_question_form_multiple_last_ok() {
    // Multiple question-form blocks are allowed as long as the final block is a
    // question-form (open questions render at the end).
    let body = r#"# Proposal

<QuestionForm id="questions-a" title="Open questions" />

<QuestionForm id="questions-b" title="More questions" />"#;

    assert!(validate_question_form_placement(body).is_ok());
}

#[test]
fn test_validate_question_form_not_last() {
    let body = r#"# Proposal

<QuestionForm id="questions" title="Open questions" />

<Decisions id="decisions" />"#;

    let err = validate_question_form_placement(body).unwrap_err();
    assert_eq!(
        err,
        "The question-form block must be the last block in the proposal body"
    );
}

#[test]
fn test_validate_question_form_valid() {
    let body = r#"# Proposal

<FileTree id="schema" name="repo" />

<QuestionForm id="questions" title="Open questions" />"#;

    assert!(validate_question_form_placement(body).is_ok());
}

#[test]
fn test_validate_question_form_markdown_skipped() {
    let body = "# Proposal\n\nPlain markdown proposal with no MDX blocks.";

    assert!(validate_question_form_placement_for_format(body, "markdown").is_ok());
}

#[test]
fn extract_tags_returns_registered_and_unknown() {
    let body = r#"
# Proposal

<RichText id="intro" />
<UnknownBlock id="x" />
<Diagram id="flow">
graph TD;
</Diagram>
"#;
    let tags = extract_custom_block_tags(body);
    // PascalCase tags, first-seen order, de-duplicated.
    assert_eq!(tags, vec!["RichText", "UnknownBlock", "Diagram"]);
}

#[test]
fn extract_tags_ignores_lowercase_and_closing_tags() {
    let body = "<div>\n<RichText />\n</RichText>\n</div>\n<span>hi</span>";
    let tags = extract_custom_block_tags(body);
    // `div`/`span` (lowercase) and `</RichText>` (closing) are ignored.
    assert_eq!(tags, vec!["RichText"]);
}

#[test]
fn extract_tags_dedupes_repeated_tags() {
    let body = "<RichText />\n<RichText />\n<Diagram />";
    let tags = extract_custom_block_tags(body);
    assert_eq!(tags, vec!["RichText", "Diagram"]);
}

#[test]
fn extract_tags_nested_blocks() {
    // A registered block nested inside another registered block — both
    // opening tags are extracted, enabling validation of nesting.
    let body = "<RichText>\n  <Diagram>\n    graph TD\n  </Diagram>\n</RichText>";
    let tags = extract_custom_block_tags(body);
    assert_eq!(tags, vec!["RichText", "Diagram"]);
}

#[test]
fn extract_tags_empty_body() {
    assert!(extract_custom_block_tags("").is_empty());
    assert!(extract_custom_block_tags("plain markdown only").is_empty());
}

// ── AST-parser parity tests (regressions the old regex could not handle) ──

#[test]
fn parse_attribute_value_containing_gt() {
    // The old regex's `([^>]*)` attribute group stopped at the first `>`,
    // truncating both the `path` attribute and the raw_content. The AST
    // walker reads the full attribute and keeps the children intact.
    let body = r#"<ApiEndpoint id="x" path="/a?to=>b">body</ApiEndpoint>"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].tag, "ApiEndpoint");
    assert_eq!(blocks[0].id, "x");
    assert_eq!(
        blocks[0].attributes.get("path").map(String::as_str),
        Some("/a?to=>b"),
        "attribute value with a `>` must be captured in full"
    );
    assert_eq!(blocks[0].raw_content, "body");
}

#[test]
fn parse_nested_same_tag_children_not_truncated() {
    // The old non-greedy `([\s\S]*?)</tag>` matched the FIRST close tag, so
    // a nested same-named block truncated the outer block's raw_content.
    // The AST slices the full children span between the OUTER open/close.
    let body =
        "<Callout id=\"outer\">\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n</Callout>";
    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 1, "only the outer block is a top-level block");
    assert_eq!(blocks[0].id, "outer");
    assert_eq!(
        blocks[0].raw_content, "\nbefore\n<Callout id=\"inner\">nested</Callout>\nafter\n",
        "outer raw_content must contain the whole nested same-tag child"
    );
}

#[test]
fn parse_json_expression_attribute_captured_as_raw_text() {
    // A `{...}` JSX attribute expression is stored as raw (brace-balanced)
    // text — never JS-evaluated — for forward-compat container blocks.
    let body = r#"<Diagram id="d" config={{ "a": 1 }} />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].tag, "Diagram");
    assert_eq!(
        blocks[0].attributes.get("config").map(String::as_str),
        Some(r#"{ "a": 1 }"#),
        "expression attribute must store the raw inner expression text"
    );
    assert!(blocks[0].raw_content.is_empty());
}

#[test]
fn parse_block_with_bare_json_children_no_error() {
    // JsonExplorer children are bare `{ ... }` JSON. The MDX expression
    // constructs are disabled, so this parses without error and the raw_content
    // bytes are preserved verbatim.
    let json =
        "\n{\n  \"id\": \"abc\",\n  \"nested\": { \"a\": [1, 2, 3] },\n  \"active\": true\n}\n";
    let body = format!("<JsonExplorer id=\"cfg\" title=\"Sample\">{json}</JsonExplorer>");
    let blocks = parse_mdx_blocks(&body).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].block_type, "json-explorer");
    assert_eq!(
        blocks[0].raw_content, json,
        "bare-brace JSON children must be preserved byte-for-byte"
    );
}

// ── get_block_catalog lean surface tests ──────────────────────────────

#[test]
fn proposal_block_catalog_returns_14_entries() {
    let catalog = proposal_block_catalog();
    assert_eq!(
        catalog.len(),
        14,
        "catalog must cover all 14 v1 block types"
    );
}

#[test]
fn proposal_block_catalog_entries_match_canonical_types() {
    let catalog = proposal_block_catalog();
    // The catalog is the lean (type, tag) projection of CANONICAL_BLOCK_TYPES.
    let expected: std::collections::HashSet<(&str, &str)> = CANONICAL_BLOCK_TYPES
        .iter()
        .map(|(ty, tag)| (*ty, *tag))
        .collect();
    let actual: std::collections::HashSet<(&str, &str)> = catalog
        .iter()
        .map(|entry| (entry.block_type.as_str(), entry.tag.as_str()))
        .collect();
    assert_eq!(
        actual, expected,
        "proposal_block_catalog() entries must match CANONICAL_BLOCK_TYPES"
    );
}

#[test]
fn proposal_block_catalog_sourced_from_json_file() {
    // Verify the catalog function deserializes from the committed JSON artifact
    // by round-tripping: read the file, parse it, and compare.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/proposal_blocks/proposal_block_catalog.json");
    let raw = std::fs::read_to_string(&path).expect("proposal_block_catalog.json must exist");
    let from_disk: Vec<super::types::BlockCatalogEntry> =
        serde_json::from_str(&raw).expect("proposal_block_catalog.json must be valid JSON");
    let from_fn = proposal_block_catalog();
    assert_eq!(
        from_fn, from_disk,
        "proposal_block_catalog() must return the same entries as the committed JSON file"
    );
}

#[test]
fn proposal_block_catalog_covers_all_registry_tags() {
    let catalog = proposal_block_catalog();
    let catalog_tags: std::collections::HashSet<&str> =
        catalog.iter().map(|e| e.tag.as_str()).collect();
    let registry_tags: std::collections::HashSet<&str> =
        proposal_block_tags().into_iter().collect();
    assert_eq!(
        catalog_tags, registry_tags,
        "catalog tags must be identical to registry tags"
    );
}

// ── drift-gate hardening (i8is) ──────────────────────────────────────

/// The committed `proposal_block_catalog.json` must never contain duplicate
/// `type` or `tag` values — drift would silently add a second vocabulary entry
/// that shadows or conflicts with an existing one.
#[test]
fn committed_catalog_json_has_unique_types_and_tags() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/proposal_blocks/proposal_block_catalog.json");
    let raw = std::fs::read_to_string(&path).expect("proposal_block_catalog.json must exist");
    let entries: Vec<super::types::BlockCatalogEntry> =
        serde_json::from_str(&raw).expect("proposal_block_catalog.json must be valid JSON");

    let mut seen_types = std::collections::HashSet::new();
    let mut seen_tags = std::collections::HashSet::new();
    for entry in &entries {
        assert!(
            seen_types.insert(&entry.block_type),
            "duplicate type in proposal_block_catalog.json: {:?}",
            entry.block_type
        );
        assert!(
            seen_tags.insert(&entry.tag),
            "duplicate tag in proposal_block_catalog.json: {:?}",
            entry.tag
        );
    }
}

/// Every catalog entry must have non-empty `type` and `tag` — the JSON shape
/// is otherwise valid (deserializes) but semantically broken.
#[test]
fn committed_catalog_json_entries_have_valid_shape() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/proposal_blocks/proposal_block_catalog.json");
    let raw = std::fs::read_to_string(&path).expect("proposal_block_catalog.json must exist");
    let entries: Vec<super::types::BlockCatalogEntry> =
        serde_json::from_str(&raw).expect("proposal_block_catalog.json must be valid JSON");

    assert!(
        !entries.is_empty(),
        "proposal_block_catalog.json must not be empty"
    );
    for entry in &entries {
        assert!(
            !entry.block_type.is_empty(),
            "catalog entry has empty type: {:?}",
            entry
        );
        assert!(
            !entry.tag.is_empty(),
            "catalog entry has empty tag: {:?}",
            entry
        );
    }
}

/// `proposal_block_catalog()` returns the same type/tag vocabulary as the
/// committed JSON file, sorted deterministically by `type` key, byte-for-byte.
/// This proves the catalog pull surface and the JSON artifact cannot drift.
#[test]
fn get_block_catalog_output_matches_committed_json_byte_for_byte() {
    use std::path::Path;

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/tools/proposal_blocks/proposal_block_catalog.json");
    let on_disk = std::fs::read_to_string(&path).expect("proposal_block_catalog.json must exist");

    // Reconstruct the expected JSON from the catalog function output.
    // The committed file is sorted by type; the function returns the same order.
    let mut catalog = proposal_block_catalog();
    catalog.sort_by(|a, b| a.block_type.cmp(&b.block_type));
    let mut reconstructed =
        serde_json::to_string_pretty(&catalog).expect("serialize catalog entries");
    reconstructed.push('\n');

    assert_eq!(
        on_disk, reconstructed,
        "proposal_block_catalog() output must match committed proposal_block_catalog.json byte-for-byte"
    );
}

/// The rich `proposal_block_registry()` keys (kebab-case type strings) must be
/// a superset-or-equal of the catalog types. Since both are 14 entries, this is
/// an exact set match — the catalog cannot introduce types absent from the
/// registry or vice-versa.
#[test]
fn registry_keys_match_catalog_types() {
    let catalog = proposal_block_catalog();
    let catalog_types: std::collections::HashSet<&str> =
        catalog.iter().map(|e| e.block_type.as_str()).collect();
    let registry_types: std::collections::HashSet<&str> =
        proposal_block_registry().keys().copied().collect();
    assert_eq!(
        catalog_types, registry_types,
        "catalog type strings must exactly match proposal_block_registry() keys"
    );
}

/// The rich registry's tag values must match the catalog's tag vocabulary.
/// Combined with `registry_keys_match_catalog_types`, this proves the registry
/// and the lean catalog cannot drift apart on either dimension.
#[test]
fn registry_tags_match_catalog_tags() {
    let catalog = proposal_block_catalog();
    let catalog_tags: std::collections::HashSet<&str> =
        catalog.iter().map(|e| e.tag.as_str()).collect();
    let registry_tags: std::collections::HashSet<&str> = proposal_block_registry()
        .values()
        .map(|def| def.tag)
        .collect();
    assert_eq!(
        catalog_tags, registry_tags,
        "catalog tags must exactly match proposal_block_registry() tag values"
    );
}

// ── Empty children-based block rejection (validate_block_content) ────────────
//
// A known block whose catalog grammar is CHILDREN-based (decisions, file-tree,
// checklist, diff, json-explorer, wireframe, callout) that arrives self-closing
// or with blank children validates as a "known tag" yet renders empty. These
// tests pin the write-time rejection and the actionable error text, plus the
// content-attribute escape hatch for the attribute-form blocks.

/// The exact production failure: `<Decisions ... decisions={[…]} />` written in
/// the unsupported self-closing attribute form is rejected, and the error tells
/// the author to write children markdown with `###` headings.
#[test]
fn block_content_rejects_self_closing_decisions_attr_form() {
    let body = r#"<Decisions id="auth" title="Auth" decisions={[{"decision":"JWT"}]} />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    let err = validate_block_content(&blocks).unwrap_err();
    assert!(err.contains("Decisions block"), "error was: {err}");
    assert!(
        err.contains("`auth`"),
        "error must name the block id: {err}"
    );
    assert!(
        err.contains("###"),
        "decisions error must direct the author to `###` heading children: {err}"
    );
}

/// A self-closing FileTree (children-only block) is rejected with an
/// id-naming, grammar-quoting error.
#[test]
fn block_content_rejects_self_closing_file_tree() {
    let body = r#"<FileTree id="layout" root="src" />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    let err = validate_block_content(&blocks).unwrap_err();
    assert!(err.contains("FileTree block"), "error was: {err}");
    assert!(err.contains("`layout`"), "error was: {err}");
    assert!(err.contains("children"), "error was: {err}");
}

/// A self-closing Checklist (children-only, no attribute alternative) is
/// rejected.
#[test]
fn block_content_rejects_self_closing_checklist() {
    let body = r#"<Checklist id="acceptance" />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    let err = validate_block_content(&blocks).unwrap_err();
    assert!(err.contains("Checklist block"), "error was: {err}");
    assert!(err.contains("`acceptance`"), "error was: {err}");
}

/// Blank (whitespace-only) children are treated the same as self-closing.
#[test]
fn block_content_rejects_blank_children() {
    let body = "<Callout id=\"c\" tone=\"warning\">\n   \n</Callout>";
    let blocks = parse_mdx_blocks(body).unwrap();
    let err = validate_block_content(&blocks).unwrap_err();
    assert!(err.contains("Callout block"), "error was: {err}");
}

/// A valid children-form Decisions block is accepted.
#[test]
fn block_content_accepts_children_form_decisions() {
    let body = "<Decisions id=\"auth\">\n### Use JWT for stateless auth\nStatus: accepted\n\nWe scale horizontally.\n</Decisions>";
    let blocks = parse_mdx_blocks(body).unwrap();
    assert!(validate_block_content(&blocks).is_ok());
}

/// The attribute-form blocks are accepted when their content attribute is
/// present: annotated-code `code=`, rich-text `content=`, tabs `tabs=`,
/// columns `columns=`.
#[test]
fn block_content_accepts_content_attribute_forms() {
    let ac = r#"<AnnotatedCode id="ex" language="rust" code={`fn main() {}`} />"#;
    assert!(validate_block_content(&parse_mdx_blocks(ac).unwrap()).is_ok());

    let rt = r#"<RichText id="intro" content="Hello" />"#;
    assert!(validate_block_content(&parse_mdx_blocks(rt).unwrap()).is_ok());

    let tabs = r#"<Tabs id="t" tabs={[{ "label": "A", "body": "hi" }]} />"#;
    assert!(validate_block_content(&parse_mdx_blocks(tabs).unwrap()).is_ok());

    let cols = r#"<Columns id="c" columns={[{ "body": "left" }, { "body": "right" }]} />"#;
    assert!(validate_block_content(&parse_mdx_blocks(cols).unwrap()).is_ok());
}

/// An attribute-form block with the content attribute MISSING is still rejected
/// (an empty RichText renders nothing).
#[test]
fn block_content_rejects_attribute_form_without_content() {
    let body = r#"<RichText id="intro" />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    let err = validate_block_content(&blocks).unwrap_err();
    assert!(err.contains("RichText block"), "error was: {err}");
    assert!(
        err.contains("`content`"),
        "error must mention the content attribute alternative: {err}"
    );
}

/// Blocks that are not content-required (e.g. api-endpoint, diagram) are not
/// subject to the empty-children guard here (diagram emptiness is guarded by
/// `validate_mdx_blocks`).
#[test]
fn block_content_ignores_non_content_required_blocks() {
    let body = r#"<ApiEndpoint id="get" method="GET" path="/x" />"#;
    let blocks = parse_mdx_blocks(body).unwrap();
    assert!(validate_block_content(&blocks).is_ok());
}
