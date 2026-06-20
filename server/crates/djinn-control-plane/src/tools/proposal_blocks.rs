//! Proposal MDX block registry and introspection tool.
//!
//! This module is the Rust source for the v1 proposal block contract: each
//! stable block type maps to the MDX tag clients should emit plus the field
//! schema workers can use when validating or generating proposal bodies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockFieldSchema {
    /// Primitive schema kind: `string`, `boolean`, `object`, or `array`.
    #[serde(rename = "type")]
    pub field_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<BTreeMap<&'static str, ProposalBlockFieldSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<ProposalBlockFieldSchema>>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlockDefinition {
    /// Stable proposal block type identifier, e.g. `annotated-code`.
    #[serde(rename = "type")]
    pub block_type: &'static str,
    /// MDX component tag, e.g. `AnnotatedCode`.
    pub tag: &'static str,
    /// Field schema keyed by field name.
    pub fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalBlocksParams {}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlocksResponse {
    pub blocks: BTreeMap<&'static str, ProposalBlockDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedProposalBlock {
    /// Stable proposal block type identifier, e.g. `annotated-code`.
    pub block_type: String,
    /// MDX component tag found in the body, e.g. `AnnotatedCode`.
    pub tag: String,
    /// Stable identifier for this block from the optional `id` attribute.
    pub id: String,
    /// All attributes from the opening tag, including `id` when present.
    pub attributes: HashMap<String, String>,
    /// Raw content between opening and closing tags.
    pub raw_content: String,
}

impl ParsedProposalBlock {
    pub fn block_id(&self) -> &str {
        &self.id
    }
}

fn string_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn boolean_field() -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "boolean",
        enum_values: None,
        fields: None,
        items: None,
    }
}

fn enum_string_field(values: Vec<&'static str>) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "string",
        enum_values: Some(values),
        fields: None,
        items: None,
    }
}

fn object_field(
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "object",
        enum_values: None,
        fields: Some(fields),
        items: None,
    }
}

fn array_field(items: ProposalBlockFieldSchema) -> ProposalBlockFieldSchema {
    ProposalBlockFieldSchema {
        field_type: "array",
        enum_values: None,
        fields: None,
        items: Some(Box::new(items)),
    }
}

fn fields(
    entries: Vec<(&'static str, ProposalBlockFieldSchema)>,
) -> BTreeMap<&'static str, ProposalBlockFieldSchema> {
    entries.into_iter().collect()
}

fn block(
    block_type: &'static str,
    tag: &'static str,
    fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
) -> ProposalBlockDefinition {
    ProposalBlockDefinition {
        block_type,
        tag,
        fields,
    }
}

pub static PROPOSAL_BLOCK_REGISTRY: LazyLock<BTreeMap<&'static str, ProposalBlockDefinition>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                "rich-text",
                block(
                    "rich-text",
                    "RichText",
                    fields(vec![("content", string_field())]),
                ),
            ),
            (
                "diagram",
                block(
                    "diagram",
                    "Diagram",
                    fields(vec![
                        (
                            "type",
                            enum_string_field(vec!["mermaid", "plantuml", "svg"]),
                        ),
                        ("source", string_field()),
                    ]),
                ),
            ),
            (
                "annotated-code",
                block(
                    "annotated-code",
                    "AnnotatedCode",
                    fields(vec![
                        ("language", string_field()),
                        ("code", string_field()),
                        (
                            "annotations",
                            array_field(object_field(fields(vec![
                                ("line", string_field()),
                                ("note", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "data-model",
                block(
                    "data-model",
                    "DataModel",
                    fields(vec![
                        ("name", string_field()),
                        (
                            "fields",
                            array_field(object_field(fields(vec![
                                ("name", string_field()),
                                ("type", string_field()),
                                ("optional", boolean_field()),
                                ("description", string_field()),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "api-endpoint",
                block(
                    "api-endpoint",
                    "ApiEndpoint",
                    fields(vec![
                        ("method", string_field()),
                        ("path", string_field()),
                        ("description", string_field()),
                        ("request_schema", string_field()),
                        ("response_schema", string_field()),
                    ]),
                ),
            ),
            (
                "decisions",
                block(
                    "decisions",
                    "Decisions",
                    fields(vec![(
                        "items",
                        array_field(object_field(fields(vec![
                            ("decision", string_field()),
                            ("rationale", string_field()),
                            ("status", string_field()),
                        ]))),
                    )]),
                ),
            ),
            (
                "file-tree",
                block(
                    "file-tree",
                    "FileTree",
                    fields(vec![
                        ("root", string_field()),
                        (
                            "entries",
                            array_field(object_field(fields(vec![
                                ("path", string_field()),
                                ("kind", enum_string_field(vec!["file", "dir"])),
                            ]))),
                        ),
                    ]),
                ),
            ),
            (
                "question-form",
                block(
                    "question-form",
                    "QuestionForm",
                    fields(vec![
                        ("title", string_field()),
                        (
                            "questions",
                            array_field(object_field(fields(vec![
                                ("question", string_field()),
                                ("kind", enum_string_field(vec!["text", "single", "multi"])),
                                ("options", array_field(string_field())),
                            ]))),
                        ),
                    ]),
                ),
            ),
        ])
    });

pub fn proposal_block_registry() -> BTreeMap<&'static str, ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY.clone()
}

pub fn proposal_block_definition_for_tag(tag: &str) -> Option<&'static ProposalBlockDefinition> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .find(|definition| definition.tag == tag)
}

pub fn proposal_block_tags() -> HashSet<&'static str> {
    PROPOSAL_BLOCK_REGISTRY
        .values()
        .map(|definition| definition.tag)
        .collect()
}

/// Extract registered proposal MDX blocks from a body string.
///
/// This lightweight parser is intended for validation/introspection workflows,
/// not for rendering arbitrary MDX. It recognizes the PascalCase component tags
/// published in [`PROPOSAL_BLOCK_REGISTRY`] and skips unrelated HTML/MDX tags.
pub fn parse_mdx_blocks(body: &str) -> Result<Vec<ParsedProposalBlock>, String> {
    let re = regex::Regex::new(r"<([A-Z][A-Za-z0-9]*)([^>]*)>")
        .map_err(|e| format!("block parser regex error: {e}"))?;
    let mut blocks = Vec::new();

    for cap in re.captures_iter(body) {
        let tag = &cap[1];
        let Some(definition) = proposal_block_definition_for_tag(tag) else {
            continue;
        };
        let attrs_str = &cap[2];
        let open_end = cap.get(0).expect("full match exists").end();
        let attributes = parse_attributes(attrs_str);
        let id = attributes.get("id").cloned().unwrap_or_default();
        let raw_content = if attrs_str.trim_end().ends_with('/') {
            String::new()
        } else {
            let closing_tag = format!("</{tag}>");
            let close_start = body[open_end..]
                .find(&closing_tag)
                .map(|pos| open_end + pos)
                .ok_or_else(|| {
                    format!("unclosed <{tag}> block (no closing {closing_tag} found)")
                })?;
            body[open_end..close_start].to_string()
        };

        blocks.push(ParsedProposalBlock {
            block_type: definition.block_type.to_string(),
            tag: tag.to_string(),
            id,
            attributes,
            raw_content,
        });
    }

    Ok(blocks)
}

/// Parse `key="value"` and `key='value'` attributes from an MDX opening tag.
fn parse_attributes(attrs_str: &str) -> HashMap<String, String> {
    let re = regex::Regex::new(r#"([A-Za-z_][A-Za-z0-9_-]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
        .expect("attr regex should compile");
    let mut map = HashMap::new();
    for cap in re.captures_iter(attrs_str) {
        let key = cap[1].to_string();
        let value = cap
            .get(2)
            .or_else(|| cap.get(3))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        map.insert(key, value);
    }
    map
}

/// Ensure all parsed blocks have non-empty, unique `id` attributes.
pub fn validate_block_ids(blocks: &[ParsedProposalBlock]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for block in blocks {
        if block.id.is_empty() {
            return Err(format!(
                "{} block is missing a required `id` attribute",
                block.tag
            ));
        }
        if !seen.insert(block.id.as_str()) {
            return Err(format!("duplicate block id: `{}`", block.block_id()));
        }
    }
    Ok(())
}

#[tool_router(router = proposal_blocks_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Return the v1 proposal MDX block registry, including stable block types, MDX tags, and field schemas.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn proposal_blocks(
        &self,
        Parameters(_): Parameters<ProposalBlocksParams>,
    ) -> Json<ProposalBlocksResponse> {
        Json(ProposalBlocksResponse {
            blocks: proposal_block_registry(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_v1_blocks() {
        let registry = proposal_block_registry();
        assert_eq!(registry.len(), 8);
        assert_eq!(registry["rich-text"].tag, "RichText");
        assert_eq!(registry["diagram"].tag, "Diagram");
        assert_eq!(registry["annotated-code"].tag, "AnnotatedCode");
        assert_eq!(registry["data-model"].tag, "DataModel");
        assert_eq!(registry["api-endpoint"].tag, "ApiEndpoint");
        assert_eq!(registry["decisions"].tag, "Decisions");
        assert_eq!(registry["file-tree"].tag, "FileTree");
        assert_eq!(registry["question-form"].tag, "QuestionForm");
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
    fn validate_ids_passes_with_unique_ids() {
        let blocks = vec![
            ParsedProposalBlock {
                block_type: "data-model".to_string(),
                tag: "DataModel".to_string(),
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
            block_type: "data-model".to_string(),
            tag: "DataModel".to_string(),
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
                block_type: "data-model".to_string(),
                tag: "DataModel".to_string(),
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
}
