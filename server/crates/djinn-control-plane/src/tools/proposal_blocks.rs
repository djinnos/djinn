// Block registry for MDX-aware proposals.
//
// Defines the P2 block types with their MDX tag names, expected attributes,
// and a simple regex-based parser that extracts blocks with stable IDs from
// MDX body text. When `body_format == 'mdx'`, the parser is used for
// validation in proposal_create / proposal_update.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── Block type definitions ──────────────────────────────────────────────────

/// Every P2 block type carries an MDX tag name, a human display name, and a
/// list of required attribute names (beyond the always-required `id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockType {
    DataModel,
    ApiEndpoint,
    Decisions,
    FileTree,
    QuestionForm,
}

impl BlockType {
    /// The MDX tag name used in proposal bodies (e.g. `<data-model>`).
    pub fn mdx_tag(&self) -> &'static str {
        match self {
            Self::DataModel => "data-model",
            Self::ApiEndpoint => "api-endpoint",
            Self::Decisions => "decisions",
            Self::FileTree => "file-tree",
            Self::QuestionForm => "question-form",
        }
    }

    /// Human-readable display name for UI/reporting.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::DataModel => "Data Model",
            Self::ApiEndpoint => "API Endpoint",
            Self::Decisions => "Decisions",
            Self::FileTree => "File Tree",
            Self::QuestionForm => "Question Form",
        }
    }

    /// Attribute names required on this block type beyond the universal `id`.
    pub fn required_fields(&self) -> &'static [&'static str] {
        match self {
            Self::DataModel => &["title"],
            Self::ApiEndpoint => &["method", "path"],
            Self::Decisions => &[],
            Self::FileTree => &["root"],
            Self::QuestionForm => &[],
        }
    }

    /// All known block types.
    pub fn all() -> &'static [BlockType] {
        &[
            Self::DataModel,
            Self::ApiEndpoint,
            Self::Decisions,
            Self::FileTree,
            Self::QuestionForm,
        ]
    }

    /// Try to match a tag name to a known block type.
    pub fn from_tag(tag: &str) -> Option<BlockType> {
        Self::all()
            .iter()
            .find(|bt| bt.mdx_tag() == tag)
            .copied()
    }
}

// ── Parsed block ────────────────────────────────────────────────────────────

/// A single block extracted from MDX body text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedBlock {
    pub block_type: BlockType,
    /// Stable identifier for this block (from the `id` attribute).
    pub id: String,
    /// All attributes from the opening tag (including `id`).
    pub attributes: HashMap<String, String>,
    /// Raw content between the opening and closing tags.
    pub raw_content: String,
}

// ── MDX block parser ────────────────────────────────────────────────────────

/// Extract all known blocks from an MDX body string.
///
/// Uses a regex to find opening tags `<tagname ...>`, then searches for the
/// matching closing tag `</tagname>` to extract block content. Returns an
/// error for malformed blocks.
pub fn parse_mdx_blocks(body: &str) -> Result<Vec<ParsedBlock>, String> {
    // Regex to find opening tags: `<tagname ...>` where tagname is lowercase with hyphens.
    let re = regex::Regex::new(r"<([a-z][a-z0-9-]*)([^>]*)>")
        .map_err(|e| format!("block parser regex error: {e}"))?;

    let mut blocks = Vec::new();

    for cap in re.captures_iter(body) {
        let tag_name = &cap[1];
        let attrs_str = &cap[2];
        let open_end = cap.get(0).unwrap().end();

        let block_type = match BlockType::from_tag(tag_name) {
            Some(bt) => bt,
            None => {
                // Skip unknown tags — they may be HTML or other MDX components.
                continue;
            }
        };

        // Find the matching closing tag.
        let closing_tag = format!("</{}>", tag_name);
        let close_start = body[open_end..]
            .find(&closing_tag)
            .map(|pos| open_end + pos)
            .ok_or_else(|| {
                format!("unclosed <{}> block (no closing {} found)", tag_name, closing_tag)
            })?;

        let raw_content = body[open_end..close_start].to_string();

        let attributes = parse_attributes(attrs_str);
        let id = attributes
            .get("id")
            .cloned()
            .unwrap_or_default();

        blocks.push(ParsedBlock {
            block_type,
            id,
            attributes,
            raw_content,
        });
    }

    Ok(blocks)
}

/// Parse attributes from an opening tag's attribute string.
///
/// Handles `key="value"` and `key='value'` pairs. Returns a map of
/// attribute name → value.
fn parse_attributes(attrs_str: &str) -> HashMap<String, String> {
    let re = regex::Regex::new(r#"([a-z][a-z0-9_-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
        .expect("attr regex should compile");
    let mut map = HashMap::new();
    for cap in re.captures_iter(attrs_str) {
        let key = cap[1].to_string();
        let value = cap.get(2).or(cap.get(3)).map(|m| m.as_str().to_string()).unwrap_or_default();
        map.insert(key, value);
    }
    map
}

// ── Block ID validation ─────────────────────────────────────────────────────

/// Ensure all blocks have non-empty `id` attributes and that IDs are unique
/// within the proposal.
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
                "duplicate block id: `{}`",
                block.block_id()
            ));
        }
    }
    Ok(())
}

impl ParsedBlock {
    /// Convenience accessor for the block id.
    pub fn block_id(&self) -> &str {
        &self.id
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_block() {
        let body = r#"# Proposal

Some intro text.

<data-model id="user-schema" title="User Schema">
field: name (string)
field: email (string)
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
        assert!(blocks[0].raw_content.contains("field: name"));
    }

    #[test]
    fn parse_multiple_blocks() {
        let body = r#"
<api-endpoint id="create-user" method="POST" path="/api/users">
Create a new user.
</api-endpoint>

<decisions id="auth-choice">
Use JWT for authentication.
</decisions>

<file-tree id="project-layout" root=".">
src/
  main.rs
Cargo.toml
</file-tree>
"#;

        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].block_type, BlockType::ApiEndpoint);
        assert_eq!(blocks[0].id, "create-user");
        assert_eq!(blocks[1].block_type, BlockType::Decisions);
        assert_eq!(blocks[1].id, "auth-choice");
        assert_eq!(blocks[2].block_type, BlockType::FileTree);
        assert_eq!(blocks[2].id, "project-layout");
    }

    #[test]
    fn validate_ids_passes_with_unique_ids() {
        let blocks = vec![
            ParsedBlock {
                block_type: BlockType::DataModel,
                id: "schema-a".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
            ParsedBlock {
                block_type: BlockType::Decisions,
                id: "decisions-1".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
        ];
        assert!(validate_block_ids(&blocks).is_ok());
    }

    #[test]
    fn validate_ids_fails_on_empty() {
        let blocks = vec![ParsedBlock {
            block_type: BlockType::DataModel,
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
            ParsedBlock {
                block_type: BlockType::DataModel,
                id: "same-id".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
            ParsedBlock {
                block_type: BlockType::Decisions,
                id: "same-id".to_string(),
                attributes: HashMap::new(),
                raw_content: String::new(),
            },
        ];
        let err = validate_block_ids(&blocks).unwrap_err();
        assert!(err.contains("duplicate block id"));
    }

    #[test]
    fn parse_unknown_tags_skipped() {
        let body = r#"<div class="custom">content</div>
<question-form id="open-questions">
What needs clarification?
</question-form>"#;

        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, BlockType::QuestionForm);
        assert_eq!(blocks[0].id, "open-questions");
    }

    #[test]
    fn parse_attributes_handles_single_quotes() {
        let body = r#"<data-model id='test-id' title='Test'>content</data-model>"#;
        let blocks = parse_mdx_blocks(body).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, "test-id");
        assert_eq!(
            blocks[0].attributes.get("title").map(|s| s.as_str()),
            Some("Test")
        );
    }

    #[test]
    fn block_type_from_tag_roundtrip() {
        for bt in BlockType::all() {
            assert_eq!(BlockType::from_tag(bt.mdx_tag()), Some(*bt));
        }
        assert_eq!(BlockType::from_tag("unknown-tag"), None);
    }
}
