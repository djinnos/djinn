// Block registry for MDX proposal bodies.
//
// Defines block types, their MDX tag names, expected attributes, and a simple
// parser that extracts blocks with stable IDs from MDX body text.
// Uses manual string parsing (no regex dependency).

use std::collections::HashMap;

// ── Block type definitions ──────────────────────────────────────────────────

/// Supported P2 block types for MDX proposal bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    DataModel,
    ApiEndpoint,
    Decisions,
    FileTree,
    QuestionForm,
}

impl BlockType {
    /// The MDX tag name (lowercase kebab-case).
    pub fn mdx_tag(&self) -> &'static str {
        match self {
            Self::DataModel => "data-model",
            Self::ApiEndpoint => "api-endpoint",
            Self::Decisions => "decisions",
            Self::FileTree => "file-tree",
            Self::QuestionForm => "question-form",
        }
    }

    /// Human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::DataModel => "Data Model",
            Self::ApiEndpoint => "API Endpoint",
            Self::Decisions => "Decisions",
            Self::FileTree => "File Tree",
            Self::QuestionForm => "Question Form",
        }
    }

    /// Required attributes for this block type (excluding the universal `id`).
    pub fn required_fields(&self) -> &[&str] {
        match self {
            Self::DataModel => &["title"],
            Self::ApiEndpoint => &["method", "path"],
            Self::Decisions => &[],
            Self::FileTree => &["root"],
            Self::QuestionForm => &[],
        }
    }

    /// Resolve a tag name to a `BlockType`, if it matches a known type.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "data-model" => Some(Self::DataModel),
            "api-endpoint" => Some(Self::ApiEndpoint),
            "decisions" => Some(Self::Decisions),
            "file-tree" => Some(Self::FileTree),
            "question-form" => Some(Self::QuestionForm),
            _ => None,
        }
    }
}

/// All known block types, for iteration.
pub const ALL_BLOCK_TYPES: &[BlockType] = &[
    BlockType::DataModel,
    BlockType::ApiEndpoint,
    BlockType::Decisions,
    BlockType::FileTree,
    BlockType::QuestionForm,
];

// ── Parsed block ────────────────────────────────────────────────────────────

/// A single block extracted from MDX body text.
#[derive(Debug, Clone)]
pub struct ParsedBlock {
    pub block_type: BlockType,
    pub id: String,
    pub attributes: HashMap<String, String>,
    pub raw_content: String,
}

// ── Parser ──────────────────────────────────────────────────────────────────

/// Parse attributes from the opening tag's attribute string (everything between
/// the tag name and the closing `>`). Handles `key="value"` pairs separated by
/// whitespace.
fn parse_attributes(raw: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    let chars: Vec<char> = raw.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Skip whitespace.
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || chars[i] == '>' {
            break;
        }
        // Read key (letters, digits, hyphens, underscores).
        let key_start = i;
        while i < len && (chars[i].is_ascii_alphanumeric() || chars[i] == '-' || chars[i] == '_') {
            i += 1;
        }
        if i == key_start {
            break; // no valid key found
        }
        let key: String = chars[key_start..i].iter().collect();

        // Skip whitespace around '='.
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || chars[i] != '=' {
            break;
        }
        i += 1; // skip '='
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        // Read value (quoted).
        if i >= len || chars[i] != '"' {
            break;
        }
        i += 1; // skip opening quote
        let val_start = i;
        while i < len && chars[i] != '"' {
            i += 1;
        }
        if i >= len {
            break; // unterminated quote
        }
        let value: String = chars[val_start..i].iter().collect();
        i += 1; // skip closing quote

        attrs.insert(key, value);
    }
    attrs
}

/// Find a block in the body text starting from `offset`.
/// Returns `Some((tag_name, attrs_str, content, end_offset))` if found.
fn find_next_block(body: &str, offset: usize) -> Option<(String, String, String, usize)> {
    let remaining = &body[offset..];

    // Find opening '<' that starts a tag.
    let mut search_from = 0;
    loop {
        let open_idx = remaining[search_from..].find('<')?;
        let abs_open = search_from + open_idx;
        let after_open = &remaining[abs_open + 1..];

        // Read tag name (letters, digits, hyphens).
        let tag_end_in_after = after_open
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(after_open.len());
        if tag_end_in_after == 0 {
            search_from = abs_open + 1;
            continue;
        }
        let tag_name = &after_open[..tag_end_in_after];

        // Skip self-closing tags like `<br/>` and non-block tags.
        // Find end of opening tag '>'.
        let gt_in_after = match after_open.find('>') {
            Some(i) => i,
            None => return None,
        };
        let attrs_str = after_open[tag_end_in_after..gt_in_after].trim();

        // Look for matching closing tag.
        let closing_tag = format!("</{}>", tag_name);
        let content_start_in_after = gt_in_after + 1;
        let after_content_start = &after_open[content_start_in_after..];

        match after_content_start.find(&closing_tag) {
            Some(close_pos) => {
                let content = &after_content_start[..close_pos];
                let end_offset =
                    offset + abs_open + 1 + content_start_in_after + close_pos + closing_tag.len();
                return Some((
                    tag_name.to_string(),
                    attrs_str.to_string(),
                    content.to_string(),
                    end_offset,
                ));
            }
            None => {
                search_from = abs_open + 1;
                continue;
            }
        }
    }
}

/// Extract blocks from MDX body text.
///
/// Returns a `Vec<ParsedBlock>` for each recognized block type found. Unknown
/// tag names are silently skipped (they may be regular HTML elements).
pub fn parse_mdx_blocks(body: &str) -> Result<Vec<ParsedBlock>, String> {
    let mut blocks = Vec::new();
    let mut offset = 0;

    while let Some((tag_name, attrs_str, content, end)) = find_next_block(body, offset) {
        offset = end;
        let Some(block_type) = BlockType::from_tag(&tag_name) else {
            continue;
        };

        let attributes = parse_attributes(&attrs_str);
        let id = attributes.get("id").cloned().unwrap_or_default();

        blocks.push(ParsedBlock {
            block_type,
            id,
            attributes,
            raw_content: content,
        });
    }
    Ok(blocks)
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Validate that all blocks have non-empty `id` attributes and that IDs are
/// unique within the proposal.
pub fn validate_block_ids(blocks: &[ParsedBlock]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for block in blocks {
        if block.id.is_empty() {
            return Err(format!(
                "{} block is missing a required `id` attribute",
                block.block_type.mdx_tag()
            ));
        }
        if !seen.insert(&block.id) {
            return Err(format!(
                "duplicate block id `{}` — ids must be unique within a proposal",
                block.id
            ));
        }
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_data_model_block() {
        let body = r#"Some intro text.

<data-model id="user-schema" title="User Schema">
field definitions here
</data-model>

More text."#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::DataModel);
        assert_eq!(blocks[0].id, "user-schema");
        assert_eq!(
            blocks[0].attributes.get("title").map(|s| s.as_str()),
            Some("User Schema")
        );
        assert!(blocks[0].raw_content.contains("field definitions"));
    }

    #[test]
    fn parse_multiple_block_types() {
        let body = r#"
<api-endpoint id="create-user" method="POST" path="/api/users">
Create a new user
</api-endpoint>

<decisions id="auth-choice">
We chose JWT over sessions.
</decisions>

<file-tree id="structure" root="src/">
src/
  main.rs
  lib.rs
</file-tree>
"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].block_type, BlockType::ApiEndpoint);
        assert_eq!(blocks[0].id, "create-user");
        assert_eq!(
            blocks[0].attributes.get("method").map(|s| s.as_str()),
            Some("POST")
        );
        assert_eq!(
            blocks[0].attributes.get("path").map(|s| s.as_str()),
            Some("/api/users")
        );
        assert_eq!(blocks[1].block_type, BlockType::Decisions);
        assert_eq!(blocks[1].id, "auth-choice");
        assert_eq!(blocks[2].block_type, BlockType::FileTree);
        assert_eq!(blocks[2].id, "structure");
    }

    #[test]
    fn parse_question_form() {
        let body = r#"## Open Questions

<question-form id="open-questions">
What should the auth strategy be?
</question-form>"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::QuestionForm);
        assert_eq!(blocks[0].id, "open-questions");
    }

    #[test]
    fn skips_unknown_tags() {
        let body = r#"<div class="foo">not a block</div>
<data-model id="known" title="Known">content</data-model>"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::DataModel);
    }

    #[test]
    fn validate_non_empty_ids() {
        let blocks = vec![ParsedBlock {
            block_type: BlockType::DataModel,
            id: "".to_string(),
            attributes: HashMap::new(),
            raw_content: "".to_string(),
        }];
        let result = validate_block_ids(&blocks);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing a required `id`"));
    }

    #[test]
    fn validate_unique_ids() {
        let blocks = vec![
            ParsedBlock {
                block_type: BlockType::DataModel,
                id: "dup".to_string(),
                attributes: HashMap::new(),
                raw_content: "".to_string(),
            },
            ParsedBlock {
                block_type: BlockType::Decisions,
                id: "dup".to_string(),
                attributes: HashMap::new(),
                raw_content: "".to_string(),
            },
        ];
        let result = validate_block_ids(&blocks);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("duplicate block id"));
    }

    #[test]
    fn validate_passes_with_unique_ids() {
        let blocks = vec![
            ParsedBlock {
                block_type: BlockType::DataModel,
                id: "a".to_string(),
                attributes: HashMap::new(),
                raw_content: "".to_string(),
            },
            ParsedBlock {
                block_type: BlockType::Decisions,
                id: "b".to_string(),
                attributes: HashMap::new(),
                raw_content: "".to_string(),
            },
        ];
        assert!(validate_block_ids(&blocks).is_ok());
    }

    #[test]
    fn block_type_from_tag_round_trips() {
        for bt in ALL_BLOCK_TYPES {
            assert_eq!(BlockType::from_tag(bt.mdx_tag()), Some(*bt));
        }
        assert_eq!(BlockType::from_tag("not-a-block"), None);
    }

    #[test]
    fn block_type_required_fields() {
        assert_eq!(BlockType::DataModel.required_fields(), &["title"]);
        assert_eq!(
            BlockType::ApiEndpoint.required_fields(),
            &["method", "path"]
        );
        assert!(BlockType::Decisions.required_fields().is_empty());
        assert_eq!(BlockType::FileTree.required_fields(), &["root"]);
        assert!(BlockType::QuestionForm.required_fields().is_empty());
    }

    #[test]
    fn parse_empty_body() {
        let blocks = parse_mdx_blocks("").unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn parse_no_blocks() {
        let body = "Just some regular markdown text.\n\nNo blocks here.";
        let blocks = parse_mdx_blocks(body).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn nested_angle_brackets_in_content() {
        let body = r#"<data-model id="nested" title="Nested">
Some content with <em>markup</em> inside.
</data-model>"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, "nested");
        assert!(blocks[0].raw_content.contains("<em>markup</em>"));
    }

    #[test]
    fn parse_attributes_basic() {
        let attrs = parse_attributes(r#"id="foo" title="Bar""#);
        assert_eq!(attrs.get("id").map(|s| s.as_str()), Some("foo"));
        assert_eq!(attrs.get("title").map(|s| s.as_str()), Some("Bar"));
    }
}
