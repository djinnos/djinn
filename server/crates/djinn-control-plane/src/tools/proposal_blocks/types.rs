//! Core data types for the proposal MDX block registry.
//!
//! These struct shapes back the `tool_schemas` insta snapshot — keep them
//! byte-identical (do NOT add, rename, or reorder fields).

use std::collections::{BTreeMap, HashMap, HashSet};

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::catalog::proposal_block_registry;

/// Errors produced by the block registry parser and validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// An unknown MDX block tag was encountered.
    UnknownBlock(String),
    /// A block is missing its required `id` attribute.
    MissingId(String),
    /// A block id is used more than once.
    DuplicateId(String),
    /// An opening tag has no matching closing tag.
    UnclosedBlock(String),
    /// A low-level regex or parsing failure.
    ParseError(String),
    /// A `Diagram` block carries no source (empty `source` attribute and empty
    /// children) — it would render as a broken "Empty mermaid diagram" box.
    EmptyDiagram(String),
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::UnknownBlock(tag) => write!(f, "Unknown MDX block tag: '{tag}'"),
            BlockError::MissingId(tag) => {
                write!(f, "{tag} block is missing a required `id` attribute")
            }
            BlockError::DuplicateId(id) => write!(f, "duplicate block id: `{id}`"),
            BlockError::UnclosedBlock(tag) => {
                write!(f, "unclosed <{tag}> block (no closing </{tag}> found)")
            }
            BlockError::ParseError(msg) => write!(f, "block parser error: {msg}"),
            BlockError::EmptyDiagram(id) => write!(
                f,
                "Diagram block `{id}` has no source — provide a non-empty `source` \
                 (e.g. `source={{`flowchart LR; A-->B`}}`) or block content"
            ),
        }
    }
}

impl std::error::Error for BlockError {}

/// Registry mapping each block type to its MDX tag name and field schema.
#[derive(Debug, Clone)]
pub struct BlockRegistry {
    definitions: BTreeMap<&'static str, ProposalBlockDefinition>,
}

impl BlockRegistry {
    /// Create a new registry populated with the v1 block definitions.
    pub fn new() -> Self {
        Self {
            definitions: proposal_block_registry(),
        }
    }

    /// Look up a block definition by its MDX tag name.
    pub fn definition_for_tag(&self, tag: &str) -> Option<&ProposalBlockDefinition> {
        self.definitions.values().find(|def| def.tag == tag)
    }

    /// Return the set of known MDX tag names.
    pub fn tags(&self) -> HashSet<&str> {
        self.definitions.values().map(|def| def.tag).collect()
    }

    /// Return all registered block definitions.
    pub fn definitions(&self) -> &BTreeMap<&'static str, ProposalBlockDefinition> {
        &self.definitions
    }
}

impl Default for BlockRegistry {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Optional authoring guidance for the LLM: how to encode this block's
    /// children/attributes. Absent for blocks whose shape is self-evident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'static str>,
    /// Field schema keyed by field name.
    pub fields: BTreeMap<&'static str, ProposalBlockFieldSchema>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalBlocksParams {}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProposalBlocksResponse {
    pub blocks: BTreeMap<&'static str, ProposalBlockDefinition>,
}

/// Params for the lean `get_block_catalog` tool (no fields required).
#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetBlockCatalogParams {}

/// A single entry in the lean block catalog: just the stable type tag and
/// MDX component tag name, loaded from `proposal_block_catalog.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlockCatalogEntry {
    /// Stable kebab-case block type, e.g. `annotated-code`.
    #[serde(rename = "type")]
    pub block_type: String,
    /// MDX component tag, e.g. `AnnotatedCode`.
    pub tag: String,
}

/// Response envelope for `get_block_catalog`: a lean list of type/tag pairs.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GetBlockCatalogResponse {
    pub blocks: Vec<BlockCatalogEntry>,
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
